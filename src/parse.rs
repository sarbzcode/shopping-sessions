//! The CSV dialect (R5-R11), timestamps (R12-R14) and prices (R24), byte by byte.

pub const DAY_MS: i64 = 86_400_000;

/// Bitmask of the bytes in `b[0..64]` that are ',', '"' or '\n'.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn specials64(b: &[u8]) -> u64 {
    use std::arch::x86_64::*;
    debug_assert!(b.len() >= 64);
    // SAFETY: SSE2 is part of the x86_64 baseline; the loads read b[0..64], which exists.
    unsafe {
        let p = b.as_ptr();
        let comma = _mm_set1_epi8(b',' as i8);
        let quote = _mm_set1_epi8(b'"' as i8);
        let nl = _mm_set1_epi8(b'\n' as i8);
        let mut m = 0u64;
        let mut i = 0;
        while i < 4 {
            let v = _mm_loadu_si128(p.add(16 * i) as *const __m128i);
            let x = _mm_or_si128(
                _mm_or_si128(_mm_cmpeq_epi8(v, comma), _mm_cmpeq_epi8(v, quote)),
                _mm_cmpeq_epi8(v, nl),
            );
            m |= (_mm_movemask_epi8(x) as u32 as u64) << (16 * i);
            i += 1;
        }
        m
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn specials64(b: &[u8]) -> u64 {
    let mut m = 0u64;
    for (i, &c) in b[..64].iter().enumerate() {
        if c == b',' || c == b'"' || c == b'\n' {
            m |= 1 << i;
        }
    }
    m
}

/// Comma, quote and newline bitmasks of 64 bytes.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn masks64(b: &[u8]) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    debug_assert!(b.len() >= 64);
    // SAFETY: SSE2 is part of the x86_64 baseline; the loads read b[0..64], which exists.
    unsafe {
        let p = b.as_ptr();
        let comma = _mm_set1_epi8(b',' as i8);
        let quote = _mm_set1_epi8(b'"' as i8);
        let nl = _mm_set1_epi8(b'\n' as i8);
        let (mut c, mut q, mut n) = (0u64, 0u64, 0u64);
        let mut i = 0;
        while i < 4 {
            let v = _mm_loadu_si128(p.add(16 * i) as *const __m128i);
            c |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, comma)) as u32 as u64) << (16 * i);
            q |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, quote)) as u32 as u64) << (16 * i);
            n |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, nl)) as u32 as u64) << (16 * i);
            i += 1;
        }
        (c, q, n)
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn masks64(b: &[u8]) -> (u64, u64, u64) {
    let (mut c, mut q, mut n) = (0u64, 0u64, 0u64);
    for (i, &x) in b[..64].iter().enumerate() {
        c |= ((x == b',') as u64) << i;
        q |= ((x == b'"') as u64) << i;
        n |= ((x == b'\n') as u64) << i;
    }
    (c, q, n)
}

const SUB_BLOCKS: usize = 256; // lines are indexed 16 KB at a time
const LOOKAHEAD_BLOCKS: usize = 4; // masks extend 256 bytes past the 16 KB so lines can finish

/// 128 mask bits starting at bit `o` of block `bi`.
#[inline(always)]
fn window(m: &[u64], bi: usize, o: u32) -> u128 {
    let lo = m[bi] as u128 | (m[bi + 1] as u128) << 64;
    if o == 0 {
        lo
    } else {
        (lo >> o) | ((m[bi + 2] as u128) << (128 - o))
    }
}

