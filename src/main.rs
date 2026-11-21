mod csv;
mod ids;
mod output;
mod ts;

use hashbrown::HashMap;
use ids::{Longs, mix, pack, pack_rec};
use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use ts::{DAY, parse_ts};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 4 << 20;

macro_rules! log {
    ($t0:expr, $($arg:tt)*) => {
        eprintln!("[{:>8.3}s] {}", $t0.elapsed().as_secs_f64(), format!($($arg)*))
    };
}

/// Parsed event row (one copy). `ok` folds validation and lateness.
#[derive(Clone, Copy)]
struct Ev {
    eid: u128,
    user: u128,
    ingest: i32, // seconds from window start (ingest_ts is always whole-second F1)
    ts: u32,     // ms from window start
    price: u32,
    cat: u8,
    ok: bool,
}

/// Surviving event, keyed by person.
#[derive(Clone, Copy)]
struct SEv {
    person: u128,
    ts: u32, // ms from window start
    price: u32,
    h: u32, // high bits of mix(person): sort key prefix
    cat: u8,
}

/// Valid click.
#[derive(Clone, Copy)]
struct Ck {
    cid: u128,
    user: u128,
    ts: i64,
    h: u32, // high bits of mix(person), once mapped
    camp: u16,
}

/// Exact lookup by ID: a direct array when every key is `prefix` + a canonical
/// decimal number (no leading zero), else a hash map. With the array, any ID
/// not of that shape is certainly absent.
struct IdMap<T: Copy> {
    prefix: &'static [u8],
    direct: Option<Vec<Option<T>>>,
    map: HashMap<u128, T>,
}

fn canonical_number(prefix: &[u8], id: &[u8]) -> Option<usize> {
    let d = id.strip_prefix(prefix)?;
    if d.is_empty() || d.len() > 7 || d[0] == b'0' {
        return None;
    }
    let mut v = 0usize;
    for &c in d {
        let x = c.wrapping_sub(b'0');
        if x >= 10 {
            return None;
        }
        v = v * 10 + x as usize;
    }
    Some(v)
}

impl<T: Copy> IdMap<T> {
    fn new(prefix: &'static [u8], items: &[(Vec<u8>, T)]) -> Self {
        let map: HashMap<u128, T> = items.iter().map(|(k, v)| (pack(k), *v)).collect();
        let nums: Option<Vec<usize>> = items.iter().map(|(k, _)| canonical_number(prefix, k)).collect();
        let direct = nums.and_then(|nums| {
            let max = nums.iter().copied().max().unwrap_or(0);
            if max > 4_000_000 {
                return None;
            }
            let mut d = vec![None; max + 1];
            for (n, (_, v)) in nums.iter().zip(items) {
                d[*n] = Some(*v);
            }
            Some(d)
        });
        IdMap { prefix, direct, map }
    }
    #[inline(always)]
    fn get(&self, id: &[u8]) -> Option<T> {
        match &self.direct {
            Some(d) => canonical_number(self.prefix, id).and_then(|n| d.get(n).copied().flatten()),
            None => self.map.get(&pack(id)).copied(),
        }
    }
    fn len(&self) -> usize {
        self.map.len()
    }
}

pub struct Lookups {
    products: IdMap<u8>,
    pub categories: Vec<Vec<u8>>, // sorted byte-wise; includes "unknown"
    unknown: u8,
    campaigns: IdMap<u16>,
    pub campaign_names: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>, // per campaign index; last = organic
}

struct Ctx {
    eb: usize, // event-id buckets (power of two)
    ws: i64,
    we: i64,
    lk: Lookups,
}

#[derive(Default)]
struct ParseAcc {
    ev: Vec<Vec<Ev>>,
    ck: Vec<Ck>,
    links: Vec<(u128, u128)>,
    longs: Vec<(u128, Box<[u8]>)>,
    seps: csv::Seps,
}

