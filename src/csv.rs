//! CSV dialect of the spec: only `\n` ends a line, every comma separates
//! fields, a field may be wrapped whole in double quotes, any other quote makes
//! the line malformed, an empty line is malformed, nothing is trimmed.

use memchr::memchr;

/// Split `line` into exactly N fields with quotes removed, or None if malformed.
#[inline(always)]
pub fn split<'a, const N: usize>(line: &'a [u8]) -> Option<[&'a [u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut start = 0;
    for (i, slot) in out.iter_mut().enumerate() {
        let end = if i + 1 < N {
            start + memchr(b',', &line[start..])?
        } else {
            if memchr(b',', &line[start..]).is_some() {
                return None;
            }
            line.len()
        };
        *slot = &line[start..end];
        start = end + 1;
    }
    if memchr(b'"', line).is_some() {
        for f in out.iter_mut() {
            if memchr(b'"', f).is_some() {
                let n = f.len();
                if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr(b'"', &f[1..n - 1]).is_none() {
                    *f = &f[1..n - 1];
                } else {
                    return None;
                }
            }
        }
    }
    Some(out)
}

/// Calls `f` for each line whose first byte lies in `[s, e)` of `data`,
/// skipping the header (the line starting at byte 0).
#[inline]
pub fn for_lines(data: &[u8], s: usize, e: usize, mut f: impl FnMut(&[u8])) {
    let len = data.len();
    let mut pos = if s == 0 {
        // skip header
        match memchr(b'\n', data) {
            Some(p) => p + 1,
            None => return,
        }
    } else {
        match memchr(b'\n', &data[s - 1..]) {
            Some(p) => s - 1 + p + 1,
            None => return,
        }
    };
    while pos < e && pos < len {
        let end = match memchr(b'\n', &data[pos..]) {
            Some(p) => pos + p,
            None => len,
        };
        f(&data[pos..end]);
        pos = end + 1;
    }
}

/// Bitmasks of `,` `\n` `"` over 64 bytes at `p`.
#[inline(always)]
fn masks(block: &[u8; 64]) -> (u64, u64, u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        let (cc, cn, cq) = (_mm_set1_epi8(b',' as i8), _mm_set1_epi8(b'\n' as i8), _mm_set1_epi8(b'"' as i8));
        let (mut c, mut n, mut q) = (0u64, 0u64, 0u64);
        for k in 0..4 {
            let v = _mm_loadu_si128(block.as_ptr().add(16 * k) as *const __m128i);
            c |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, cc)) as u32 as u64) << (16 * k);
            n |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, cn)) as u32 as u64) << (16 * k);
            q |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, cq)) as u32 as u64) << (16 * k);
        }
        (c, n, q)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let (mut c, mut n, mut q) = (0u64, 0u64, 0u64);
        for (i, &b) in block.iter().enumerate() {
            c |= ((b == b',') as u64) << i;
            n |= ((b == b'\n') as u64) << i;
            q |= ((b == b'"') as u64) << i;
        }
        (c, n, q)
    }
}

/// Per-thread scan scratch (currently stateless; kept for the scan signature).
#[derive(Default)]
pub struct Seps;

