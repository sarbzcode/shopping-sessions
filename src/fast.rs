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

use crate::ids::CODE;
use crate::parse::{parse_civil, parse_f3_13};

const MAX_FREE_FIELD: usize = 40;

/// An ID field of 0..=10 bytes from [0-9A-Za-z], optionally quoted, followed by `term`.
/// Returns its packed key (0 for an empty field) and the position after `term`.
#[inline(always)]
pub fn id_field(b: &[u8], mut p: usize, term: u8) -> Option<(u64, usize)> {
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
            return None; // too long to pack: the generic path interns it
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

/// A timestamp field in F1 (20 bytes), F2 (25 bytes) or 13-digit F3, optionally quoted, followed
/// by `term`. Anything else (including every bad timestamp) bails.
#[inline(always)]
pub fn ts_field(b: &[u8], mut p: usize, term: u8) -> Option<(i64, usize)> {
    let q = b[p] == b'"';
    p += q as usize;
    let (v, len) = if b[p + 4] == b'-' {
        if b[p + 19] == b'Z' {
            (parse_civil(&b[p..p + 20])?, 20)
        } else {
            (parse_civil(&b[p..p + 25])?, 25)
        }
    } else {
        (parse_f3_13(&b[p..p + 13])?, 13)
    };
    p += len;
    if q {
        if b[p] != b'"' {
            return None;
        }
        p += 1;
    }
    if b[p] != term {
        return None;
    }
    Some((v, p + 1))
}

/// A free-text field (product_id, campaign_id) of at most MAX_FREE_FIELD bytes with no quote or
/// newline inside, optionally quoted, followed by `term`. Returns the value's bounds.
#[inline(always)]
pub fn free_field(b: &[u8], mut p: usize, term: u8) -> Option<(usize, usize, usize)> {
    let q = b[p] == b'"';
    p += q as usize;
    let s = p;
    loop {
        let c = b[p];
        if c == term || c == b'"' || c == b'\n' || c == b',' {
            break;
        }
        p += 1;
        if p - s > MAX_FREE_FIELD {
            return None;
        }
    }
    let e = p;
    if q {
        if b[p] != b'"' {
            return None;
        }
        p += 1;
    }
    if b[p] != term {
        return None;
    }
    Some((s, e, p + 1))
}

/// The last field of a line: like `free_field`, but ended by '\n' (or the end of the buffer).
#[inline(always)]
pub fn last_field(b: &[u8], mut p: usize) -> Option<(usize, usize, usize)> {
    let q = b[p] == b'"';
    p += q as usize;
    let s = p;
    while p < b.len() {
        let c = b[p];
        if c == b'\n' || c == b'"' || c == b',' {
            break;
        }
        p += 1;
        if p - s > MAX_FREE_FIELD {
            return None;
        }
    }
    let e = p;
    if q {
        if p >= b.len() || b[p] != b'"' {
            return None;
        }
        p += 1;
    }
    if p == b.len() {
        return Some((s, e, p)); // the last line of the file, without '\n'
    }
    if b[p] != b'\n' {
        return None;
    }
    Some((s, e, p + 1))
}

/// view = 0, cart = 1, purchase = 2; anything else bails (the generic path marks it invalid).
#[inline(always)]
pub fn type_field(b: &[u8], mut p: usize, term: u8) -> Option<(u8, usize)> {
    let q = b[p] == b'"';
    p += q as usize;
    let w4 = &b[p..p + 4];
    let (kind, len) = if w4 == b"view" {
        (0, 4)
    } else if w4 == b"cart" {
        (1, 4)
    } else if &b[p..p + 8] == b"purchase" {
        (2, 8)
    } else {
        return None;
    };
    p += len;
    if q {
        if b[p] != b'"' {
            return None;
        }
        p += 1;
    }
    if b[p] != term {
        return None;
    }
    Some((kind, p + 1))
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
