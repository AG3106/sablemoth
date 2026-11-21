//! IDs as exact u128 keys. An ID of at most 15 bytes is packed big-endian with
//! its length in the low byte: equality and byte-wise order are both exact
//! integer operations. A longer ID becomes a 120-bit xxh3 hash tagged with low
//! byte 0xFF; its bytes are kept in a side table (checked for collisions) so
//! ordering against it stays exact.

use hashbrown::HashMap;
use std::cmp::Ordering;

pub const LONG_TAG: u128 = 0xFF;

#[inline(always)]
pub fn pack(b: &[u8]) -> u128 {
    let n = b.len();
    if n <= 15 {
        let mut buf = [0u8; 16];
        buf[..n].copy_from_slice(b);
        buf[15] = n as u8;
        u128::from_be_bytes(buf)
    } else {
        (xxhash_rust::xxh3::xxh3_128(b) & !0xFF) | LONG_TAG
    }
}

#[inline(always)]
pub fn is_long(x: u128) -> bool {
    x & 0xFF == LONG_TAG
}

/// Pack, remembering a long ID's bytes.
#[inline(always)]
pub fn pack_rec(b: &[u8], longs: &mut Vec<(u128, Box<[u8]>)>) -> u128 {
    let x = pack(b);
    if is_long(x) {
        longs.push((x, b.into()));
    }
    x
}

#[inline(always)]
pub fn mix(x: u128) -> u64 {
    let h = (x as u64) ^ ((x >> 64) as u64).rotate_left(23);
    let h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 29)
}

#[derive(Default)]
pub struct Longs {
    map: HashMap<u128, Box<[u8]>>,
}

impl Longs {
    pub fn add_all(&mut self, items: Vec<(u128, Box<[u8]>)>) {
        for (k, v) in items {
            match self.map.get(&k) {
                Some(old) => assert!(*old == v, "long id hash collision"),
                None => {
                    self.map.insert(k, v);
                }
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes<'a>(&'a self, x: u128, buf: &'a mut [u8; 16]) -> &'a [u8] {
        if is_long(x) {
            &self.map[&x]
        } else {
            *buf = x.to_be_bytes();
            &buf[..(x & 0xFF) as usize]
        }
    }

    #[inline]
    pub fn cmp(&self, a: u128, b: u128) -> Ordering {
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
    fn order() {
        let v: Vec<&[u8]> = vec![b"a", b"a\0", b"ab", b"U1", b"u1", b"u1 ", b" u1", b"z", b"abcdefghijklmno"];
        for a in &v {
            for b in &v {
                assert_eq!(pack(a).cmp(&pack(b)), a.cmp(b), "{a:?} {b:?}");
            }
        }
    }
}
