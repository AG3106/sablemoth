//! IDs as exact u64 keys.
//!
//! An ID of at most 10 bytes drawn from `0-9 A-Z _ a-z` (63 symbols) is packed
//! as 6-bit codes, big-endian from bit 63, with codes assigned in ASCII order
//! and 0 as padding. Integer equality and order are then exactly byte-wise
//! equality and order, and the low 4 bits are 0. Every generator ID (`e`, `u`,
//! `k` + up to 8 base-36 chars, upper-cased variants) fits.
//!
//! Any other ID (longer, or with other bytes such as spaces, `\r`, UTF-8)
//! becomes a 60-bit xxh3 hash tagged with low nibble 0xF. Its bytes go in the
//! `Longs` side table, which asserts on hash collisions and gives exact ordering.
//! A packed ID and a hashed ID never compare equal (different low nibble).

use hashbrown::HashMap;
use std::cmp::Ordering;

pub type Id = u64;

const TAG: u64 = 0xF;

const fn code_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut code = 1u8;
    let mut b = 0usize;
    while b < 256 {
        let c = b as u8;
        if c.is_ascii_digit() || c.is_ascii_uppercase() || c == b'_' || c.is_ascii_lowercase() {
            t[b] = code;
            code += 1;
        }
        b += 1;
    }
    t
}
static CODE: [u8; 256] = code_table();

const fn decode_table() -> [u8; 64] {
    let mut t = [0u8; 64];
    let codes = code_table();
    let mut b = 0usize;
    while b < 256 {
        if codes[b] != 0 {
            t[codes[b] as usize] = b as u8;
        }
        b += 1;
    }
    t
}
static DECODE: [u8; 64] = decode_table();

#[inline(always)]
pub fn pack(b: &[u8]) -> Id {
    if b.len() <= 10 {
        let mut acc = 0u64;
        let mut bad = 0u8;
        for (i, &c) in b.iter().enumerate() {
            let code = CODE[c as usize];
            bad |= (code == 0) as u8;
            acc |= (code as u64) << (58 - 6 * i);
        }
        if bad == 0 {
            return acc;
        }
    }
    hashed(b)
}

#[cold]
fn hashed(b: &[u8]) -> Id {
    (xxhash_rust::xxh3::xxh3_64(b) & !TAG) | TAG
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
            while n < 10 {
                let code = (x >> (58 - 6 * n)) & 63;
                if code == 0 {
                    break;
                }
                buf[n] = DECODE[code as usize];
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
        // every printable short ID in the alphabet round-trips and orders exactly
        let alpha: Vec<u8> = (0u8..=255).filter(|&c| CODE[c as usize] != 0).collect();
        assert_eq!(alpha.len(), 63);
        for &c1 in &alpha {
            for &c2 in &alpha {
                let (a, b) = ([b'u', c1], [b'u', c1, c2]);
                assert!(pack(&a) < pack(&b));
                assert_eq!(pack(&[c1]).cmp(&pack(&[c2])), c1.cmp(&c2));
            }
        }
    }
}
