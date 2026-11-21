//! Timestamp parsing: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS±HH:MM`,
//! F3 epoch-millisecond digits. Returns `None` for anything malformed or not
//! representable as i64 (which is outside every window anyway).

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const DAY: i64 = 86_400_000;

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

#[inline(always)]
fn d2(v: &[u8], i: usize) -> Option<i64> {
    let a = v[i].wrapping_sub(b'0');
    let b = v[i + 1].wrapping_sub(b'0');
    if a < 10 && b < 10 { Some((a * 10 + b) as i64) } else { None }
}

#[inline(always)]
fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days since epoch for real calendar dates near the window, keyed by
/// (year, month, day). Any date outside it gives a timestamp outside every
/// window (or is impossible), so callers may treat a miss as "bad" — but to
/// stay exact for every caller, a miss falls back to full validation.
pub struct DateTable {
    first_key: i64,
    days: Vec<i32>, // i32::MIN = not a date in range
}

#[inline(always)]
fn date_key(y: i64, m: i64, d: i64) -> i64 {
    (y * 16 + m) * 32 + d
}

impl DateTable {
    pub fn new(from_day: i64, to_day: i64) -> Self {
        let civ = |days: i64| {
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            (yoe + era * 400 + (m <= 2) as i64, m, d)
        };
        let (y0, m0, d0) = civ(from_day);
        let (y1, m1, d1) = civ(to_day);
        let first_key = date_key(y0, m0, d0);
        let last_key = date_key(y1, m1, d1);
        let mut days = vec![i32::MIN; (last_key - first_key + 1) as usize];
        for day in from_day..=to_day {
            let (y, m, d) = civ(day);
            debug_assert_eq!(days_from_civil(y, m, d), day);
            days[(date_key(y, m, d) - first_key) as usize] = day as i32;
        }
        DateTable { first_key, days }
    }
}

static TABLE: std::sync::OnceLock<DateTable> = std::sync::OnceLock::new();

#[derive(Clone, Copy, Default)]
struct CachedDate {
    month: u64,
    day: [u8; 2],
    midnight: i64,
}

#[inline(always)]
fn day_slot(day: &[u8]) -> usize {
    (usize::from(day[0]) * 10 + usize::from(day[1])) & 31
}

/// Exact day keys accelerate common dates. Collisions fall through to the month lookup.
struct Months {
    dates: [CachedDate; 32],
    n: usize,
    prefix: [u64; 8],
    base: [i64; 8],
    dim: [u8; 8],
}

static MONTHS: std::sync::OnceLock<Months> = std::sync::OnceLock::new();

