//! Fast-path line parsers for the common shapes of generated lines.
//!
//! Each parser walks one line field by field, packing IDs while it scans and parsing timestamps at
//! fixed offsets. Whenever a line is not exactly in the shape it expects (a stray or unbalanced
//! quote, an extra or missing field, a bad timestamp, an ID that does not pack, a long field), it
//! returns None ("bail") and the caller parses that one line with the generic splitter
//! (`parse::for_each_line_slow`) instead. So the fast path never decides anything the generic
//! path would decide differently; tests check that on randomized dirty lines.
//!
//! All indexing is bounds-checked; callers keep >= 160 bytes of slack before the buffer end.

use crate::parse::{parse_civil, parse_f3_13};

const MAX_FREE_FIELD: usize = 40;

const HI: u64 = 0x8080_8080_8080_8080;
const LO7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
const ONES: u64 = 0x0101_0101_0101_0101;

#[inline(always)]
fn load8(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

/// Per byte of `x`: 0x80 if the byte is in [0-9A-Za-z], else 0.
/// (`lo + 0x80 - c` cannot carry between bytes: lo <= 0x7F and 0x80 - c <= 0x50.)
#[inline(always)]
fn alnum_bits(x: u64) -> u64 {
    let lo = x & LO7;
    let ge = |c: u64| (lo + (0x80 - c) * ONES) & HI;
    ((ge(0x30) & !ge(0x3A)) | (ge(0x41) & !ge(0x5B)) | (ge(0x61) & !ge(0x7B))) & !(x & HI)
}

/// Per byte: the 6-bit packing code (as `ids::CODE`) of an alphanumeric byte. Bytes that are not
/// alphanumeric produce garbage, and may borrow from higher bytes only; callers mask them off.
#[inline(always)]
fn codes(x: u64) -> u64 {
    let lo = x & LO7;
    let ge_upper = ((lo + 0x3F * ONES) & HI) >> 7; // byte >= 'A'
    let ge_lower = ((lo + 0x1F * ONES) & HI) >> 7; // byte >= 'a'
    lo.wrapping_sub(0x2F * ONES).wrapping_sub(ge_upper * 7).wrapping_sub(ge_lower * 6)
}

/// Eight codes (code of char i in byte i) to 48 bits with char 0 highest (as `ids::pack`).
#[inline(always)]
fn pack8(c: u64) -> u64 {
    let t1 = ((c & 0x003F_003F_003F_003F) << 6) | ((c >> 8) & 0x003F_003F_003F_003F);
    let t2 = ((t1 & 0x0000_0FFF_0000_0FFF) << 12) | ((t1 >> 16) & 0x0000_0FFF_0000_0FFF);
    ((t2 & 0xFF_FFFF) << 24) | ((t2 >> 32) & 0xFF_FFFF)
}

/// 0x80 in every byte of `x` equal to `c` (exact: no false positives from borrows).
#[inline(always)]
fn eq_bits(x: u64, c: u8) -> u64 {
    let y = x ^ (ONES * c as u64);
    !(((y & LO7) + LO7) | y | LO7)
}

/// An ID field of 0..=10 bytes from [0-9A-Za-z], optionally quoted, followed by `term`.
/// Returns its packed key (0 for an empty field) and the position after `term`.
/// (A byte loop: measured faster here than a word-at-a-time version, since most IDs have the
/// same length and the loop branch predicts well. `id_field_swar` is kept for comparison.)
#[inline(always)]
pub fn id_field(b: &[u8], p: usize, term: u8) -> Option<(u64, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let mut k = 0u64;
    let mut n = 0usize;
    loop {
        let c = crate::ids::CODE[b[s + n] as usize];
        if c == 0 {
            break;
        }
        if n == 10 {
            return None; // too long to pack: the generic path interns it
        }
        k |= (c as u64) << (54 - 6 * n);
        n += 1;
    }
    let e = s + n;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != term {
        return None;
    }
    Some((k, after + 1))
}

/// Word-at-a-time version of `id_field` (no per-character loop); same results.
#[allow(dead_code)]
#[inline(always)]
pub fn id_field_swar(b: &[u8], p: usize, term: u8) -> Option<(u64, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let (x0, x1) = (load8(b, s), load8(b, s + 8));
    let n0 = ((!alnum_bits(x0) & HI).trailing_zeros() / 8) as usize;
    let n = if n0 < 8 { n0 } else { 8 + ((!alnum_bits(x1) & HI).trailing_zeros() / 8) as usize };
    if n > 10 {
        return None;
    }
    let mask0 = if n >= 8 { u64::MAX } else { (1u64 << (8 * n)) - 1 };
    let mut k = pack8(codes(x0) & mask0) << 12;
    if n > 8 {
        let c1 = codes(x1);
        let c9 = if n > 9 { (c1 >> 8) & 0x3F } else { 0 };
        k |= ((c1 & 0x3F) << 6) | c9;
    }
    let e = s + n;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != term {
        return None;
    }
    Some((k, after + 1))
}

/// A timestamp field in F1 (20 bytes), F2 (25 bytes) or 13-digit F3, optionally quoted, followed
/// by `term`. Anything else (including every bad timestamp) bails.
#[inline(always)]
pub fn ts_field(b: &[u8], p: usize, term: u8) -> Option<(i64, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let (v, len) = if b[s + 4] == b'-' {
        if b[s + 19] == b'Z' {
            (parse_civil(&b[s..s + 20])?, 20)
        } else {
            (parse_civil(&b[s..s + 25])?, 25)
        }
    } else {
        (parse_f3_13(&b[s..s + 13])?, 13)
    };
    let e = s + len;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != term {
        return None;
    }
    Some((v, after + 1))
}

/// The first byte at or after `s` that is ',', '"' or '\n' (word at a time), within
/// MAX_FREE_FIELD bytes; None if there is none that close.
#[inline(always)]
fn delimiter(b: &[u8], s: usize) -> Option<usize> {
    let mut w = s;
    while w - s <= MAX_FREE_FIELD {
        let x = load8(b, w);
        let m = eq_bits(x, b',') | eq_bits(x, b'"') | eq_bits(x, b'\n');
        if m != 0 {
            return Some(w + (m.trailing_zeros() / 8) as usize);
        }
        w += 8;
    }
    None
}

/// A free-text field (product_id, campaign_id) of at most MAX_FREE_FIELD bytes with no comma,
/// quote or newline inside, optionally quoted, followed by `term`. Returns the value's bounds.
#[inline(always)]
pub fn free_field(b: &[u8], p: usize, term: u8) -> Option<(usize, usize, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let e = delimiter(b, s)?;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != term {
        return None;
    }
    Some((s, e, after + 1))
}

/// The last field of a line: like `free_field`, but ended by '\n'. (A last line without '\n'
/// is never in the fast region, which keeps SLACK bytes before the buffer end.)
#[inline(always)]
pub fn last_field(b: &[u8], p: usize) -> Option<(usize, usize, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let e = delimiter(b, s)?;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != b'\n' {
        return None;
    }
    Some((s, e, after + 1))
}

/// view = 0, cart = 1, purchase = 2; anything else bails (the generic path marks it invalid).
#[inline(always)]
pub fn type_field(b: &[u8], p: usize, term: u8) -> Option<(u8, usize)> {
    let q = (b[p] == b'"') as usize;
    let s = p + q;
    let w8 = load8(b, s);
    let w4 = w8 & 0xFFFF_FFFF;
    let (kind, len) = if w4 == u32::from_le_bytes(*b"view") as u64 {
        (0, 4)
    } else if w4 == u32::from_le_bytes(*b"cart") as u64 {
        (1, 4)
    } else if w8 == u64::from_le_bytes(*b"purchase") {
        (2, 8)
    } else {
        return None;
    };
    let e = s + len;
    let close_ok = q == 0 || b[e] == b'"';
    let after = e + q;
    if !close_ok || b[after] != term {
        return None;
    }
    Some((kind, after + 1))
}

pub struct EventLine {
    pub ekey: u64,
    pub ukey: u64,
    pub ts: i64,
    pub ingest: i64,
    pub kind: u8,
    pub product: (usize, usize),
    pub price: (usize, usize),
}

/// `event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents`
#[inline(always)]
pub fn event_line(b: &[u8], p: usize) -> Option<(EventLine, usize)> {
    let (ekey, p) = id_field(b, p, b',')?;
    let (ukey, p) = id_field(b, p, b',')?;
    let (ts, p) = ts_field(b, p, b',')?;
    let (ingest, p) = ts_field(b, p, b',')?;
    let (kind, p) = type_field(b, p, b',')?;
    let (ps, pe, p) = free_field(b, p, b',')?;
    let (rs, re, p) = last_field(b, p)?;
    Some((EventLine { ekey, ukey, ts, ingest, kind, product: (ps, pe), price: (rs, re) }, p))
}

pub struct ClickLine {
    pub ckey: u64,
    pub ukey: u64,
    pub campaign: (usize, usize),
    pub ts: i64,
}

/// `click_id,user_id,campaign_id,ts`
#[inline(always)]
pub fn click_line(b: &[u8], p: usize) -> Option<(ClickLine, usize)> {
    let (ckey, p) = id_field(b, p, b',')?;
    let (ukey, p) = id_field(b, p, b',')?;
    let (cs, ce, p) = free_field(b, p, b',')?;
    let (ts, p) = ts_field(b, p, b'\n')?;
    Some((ClickLine { ckey, ukey, campaign: (cs, ce), ts }, p))
}

/// `user_id_a,user_id_b`
#[inline(always)]
pub fn link_line(b: &[u8], p: usize) -> Option<((u64, u64), usize)> {
    let (a, p) = id_field(b, p, b',')?;
    let (k, p) = id_field(b, p, b'\n')?;
    Some(((a, k), p))
}

/// The fast parsers read at most ~140 bytes past a line start (and `last_field` checks the end),
/// so they run only while this much of the buffer is left; the rest goes to the generic path.
pub const SLACK: usize = 192;

/// The end of the line starting at `p` (the position of its '\n', or the buffer end).
#[inline]
pub fn line_end(b: &[u8], p: usize) -> usize {
    b[p..].iter().position(|&c| c == b'\n').map_or(b.len(), |i| p + i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CODE;

    /// The byte-at-a-time version id_field replaced: the reference.
    fn id_field_ref(b: &[u8], mut p: usize, term: u8) -> Option<(u64, usize)> {
        let q = b[p] == b'"';
        p += q as usize;
        let mut k = 0u64;
        let mut n = 0usize;
        loop {
            let c = CODE[b[p] as usize];
            if c == 0 {
                break;
            }
            if n == 10 {
                return None;
            }
            k |= (c as u64) << (54 - 6 * n);
            n += 1;
            p += 1;
        }
        if q {
            if b[p] != b'"' {
                return None;
            }
            p += 1;
        }
        if b[p] != term {
            return None;
        }
        Some((k, p + 1))
    }

    #[test]
    fn swar_id_field_matches_reference() {
        let pool: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ _,\"\r\n\x80\xff@[`{/:";
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..400_000 {
            let len = (next() % 13) as usize;
            let mut line = Vec::new();
            let quoted = next() % 4 == 0;
            if quoted {
                line.push(b'"');
            }
            for _ in 0..len {
                // mostly alphanumeric, sometimes anything from the pool
                let r = next();
                let c = if r % 8 == 0 { pool[(r >> 8) as usize % pool.len()] } else { pool[(r >> 8) as usize % 62] };
                line.push(c);
            }
            if quoted && next() % 8 != 0 {
                line.push(b'"');
            }
            let term = if next() % 2 == 0 { b',' } else { b'\n' };
            line.push(if next() % 16 == 0 { b'x' } else { term });
            line.extend_from_slice(&[b'z'; 40]);
            let want = id_field_ref(&line, 0, term);
            assert_eq!(id_field(&line, 0, term), want, "{:?}", String::from_utf8_lossy(&line[..len + 3]));
            assert_eq!(id_field_swar(&line, 0, term), want, "{:?}", String::from_utf8_lossy(&line[..len + 3]));
        }
    }

    #[test]
    fn swar_codes_match_table() {
        for c in 0u8..=255 {
            let a = alnum_bits(c as u64) & 0x80 != 0;
            assert_eq!(a, CODE[c as usize] != 0, "byte {c}");
            if a {
                assert_eq!((codes(c as u64) & 0xFF) as u8, CODE[c as usize], "byte {c}");
            }
        }
    }
}
