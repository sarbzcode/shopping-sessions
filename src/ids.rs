//! ID keys. Every ID (event, user, click) becomes a u64 key:
//! - 1..=10 bytes, all in [0-9A-Za-z]: packed, 6 bits per byte with codes in ASCII order, first byte
//!   in the high bits, zero padding. Numeric order of packed keys is then the byte-wise order of the
//!   IDs (R20 names, R32 click_id ties), and 0 is never a packed key.
//! - anything else (spaces, \r, '_', long IDs, non-ASCII): FALLBACK | index into an interner.
//! IDs compare exactly (R19, R36): the mapping is injective, nothing is normalized.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub const FALLBACK: u64 = 1 << 63;

const fn build_code() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut c = 0;
    while c < 10 {
        t[b'0' as usize + c] = 1 + c as u8;
        c += 1;
    }
    c = 0;
    while c < 26 {
        t[b'A' as usize + c] = 11 + c as u8;
        t[b'a' as usize + c] = 37 + c as u8;
        c += 1;
    }
    t
}
/// 6-bit code of each byte that packed keys allow ([0-9A-Za-z], in ASCII order from 1), else 0.
pub static CODE: [u8; 256] = build_code();

const fn build_chars() -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut c = 0;
    while c < 10 {
        t[1 + c] = b'0' + c as u8;
        c += 1;
    }
    c = 0;
    while c < 26 {
        t[11 + c] = b'A' + c as u8;
        t[37 + c] = b'a' + c as u8;
        c += 1;
    }
    t
}
static CHARS: [u8; 64] = build_chars();

/// The packed key of `b`, or 0 if `b` cannot be packed.
#[inline(always)]
pub fn pack(b: &[u8]) -> u64 {
    let n = b.len();
    if n == 0 || n > 10 {
        return 0;
    }
    let mut k = 0u64;
    let mut bad = false;
    for (i, &byte) in b.iter().enumerate() {
        let c = CODE[byte as usize];
        bad |= c == 0;
        k |= (c as u64) << (54 - 6 * i);
    }
    if bad {
        0
    } else {
        k
    }
}

pub fn unpack(k: u64, out: &mut Vec<u8>) {
    for i in 0..10 {
        let c = ((k >> (54 - 6 * i)) & 63) as usize;
        if c == 0 {
            break;
        }
        out.push(CHARS[c]);
    }
}

#[derive(Default)]
struct Inner {
    map: HashMap<Box<[u8]>, u32>,
    strings: Vec<Box<[u8]>>,
}

impl Inner {
    fn write(&self, k: u64, out: &mut Vec<u8>) {
        if k & FALLBACK != 0 {
            out.extend_from_slice(&self.strings[(k & !FALLBACK) as usize]);
        } else {
            unpack(k, out);
        }
    }
}

/// Shared by all parsing threads. Packed IDs never take the lock; only fallback IDs do, and the
/// generator never writes one (they exist for hand-made inputs such as the fixtures).
#[derive(Default)]
pub struct Interner {
    inner: Mutex<Inner>,
}

impl Interner {
    #[inline(always)]
    pub fn key(&self, b: &[u8]) -> u64 {
        let k = pack(b);
        if k != 0 {
            k
        } else {
            self.slow(b)
        }
    }

    #[cold]
    #[inline(never)]
    fn slow(&self, b: &[u8]) -> u64 {
        let mut g = self.inner.lock().unwrap();
        if let Some(&i) = g.map.get(b) {
            return FALLBACK | i as u64;
        }
        let i = g.strings.len() as u32;
        g.strings.push(b.into());
        g.map.insert(b.into(), i);
        FALLBACK | i as u64
    }

    /// Byte-wise comparison of the IDs behind two keys (usable while parsing continues).
    pub fn cmp(&self, a: u64, b: u64) -> Ordering {
        if (a | b) & FALLBACK == 0 {
            return a.cmp(&b);
        }
        let g = self.inner.lock().unwrap();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        g.write(a, &mut x);
        g.write(b, &mut y);
        x.cmp(&y)
    }

    pub fn freeze(self) -> Names {
        Names { inner: self.inner.into_inner().unwrap() }
    }
}

/// The frozen interner: key -> bytes, lock-free.
pub struct Names {
    inner: Inner,
}

impl Names {
    /// Whether any fallback (interned) key exists; if not, every key compares numerically.
    pub fn any_fallback(&self) -> bool {
        !self.inner.strings.is_empty()
    }

    #[inline]
    pub fn write(&self, k: u64, out: &mut Vec<u8>) {
        self.inner.write(k, out)
    }

    #[inline]
    pub fn cmp(&self, a: u64, b: u64) -> Ordering {
        if (a | b) & FALLBACK == 0 {
            return a.cmp(&b);
        }
        let (mut x, mut y) = (Vec::new(), Vec::new());
        self.inner.write(a, &mut x);
        self.inner.write(b, &mut y);
        x.cmp(&y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_is_injective_and_order_preserving() {
        let ids: Vec<&[u8]> = vec![
            b"u1", b"u10", b"u2", b"U7", b"0", b"z", b"Z", b"a", b"9", b"uzzzzzzzzz", b"e42huhoug", b"E42HUHOUG",
        ];
        for a in &ids {
            for b in &ids {
                let (ka, kb) = (pack(a), pack(b));
                assert!(ka != 0 && kb != 0);
                assert_eq!(ka.cmp(&kb), a.cmp(b), "{:?} vs {:?}", a, b);
            }
            let mut out = Vec::new();
            unpack(pack(a), &mut out);
            assert_eq!(&out[..], *a);
        }
        for bad in [&b""[..], b" u1", b"u1 ", b"u_1", b"e3\r", b"A\tB", b"abcdefghijk", "é".as_bytes()] {
            assert_eq!(pack(bad), 0, "{:?}", bad);
        }
    }

    #[test]
    fn fallback_compares_bytewise() {
        let i = Interner::default();
        let a = i.key(b"A\tB");
        let b = i.key(b"u1");
        let c = i.key(b"user_id_a");
        assert_eq!(i.cmp(a, b), Ordering::Less);
        assert_eq!(i.cmp(b, c), Ordering::Less);
        assert_eq!(i.key(b"A\tB"), a);
        let n = i.freeze();
        assert_eq!(n.cmp(c, a), Ordering::Greater);
        let mut out = Vec::new();
        n.write(c, &mut out);
        assert_eq!(out, b"user_id_a");
    }
}
