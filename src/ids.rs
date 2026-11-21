//! IDs as exact u64 keys.
//!
//! An ID of at most 9 bytes, each in 0x01..=0x7F (non-NUL ASCII), is packed as
//! 7-bit codes (the bytes themselves), big-endian from bit 63, zero-padded, low
//! bit 0. Integer equality and order are then exactly byte-wise equality and
//! order. Every generator ID (`e`, `u`, `k` + up to 8 base-36 chars, and
//! upper-cased variants) fits. Packing is a few SWAR shifts on one 8-byte load.
//!
//! Any other ID (longer, NUL, or non-ASCII bytes) becomes a 63-bit xxh3 hash
//! tagged with low bit 1. Its bytes go in the `Longs` side table, which asserts
//! on hash collisions and gives exact ordering. A packed ID and a hashed ID
//! never compare equal (different low bit).

use hashbrown::HashMap;
use std::cmp::Ordering;

pub type Id = u64;

const TAG: u64 = 1;
const H80: u64 = 0x8080_8080_8080_8080;
const L7F: u64 = 0x7f7f_7f7f_7f7f_7f7f;

/// 8 big-endian 7-bit codes (top bit of each byte clear) -> 56 bits.
#[inline(always)]
fn compact(y: u64) -> u64 {
    let y = (y & 0x007f_007f_007f_007f) | ((y & 0x7f00_7f00_7f00_7f00) >> 1);
    let y = (y & 0x0000_3fff_0000_3fff) | ((y & 0x3fff_0000_3fff_0000) >> 2);
    (y & 0x0000_0000_0fff_ffff) | ((y & 0x0fff_ffff_0000_0000) >> 4)
}

#[inline(always)]
pub fn pack(b: &[u8]) -> Id {
    let n = b.len();
    if n <= 9 {
        let (hi, last) = if n >= 8 {
            (u64::from_be_bytes(b[..8].try_into().unwrap()), if n == 9 { b[8] } else { 0 })
        } else {
            let mut x = 0u64;
            for (i, &c) in b.iter().enumerate() {
                x |= (c as u64) << (56 - 8 * i);
            }
            (x, 0)
        };
        // high bit of each byte set iff that byte is non-zero (exact: no carries)
        let nonzero = ((hi & L7F).wrapping_add(L7F) | hi) & H80;
        let k = n.min(8);
        let want = if k == 0 { 0 } else { H80 << (8 * (8 - k)) };
        if hi & H80 == 0 && nonzero == want && last < 0x80 && (n < 9 || last != 0) {
            return (compact(hi) << 8) | ((last as u64) << 1);
        }
    }
    hashed(b)
}

#[cold]
fn hashed(b: &[u8]) -> Id {
    xxhash_rust::xxh3::xxh3_64(b) | TAG
}

#[inline(always)]
pub fn is_long(x: Id) -> bool {
    x & TAG == TAG
}

/// Pack, remembering a hashed ID's bytes.
#[inline(always)]
pub fn pack_rec(b: &[u8], longs: &mut Vec<(Id, Box<[u8]>)>) -> Id {
    let x = pack(b);
    if is_long(x) {
        longs.push((x, b.into()));
    }
    x
}

#[inline(always)]
pub fn mix(x: Id) -> u64 {
    let h = (x ^ (x >> 31)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 29)
}

#[derive(Default)]
pub struct Longs {
    map: HashMap<Id, Box<[u8]>>,
}

impl Longs {
    pub fn add_all(&mut self, items: Vec<(Id, Box<[u8]>)>) {
        for (k, v) in items {
            match self.map.get(&k) {
                Some(old) => assert!(*old == v, "hashed id collision"),
                None => {
                    self.map.insert(k, v);
                }
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes<'a>(&'a self, x: Id, buf: &'a mut [u8; 16]) -> &'a [u8] {
        if is_long(x) {
            &self.map[&x]
        } else {
            let mut n = 0;
            while n < 9 {
                let code = ((x >> (57 - 7 * n)) & 0x7f) as u8;
                if code == 0 {
                    break;
                }
                buf[n] = code;
                n += 1;
            }
            &buf[..n]
        }
    }

    #[inline]
    pub fn cmp(&self, a: Id, b: Id) -> Ordering {
        if !is_long(a) && !is_long(b) {
            return a.cmp(&b);
        }
        let (mut ba, mut bb) = ([0u8; 16], [0u8; 16]);
        self.bytes(a, &mut ba).cmp(self.bytes(b, &mut bb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_and_roundtrip() {
        let v: Vec<&[u8]> = vec![
            b"a", b"a\0", b"ab", b"U1", b"u1", b"u1 ", b" u1", b"z", b"abcdefghij", b"abcdefghijk", b"_", b"Z_",
            b"0", b"09azAZ", b"e1a2b3c4d", b"E1A2B3C4D", b"u\r", "é".as_bytes(), b"abcdefghijklmnopqrstuvwxyz",
        ];
        let mut longs = Vec::new();
        let ids: Vec<Id> = v.iter().map(|b| pack_rec(b, &mut longs)).collect();
        let mut t = Longs::default();
        t.add_all(longs);
        for (i, a) in v.iter().enumerate() {
            let mut buf = [0u8; 16];
            assert_eq!(t.bytes(ids[i], &mut buf), *a);
            for (j, b) in v.iter().enumerate() {
                assert_eq!(t.cmp(ids[i], ids[j]), a.cmp(b), "{a:?} {b:?}");
                assert_eq!(ids[i] == ids[j], a == b);
            }
        }
        // exhaustive over short ASCII IDs: round-trip, equality and order exact
        let mut all: Vec<Vec<u8>> = vec![];
        for c1 in [1u8, b' ', b'0', b'A', b'Z', b'_', b'a', b'z', 0x7f, 0x80, 0] {
            all.push(vec![c1]);
            for c2 in [1u8, b'0', b'a', 0x7f, 0x80, 0, b'\r'] {
                all.push(vec![b'u', c1, c2]);
                all.push(vec![b'u', b'1', b'2', b'3', b'4', b'5', b'6', c1, c2]);
                all.push(vec![b'u', b'1', b'2', b'3', b'4', b'5', b'6', b'7', c1, c2]);
            }
        }
        all.push(vec![]);
        let mut longs = Vec::new();
        let ids: Vec<Id> = all.iter().map(|b| pack_rec(b, &mut longs)).collect();
        let mut t = Longs::default();
        t.add_all(longs);
        for (i, a) in all.iter().enumerate() {
            let mut buf = [0u8; 16];
            assert_eq!(t.bytes(ids[i], &mut buf), &a[..], "{a:?}");
            for (j, b) in all.iter().enumerate() {
                assert_eq!(t.cmp(ids[i], ids[j]), a.cmp(b), "{a:?} {b:?}");
                assert_eq!(ids[i] == ids[j], a == b, "{a:?} {b:?}");
            }
        }
    }
}