/// Calls `f` with the N unquoted fields of every well-formed line starting in
/// `[s, e)` of `data`, skipping the header line at byte 0; malformed lines are
/// dropped. Lines are emitted straight from 64-byte separator bitmasks.
pub fn scan<const N: usize>(data: &[u8], s: usize, e: usize, _seps: &mut Seps, mut f: impl FnMut([&[u8]; N])) {
    let len = data.len();
    let start = if s == 0 {
        match memchr(b'\n', data) {
            Some(p) => p + 1,
            None => return,
        }
    } else {
        match memchr(b'\n', &data[s - 1..]) {
            Some(p) => s - 1 + p + 1,
            None => return,
        }
    };
    if start >= e || start >= len {
        return;
    }
    // Lines starting in [start, e) are exactly the bytes [start, limit).
    let limit = match memchr(b'\n', &data[e - 1..]) {
        Some(p) => e - 1 + p + 1,
        None => len,
    };
    let buf = &data[start..limit];
    let mut starts = [0usize; N];
    let mut ends = [0usize; N];
    let mut line_start = 0usize;
    let mut nf = 0usize;
    let mut cumq = 0u32; // quotes before the current block
    let mut base_q = 0u32; // quotes before the current line
    let mut emit = |p: usize, qv: u32, nf: usize, line_start: usize, starts: &[usize; N], ends: &mut [usize; N]| {
        if nf + 1 == N && p > line_start {
            ends[N - 1] = p;
            let mut out: [&[u8]; N] = [&[]; N];
            for k in 0..N {
                out[k] = &buf[starts[k]..ends[k]];
            }
            let quotes = qv;
            if quotes != 0 {
                let mut wrapped = 0u32;
                for fld in out.iter_mut() {
                    let n = fld.len();
                    if n >= 2 && fld[0] == b'"' && fld[n - 1] == b'"' {
                        *fld = &fld[1..n - 1];
                        wrapped += 1;
                    }
                }
                if quotes != 2 * wrapped {
                    return;
                }
            }
            f(out);
        }
    };
    let mut base = 0usize;
    let mut tail = [0u8; 64];
    while base < buf.len() {
        let block: &[u8; 64] = if base + 64 <= buf.len() {
            buf[base..base + 64].try_into().unwrap()
        } else {
            tail[..buf.len() - base].copy_from_slice(&buf[base..]);
            tail[buf.len() - base..].fill(0);
            &tail
        };
        let (c, n, q) = masks(block);
        let mut sep = c | n;
        while sep != 0 {
            let b = sep.trailing_zeros();
            let p = base + b as usize;
            if (n >> b) & 1 == 0 {
                if nf + 1 < N {
                    ends[nf] = p;
                    starts[nf + 1] = p + 1;
                }
                nf += 1;
            } else {
                let qv = cumq + (q & ((1u64 << b) - 1)).count_ones();
                emit(p, qv - base_q, nf, line_start, &starts, &mut ends);
                line_start = p + 1;
                starts[0] = line_start;
                base_q = qv;
                nf = 0;
            }
            sep &= sep - 1;
        }
        cumq += q.count_ones();
        base += 64;
    }
    if buf.last() != Some(&b'\n') {
        // last line of the file without a trailing newline
        emit(buf.len(), cumq - base_q, nf, line_start, &starts, &mut ends);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialect() {
        assert_eq!(split::<2>(b"a,b"), Some([&b"a"[..], b"b"]));
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], b""]));
        assert_eq!(split::<2>(b" a,b\r"), Some([&b" a"[..], b"b\r"]));
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a"), None);
        assert_eq!(split::<2>(b""), None);
        assert_eq!(split::<2>(b"\"a,b"), None);
        assert_eq!(split::<2>(b"a\",b"), None);
        assert_eq!(split::<2>(b"\",b"), None);
        assert_eq!(split::<2>(b"a\"b\",c"), None);
        assert_eq!(split::<2>(b"\"a\"b\",c"), None);
        assert_eq!(split::<1>(b"\"\""), Some([&b""[..]]));
    }
    #[test]
    fn scan_matches_split() {
        let d: &[u8] = b"h\n\"\",\"\"\"\n\"\"\"\",x\n\",\"\n\"\"\",\"\n\"a\"\",\"b\n\"a\"\"\",b\nzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz,\"q\"\na,b\n\"a\",\"\"\n\na,b,c\nx\n\"q,w\na\"b\",c\n0123456789abcdef,\"0123456789abcdef\"\n,\nlast,line";
        for chunk in [1usize, 3, 7, 1000] {
            let mut want = vec![];
            let mut got = vec![];
            let mut s = 0;
            while s < d.len() {
                let e = (s + chunk).min(d.len());
                for_lines(d, s, e, |l| if let Some(f) = split::<2>(l) { want.push(f.map(|x| x.to_vec())) });
                scan::<2>(d, s, e, &mut Seps::default(), |f| got.push(f.map(|x| x.to_vec())));
                s = e;
            }
            assert_eq!(want, got);
            assert_eq!(want.len(), 6);
        }
    }

    #[test]
    fn lines() {
        let d = b"h\nl1\n\nl3";
        let mut v = vec![];
        for_lines(d, 0, d.len(), |l| v.push(l.to_vec()));
        assert_eq!(v, vec![b"l1".to_vec(), b"".to_vec(), b"l3".to_vec()]);
        // chunked
        let mut w = vec![];
        for (s, e) in [(0, 3), (3, 6), (6, d.len())] {
            for_lines(d, s, e, |l| w.push(l.to_vec()));
        }
        assert_eq!(v, w);
        let mut x = vec![];
        for_lines(b"h\n", 0, 2, |l| x.push(l.to_vec()));
        assert!(x.is_empty());
    }
}