fn read_small_csv(path: &Path) -> Vec<[Vec<u8>; 2]> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut out = Vec::new();
    csv::for_lines(&data, 0, data.len(), |l| {
        if let Some([a, b]) = csv::split::<2>(l) {
            out.push([a.to_vec(), b.to_vec()]);
        }
    });
    out
}

fn manifest_value(text: &str, key: &str) -> i64 {
    let at = text.find(&format!("\"{key}\"")).expect("manifest key") + key.len() + 2;
    let rest = &text[at..];
    let colon = rest.find(':').unwrap();
    let rest = &rest[colon + 1..];
    let q1 = rest.find('"').unwrap();
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"').unwrap();
    parse_ts(rest[..q2].as_bytes()).expect("manifest timestamp")
}

fn load_lookups(data: &Path) -> Lookups {
    let prows = read_small_csv(&data.join("products.csv"));
    let mut cats: Vec<Vec<u8>> = prows.iter().map(|r| r[1].clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    assert!(cats.len() < 255, "too many categories");
    let cat_index: HashMap<Vec<u8>, u8> =
        cats.iter().enumerate().map(|(i, c)| (c.clone(), i as u8)).collect();
    let unknown = cat_index[&b"unknown"[..]];
    let pitems: Vec<(Vec<u8>, u8)> = prows.iter().map(|r| (r[0].clone(), cat_index[&r[1]])).collect();
    let products = IdMap::new(b"p", &pitems);

    let crows = read_small_csv(&data.join("campaigns.csv"));
    let mut citems: Vec<(Vec<u8>, u16)> = Vec::new();
    let mut campaign_names = Vec::new();
    let mut channels = Vec::new();
    for r in crows {
        citems.push((r[0].clone(), campaign_names.len() as u16));
        campaign_names.push(r[0].clone());
        channels.push(r[1].clone());
    }
    assert!(campaign_names.len() < 65_000);
    let campaigns = IdMap::new(b"c_", &citems);
    campaign_names.push(b"organic".to_vec());
    channels.push(b"organic".to_vec());
    Lookups { products, categories: cats, unknown, campaigns, campaign_names, channels }
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

#[derive(Clone, Copy)]
enum Kind {
    Events,
    Clicks,
    Links,
}

#[inline]
fn valid_price(p: &[u8]) -> Option<u32> {
    if p.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < p.len() && p[i] == b'0' {
        i += 1;
    }
    if p.len() - i > 8 {
        return None; // > 10,000,000 or not digits: invalid either way
    }
    let mut v: u32 = 0;
    for &c in &p[i..] {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u32;
    }
    if (1..=10_000_000).contains(&v) { Some(v) } else { None }
}

#[inline]
fn parse_event(f: [&[u8]; 7], cx: &Ctx, acc: &mut ParseAcc) {
    let [eid, uid, ts, ingest, typ, pid, price] = f;
    if eid.is_empty() {
        return;
    }
    let eid = pack_rec(eid, &mut acc.longs);
    let ingest = parse_ts(ingest).unwrap_or(i64::MAX / 2);
    let ingest_s = (ingest - cx.ws).div_euclid(1000).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let mut e = Ev { eid, user: 0, ingest: ingest_s, ts: 0, price: 0, cat: 0, ok: false };
    'v: {
        if uid.is_empty() || pid.is_empty() {
            break 'v;
        }
        let Some(t) = parse_ts(ts) else { break 'v };
        if t < cx.ws || t >= cx.we {
            break 'v;
        }
        let purchase = match typ {
            b"view" | b"cart" => false,
            b"purchase" => true,
            _ => break 'v,
        };
        if purchase {
            match valid_price(price) {
                Some(p) => e.price = p,
                None => break 'v,
            }
        }
        if ingest - t > DAY {
            break 'v;
        }
        e.ts = (t - cx.ws) as u32;
        e.user = pack_rec(uid, &mut acc.longs);
        e.cat = cx.lk.products.get(pid).unwrap_or(cx.lk.unknown);
        e.ok = true;
    }
    let b = (mix(eid) as usize) & (cx.eb - 1);
    acc.ev[b].push(e);
}

#[inline]
fn parse_click(f: [&[u8]; 4], cx: &Ctx, acc: &mut ParseAcc) {
    let [cid, uid, camp, ts] = f;
    if cid.is_empty() || uid.is_empty() {
        return;
    }
    let Some(t) = parse_ts(ts) else { return };
    if t < cx.ws - WEEK || t >= cx.we {
        return;
    }
    let Some(camp) = cx.lk.campaigns.get(camp) else { return };
    let cid = pack_rec(cid, &mut acc.longs);
    let user = pack_rec(uid, &mut acc.longs);
    acc.ck.push(Ck { cid, user, ts: t, h: 0, camp });
}

#[inline]
fn parse_link(f: [&[u8]; 2], acc: &mut ParseAcc) {
    let [a, b] = f;
    if a.is_empty() || b.is_empty() {
        return;
    }
    let a = pack_rec(a, &mut acc.longs);
    let b = pack_rec(b, &mut acc.longs);
    acc.links.push((a, b));
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    let mut r = x;
    while parent[r as usize] != r {
        r = parent[r as usize];
    }
    while parent[x as usize] != r {
        let n = parent[x as usize];
        parent[x as usize] = r;
        x = n;
    }
    r
}

/// Map each linked ID to its person's name (smallest ID).
/// Linked ID -> person name, open addressing with (key, name) in one 32-byte
/// slot. Key 0 marks an empty slot (no packed ID is 0).
struct PersonMap {
    mask: usize,
    slots: Vec<[u128; 2]>,
    len: usize,
}

impl PersonMap {
    fn new(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        PersonMap { mask: cap - 1, slots: huge_zeroed(cap), len: 0 }
    }
    #[inline(always)]
    fn slot(&self, k: u128) -> usize {
        (mix(k) >> 20) as usize & self.mask
    }
    fn insert(&mut self, k: u128, v: u128) {
        let mut h = self.slot(k);
        while self.slots[h][0] != 0 {
            h = (h + 1) & self.mask;
        }
        self.slots[h] = [k, v];
        self.len += 1;
    }
    #[inline(always)]
    fn get(&self, k: u128) -> u128 {
        let mut h = self.slot(k);
        loop {
            let [key, val] = self.slots[h];
            if key == k {
                return val;
            }
            if key == 0 {
                return k;
            }
            h = (h + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn prefetch(&self, k: u128) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            _mm_prefetch(self.slots.as_ptr().add(self.slot(k)) as *const i8, _MM_HINT_T0);
        }
    }
}

/// A zeroed Vec whose memory is 2 MB aligned and advised for transparent huge
/// pages (fewer TLB misses on large random-access tables).
fn huge_zeroed<T: Copy>(n: usize) -> Vec<T> {
    const HUGE: usize = 2 << 20;
    let bytes = (n * std::mem::size_of::<T>()).max(1);
    if bytes < HUGE {
        let mut v = Vec::with_capacity(n);
        unsafe {
            std::ptr::write_bytes(v.as_mut_ptr(), 0, n);
            v.set_len(n);
        }
        return v;
    }
    unsafe {
        let layout = std::alloc::Layout::from_size_align(bytes.next_multiple_of(HUGE), HUGE).unwrap();
        let p = std::alloc::alloc_zeroed(layout);
        assert!(!p.is_null());
        #[cfg(target_os = "linux")]
        libc::madvise(p as *mut libc::c_void, layout.size(), libc::MADV_HUGEPAGE);
        // capacity must match the layout's size for a sound dealloc; we never free it
        // (the process exits without dropping), but keep it consistent anyway.
        let cap = layout.size() / std::mem::size_of::<T>();
        Vec::from_raw_parts(p as *mut T, n, cap)
    }
}

fn build_persons(links: &[(u128, u128)], longs: &Longs) -> PersonMap {
    let mut index: HashMap<u128, u32> = HashMap::with_capacity(links.len() * 2);
    let mut ids: Vec<u128> = Vec::with_capacity(links.len() * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(links.len() * 2);
    let mut get = |x: u128, ids: &mut Vec<u128>, parent: &mut Vec<u32>| -> u32 {
        *index.entry(x).or_insert_with(|| {
            ids.push(x);
            parent.push(parent.len() as u32);
            (parent.len() - 1) as u32
        })
    };
    for &(a, b) in links {
        let ia = get(a, &mut ids, &mut parent);
        let ib = get(b, &mut ids, &mut parent);
        let (ra, rb) = (find(&mut parent, ia), find(&mut parent, ib));
        if ra != rb {
            parent[ra as usize] = rb;
        }
    }
    let n = ids.len();
    let mut name: Vec<u128> = vec![0; n];
    let mut has: Vec<bool> = vec![false; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if !has[r] || longs.cmp(ids[i], name[r]).is_lt() {
            name[r] = ids[i];
            has[r] = true;
        }
    }
    let mut map = PersonMap::new(n);
    for i in 0..n {
        let r = parent[i] as usize;
        map.insert(ids[i], name[r]);
    }
    map
}

pub struct UserRow {
    pub person: u128,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: i64,
    pub top: u8, // 255 = null
}

struct CampAcc {
    nd: usize,
    sessions: Vec<i64>,
    purchasing: Vec<i64>,
    revenue: Vec<i64>,
    assisted: Vec<i64>,
    stamp: Vec<u32>,
    stamp_now: u32,
    users: Vec<UserRow>,
}

impl CampAcc {
    fn new(nc: usize, nd: usize) -> Self {
        let n = nc * nd;
        CampAcc {
            nd,
            sessions: vec![0; n],
            purchasing: vec![0; n],
            revenue: vec![0; n],
            assisted: vec![0; n],
            stamp: vec![0; nc],
            stamp_now: 0,
            users: Vec::new(),
        }
    }
    fn merge(mut self, o: CampAcc) -> CampAcc {
        for i in 0..self.sessions.len() {
            self.sessions[i] += o.sessions[i];
            self.purchasing[i] += o.purchasing[i];
            self.revenue[i] += o.revenue[i];
            self.assisted[i] += o.assisted[i];
        }
        let mut users = o.users;
        if users.len() > self.users.len() {
            std::mem::swap(&mut users, &mut self.users);
        }
        self.users.append(&mut users);
        self
    }
}

/// Process one person: events sorted by ts, clicks sorted by (ts asc, cid desc).
#[allow(clippy::too_many_arguments)]
fn process_person(
    person: u128,
    evs: &[SEv],
    cks: &[Ck],
    ws: i64,
    day0: i64,
    organic: usize,
    cat_rev: &mut [i64],
    acc: &mut CampAcc,
) {
    let nd = acc.nd;
    let mut i = 0;
    let mut lo = 0; // first click with ts >= start - WEEK
    let mut hi = 0; // first click with ts > start
    let (mut d_sess, mut d_ev, mut d_rev, mut d_anyp) = (0u32, 0u32, 0i64, false);
    while i < evs.len() {
        let start = ws + evs[i].ts as i64;
        let mut j = i;
        let mut rev = 0i64;
        let mut anyp = false;
        let mut prev = evs[i].ts;
        while j < evs.len() && evs[j].ts - prev <= GAP as u32 {
            let e = &evs[j];
            if e.price > 0 {
                rev += e.price as i64;
                anyp = true;
                cat_rev[e.cat as usize] += e.price as i64;
            }
            prev = e.ts;
            j += 1;
        }
        let day = start.div_euclid(DAY);
        let di = (day - day0) as usize;
        // credit + assists
        while hi < cks.len() && cks[hi].ts <= start {
            hi += 1;
        }
        while lo < hi && cks[lo].ts < start - WEEK {
            lo += 1;
        }
        let credit = if hi > lo { cks[hi - 1].camp as usize } else { organic };
        let k = credit * nd + di;
        acc.sessions[k] += 1;
        acc.purchasing[k] += anyp as i64;
        acc.revenue[k] += rev;
        if hi > lo {
            acc.stamp_now = acc.stamp_now.wrapping_add(1);
            if acc.stamp_now == 0 {
                acc.stamp.iter_mut().for_each(|s| *s = 0);
                acc.stamp_now = 1;
            }
            let now = acc.stamp_now;
            for c in &cks[lo..hi] {
                let ci = c.camp as usize;
                if acc.stamp[ci] != now {
                    acc.stamp[ci] = now;
                    acc.assisted[ci * nd + di] += 1;
                }
            }
        }
        d_sess += 1;
        d_ev += (j - i) as u32;
        d_rev += rev;
        d_anyp |= anyp;
        i = j;
        // flush the day when the next session starts on another day (or none left)
        if i == evs.len() || (ws + evs[i].ts as i64).div_euclid(DAY) != day {
            let mut top = 255u8;
            if d_anyp {
                let mut best = 0i64;
                for (c, v) in cat_rev.iter_mut().enumerate() {
                    if *v > best {
                        best = *v;
                        top = c as u8;
                    }
                    *v = 0;
                }
            }
            acc.users.push(UserRow { person, day: day as i32, sessions: d_sess, events: d_ev, revenue: d_rev, top });
            (d_sess, d_ev, d_rev, d_anyp) = (0, 0, 0, false);
        }
    }
}

/// Run tasks 0..n on every pool thread, pulling indexes from a shared counter;
/// one accumulator per thread.
fn run_tasks<A: Send>(n: usize, init: impl Fn() -> A + Sync, work: impl Fn(&mut A, usize) + Sync) -> Vec<A> {
    let next = AtomicUsize::new(0);
    rayon::broadcast(|_| {
        let mut a = init();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            work(&mut a, i);
        }
        a
    })
}

/// [thread][bucket] -> [bucket] of per-thread parts.
fn transpose<T>(v: Vec<Vec<Vec<T>>>) -> Vec<Mutex<Vec<Vec<T>>>> {
    let nb = v.first().map_or(0, |x| x.len());
    let mut out: Vec<Vec<Vec<T>>> = (0..nb).map(|_| Vec::with_capacity(v.len())).collect();
    for parts in v {
        for (b, x) in parts.into_iter().enumerate() {
            if !x.is_empty() {
                out[b].push(x);
            }
        }
    }
    out.into_iter().map(Mutex::new).collect()
}

fn main() {
    let t0 = Instant::now();
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let data = PathBuf::from(env("DATA_DIR", "/data"));
    let out = PathBuf::from(env("OUTPUT_DIR", "/output"));
    let ncpu: usize = env("NCPU", "8").parse().unwrap_or(8);
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().unwrap();

    let manifest = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let ws = manifest_value(&manifest, "window_start");
    let we = manifest_value(&manifest, "window_end");
    assert!(we > ws && we - ws < 40 * DAY, "window too long for u32 offsets");
    let mut cx = Ctx { eb: 1, ws, we, lk: load_lookups(&data) };
    let day0 = ws.div_euclid(DAY);
    // dates that can matter: click window start - 1 day .. window end + 1 day (offsets)
    ts::init_dates(day0 - 9, (we - 1).div_euclid(DAY) + 2);
    let nd = ((we - 1).div_euclid(DAY) - day0 + 1).max(1) as usize;
    let nc = cx.lk.campaign_names.len(); // includes organic
    let organic = nc - 1;
    let ncat = cx.lk.categories.len();
    log!(t0, "lookups: {} products (direct {}), {} campaigns (direct {}), {} days", cx.lk.products.len(), cx.lk.products.direct.is_some(), nc - 1, cx.lk.campaigns.direct.is_some(), nd);

    // ---------- map files, build chunk tasks ----------
    let mut files: Vec<(Kind, Mmap)> = Vec::new();
    for (kind, sub) in [(Kind::Links, "links"), (Kind::Clicks, "clicks"), (Kind::Events, "events")] {
        for p in list_csv(&data.join(sub)) {
            let f = File::open(&p).unwrap();
            if f.metadata().unwrap().len() == 0 {
                continue;
            }
            let m = unsafe { Mmap::map(&f).unwrap() };
            files.push((kind, m));
        }
    }
    let mut tasks: Vec<(usize, usize, usize)> = Vec::new();
    for (fi, (_, m)) in files.iter().enumerate() {
        let len = m.len();
        let mut s = 0;
        while s < len {
            let e = (s + CHUNK).min(len);
            tasks.push((fi, s, e));
            s = e;
        }
    }
    // big work first is handled by rayon's work stealing; keep order (links first).
    log!(t0, "mapped {} files, {} chunks", files.len(), tasks.len());

    // ---------- parse links, then union-find on a side thread ----------
    let link_tasks: Vec<(usize, usize, usize)> =
        tasks.iter().copied().filter(|t| matches!(files[t.0].0, Kind::Links)).collect();
    let other_tasks: Vec<(usize, usize, usize)> =
        tasks.iter().copied().filter(|t| !matches!(files[t.0].0, Kind::Links)).collect();
    // Reserve each thread's event buckets up front (a generous estimate: lines
    // are ~76 bytes); untouched capacity is never committed by the OS.
    let event_bytes: usize = files.iter().filter(|f| matches!(f.0, Kind::Events)).map(|f| f.1.len()).sum();
    let click_bytes: usize = files.iter().filter(|f| matches!(f.0, Kind::Clicks)).map(|f| f.1.len()).sum();
    let nthreads = rayon::current_num_threads();
    // ~16k event rows per bucket keeps each bucket's dedup table and sort in L2
    let est_rows = event_bytes / 60;
    cx.eb = std::env::var("SS_EB").ok().and_then(|v| v.parse().ok()).unwrap_or(if est_rows < 2_000_000 { 256 } else { 1024 });
    #[allow(non_snake_case)]
    let (EB, PB) = (cx.eb, cx.eb);
    let cx = &cx;
    log!(t0, "buckets: {EB}");
    let per_bucket = est_rows / EB / nthreads + 64;
    let new_acc = || ParseAcc {
        ev: (0..EB).map(|_| Vec::with_capacity(per_bucket)).collect(),
        ck: Vec::with_capacity(click_bytes / 40 / nthreads + 64),
        ..Default::default()
    };
    let parse_task = |acc: &mut ParseAcc, (fi, s, e): (usize, usize, usize)| {
        let (kind, m) = &files[fi];
        let d: &[u8] = m;
        let mut seps = std::mem::take(&mut acc.seps);
        match kind {
            Kind::Events => csv::scan::<7>(d, s, e, &mut seps, |f| parse_event(f, &cx, acc)),
            Kind::Clicks => csv::scan::<4>(d, s, e, &mut seps, |f| parse_click(f, &cx, acc)),
            Kind::Links => csv::scan::<2>(d, s, e, &mut seps, |f| parse_link(f, acc)),
        }
        acc.seps = seps;
    };
    let link_accs = run_tasks(link_tasks.len(), ParseAcc::default, |a, i| parse_task(a, link_tasks[i]));
    let mut longs = Longs::default();
    let mut links: Vec<(u128, u128)> = Vec::new();
    for a in link_accs {
        longs.add_all(a.longs);
        links.extend_from_slice(&a.links);
    }
    log!(t0, "links parsed: {}", links.len());
    let (persons, longs, accs) = std::thread::scope(|sc| {
        let uf = sc.spawn(move || {
            let p = build_persons(&links, &longs);
            (p, longs)
        });
        let accs = run_tasks(other_tasks.len(), new_acc, |a, i| parse_task(a, other_tasks[i]));
        let (p, l) = uf.join().unwrap();
        (p, l, accs)
    });
    let mut longs = longs;
    log!(t0, "parsed; persons: {} linked ids", persons.len);

    let mut ck_parts: Vec<Mutex<Vec<Ck>>> = Vec::new();
    let mut ev_in: Vec<Vec<Vec<Ev>>> = (0..EB).map(|_| Vec::with_capacity(accs.len())).collect();
    for a in accs {
        longs.add_all(a.longs);
        ck_parts.push(Mutex::new(a.ck));
        for (b, v) in a.ev.into_iter().enumerate() {
            if !v.is_empty() {
                ev_in[b].push(v);
            }
        }
    }
    let ev_in: Vec<Mutex<Vec<Vec<Ev>>>> = ev_in.into_iter().map(Mutex::new).collect();
    let persons = &persons;
    // test hook: SS_HMASK=15 forces sort-key hash collisions
    let hmask: u32 = env("SS_HMASK", "4294967295").parse().unwrap();

    // ---------- event dedup per eid bucket, scatter by person ----------
    let out_cap = est_rows / PB / nthreads + 64;
    let new_out = || (0..PB).map(|_| Vec::with_capacity(out_cap)).collect::<Vec<Vec<SEv>>>();
    let ev_scattered = run_tasks(EB, new_out, |out, b| {
        let parts = std::mem::take(&mut *ev_in[b].lock().unwrap());
        let n: usize = parts.iter().map(|p| p.len()).sum();
        // Open addressing over (part << 32 | index) into the input parts; losing
        // copies are marked in a per-part bitset, so survivors are read in order
        // with no intermediate copy.
        let cap = (n * 2).next_power_of_two().max(16);
        let mask = cap - 1;
        let mut table: Vec<u64> = vec![u64::MAX; cap];
        let mut dead: Vec<Vec<u64>> = parts.iter().map(|p| vec![0u64; p.len() / 64 + 1]).collect();
        for (pi, part) in parts.iter().enumerate() {
            for (i, e) in part.iter().enumerate() {
                // the bucket was picked by mix's low bits; probe with its high bits
                let mut h = (mix(e.eid) >> 32) as usize & mask;
                loop {
                    let k = table[h];
                    if k == u64::MAX {
                        table[h] = ((pi as u64) << 32) | i as u64;
                        break;
                    }
                    let (cp, ci) = ((k >> 32) as usize, (k & 0xFFFF_FFFF) as usize);
                    let cur = &parts[cp][ci];
                    if cur.eid == e.eid {
                        if e.ingest < cur.ingest {
                            dead[cp][ci / 64] |= 1 << (ci % 64);
                            table[h] = ((pi as u64) << 32) | i as u64;
                        } else {
                            dead[pi][i / 64] |= 1 << (i % 64);
                        }
                        break;
                    }
                    h = (h + 1) & mask;
                }
            }
        }
        drop(table);
        const AHEAD: usize = 16;
        for (part, dead) in parts.iter().zip(&dead) {
            for i in 0..part.len() {
                if i + AHEAD < part.len() {
                    persons.prefetch(part[i + AHEAD].user);
                }
                let e = &part[i];
                if e.ok && dead[i / 64] & (1 << (i % 64)) == 0 {
                    let person = persons.get(e.user);
                    let m = mix(person);
                    out[(m as usize) & (PB - 1)].push(SEv { person, ts: e.ts, price: e.price, h: (m >> 32) as u32 & hmask, cat: e.cat });
                }
            }
        }
    });
    drop(ev_in);
    let ev_buckets = transpose(ev_scattered);
    log!(t0, "events deduped");

    // ---------- clicks: map to person, scatter ----------
    let ck_cap = click_bytes / 40 / PB / nthreads + 64;
    let ck_scattered = run_tasks(ck_parts.len(), || (0..PB).map(|_| Vec::with_capacity(ck_cap)).collect::<Vec<Vec<Ck>>>(), |out, i| {
        let part = std::mem::take(&mut *ck_parts[i].lock().unwrap());
        for i in 0..part.len() {
            if i + 16 < part.len() {
                persons.prefetch(part[i + 16].user);
            }
            let c = part[i];
            let person = persons.get(c.user);
            let m = mix(person);
            out[(m as usize) & (PB - 1)].push(Ck { user: person, h: (m >> 32) as u32 & hmask, ..c });
        }
    });
    let ck_buckets = transpose(ck_scattered);
    log!(t0, "clicks scattered");

    // ---------- per person bucket ----------
    let longs_ref = &longs;
    let accs = run_tasks(PB, || CampAcc::new(nc, nd), |acc, b| {
        let eparts = std::mem::take(&mut *ev_buckets[b].lock().unwrap());
        let cparts = std::mem::take(&mut *ck_buckets[b].lock().unwrap());
        let mut evs: Vec<SEv> = Vec::with_capacity(eparts.iter().map(|p| p.len()).sum());
        for p in eparts {
            evs.extend_from_slice(&p);
        }
        // Order persons by (h, person), events by ts: sort on the u64 (h, ts), then
        // fix up any run of equal h that mixes persons (a 32-bit hash collision).
        evs.sort_unstable_by_key(|e| ((e.h as u64) << 32) | e.ts as u64);
        {
            let mut i = 0;
            while i < evs.len() {
                let h = evs[i].h;
                let mut j = i + 1;
                let mut mixed = false;
                while j < evs.len() && evs[j].h == h {
                    mixed |= evs[j].person != evs[i].person;
                    j += 1;
                }
                if mixed {
                    evs[i..j].sort_by_key(|e| (e.person, e.ts));
                }
                i = j;
            }
        }
        let mut cks: Vec<Ck> = Vec::with_capacity(cparts.iter().map(|p| p.len()).sum());
        for p in cparts {
            cks.extend_from_slice(&p);
        }
        if longs_ref.is_empty() {
            cks.sort_unstable_by(|a, b| (a.h, a.user, a.ts).cmp(&(b.h, b.user, b.ts)).then(b.cid.cmp(&a.cid)));
        } else {
            cks.sort_unstable_by(|a, b| (a.h, a.user, a.ts).cmp(&(b.h, b.user, b.ts)).then(longs_ref.cmp(b.cid, a.cid)));
        }
        cks.dedup_by(|a, b| a.cid == b.cid);
        let mut cat_rev = vec![0i64; ncat];
        let (mut i, mut k) = (0, 0);
        while i < evs.len() {
            let p = evs[i].person;
            let mut j = i + 1;
            while j < evs.len() && evs[j].person == p {
                j += 1;
            }
            let ph = evs[i].h;
            while k < cks.len() && (cks[k].h, cks[k].user) < (ph, p) {
                k += 1;
            }
            let mut l = k;
            while l < cks.len() && cks[l].user == p {
                l += 1;
            }
            process_person(p, &evs[i..j], &cks[k..l], ws, day0, organic, &mut cat_rev, acc);
            i = j;
            k = l;
        }
    });
    let total = accs.into_iter().reduce(|a, b| a.merge(b)).unwrap();
    log!(t0, "sessions: {} user-day rows", total.users.len());

    output::write(&out, &cx.lk, &longs, &total.users, day0, nd, &total.sessions, &total.purchasing, &total.revenue, &total.assisted);
    log!(t0, "written");
    std::process::exit(0);
}