/// Install the fast date tables (call once, before parsing).
pub fn init_dates(from_day: i64, to_day: i64) {
    let t = DateTable::new(from_day, to_day);
    let mut m = Months { dates: [CachedDate::default(); 32], n: 0, prefix: [0; 8], base: [0; 8], dim: [0; 8] };
    for day in from_day..=to_day {
        let k = t.days.iter().position(|&d| d as i64 == day).unwrap() as i64 + t.first_key;
        let (y, mo) = (k / 32 / 16, k / 32 % 16);
        let text = format!("{y:04}-{mo:02}-");
        let pre = u64::from_le_bytes(text.as_bytes().try_into().unwrap());
        let d = k % 32;
        let digits = [b'0' + (d / 10) as u8, b'0' + (d % 10) as u8];
        m.dates[day_slot(&digits)] = CachedDate { month: pre, day: digits, midnight: day * DAY };
        if m.prefix[..m.n].contains(&pre) || m.n == 8 {
            continue;
        }
        let dim = match mo {
            2 => if leap(y) { 29 } else { 28 },
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        m.prefix[m.n] = pre;
        m.base[m.n] = days_from_civil(y, mo, 1);
        m.dim[m.n] = dim;
        m.n += 1;
    }
    let _ = MONTHS.set(m);
    let _ = TABLE.set(t);
}

/// `HH:MM:SS` at v[0..8] -> seconds of day, validated.
#[inline(always)]
fn hms(v: &[u8]) -> Option<i64> {
    let x = u64::from_le_bytes(v[..8].try_into().unwrap());
    let t = x ^ 0x3030_3a30_303a_3030; // "00:00:00"
    // every byte must be < 10 (digits) and the two colon bytes exactly 0
    if ((t.wrapping_add(0x7676_7676_7676_7676) | t) & 0x8080_8080_8080_8080) != 0 || (t & 0x0000_ff00_00ff_0000) != 0 {
        return None;
    }
    let b = t.to_le_bytes();
    let (h, m, s) = (b[0] as i64 * 10 + b[1] as i64, b[3] as i64 * 10 + b[4] as i64, b[6] as i64 * 10 + b[7] as i64);
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

#[inline(always)]
fn days_of(year: i64, month: i64, day: i64) -> Option<i64> {
    if let Some(t) = TABLE.get() {
        let k = date_key(year, month, day) - t.first_key;
        if k >= 0 && (k as usize) < t.days.len() {
            let v = t.days[k as usize];
            if v != i32::MIN {
                return Some(v as i64);
            }
        }
    }
    slow_days(year, month, day)
}

#[cold]
fn slow_days(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    let dim = match month {
        2 => {
            if leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > dim {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// The UTC (or local, for F2) instant of the first 19 bytes, validated.
#[inline(always)]
fn civil_ms(v: &[u8]) -> Option<i64> {
    if let Some(m) = MONTHS.get() {
        let pre = u64::from_le_bytes(v[..8].try_into().unwrap());
        let date = &m.dates[day_slot(&v[8..10])];
        if date.month == pre && date.day == v[8..10] && v[10] == b'T' && pre != 0 {
            return Some(date.midnight + hms(&v[11..19])? * SECOND);
        }
        for k in 0..m.n {
            if m.prefix[k] == pre {
                if v[10] != b'T' {
                    return None;
                }
                let a = v[8].wrapping_sub(b'0');
                let b = v[9].wrapping_sub(b'0');
                if a >= 10 || b >= 10 {
                    return None;
                }
                let day = (a * 10 + b) as i64;
                if day < 1 || day > m.dim[k] as i64 {
                    return None;
                }
                let secs = hms(&v[11..19])?;
                return Some((m.base[k] + day - 1) * DAY + secs * SECOND);
            }
        }
    }
    civil_ms_slow(v)
}

#[cold]
fn civil_ms_slow(v: &[u8]) -> Option<i64> {
    if v[4] != b'-' || v[7] != b'-' || v[10] != b'T' || v[13] != b':' || v[16] != b':' {
        return None;
    }
    let year = d2(v, 0)? * 100 + d2(v, 2)?;
    let month = d2(v, 5)?;
    let day = d2(v, 8)?;
    let hour = d2(v, 11)?;
    let minute = d2(v, 14)?;
    let second = d2(v, 17)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_of(year, month, day)?;
    Some(days * DAY + hour * 3_600_000 + minute * MINUTE + second * SECOND)
}

#[inline(always)]
pub fn parse_ts(v: &[u8]) -> Option<i64> {
    let n = v.len();
    if n == 0 {
        return None;
    }
    if n == 20 && v[19] == b'Z' {
        return civil_ms(v);
    }
    if n == 25 && (v[19] == b'+' || v[19] == b'-') && v[22] == b':' {
        let local = civil_ms(v)?;
        let oh = d2(v, 20)?;
        let om = d2(v, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let off = (oh * 60 + om) * MINUTE;
        return Some(if v[19] == b'+' { local - off } else { local + off });
    }
    // Common epoch-millisecond width. Reduce the first eight digits in parallel.
    if n == 13 {
        let digits = u64::from_le_bytes(v[..8].try_into().unwrap()) ^ 0x3030_3030_3030_3030;
        if (digits | digits.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
            return None;
        }
        let pairs = (digits.wrapping_mul(10) + (digits >> 8)) & 0x00ff_00ff_00ff_00ff;
        let quads = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
        let mut value = (quads as u32 as i64) * 10_000 + (quads >> 32) as i64;
        let mut invalid = false;
        for &byte in &v[8..] {
            let digit = byte.wrapping_sub(b'0');
            invalid |= digit > 9;
            value = value * 10 + i64::from(digit);
        }
        return (!invalid).then_some(value);
    }
    // F3: digits only, leading zeros allowed.
    let mut i = 0;
    while i < n && v[i] == b'0' {
        i += 1;
    }
    let digits = &v[i..];
    let mut acc: u64 = 0;
    if digits.len() <= 18 {
        let mut bad = 0u8;
        for &c in digits {
            let d = c.wrapping_sub(b'0');
            bad |= (d >= 10) as u8;
            acc = acc * 10 + d as u64;
        }
        if bad != 0 {
            return None;
        }
        return Some(acc as i64);
    }
    for &c in digits {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(d as u64)?;
    }
    // All digits; too-large values are simply outside every window.
    if acc > i64::MAX as u64 { None } else { Some(acc as i64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(s: &str) -> Option<i64> {
        parse_ts(s.as_bytes())
    }
    #[test]
    fn table_agrees() {
        let t = DateTable::new(days_from_civil(2025, 1, 1), days_from_civil(2029, 1, 1));
        for y in 2024..2030 {
            for m in 0..15 {
                for d in 0..33 {
                    let k = date_key(y, m, d) - t.first_key;
                    let fast = if k >= 0 && (k as usize) < t.days.len() && t.days[k as usize] != i32::MIN {
                        Some(t.days[k as usize] as i64)
                    } else {
                        None
                    };
                    let slow = slow_days(y, m, d);
                    if fast.is_some() {
                        assert_eq!(fast, slow, "{y}-{m}-{d}");
                    }
                    if (2025..2029).contains(&y) {
                        assert_eq!(fast, slow, "{y}-{m}-{d}");
                    }
                }
            }
        }
    }
    #[test]
    fn hms_exact() {
        for h in 0..30 { for m in 0..65 { for s in [0, 1, 59, 60, 61, 99] {
            let txt = format!("{h:02}:{m:02}:{s:02}");
            let want = if h < 24 && m < 60 && s < 60 { Some(h * 3600 + m * 60 + s) } else { None };
            assert_eq!(hms(txt.as_bytes()), want, "{txt}");
        }}}
        for bad in ["0a:00:00", "00-00:00", "00:00;00", "00:0/:00", "\u{0}0:00:00", "00:00:0:", " 0:00:00", "~0:00:00"] {
            assert_eq!(hms(bad.as_bytes()), None, "{bad}");
        }
        for b in 0..=255u8 {
            let mut v = *b"12:34:56";
            for pos in 0..8 {
                let orig = v[pos];
                v[pos] = b;
                let ok = std::str::from_utf8(&v).ok().and_then(|t| {
                    let p: Vec<&str> = t.split(':').collect();
                    if p.len() == 3 && p.iter().all(|x| x.len() == 2 && x.bytes().all(|c| c.is_ascii_digit())) {
                        let (h, m, s) = (p[0].parse::<i64>().unwrap(), p[1].parse::<i64>().unwrap(), p[2].parse::<i64>().unwrap());
                        (h < 24 && m < 60 && s < 60).then_some(h * 3600 + m * 60 + s)
                    } else { None }
                });
                assert_eq!(hms(&v), ok, "{v:?}");
                v[pos] = orig;
            }
        }
    }
    #[test]
    fn forms_with_table() {
        init_dates(days_from_civil(2026, 9, 1), days_from_civil(2026, 11, 1));
        forms();
    }
    #[test]
    fn forms() {
        assert_eq!(p("2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(p("2026-10-03T19:58:10-04:00"), Some(1791071890000));
        assert_eq!(p("1791071890000"), Some(1791071890000));
        assert_eq!(p("0001791071890000"), Some(1791071890000));
        assert_eq!(p("2026-10-03T23:58:10-00:00"), Some(1791071890000));
        assert_eq!(p("000000000000000000000000001791071890000"), Some(1791071890000));
        assert_eq!(p("2028-02-29T00:00:00Z").is_some(), true);
        assert_eq!(p("99999999999999999999999"), None);
        for bad in [
            "", "2026-10-03T23:58:10.000Z", "2026-10-03T23:58:10z", "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z", "2026-10-03T23:58:10", "1791071890000.0", "1.79e12", "-5",
            "2026-13-03T23:58:10Z", "2026-02-30T23:58:10Z", "2027-02-29T23:58:10Z",
            "2026-10-03T24:58:10Z", "2026-10-03T23:58:60Z", "2026-10-03T23:58:10+24:00",
            "2026-10-03T23:58:10+05:60", "2026-10-03T23:58:10+0200", "2026-10-03T23:58:10Z\r",
            "+1791071890000", "２５０", "2026-10-03T23:58:10+02:00\r",
        ] {
            assert_eq!(p(bad), None, "{bad:?}");
        }
    }
}