/// One line of at most 127 bytes, with its comma and quote bits (bit i = byte i of the line).
#[inline(always)]
fn fast_line<'a, const N: usize>(line: &'a [u8], cw: u128, qw: u128, f: &mut impl FnMut(&[&'a [u8]; N])) {
    if cw.count_ones() as usize != N - 1 {
        return; // R9: wrong field count (this includes the empty line, which has one field)
    }
    let nl = line.len();
    let mut bounds = [(0usize, 0usize); N];
    let mut c = cw;
    let mut s = 0usize;
    for b in bounds.iter_mut().take(N - 1) {
        let p = c.trailing_zeros() as usize;
        c &= c - 1;
        *b = (s, p);
        s = p + 1;
    }
    bounds[N - 1] = (s, nl);
    let mut fields: [&'a [u8]; N] = [&[]; N];
    if qw == 0 {
        for (fl, &(s, e)) in fields.iter_mut().zip(bounds.iter()) {
            *fl = &line[s..e];
        }
    } else {
        // R9: every quote must be one of a pair wrapping a whole field of length >= 2.
        let mut expect = 0u128;
        for (fl, &(s, e)) in fields.iter_mut().zip(bounds.iter()) {
            if s < e && line[s] == b'"' {
                if e - s < 2 || line[e - 1] != b'"' {
                    return;
                }
                expect |= (1u128 << s) | (1u128 << (e - 1));
                *fl = &line[s + 1..e - 1];
            } else {
                *fl = &line[s..e];
            }
        }
        if expect != qw {
            return;
        }
    }
    f(&fields);
}

/// Calls `f` with the N values of every well-formed line in `buf`.
///
/// `buf` holds whole lines. Only '\n' ends a line (R5); the last line may lack it (end of file,
/// NOTES ambiguity 1). Every ',' separates fields (R6). A field is valid if it has no '"' at all,
/// or is exactly one pair of quotes around the whole field (R7, R9); the quotes are stripped and
/// an empty value means null (R8). Lines with the wrong field count, a stray quote, or nothing at
/// all are malformed and skipped (R9). Spaces and '\r' are ordinary bytes (R5, R10).
///
/// Lines under 128 bytes take the bitmask path; longer ones fall back to `for_each_line_slow`.
#[inline(always)]
pub fn for_each_line<'a, const N: usize>(buf: &'a [u8], mut f: impl FnMut(&[&'a [u8]; N])) {
    let len = buf.len();
    let mut cm = [0u64; SUB_BLOCKS + LOOKAHEAD_BLOCKS + 3];
    let mut qm = [0u64; SUB_BLOCKS + LOOKAHEAD_BLOCKS + 3];
    let mut nm = [0u64; SUB_BLOCKS + LOOKAHEAD_BLOCKS + 3];
    let mut ls = 0usize;
    while ls < len {
        let a = ls;
        let b = (a + SUB_BLOCKS * 64).min(len);
        let end = (b + LOOKAHEAD_BLOCKS * 64).min(len);
        let nb = (end - a).div_ceil(64);
        for k in 0..nb {
            let p = a + 64 * k;
            let (c, q, n) = if p + 64 <= len {
                masks64(&buf[p..p + 64])
            } else {
                let mut tail = [0u8; 64];
                tail[..len - p].copy_from_slice(&buf[p..len]);
                masks64(&tail)
            };
            cm[k] = c;
            qm[k] = q;
            nm[k] = n;
        }
        for k in nb..nb + 3 {
            cm[k] = 0;
            qm[k] = 0;
            nm[k] = 0;
        }
        while ls < b {
            let off = ls - a;
            let (bi, o) = (off / 64, (off % 64) as u32);
            // the end of this line: the first '\n' at or after ls
            let mut le = usize::MAX;
            let m = nm[bi] >> o;
            if m != 0 {
                le = ls + m.trailing_zeros() as usize;
            } else {
                for k in bi + 1..nb {
                    if nm[k] != 0 {
                        le = a + 64 * k + nm[k].trailing_zeros() as usize;
                        break;
                    }
                }
            }
            if le == usize::MAX {
                if end == len {
                    le = len; // the last line has no '\n'
                } else {
                    // a line longer than the lookahead: find its end the slow way
                    le = buf[ls..].iter().position(|&x| x == b'\n').map_or(len, |i| ls + i);
                }
            }
            let nl = le - ls;
            if nl < 128 && le <= end {
                let lmask = (1u128 << nl) - 1;
                fast_line::<N>(&buf[ls..le], window(&cm, bi, o) & lmask, window(&qm, bi, o) & lmask, &mut f);
            } else {
                for_each_line_slow::<N>(&buf[ls..le], &mut f);
            }
            ls = le + 1;
        }
    }
}

/// Development aid: the cost of the splitter's layers on `buf` (masks; + line ends; + windows).
pub fn bench_layers(buf: &[u8]) -> [u64; 3] {
    let len = buf.len();
    let mut acc = [0u64; 3];
    // 1: masks only
    let mut p = 0;
    while p + 64 <= len {
        let (c, q, n) = masks64(&buf[p..p + 64]);
        acc[0] = acc[0].wrapping_add((c ^ q ^ n).count_ones() as u64);
        p += 64;
    }
    // 2: masks + line ends via the newline mask
    let mut p = 0;
    let mut prev = 0usize;
    while p + 64 <= len {
        let (_, _, mut n) = masks64(&buf[p..p + 64]);
        while n != 0 {
            let e = p + n.trailing_zeros() as usize;
            acc[1] = acc[1].wrapping_add((e - prev) as u64);
            prev = e + 1;
            n &= n - 1;
        }
        p += 64;
    }
    // 3: + 128-bit windows and popcounts per line (as for_each_line does)
    let mut cm = vec![0u64; len / 64 + 4];
    let mut qm = vec![0u64; len / 64 + 4];
    let mut nm = vec![0u64; len / 64 + 4];
    let mut p = 0;
    let mut k = 0;
    while p + 64 <= len {
        let (c, q, n) = masks64(&buf[p..p + 64]);
        cm[k] = c;
        qm[k] = q;
        nm[k] = n;
        p += 64;
        k += 1;
    }
    let mut ls = 0usize;
    let limit = k.saturating_sub(3) * 64;
    while ls < limit {
        let (bi, o) = (ls / 64, (ls % 64) as u32);
        let mut kk = bi;
        let mut m = nm[kk] >> o;
        let le = loop {
            if m != 0 {
                break kk * 64 + m.trailing_zeros() as usize + if kk == bi { o as usize } else { 0 };
            }
            kk += 1;
            m = nm[kk];
        };
        let nl = (le - ls).min(127);
        let lmask = (1u128 << nl) - 1;
        let cw = window(&cm, bi, o) & lmask;
        let qw = window(&qm, bi, o) & lmask;
        acc[2] = acc[2].wrapping_add(cw.count_ones() as u64 + qw.count_ones() as u64);
        ls = le + 1;
    }
    acc
}

/// The byte-at-a-time version of `for_each_line`: the reference for tests and the path for long lines.
#[inline(never)]
#[allow(unused_assignments)] // the macro's last expansion resets state nobody reads again
pub fn for_each_line_slow<'a, const N: usize>(buf: &'a [u8], mut f: impl FnMut(&[&'a [u8]; N])) {
    let len = buf.len();
    let mut fields: [&'a [u8]; N] = [&[]; N];
    let mut nf = 0usize; // fields closed so far on this line
    let mut bad = false;
    let mut fstart = 0usize; // start of the current field
    let mut q = 0u32; // quotes seen in the current field
    let mut q0 = 0usize; // position of its first quote
    let mut q1 = 0usize; // position of its last quote

    macro_rules! special {
        ($pos:expr, $c:expr) => {{
            let pos: usize = $pos;
            let c: u8 = $c;
            if c == b'"' {
                if q == 0 {
                    q0 = pos;
                }
                q1 = pos;
                q += 1;
            } else {
                let v: &'a [u8] = if q == 0 {
                    &buf[fstart..pos]
                } else if q == 2 && q0 == fstart && q1 + 1 == pos && pos - fstart >= 2 {
                    &buf[fstart + 1..pos - 1]
                } else {
                    bad = true;
                    &[]
                };
                if nf < N {
                    fields[nf] = v;
                }
                nf += 1;
                q = 0;
                fstart = pos + 1;
                if c == b'\n' {
                    if !bad && nf == N {
                        f(&fields);
                    }
                    nf = 0;
                    bad = false;
                }
            }
        }};
    }

    let mut base = 0usize;
    while base + 64 <= len {
        let mut m = specials64(&buf[base..base + 64]);
        while m != 0 {
            let pos = base + m.trailing_zeros() as usize;
            special!(pos, buf[pos]);
            m &= m - 1;
        }
        base += 64;
    }
    for pos in base..len {
        let c = buf[pos];
        if c == b',' || c == b'"' || c == b'\n' {
            special!(pos, c);
        }
    }
    if fstart < len || nf > 0 {
        special!(len, b'\n'); // the last line had no '\n'
    }
}

#[inline(always)]
fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[inline(always)]
fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a valid proleptic Gregorian date (Howard Hinnant's days_from_civil).
#[inline(always)]
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[inline(always)]
fn dig(c: u8) -> u32 {
    c.wrapping_sub(b'0') as u32
}

#[inline(always)]
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

/// Byte-mask with 0xFF at the given byte positions.
const fn bytes_mask(pos: &[usize]) -> u64 {
    let mut m = 0u64;
    let mut i = 0;
    while i < pos.len() {
        m |= 0xFF << (8 * pos[i]);
        i += 1;
    }
    m
}

const ZEROS: u64 = 0x3030_3030_3030_3030;
const LOW7: u64 = 0x7676_7676_7676_7676;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// After `x = w ^ ZEROS`, an ASCII digit byte is 0..=9 and every other byte is >= 10 (no borrows).
/// True if every byte of `x` selected by `mask` is <= 9 (`x + 0x76` cannot carry out of a valid byte).
#[inline(always)]
fn digits_ok(x: u64, mask: u64) -> bool {
    let d = x & mask;
    ((d.wrapping_add(LOW7 & mask) | d) & (HIGH & mask)) == 0
}

#[inline(always)]
fn byte(x: u64, i: u32) -> u32 {
    ((x >> (8 * i)) & 0xFF) as u32
}

/// F1 `YYYY-MM-DDTHH:MM:SSZ` (20 bytes) or F2 `YYYY-MM-DDTHH:MM:SS±HH:MM` (25 bytes).
#[inline(always)]
pub fn parse_civil(f: &[u8]) -> Option<i64> {
    let n = f.len();
    let w0 = le64(&f[0..8]); // YYYY-MM-
    let w1 = le64(&f[8..16]); // DDTHH:MM
    let w2 = u32::from_le_bytes(f[16..20].try_into().unwrap()) as u64; // :SSZ  or  :SS±
    const SEP0_M: u64 = bytes_mask(&[4, 7]);
    const SEP0: u64 = (b'-' as u64) << 32 | (b'-' as u64) << 56;
    const SEP1_M: u64 = bytes_mask(&[2, 5]);
    const SEP1: u64 = (b'T' as u64) << 16 | (b':' as u64) << 40;
    const DIG0: u64 = bytes_mask(&[0, 1, 2, 3, 5, 6]);
    const DIG1: u64 = bytes_mask(&[0, 1, 3, 4, 6, 7]);
    const DIG2: u64 = bytes_mask(&[1, 2]);
    let (x0, x1, x2) = (w0 ^ ZEROS, w1 ^ ZEROS, w2 ^ ZEROS);
    let mut ok = (w0 & SEP0_M) == SEP0
        && (w1 & SEP1_M) == SEP1
        && (w2 & 0xFF) == b':' as u64
        && digits_ok(x0, DIG0)
        && digits_ok(x1, DIG1)
        && digits_ok(x2, DIG2);
    let y = (byte(x0, 0) * 1000 + byte(x0, 1) * 100 + byte(x0, 2) * 10 + byte(x0, 3)) as i64;
    let mo = byte(x0, 5) * 10 + byte(x0, 6);
    let d = byte(x1, 0) * 10 + byte(x1, 1);
    let h = byte(x1, 3) * 10 + byte(x1, 4);
    let mi = byte(x1, 6) * 10 + byte(x1, 7);
    let s = byte(x2, 1) * 10 + byte(x2, 2);
    let zone = byte(w2, 3) as u8;
    let offset_min: i64 = if n == 20 {
        ok &= zone == b'Z';
        0
    } else {
        // F2: f[19] is the sign, then HH:MM
        let sign: i64 = if zone == b'+' { 1 } else { -1 };
        ok &= zone == b'+' || zone == b'-';
        ok &= f[22] == b':';
        let (a, b, c, e) = (dig(f[20]), dig(f[21]), dig(f[23]), dig(f[24]));
        ok &= a <= 9 && b <= 9 && c <= 9 && e <= 9;
        let (oh, om) = (a * 10 + b, c * 10 + e);
        ok &= oh <= 23 && om <= 59;
        sign * (oh * 60 + om) as i64
    };
    ok &= mo >= 1 && mo <= 12 && d >= 1 && h <= 23 && mi <= 59 && s <= 59;
    if !ok || d > days_in_month(y, mo) {
        return None;
    }
    let secs = (h * 3600 + mi * 60 + s) as i64;
    Some(days_from_civil(y, mo, d) * DAY_MS + secs * 1000 - offset_min * 60_000)
}

/// Eight ASCII digits (most significant first, little-endian load) to their value.
#[inline(always)]
fn parse8(x: u64) -> u64 {
    // x = w ^ ZEROS (digit values per byte); the SWAR reduction from simdjson.
    let v = (x & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    let v = (v & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
    ((v & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32) & 0xFFFF_FFFF
}

/// F3 with exactly 13 characters (every epoch-ms value from 2001 to 2286): the common case.
#[inline(always)]
pub fn parse_f3_13(f: &[u8]) -> Option<i64> {
    let a = le64(&f[0..8]) ^ ZEROS;
    let b = le64(&f[5..13]) ^ ZEROS;
    if !(digits_ok(a, u64::MAX) && digits_ok(b, u64::MAX)) {
        return None;
    }
    Some((parse8(a) * 100_000 + parse8(b) % 100_000) as i64)
}

/// F3: digits only, epoch ms, leading zeros allowed. More than 15 significant digits is at least
/// 1e15 ms (year 33658), outside any window an F1 manifest can express, so it is bad (R14).
#[inline(always)]
fn parse_f3(f: &[u8]) -> Option<i64> {
    if f.len() == 13 {
        return parse_f3_13(f);
    }
    parse_f3_generic(f)
}

#[inline(never)]
fn parse_f3_generic(f: &[u8]) -> Option<i64> {
    if f.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    let mut sig = 0u32;
    for &c in f {
        let x = c.wrapping_sub(b'0');
        if x > 9 {
            return None;
        }
        if sig > 0 || x != 0 {
            sig += 1;
            if sig > 15 {
                return None;
            }
            v = v * 10 + x as i64;
        }
    }
    Some(v)
}

/// R12-R14: a timestamp in one of the forms F1, F2, F3 as epoch ms, or None if it is bad.
/// The caller applies the window (R16, R17).
#[inline(always)]
pub fn parse_ts(f: &[u8]) -> Option<i64> {
    let n = f.len();
    if (n == 20 || n == 25) && f[4] == b'-' {
        parse_civil(f)
    } else {
        parse_f3(f)
    }
}

/// R24: a purchase price is digits only with a value from 1 to 10,000,000 (leading zeros allowed).
#[inline(always)]
pub fn parse_price(f: &[u8]) -> Option<u32> {
    if f.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    let mut sig = 0u32;
    for &c in f {
        let x = c.wrapping_sub(b'0');
        if x > 9 {
            return None;
        }
        if sig > 0 || x != 0 {
            sig += 1;
            if sig > 8 {
                return None;
            }
            v = v * 10 + x as u32;
        }
    }
    if (1..=10_000_000).contains(&v) {
        Some(v)
    } else {
        None
    }
}

/// Exact string -> index lookup for products.csv / campaigns.csv keys, with a dense fast path for
/// keys of the form `<prefix><n>` (n without leading zeros), which is what the generator writes.
pub struct Lookup {
    prefix: &'static [u8],
    dense: Vec<u32>,
    map: std::collections::HashMap<Vec<u8>, u32>,
}

const ABSENT: u32 = u32::MAX;
const DENSE_LIMIT: u64 = 1 << 22;

impl Lookup {
    pub fn new(prefix: &'static [u8]) -> Self {
        Self { prefix, dense: Vec::new(), map: Default::default() }
    }

    #[inline(always)]
    fn dense_index(&self, b: &[u8]) -> Option<usize> {
        if b.len() <= self.prefix.len() || b.len() > self.prefix.len() + 7 || &b[..self.prefix.len()] != self.prefix {
            return None;
        }
        let digits = &b[self.prefix.len()..];
        if digits[0] == b'0' {
            return None;
        }
        let mut n: u64 = 0;
        for &c in digits {
            let x = c.wrapping_sub(b'0');
            if x > 9 {
                return None;
            }
            n = n * 10 + x as u64;
        }
        if n < DENSE_LIMIT {
            Some(n as usize)
        } else {
            None
        }
    }

    /// Inserts `key -> value` unless the key is already present (the first occurrence wins).
    pub fn insert_first(&mut self, key: &[u8], value: u32) {
        match self.dense_index(key) {
            Some(n) => {
                if self.dense.len() <= n {
                    self.dense.resize(n + 1, ABSENT);
                }
                if self.dense[n] == ABSENT {
                    self.dense[n] = value;
                }
            }
            None => {
                self.map.entry(key.to_vec()).or_insert(value);
            }
        }
    }

    #[inline(always)]
    pub fn get(&self, key: &[u8]) -> Option<u32> {
        match self.dense_index(key) {
            Some(n) => match self.dense.get(n) {
                Some(&v) if v != ABSENT => Some(v),
                _ => None,
            },
            None => {
                if self.map.is_empty() {
                    None
                } else {
                    self.map.get(key).copied()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines<const N: usize>(s: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for_each_line::<N>(s, |f| out.push(f.iter().map(|x| x.to_vec()).collect()));
        out
    }

    #[test]
    fn dialect() {
        let got = lines::<2>(b"a,b\n\"u4\",\"u1\"\nu1,u2,u3\nu1,\n,u3\nu1,u5\"\n\nu1\n\"\",x\n\"\"\"\",x\n\" a\",b \r\nlast,line");
        let want: Vec<Vec<&[u8]>> = vec![
            vec![b"a", b"b"],
            vec![b"u4", b"u1"],
            vec![b"u1", b""],
            vec![b"", b"u3"],
            vec![b"", b"x"],
            vec![b" a", b"b \r"],
            vec![b"last", b"line"],
        ];
        assert_eq!(got.len(), want.len(), "{:?}", got);
        for (g, w) in got.iter().zip(want.iter()) {
            assert_eq!(g.iter().map(|x| &x[..]).collect::<Vec<_>>(), *w);
        }
        // Long lines exercise the 64-byte SIMD path.
        let long = format!("{},{}\n{}\"x\",y\n", "a".repeat(70), "b".repeat(70), "c".repeat(100));
        let got = lines::<2>(long.as_bytes());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][0].len(), 70);
    }

    fn lines_slow<const N: usize>(s: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for_each_line_slow::<N>(s, |f| out.push(f.iter().map(|x| x.to_vec()).collect()));
        out
    }

    /// The bitmask splitter agrees with the byte-at-a-time one on random dirty input,
    /// including lines that cross 64-byte blocks, 16 KB windows and the 128-byte limit.
    #[test]
    fn fast_and_slow_splitters_agree() {
        let alphabet: &[u8] = b"ab,,,\"\"\"\n\n x\r19";
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..300 {
            let len = (next() % 40_000) as usize;
            let mut buf = Vec::with_capacity(len);
            let long_lines = round % 3 == 0;
            for _ in 0..len {
                let r = next();
                if long_lines && r % 97 == 0 {
                    buf.extend(std::iter::repeat_n(b'z', (r % 300) as usize));
                }
                buf.push(alphabet[(r % alphabet.len() as u64) as usize]);
            }
            assert_eq!(lines::<2>(&buf), lines_slow::<2>(&buf), "round {round} N=2");
            assert_eq!(lines::<4>(&buf), lines_slow::<4>(&buf), "round {round} N=4");
            assert_eq!(lines::<7>(&buf), lines_slow::<7>(&buf), "round {round} N=7");
        }
    }

    #[test]
    fn f3_fast_path_matches_generic() {
        let mut state = 12345u64;
        for _ in 0..200_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let v = state % 10_000_000_000_000;
            let s = format!("{v:013}");
            assert_eq!(parse_f3_13(s.as_bytes()), parse_f3_generic(s.as_bytes()), "{s}");
            assert_eq!(parse_f3_13(s.as_bytes()), Some(v as i64));
        }
        for bad in ["179102160000a", "17910216000 0", "-791021600000", "1791021600.00"] {
            assert_eq!(parse_f3_13(bad.as_bytes()), None, "{bad}");
        }
    }

    /// The SWAR civil parser agrees with a plain one on random valid and corrupted strings.
    #[test]
    fn civil_parser_matches_reference() {
        fn reference(f: &[u8]) -> Option<i64> {
            let n = f.len();
            if n != 20 && n != 25 {
                return None;
            }
            let digit_pos = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
            if f[4] != b'-' || f[7] != b'-' || f[10] != b'T' || f[13] != b':' || f[16] != b':' {
                return None;
            }
            if digit_pos.iter().any(|&i| !f[i].is_ascii_digit()) {
                return None;
            }
            let num = |a: usize, b: usize| -> u32 { f[a..b].iter().fold(0, |v, &c| v * 10 + (c - b'0') as u32) };
            let (y, mo, d, h, mi, s) = (num(0, 4) as i64, num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19));
            let off: i64 = if n == 20 {
                if f[19] != b'Z' {
                    return None;
                }
                0
            } else {
                if (f[19] != b'+' && f[19] != b'-') || f[22] != b':' || ![20, 21, 23, 24].iter().all(|&i| f[i].is_ascii_digit()) {
                    return None;
                }
                let (oh, om) = (num(20, 22), num(23, 25));
                if oh > 23 || om > 59 {
                    return None;
                }
                (if f[19] == b'+' { 1 } else { -1 }) * (oh * 60 + om) as i64
            };
            if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
                return None;
            }
            Some(days_from_civil(y, mo, d) * DAY_MS + ((h * 3600 + mi * 60 + s) as i64) * 1000 - off * 60_000)
        }
        let mut state = 777u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state >> 33
        };
        let noise = b"0123456789-:TZtz+ .\r";
        for i in 0..300_000 {
            let mut s = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                next() % 10000,
                next() % 14,
                next() % 33,
                next() % 26,
                next() % 62,
                next() % 62
            )
            .into_bytes();
            if i % 2 == 0 {
                s.push(b'Z');
            } else {
                s.extend_from_slice(format!("{}{:02}:{:02}", if next() % 2 == 0 { '+' } else { '-' }, next() % 26, next() % 62).as_bytes());
            }
            if next() % 4 == 0 {
                let at = (next() as usize) % s.len();
                s[at] = noise[(next() as usize) % noise.len()];
            }
            assert_eq!(parse_ts(&s), reference(&s), "{}", String::from_utf8_lossy(&s));
        }
    }

    #[test]
    fn timestamps() {
        let ok = |s: &str| parse_ts(s.as_bytes());
        assert_eq!(ok("2026-10-03T23:58:10Z"), Some(1_791_071_890_000));
        assert_eq!(ok("2026-10-03T19:58:10-04:00"), Some(1_791_071_890_000));
        assert_eq!(ok("1791071890000"), Some(1_791_071_890_000));
        assert_eq!(ok("0001791071890000"), Some(1_791_071_890_000));
        assert_eq!(ok("2026-10-04T01:30:00+02:00"), ok("2026-10-03T23:30:00Z"));
        assert_eq!(ok("2026-10-03T12:00:00-00:00"), ok("2026-10-03T12:00:00Z"));
        assert_eq!(ok("2026-10-04T09:59:00+23:59"), ok("2026-10-03T10:00:00Z"));
        assert_eq!(ok("2028-02-29T00:00:00Z"), Some(days_from_civil(2028, 2, 29) * DAY_MS));
        assert_eq!(ok("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ok("86400000"), Some(86_400_000));
        for bad in [
            "", "2026-10-03T23:58:10.123Z", "2026-10-03T23:58:10z", "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z", "2026-10-03T23:58:10", "1791071890000.0", "1.79e12", "-5", "+500",
            "2026-13-03T23:58:10Z", "2026-02-30T23:58:10Z", "2027-02-29T00:00:00Z", "2026-10-03T24:00:00Z",
            "2026-10-03T23:60:10Z", "2026-10-03T23:58:60Z", "2026-10-03T10:00:00+24:00",
            "2026-10-03T10:00:00+05:60", "2026-10-03T10:00:00+0200", "179102160000000000000",
            "2026-00-03T23:58:10Z", "2026-10-00T23:58:10Z", "٢٠٢٦", "２026-10-03T10:00:00Z",
            "2026-10-03T10:00:00Z\r", "1000000000000000",
        ] {
            assert_eq!(ok(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn prices() {
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        assert_eq!(parse_price(b"1"), Some(1));
        assert_eq!(parse_price(b"00000000000000000001"), Some(1));
        for bad in ["", "0", "00", "10000001", "250000000", "19.99", "-5", "+500", "1e3", "1000\r", "２５０"] {
            assert_eq!(parse_price(bad.as_bytes()), None, "{bad:?}");
        }
    }

    #[test]
    fn lookup() {
        let mut l = Lookup::new(b"p");
        l.insert_first(b"p1", 1);
        l.insert_first(b"p50000", 2);
        l.insert_first(b"P7", 3);
        l.insert_first(b"p1", 9);
        assert_eq!(l.get(b"p1"), Some(1));
        assert_eq!(l.get(b"p50000"), Some(2));
        assert_eq!(l.get(b"P7"), Some(3));
        assert_eq!(l.get(b"p01"), None);
        assert_eq!(l.get(b"p2"), None);
        assert_eq!(l.get(b" p1"), None);
        assert_eq!(l.get(b"p99999"), None);
        assert_eq!(l.get(b""), None);
    }
}
