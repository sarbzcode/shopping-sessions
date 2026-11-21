//! Shopping Sessions as one native binary. Rule numbers R1..R40 refer to NOTES.md.
//!
//! Stage 1  parse links, events and clicks in parallel (file chunks on an atomic queue). Events go to
//!          P1 partitions by event_id hash; the thread that finishes the last link chunk runs the
//!          union-find (R18-R21) while the others keep parsing.
//! Stage 2  per P1 partition: dedup on event_id (R23), keep valid kept copies, map user -> person,
//!          scatter to P2 partitions by person hash. Clicks are mapped and scattered too.
//! Stage 3  per P2 partition: group by person, sort by ts, sessionize (R26, R27), credit and assists
//!          with a sliding 7-day click window (R32-R35), aggregate both tables.
//! Stage 4  write the Parquet files in parallel.

mod fast;
mod ids;
mod parquet;
mod parse;
mod table;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use ids::{Interner, Names};
use parse::{for_each_line, for_each_line_slow, parse_price, parse_ts, Lookup, DAY_MS};
use table::{mix, Map};

const SESSION_GAP: i64 = 1_800_000; // R26: a gap over 30 min starts a new session
const CLICK_REACH: i64 = 604_800_000; // R17, R32, R35: 7 days
const LATE_MS: i64 = 86_400_000; // R25: ingest_ts - ts over 24 h is late
const NO_CATEGORY: u16 = u16::MAX;
const CHUNK_BYTES: usize = 2 << 20;

static T0: OnceLock<Instant> = OnceLock::new();

fn log(msg: &str) {
    let ms = T0.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1e3);
    eprintln!("[{ms:8.1} ms] {msg}");
}

/// Per-thread time accounting (development aid, printed with SS_PROFILE=1).
#[derive(Default, Clone)]
struct Prof {
    ns: [u64; 8],
}

impl Prof {
    #[inline(always)]
    fn add(&mut self, slot: usize, since: Instant) {
        self.ns[slot] += since.elapsed().as_nanos() as u64;
    }
}

fn report(stage: &str, labels: &[&str], profs: &[&Prof]) {
    if std::env::var("SS_PROFILE").is_err() {
        return;
    }
    for (i, l) in labels.iter().enumerate() {
        let v: Vec<f64> = profs.iter().map(|p| p.ns[i] as f64 / 1e6).collect();
        let sum: f64 = v.iter().sum();
        let max = v.iter().cloned().fold(0.0, f64::max);
        eprintln!("  {stage:8} {l:22} sum {sum:8.1} ms  max/thread {max:7.1} ms");
    }
}

fn main() {
    T0.set(Instant::now()).ok();
    if let Err(e) = run() {
        eprintln!("shopping-sessions: error: {e}");
        std::process::exit(1);
    }
}

type Res<T> = Result<T, String>;

// ---------------------------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------------------------

// Every `ts` below is a u32 millisecond offset from `base = window_start - 7 days`, the start of
// the click window (R17); events are at offsets [7 d, 7 d + window). Windows up to 42 days fit.
// `pc` packs a purchase's price (bits 0..24; 10,000,000 < 2^24) and category (bits 24..32);
// it is 0 for views and carts.

/// One parsed copy of an event (before dedup). `ukey != 0` means this copy is valid (R24, R25):
/// a valid copy always has a non-empty user_id, and no ID key is 0.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
struct Ev1 {
    ekey: u64,
    ingest: i64,
    ukey: u64,
    ts: u32,
    pc: u32,
}

/// A surviving event, keyed by its person.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Ev2 {
    person: u64,
    ts: u32,
    pc: u32,
}

#[inline(always)]
fn pc_price(pc: u32) -> u32 {
    pc & 0x00FF_FFFF
}

#[inline(always)]
fn pc_cat(pc: u32) -> u16 {
    (pc >> 24) as u16
}

/// A valid click; `key` is the user key until stage 2 and the person key after.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
#[repr(C)]
struct Ck {
    key: u64,
    ckey: u64,
    ts: u32,
    camp: u32,
}

#[derive(Clone, Copy)]
struct UdRow {
    person: u64,
    revenue: u64,
    day: i32,
    sessions: u32,
    events: u32,
    top: u16,
}

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

enum Mapped {
    Map(memmap2::Mmap),
    Empty,
}

impl Mapped {
    fn bytes(&self) -> &[u8] {
        match self {
            Mapped::Map(m) => m,
            Mapped::Empty => &[],
        }
    }
}

fn map_file(path: &Path) -> Res<Mapped> {
    let f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len == 0 {
        return Ok(Mapped::Empty);
    }
    // SAFETY: /data is read-only and nothing modifies the files while we run.
    let m = unsafe { memmap2::MmapOptions::new().map(&f) }.map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Mapped::Map(m))
}

/// R2: every `*.csv` directly in the folder (NOTES ambiguity 2).
fn list_csv(dir: &Path) -> Res<Vec<PathBuf>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".csv") && !name.starts_with('.') && entry.path().is_file() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// R3: the first line of every file is its header.
fn body(data: &[u8]) -> usize {
    data.iter().position(|&c| c == b'\n').map_or(data.len(), |i| i + 1)
}

fn json_string<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let at = text.find(&pat)? + pat.len();
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// products.csv (R28): product_id -> category index; categories in byte order for ties.
struct Catalog {
    lookup: Lookup,
    names: Vec<Vec<u8>>,
    rank: Vec<u16>,
    unknown: u16,
}

fn load_products(path: &Path) -> Res<Catalog> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut index: HashMap<Vec<u8>, u16> = HashMap::new();
    let mut intern = |b: &[u8], names: &mut Vec<Vec<u8>>| -> u16 {
        *index.entry(b.to_vec()).or_insert_with(|| {
            names.push(b.to_vec());
            (names.len() - 1) as u16
        })
    };
    let mut lookup = Lookup::new(b"p");
    let mut pending: Vec<(Vec<u8>, Option<u16>)> = Vec::new();
    for_each_line::<2>(&data[body(&data)..], |f| {
        if !f[0].is_empty() {
            let cat = if f[1].is_empty() { None } else { Some(intern(f[1], &mut names)) };
            pending.push((f[0].to_vec(), cat));
        }
    });
    // A missing category (and any product not in the file) is `unknown` (R28).
    let unknown = intern(b"unknown", &mut names);
    for (pid, cat) in pending {
        lookup.insert_first(&pid, cat.unwrap_or(unknown) as u32);
    }
    if names.len() > 256 {
        // Records keep a category in 8 bits; the generator writes 20 categories.
        return Err(format!("{} categories in products.csv; at most 255 plus `unknown` are supported", names.len() - 1));
    }
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by(|&a, &b| names[a].cmp(&names[b]));
    let mut rank = vec![0u16; names.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r as u16;
    }
    Ok(Catalog { lookup, names, rank, unknown })
}

/// campaigns.csv (R31, R39).
struct Campaigns {
    lookup: Lookup,
    ids: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
}

fn load_campaigns(path: &Path) -> Res<Campaigns> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut c = Campaigns { lookup: Lookup::new(b"c_"), ids: Vec::new(), channels: Vec::new() };
    let mut seen: HashMap<Vec<u8>, ()> = HashMap::new();
    for_each_line::<2>(&data[body(&data)..], |f| {
        if !f[0].is_empty() && seen.insert(f[0].to_vec(), ()).is_none() {
            c.lookup.insert_first(f[0], c.ids.len() as u32);
            c.ids.push(f[0].to_vec());
            c.channels.push(f[1].to_vec());
        }
    });
    Ok(c)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Link,
    Event,
    Click,
}

struct Task {
    kind: Kind,
    file: u32,
    start: usize,
    end: usize,
}

/// Splits a file's lines (after the header) into chunks that end on line boundaries.
fn split_file(kind: Kind, file: usize, data: &[u8], tasks: &mut Vec<Task>) {
    let len = data.len();
    let mut s = body(data);
    while s < len {
        let mut e = (s + CHUNK_BYTES).min(len);
        if e < len {
            e = data[e..].iter().position(|&c| c == b'\n').map_or(len, |i| e + i + 1);
        }
        tasks.push(Task { kind, file: file as u32, start: s, end: e });
        s = e;
    }
}

// ---------------------------------------------------------------------------------------------
// Stage 1: parse
// ---------------------------------------------------------------------------------------------

struct C1<'a> {
    files: &'a [Mapped],
    tasks: &'a [Task],
    next: AtomicUsize,
    links_left: AtomicUsize,
    link_pairs: Mutex<Vec<Vec<(u64, u64)>>>,
    interner: &'a Interner,
    pmap: OnceLock<Map<u64>>,
    products: &'a Catalog,
    campaigns: &'a Campaigns,
    ws: i64,
    we: i64,
    base: i64,
    p1_shift: u32,
    p1: usize,
    part_cap: usize,
}

struct S1 {
    ev: Vec<Vec<Ev1>>,
    ck: Vec<Ck>,
    prof: Prof,
}

/// R24/R25 on one parsed copy of an event. `kind`: 0 view, 1 cart, 2 purchase, 3 anything else.
/// The copy is kept even when invalid: dedup (stage 2, R23) decides which copy counts.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn build_event(c: &C1, ekey: u64, ukey: u64, ts: Option<i64>, ingest: i64, kind: u8, product: &[u8], price: &[u8]) -> Ev1 {
    let mut valid = ukey != 0 && !product.is_empty() && kind <= 2;
    let t = match ts {
        Some(t) if t >= c.ws && t < c.we => t, // R16
        _ => {
            valid = false;
            0
        }
    };
    let mut pc = 0u32;
    if valid && kind == 2 {
        match parse_price(price) {
            Some(p) => {
                let cat = c.products.lookup.get(product).unwrap_or(c.products.unknown as u32); // R28
                pc = p | cat << 24;
            }
            None => valid = false,
        }
    }
    // R25: late if ingest_ts - ts is over 24 h (an ingest before ts is fine). R15: an unparseable
    // ingest_ts (i64::MAX) never wins dedup and is never valid (ambiguity 6).
    valid &= ingest != i64::MAX && ingest - t <= LATE_MS;
    if valid {
        Ev1 { ekey, ingest, ukey, ts: (t - c.base) as u32, pc }
    } else {
        Ev1 { ekey, ingest, ukey: 0, ts: 0, pc: 0 }
    }
}

#[inline(always)]
fn push_event(c: &C1, parts: &mut [Vec<Ev1>], ev: Ev1) {
    parts[(mix(ev.ekey) >> c.p1_shift) as usize].push(ev);
}

/// The generic path for event lines (R22: malformed lines and empty event_id are dropped).
#[inline(never)]
fn slow_events(buf: &[u8], c: &C1, parts: &mut [Vec<Ev1>]) {
    for_each_line_slow::<7>(buf, |f| {
        if f[0].is_empty() {
            return;
        }
        let ekey = c.interner.key(f[0]);
        let ukey = if f[1].is_empty() { 0 } else { c.interner.key(f[1]) };
        let kind = match f[4] {
            b"view" => 0,
            b"cart" => 1,
            b"purchase" => 2,
            _ => 3,
        };
        let ev = build_event(c, ekey, ukey, parse_ts(f[2]), parse_ts(f[3]).unwrap_or(i64::MAX), kind, f[5], f[6]);
        push_event(c, parts, ev);
    });
}

fn parse_events(buf: &[u8], c: &C1, parts: &mut [Vec<Ev1>]) {
    let len = buf.len();
    let mut p = 0;
    while p + fast::SLACK <= len {
        match fast::event_line(buf, p) {
            Some((l, next)) => {
                if l.ekey != 0 {
                    let ev = build_event(c, l.ekey, l.ukey, Some(l.ts), l.ingest, l.kind, &buf[l.product.0..l.product.1], &buf[l.price.0..l.price.1]);
                    push_event(c, parts, ev);
                }
                p = next;
            }
            None => {
                let le = fast::line_end(buf, p);
                slow_events(&buf[p..le], c, parts);
                p = le + 1;
            }
        }
    }
    if p < len {
        slow_events(&buf[p..], c, parts);
    }
}

/// R29-R31 on one click. R30 (dedup) is skipped: copies are identical, so they cannot change a
/// credit or an assist set.
#[inline(always)]
fn build_click(c: &C1, ckey: u64, ukey: u64, campaign: &[u8], ts: Option<i64>) -> Option<Ck> {
    if ckey == 0 || ukey == 0 {
        return None; // R29 empty click_id, R31 empty user_id
    }
    let camp = c.campaigns.lookup.get(campaign)?; // R31 campaign in campaigns.csv
    let ts = ts?;
    if ts < c.base || ts >= c.we {
        return None; // R17: base is window_start - 7 days
    }
    Some(Ck { key: ukey, ckey, ts: (ts - c.base) as u32, camp })
}

#[inline(never)]
fn slow_clicks(buf: &[u8], c: &C1, out: &mut Vec<Ck>) {
    for_each_line_slow::<4>(buf, |f| {
        let ckey = if f[0].is_empty() { 0 } else { c.interner.key(f[0]) };
        let ukey = if f[1].is_empty() { 0 } else { c.interner.key(f[1]) };
        if let Some(ck) = build_click(c, ckey, ukey, f[2], parse_ts(f[3])) {
            out.push(ck);
        }
    });
}

fn parse_clicks(buf: &[u8], c: &C1, out: &mut Vec<Ck>) {
    let len = buf.len();
    let mut p = 0;
    while p + fast::SLACK <= len {
        match fast::click_line(buf, p) {
            Some((l, next)) => {
                if let Some(ck) = build_click(c, l.ckey, l.ukey, &buf[l.campaign.0..l.campaign.1], Some(l.ts)) {
                    out.push(ck);
                }
                p = next;
            }
            None => {
                let le = fast::line_end(buf, p);
                slow_clicks(&buf[p..le], c, out);
                p = le + 1;
            }
        }
    }
    if p < len {
        slow_clicks(&buf[p..], c, out);
    }
}

#[inline(never)]
fn slow_links(buf: &[u8], c: &C1, out: &mut Vec<(u64, u64)>) {
    for_each_line_slow::<2>(buf, |f| {
        if !f[0].is_empty() && !f[1].is_empty() {
            out.push((c.interner.key(f[0]), c.interner.key(f[1]))); // R18
        }
    });
}

fn parse_links(buf: &[u8], c: &C1, out: &mut Vec<(u64, u64)>) {
    let len = buf.len();
    let mut p = 0;
    while p + fast::SLACK <= len {
        match fast::link_line(buf, p) {
            Some(((a, b), next)) => {
                if a != 0 && b != 0 {
                    out.push((a, b)); // R18: a line with an empty side is dropped
                }
                p = next;
            }
            None => {
                let le = fast::line_end(buf, p);
                slow_links(&buf[p..le], c, out);
                p = le + 1;
            }
        }
    }
    if p < len {
        slow_links(&buf[p..], c, out);
    }
}

/// R18-R21: union-find over all link lines; each person is named by the byte-wise smallest ID.
/// Returns ID key -> person key for linked IDs that are not their own person's name.
fn build_person_map(pairs: &[Vec<(u64, u64)>], interner: &Interner) -> Map<u64> {
    let total: usize = pairs.iter().map(|p| p.len()).sum();
    let mut index: Map<u32> = Map::with_capacity(total + 16);
    let mut keys: Vec<u64> = Vec::with_capacity(total);
    let mut parent: Vec<u32> = Vec::with_capacity(total);
    let mut size: Vec<u32> = Vec::with_capacity(total);
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let p = parent[x as usize];
            parent[x as usize] = parent[p as usize]; // path halving
            x = parent[x as usize];
        }
        x
    }
    // Random probes dominate: prefetch the map slots of the pair AHEAD positions on.
    let flat: Vec<&(u64, u64)> = pairs.iter().flatten().collect();
    for (n, &&(a, b)) in flat.iter().enumerate() {
        if let Some(&&(x, y)) = flat.get(n + AHEAD) {
            index.prefetch(x);
            index.prefetch(y);
        }
        let mut id = |k: u64| -> u32 {
            let (i, inserted) = index.get_or_insert_with(k, || keys.len() as u32);
            if inserted {
                keys.push(k);
                parent.push(i);
                size.push(1);
            }
            i
        };
        let (ia, ib) = (id(a), id(b));
        let (mut ra, mut rb) = (find(&mut parent, ia), find(&mut parent, ib));
        if ra != rb {
            if size[ra as usize] < size[rb as usize] {
                std::mem::swap(&mut ra, &mut rb);
            }
            parent[rb as usize] = ra;
            size[ra as usize] += size[rb as usize];
        }
    }
    let n = keys.len();
    let mut name = vec![0u64; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if name[r] == 0 || interner.cmp(keys[i], name[r]) == std::cmp::Ordering::Less {
            name[r] = keys[i];
        }
    }
    let mut map: Map<u64> = Map::with_capacity(n);
    for i in 0..n {
        if let Some(&ahead) = keys.get(i + AHEAD) {
            map.prefetch(ahead);
        }
        let person = name[find(&mut parent, i as u32) as usize];
        if person != keys[i] {
            map.insert(keys[i], person);
        }
    }
    map
}

fn stage1_worker(c: &C1) -> S1 {
    let t_alloc = Instant::now();
    let mut out = S1 { ev: (0..c.p1).map(|_| Vec::with_capacity(c.part_cap)).collect(), ck: Vec::new(), prof: Prof::default() };
    out.prof.add(4, t_alloc);
    loop {
        let i = c.next.fetch_add(1, Relaxed);
        let Some(t) = c.tasks.get(i) else { break };
        let buf = &c.files[t.file as usize].bytes()[t.start..t.end];
        let t_task = Instant::now();
        let slot = match t.kind {
            Kind::Link => 0,
            Kind::Event => 1,
            Kind::Click => 2,
        };
        match t.kind {
            Kind::Link => {
                let mut pairs = Vec::new();
                parse_links(buf, c, &mut pairs);
                c.link_pairs.lock().unwrap().push(pairs);
                if c.links_left.fetch_sub(1, AcqRel) == 1 {
                    let t_uf = Instant::now();
                    let all = std::mem::take(&mut *c.link_pairs.lock().unwrap());
                    let map = build_person_map(&all, c.interner);
                    log(&format!("union-find: {} link pairs, {} remapped ids", all.iter().map(|p| p.len()).sum::<usize>(), map.len()));
                    c.pmap.set(map).ok();
                    out.prof.add(3, t_uf);
                }
            }
            Kind::Event => parse_events(buf, c, &mut out.ev),
            Kind::Click => parse_clicks(buf, c, &mut out.ck),
        }
        out.prof.add(slot, t_task);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Stage 2: dedup, person mapping, scatter by person
// ---------------------------------------------------------------------------------------------

/// Per-partition dedup table holding each event_id's best copy so far.
struct Dedup {
    slots: Vec<Ev1>,
    used: Vec<u32>,
    mask: usize,
}

const EMPTY_EV1: Ev1 = Ev1 { ekey: 0, ingest: 0, ukey: 0, ts: 0, pc: 0 };
const AHEAD: usize = 8; // prefetch distance for random probes

impl Dedup {
    fn new() -> Self {
        Self { slots: Vec::new(), used: Vec::new(), mask: 0 }
    }
    fn reset(&mut self, n: usize) {
        let cap = (n.max(8) * 2).next_power_of_two();
        if cap > self.slots.len() {
            self.slots = vec![EMPTY_EV1; cap];
            self.mask = cap - 1;
        } else {
            for &i in &self.used {
                self.slots[i as usize].ekey = 0;
            }
        }
        self.used.clear();
    }
    #[inline(always)]
    fn prefetch(&self, ekey: u64) {
        table::prefetch(&self.slots[mix(ekey) as usize & self.mask]);
    }
    /// R23: keep the copy with the smallest ingest_ts (equal ones are identical copies).
    #[inline(always)]
    fn offer(&mut self, r: &Ev1) {
        let mut i = mix(r.ekey) as usize & self.mask;
        loop {
            let s = &mut self.slots[i];
            if s.ekey == 0 {
                *s = *r;
                self.used.push(i as u32);
                return;
            }
            if s.ekey == r.ekey {
                if r.ingest < s.ingest {
                    *s = *r;
                }
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
}

struct S2 {
    ev: Vec<Vec<Ev2>>,
    ck: Vec<Vec<Ck>>,
    prof: Prof,
}

struct C2<'a> {
    s1: &'a [S1],
    pmap: &'a Map<u64>,
    p1: usize,
    p2_shift: u32,
    p2: usize,
    click_tasks: Vec<(usize, usize, usize)>, // (thread, start, end)
    next: AtomicUsize,
    ev_cap: usize,
    ck_cap: usize,
}

#[inline(always)]
fn person_of(pmap: &Map<u64>, ukey: u64) -> u64 {
    pmap.get(ukey).unwrap_or(ukey) // R20: an unlinked ID is a person on its own
}

fn stage2_worker(c: &C2) -> S2 {
    let t_alloc = Instant::now();
    let mut out = S2 {
        ev: (0..c.p2).map(|_| Vec::with_capacity(c.ev_cap)).collect(),
        ck: (0..c.p2).map(|_| Vec::with_capacity(c.ck_cap)).collect(),
        prof: Prof::default(),
    };
    out.prof.add(3, t_alloc);
    let mut dedup = Dedup::new();
    loop {
        let i = c.next.fetch_add(1, Relaxed);
        if i < c.p1 {
            let p = i;
            let t_dedup = Instant::now();
            let n: usize = c.s1.iter().map(|s| s.ev[p].len()).sum();
            dedup.reset(n);
            for s in c.s1 {
                let recs = &s.ev[p];
                for (j, r) in recs.iter().enumerate() {
                    if let Some(ahead) = recs.get(j + AHEAD) {
                        dedup.prefetch(ahead.ekey);
                    }
                    dedup.offer(r);
                }
            }
            out.prof.add(0, t_dedup);
            let t_emit = Instant::now();
            // Scan the table in slot order (sequential memory), prefetching person-map probes.
            let slots = &dedup.slots[..];
            for (k, rec) in slots.iter().enumerate() {
                if let Some(ahead) = slots.get(k + 2 * AHEAD) {
                    if ahead.ukey != 0 {
                        c.pmap.prefetch(ahead.ukey);
                    }
                }
                // Empty slots have ekey 0; R23: an invalid kept copy (ukey 0) drops the event.
                if rec.ekey == 0 || rec.ukey == 0 {
                    continue;
                }
                let person = person_of(c.pmap, rec.ukey);
                let q = (mix(person) >> c.p2_shift) as usize;
                out.ev[q].push(Ev2 { person, ts: rec.ts, pc: rec.pc });
            }
            out.prof.add(1, t_emit);
        } else if let Some(&(t, s, e)) = c.click_tasks.get(i - c.p1) {
            let t_ck = Instant::now();
            let cks = &c.s1[t].ck[s..e];
            for (k, ck) in cks.iter().enumerate() {
                if let Some(ahead) = cks.get(k + AHEAD) {
                    c.pmap.prefetch(ahead.key);
                }
                let person = person_of(c.pmap, ck.key);
                let q = (mix(person) >> c.p2_shift) as usize;
                out.ck[q].push(Ck { key: person, ..*ck });
            }
            out.prof.add(2, t_ck);
        } else {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Stage 3: sessions, attribution, aggregation
// ---------------------------------------------------------------------------------------------

struct C3<'a> {
    s2: &'a [S2],
    names: &'a Names,
    cat_rank: &'a [u16],
    base: i64,
    first_day: i64,
    ndays: usize,
    organic: usize, // campaign index of `organic` (= number of campaigns)
    order: Vec<usize>,
    next: AtomicUsize,
}

struct Agg {
    sess: Vec<u64>,
    purch: Vec<u64>,
    rev: Vec<u64>,
    assist: Vec<u64>,
}

impl Agg {
    fn new(n: usize) -> Self {
        Self { sess: vec![0; n], purch: vec![0; n], rev: vec![0; n], assist: vec![0; n] }
    }
}

/// The campaigns with at least one click in the current 7-day window, with counts.
struct Window {
    count: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
}

impl Window {
    #[inline(always)]
    fn add(&mut self, c: u32) {
        let n = &mut self.count[c as usize];
        if *n == 0 {
            self.pos[c as usize] = self.active.len() as u32;
            self.active.push(c);
        }
        *n += 1;
    }
    #[inline(always)]
    fn remove(&mut self, c: u32) {
        let n = &mut self.count[c as usize];
        *n -= 1;
        if *n == 0 {
            let p = self.pos[c as usize] as usize;
            let last = self.active.pop().unwrap();
            if last != c {
                self.active[p] = last;
                self.pos[last as usize] = p as u32;
            }
        }
    }
    fn clear(&mut self) {
        for &c in &self.active {
            self.count[c as usize] = 0;
        }
        self.active.clear();
    }
}

struct W3 {
    prof: Prof,
    rows: Vec<UdRow>,
    agg: Agg,
    win: Window,
    local: Map<u32>,
    persons: Vec<u64>,
    ev_start: Vec<u32>,
    ck_start: Vec<u32>,
    ev_ids: Vec<u32>,
    evs: Vec<Ev2>,
    cks: Vec<Ck>,
    cats: Vec<(u16, u64)>,
}

fn flush_day(rows: &mut Vec<UdRow>, person: u64, day: i64, sessions: u32, events: u32, revenue: u64, cats: &[(u16, u64)], rank: &[u16]) {
    // R38: top_category = highest summed purchase price; ties -> byte-wise smallest name; null if none.
    let mut top = NO_CATEGORY;
    let mut best = 0u64;
    for &(c, s) in cats {
        if top == NO_CATEGORY || s > best || (s == best && rank[c as usize] < rank[top as usize]) {
            top = c;
            best = s;
        }
    }
    rows.push(UdRow { person, revenue, day: day as i32, sessions, events, top });
}

fn process_person(person: u64, ev: &mut [Ev2], ck: &mut [Ck], c: &C3, w: &mut W3) {
    ev.sort_unstable_by_key(|e| e.ts);
    if ck.len() > 1 {
        let names = c.names;
        ck.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| names.cmp(a.ckey, b.ckey)));
    }
    let nd = c.ndays;
    let (mut lo, mut hi) = (0usize, 0usize);
    let mut day_cur = i64::MIN;
    let (mut d_sess, mut d_events, mut d_rev) = (0u32, 0u32, 0u64);
    w.cats.clear();
    let n = ev.len();
    let mut i = 0;
    while i < n {
        // R26: a session runs while gaps are at most 30 min; equal ts never split.
        let start = ev[i].ts;
        let day = (c.base + start as i64).div_euclid(DAY_MS); // R27
        if day != day_cur {
            if day_cur != i64::MIN {
                flush_day(&mut w.rows, person, day_cur, d_sess, d_events, d_rev, &w.cats, c.cat_rank);
            }
            day_cur = day;
            d_sess = 0;
            d_events = 0;
            d_rev = 0;
            w.cats.clear();
        }
        let mut j = i;
        let mut prev = start;
        let mut rev = 0u64;
        while j < n && ev[j].ts - prev <= SESSION_GAP as u32 {
            let e = ev[j];
            prev = e.ts;
            let price = pc_price(e.pc);
            if price > 0 {
                let cat = pc_cat(e.pc);
                rev += price as u64;
                match w.cats.iter_mut().find(|x| x.0 == cat) {
                    Some(x) => x.1 += price as u64,
                    None => w.cats.push((cat, price as u64)),
                }
            }
            j += 1;
        }
        d_sess += 1;
        d_events += (j - i) as u32;
        d_rev += rev;

        // R32/R35: the qualifying clicks are those with start - 7 d <= ts <= start.
        while hi < ck.len() && ck[hi].ts <= start {
            w.win.add(ck[hi].camp);
            hi += 1;
        }
        while lo < hi && (ck[lo].ts as i64) < start as i64 - CLICK_REACH {
            w.win.remove(ck[lo].camp);
            lo += 1;
        }
        // R32: the latest; on a tie, the smallest click_id (clicks are sorted by ts, then click_id).
        // R33: none -> organic.
        let credit = if hi > lo {
            let tmax = ck[hi - 1].ts;
            let mut k = hi - 1;
            while k > lo && ck[k - 1].ts == tmax {
                k -= 1;
            }
            ck[k].camp as usize
        } else {
            c.organic
        };
        let di = (day - c.first_day) as usize;
        let at = credit * nd + di;
        w.agg.sess[at] += 1;
        w.agg.purch[at] += (rev > 0) as u64;
        w.agg.rev[at] += rev;
        for &a in &w.win.active {
            w.agg.assist[a as usize * nd + di] += 1; // R35: once per campaign per session
        }
        i = j;
    }
    if day_cur != i64::MIN {
        flush_day(&mut w.rows, person, day_cur, d_sess, d_events, d_rev, &w.cats, c.cat_rank);
    }
    w.win.clear();
}

fn stage3_partition(q: usize, c: &C3, w: &mut W3) {
    let n_ev: usize = c.s2.iter().map(|s| s.ev[q].len()).sum();
    if n_ev == 0 {
        return;
    }
    let t_group = Instant::now();
    w.local.reset(n_ev / 4 + 16);
    w.persons.clear();
    w.ev_start.clear();
    w.ev_ids.clear();
    for s in c.s2 {
        for e in &s.ev[q] {
            let (id, inserted) = w.local.get_or_insert_with(e.person, || w.persons.len() as u32);
            if inserted {
                w.persons.push(e.person);
                w.ev_start.push(0);
            }
            w.ev_start[id as usize] += 1;
            w.ev_ids.push(id);
        }
    }
    let np = w.persons.len();
    // exclusive prefix sums -> start offsets; then scatter
    let mut acc = 0u32;
    for x in w.ev_start.iter_mut() {
        let v = *x;
        *x = acc;
        acc += v;
    }
    w.ev_start.push(acc);
    w.evs.clear();
    w.evs.resize(n_ev, Ev2::default());
    {
        let mut fill: Vec<u32> = w.ev_start[..np].to_vec();
        let mut k = 0;
        for s in c.s2 {
            for e in &s.ev[q] {
                let id = w.ev_ids[k] as usize;
                k += 1;
                w.evs[fill[id] as usize] = *e;
                fill[id] += 1;
            }
        }
    }
    // clicks of persons with events (others cannot matter)
    w.ck_start.clear();
    w.ck_start.resize(np + 1, 0);
    let mut ck_ids: Vec<u32> = Vec::new();
    for s in c.s2 {
        for ck in &s.ck[q] {
            match w.local.get(ck.key) {
                Some(id) => {
                    w.ck_start[id as usize] += 1;
                    ck_ids.push(id);
                }
                None => ck_ids.push(u32::MAX),
            }
        }
    }
    let mut acc = 0u32;
    for x in w.ck_start[..np].iter_mut() {
        let v = *x;
        *x = acc;
        acc += v;
    }
    w.ck_start[np] = acc;
    w.cks.clear();
    w.cks.resize(acc as usize, Ck::default());
    {
        let mut fill: Vec<u32> = w.ck_start[..np].to_vec();
        let mut k = 0;
        for s in c.s2 {
            for ck in &s.ck[q] {
                let id = ck_ids[k];
                k += 1;
                if id != u32::MAX {
                    w.cks[fill[id as usize] as usize] = *ck;
                    fill[id as usize] += 1;
                }
            }
        }
    }
    let mut evs = std::mem::take(&mut w.evs);
    let mut cks = std::mem::take(&mut w.cks);
    w.prof.add(0, t_group);
    let t_proc = Instant::now();
    for p in 0..np {
        let (es, ee) = (w.ev_start[p] as usize, w.ev_start[p + 1] as usize);
        let (cs, ce) = (w.ck_start[p] as usize, w.ck_start[p + 1] as usize);
        process_person(w.persons[p], &mut evs[es..ee], &mut cks[cs..ce], c, w);
    }
    w.prof.add(1, t_proc);
    w.evs = evs;
    w.cks = cks;
}

fn stage3_worker(c: &C3, ncamp_total: usize) -> W3 {
    let mut w = W3 {
        prof: Prof::default(),
        rows: Vec::new(),
        agg: Agg::new(ncamp_total * c.ndays),
        win: Window { count: vec![0; ncamp_total], pos: vec![0; ncamp_total], active: Vec::new() },
        local: Map::with_capacity(1024),
        persons: Vec::new(),
        ev_start: Vec::new(),
        ck_start: Vec::new(),
        ev_ids: Vec::new(),
        evs: Vec::new(),
        cks: Vec::new(),
        cats: Vec::new(),
    };
    loop {
        let i = c.next.fetch_add(1, Relaxed);
        let Some(&q) = c.order.get(i) else { break };
        stage3_partition(q, c, &mut w);
    }
    w
}

// ---------------------------------------------------------------------------------------------
// Stage 4: Parquet output (R37-R39)
// ---------------------------------------------------------------------------------------------

fn write_user_daily(path: &Path, rows: &[UdRow], names: &Names, cats: &Catalog) -> Res<()> {
    let n = rows.len();
    let mut ub = Vec::with_capacity(n * 10);
    let mut ue = Vec::with_capacity(n);
    let mut day = Vec::with_capacity(n);
    let mut sessions = Vec::with_capacity(n);
    let mut events = Vec::with_capacity(n);
    let mut revenue = Vec::with_capacity(n);
    let mut cb = Vec::new();
    let mut ce = Vec::with_capacity(n);
    let mut cv = Vec::with_capacity(n);
    for r in rows {
        names.write(r.person, &mut ub);
        ue.push(ub.len() as u32);
        day.push(r.day);
        sessions.push(r.sessions as i32);
        events.push(r.events as i32);
        revenue.push(r.revenue as i64);
        if r.top != NO_CATEGORY {
            cb.extend_from_slice(&cats.names[r.top as usize]);
            cv.push(true);
        } else {
            cv.push(false);
        }
        ce.push(cb.len() as u32);
    }
    use parquet::{Data, Kind as K, Spec};
    let specs = [
        Spec { name: "user_id", kind: K::Utf8, optional: false },
        Spec { name: "day", kind: K::Date, optional: false },
        Spec { name: "sessions", kind: K::I32, optional: false },
        Spec { name: "events", kind: K::I32, optional: false },
        Spec { name: "revenue_cents", kind: K::I64, optional: false },
        Spec { name: "top_category", kind: K::Utf8, optional: true },
    ];
    let cols = [
        Data::Str { bytes: &ub, ends: &ue, valid: None },
        Data::I32(&day),
        Data::I32(&sessions),
        Data::I32(&events),
        Data::I64(&revenue),
        Data::Str { bytes: &cb, ends: &ce, valid: Some(&cv) },
    ];
    let file = parquet::build(&specs, &cols);
    fs::write(path, file).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_campaign_daily(path: &Path, agg: &Agg, camps: &Campaigns, first_day: i64, ndays: usize) -> Res<usize> {
    let (mut ib, mut ie, mut chb, mut che) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut day, mut sess, mut purch, mut rev, mut assist) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let ncamp = camps.ids.len();
    for c in 0..=ncamp {
        for d in 0..ndays {
            let at = c * ndays + d;
            if agg.sess[at] == 0 && agg.assist[at] == 0 {
                continue; // R39: only (campaign, day) pairs with a credited or assisted session
            }
            let (id, ch): (&[u8], &[u8]) = if c == ncamp { (b"organic", b"organic") } else { (&camps.ids[c], &camps.channels[c]) };
            ib.extend_from_slice(id);
            ie.push(ib.len() as u32);
            chb.extend_from_slice(ch);
            che.push(chb.len() as u32);
            day.push((first_day + d as i64) as i32);
            sess.push(agg.sess[at] as i64);
            purch.push(agg.purch[at] as i64);
            rev.push(agg.rev[at] as i64);
            assist.push(agg.assist[at] as i64);
        }
    }
    use parquet::{Data, Kind as K, Spec};
    let specs = [
        Spec { name: "campaign_id", kind: K::Utf8, optional: false },
        Spec { name: "channel", kind: K::Utf8, optional: false },
        Spec { name: "day", kind: K::Date, optional: false },
        Spec { name: "sessions", kind: K::I64, optional: false },
        Spec { name: "purchasing_sessions", kind: K::I64, optional: false },
        Spec { name: "revenue_cents", kind: K::I64, optional: false },
        Spec { name: "assisted_sessions", kind: K::I64, optional: false },
    ];
    let cols = [
        Data::Str { bytes: &ib, ends: &ie, valid: None },
        Data::Str { bytes: &chb, ends: &che, valid: None },
        Data::I32(&day),
        Data::I64(&sess),
        Data::I64(&purch),
        Data::I64(&rev),
        Data::I64(&assist),
    ];
    let file = parquet::build(&specs, &cols);
    fs::write(path, file).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(day.len())
}

// ---------------------------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------------------------

fn par<R: Send>(threads: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = (1..threads).map(|t| s.spawn(move || f(t))).collect();
        let mut out = vec![f(0)];
        out.extend(handles.into_iter().map(|h| h.join().expect("worker panicked")));
        out
    })
}

/// `SS_BENCH=1`: single-threaded timings of the parsing layers over the event files (development aid).
fn bench_parse(data: &Path) -> Res<()> {
    let paths = list_csv(&data.join("events"))?;
    let files: Vec<Mapped> = paths.iter().map(|p| map_file(p)).collect::<Res<_>>()?;
    let bufs: Vec<&[u8]> = files.iter().map(|f| { let b = f.bytes(); &b[body(b)..] }).collect();
    let bytes: usize = bufs.iter().map(|b| b.len()).sum();
    // touch everything once so page faults are not measured
    let mut sum = 0u64;
    for b in &bufs {
        for c in b.chunks(4096) {
            sum = sum.wrapping_add(c[0] as u64);
        }
    }
    let lines: usize = bufs.iter().map(|b| b.iter().filter(|&&c| c == b'\n').count()).sum();
    let time = |name: &str, f: &mut dyn FnMut() -> u64| {
        let t = Instant::now();
        let r = f();
        let ns = t.elapsed().as_nanos() as f64;
        eprintln!("{name:28} {:7.1} ms  {:6.1} ns/line  {:5.2} ns/byte  ({r})", ns / 1e6, ns / lines as f64, ns / bytes as f64);
    };
    time("count newlines", &mut || bufs.iter().map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64).sum());
    let big: Vec<u8> = bufs.concat();
    for _ in 0..2 {
        time("bandwidth: u64 sum", &mut || {
            big.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).fold(0u64, |a, b| a.wrapping_add(b))
        });
    }
    let mut copy = vec![0u8; big.len()];
    for _ in 0..2 {
        time("bandwidth: memcpy", &mut || {
            copy.copy_from_slice(&big);
            copy[12345] as u64
        });
    }
    drop(copy);
    for (i, name) in ["layer: masks only", "layer: + line ends", "layer: + windows"].iter().enumerate() {
        time(name, &mut || parse::bench_layers(&big)[i]);
    }
    time("split lines (no-op)", &mut || {
        let mut n = 0u64;
        for b in &bufs {
            for_each_line::<7>(b, |_| n += 1);
        }
        n
    });
    time("split + field lengths", &mut || {
        let mut n = 0u64;
        for b in &bufs {
            for_each_line::<7>(b, |f| n += f.iter().map(|x| x.len() as u64).sum::<u64>());
        }
        n
    });
    time("split + parse_ts x2", &mut || {
        let mut n = 0u64;
        for b in &bufs {
            for_each_line::<7>(b, |f| {
                n = n.wrapping_add(parse_ts(f[2]).unwrap_or(0) as u64).wrapping_add(parse_ts(f[3]).unwrap_or(0) as u64)
            });
        }
        n
    });
    time("split + pack ids x2", &mut || {
        let mut n = 0u64;
        for b in &bufs {
            for_each_line::<7>(b, |f| n = n.wrapping_add(ids::pack(f[0])).wrapping_add(ids::pack(f[1])));
        }
        n
    });
    let interner = Interner::default();
    let products = load_products(&data.join("products.csv"))?;
    let campaigns = load_campaigns(&data.join("campaigns.csv"))?;
    let manifest = fs::read_to_string(data.join("manifest.json")).map_err(|e| e.to_string())?;
    let ws = json_string(&manifest, "window_start").and_then(|s| parse_ts(s.as_bytes())).unwrap_or(0);
    let we = json_string(&manifest, "window_end").and_then(|s| parse_ts(s.as_bytes())).unwrap_or(0);
    let tasks: Vec<Task> = Vec::new();
    let p1 = 256;
    let c = C1 {
        files: &files,
        tasks: &tasks,
        next: AtomicUsize::new(0),
        links_left: AtomicUsize::new(0),
        link_pairs: Mutex::new(Vec::new()),
        interner: &interner,
        pmap: OnceLock::new(),
        products: &products,
        campaigns: &campaigns,
        ws,
        we,
        base: ws - CLICK_REACH,
        p1_shift: 64 - 8,
        p1,
        part_cap: lines / p1 + 16,
    };
    time("full parse_events", &mut || {
        let mut parts: Vec<Vec<Ev1>> = (0..p1).map(|_| Vec::with_capacity(lines / p1 + 16)).collect();
        for b in &bufs {
            parse_events(b, &c, &mut parts);
        }
        parts.iter().map(|p| p.len() as u64).sum()
    });
    eprintln!("{} files, {:.1} MB, {lines} lines, checksum {sum}", bufs.len(), bytes as f64 / 1e6);
    Ok(())
}

fn run() -> Res<()> {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    if std::env::var("SS_BENCH").is_ok() {
        return bench_parse(Path::new(&env("DATA_DIR", "/data")));
    }
    let data = PathBuf::from(env("DATA_DIR", "/data"));
    let out = PathBuf::from(env("OUTPUT_DIR", "/output"));
    let threads = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
        .clamp(1, 256);

    // R1: the window from manifest.json
    let manifest = fs::read_to_string(data.join("manifest.json")).map_err(|e| format!("manifest.json: {e}"))?;
    let ws = json_string(&manifest, "window_start").and_then(|s| parse_ts(s.as_bytes())).ok_or("bad window_start")?;
    let we = json_string(&manifest, "window_end").and_then(|s| parse_ts(s.as_bytes())).ok_or("bad window_end")?;
    if we <= ws {
        return Err("empty window".into());
    }
    let products = load_products(&data.join("products.csv"))?;
    let campaigns = load_campaigns(&data.join("campaigns.csv"))?;
    let first_day = ws.div_euclid(DAY_MS);
    let ndays = ((we - 1).div_euclid(DAY_MS) - first_day + 1) as usize;
    let base = ws - CLICK_REACH; // R17: the click window starts 7 days before the event window
    if we - base > u32::MAX as i64 {
        // Timestamps are kept as u32 ms offsets from `base`; the generator's windows are 14 days.
        return Err("windows longer than 42 days are not supported".into());
    }

    let mut paths: Vec<(Kind, PathBuf)> = Vec::new();
    for (kind, dir) in [(Kind::Link, "links"), (Kind::Event, "events"), (Kind::Click, "clicks")] {
        paths.extend(list_csv(&data.join(dir))?.into_iter().map(|p| (kind, p)));
    }
    let files: Vec<Mapped> = paths.iter().map(|(_, p)| map_file(p)).collect::<Res<_>>()?;
    let mut tasks = Vec::new();
    for (i, (kind, _)) in paths.iter().enumerate() {
        split_file(*kind, i, files[i].bytes(), &mut tasks);
    }
    // Links first (so the union-find starts early), then the rest, largest chunks first.
    tasks.sort_by_key(|t| (t.kind != Kind::Link, std::cmp::Reverse(t.end - t.start)));
    let ev_bytes: usize = tasks.iter().filter(|t| t.kind == Kind::Event).map(|t| t.end - t.start).sum();
    let est_events = ev_bytes / 70 + 1;
    // At most 1024 partitions: each thread keeps one buffer per partition and stage, and large
    // buffers are separate mappings, so this keeps far below Linux's 65530-mapping limit.
    let p1 = (est_events / 16384).next_power_of_two().clamp(16, 1024);
    let p2 = p1;
    // Generous capacities: unused tails of huge-page-backed buffers are mostly never touched,
    // and growing a Vec copies it.
    let part_cap = est_events * 2 / (p1 * threads) + 16;
    log(&format!(
        "window [{ws}, {we}), {} files, {} tasks, {:.1} MB of events, {threads} threads, P1={p1}",
        files.len(),
        tasks.len(),
        ev_bytes as f64 / 1e6
    ));

    // Stage 1
    let interner = Interner::default();
    let c1 = C1 {
        files: &files,
        tasks: &tasks,
        next: AtomicUsize::new(0),
        links_left: AtomicUsize::new(tasks.iter().filter(|t| t.kind == Kind::Link).count()),
        link_pairs: Mutex::new(Vec::new()),
        interner: &interner,
        pmap: OnceLock::new(),
        products: &products,
        campaigns: &campaigns,
        ws,
        we,
        base,
        p1_shift: 64 - p1.trailing_zeros(),
        p1,
        part_cap,
    };
    let s1 = par(threads, |_| stage1_worker(&c1));
    let pmap = c1.pmap.into_inner().unwrap_or_else(|| Map::with_capacity(0));
    let copies: usize = s1.iter().map(|s| s.ev.iter().map(|v| v.len()).sum::<usize>()).sum();
    let clicks: usize = s1.iter().map(|s| s.ck.len()).sum();
    log(&format!("stage 1: {copies} event copies, {clicks} valid clicks"));
    report("stage1", &["links", "events", "clicks", "union-find", "alloc"], &s1.iter().map(|s| &s.prof).collect::<Vec<_>>());
    drop(files);

    // Stage 2
    let mut click_tasks = Vec::new();
    for (t, s) in s1.iter().enumerate() {
        let mut a = 0;
        while a < s.ck.len() {
            let b = (a + 1 << 18).min(s.ck.len());
            click_tasks.push((t, a, b));
            a = b;
        }
    }
    let c2 = C2 {
        s1: &s1,
        pmap: &pmap,
        p1,
        p2_shift: 64 - p2.trailing_zeros(),
        p2,
        click_tasks,
        next: AtomicUsize::new(0),
        ev_cap: copies * 2 / (p2 * threads) + 16,
        ck_cap: clicks * 2 / (p2 * threads) + 16,
    };
    let s2 = par(threads, |_| stage2_worker(&c2));
    let survivors: usize = s2.iter().map(|s| s.ev.iter().map(|v| v.len()).sum::<usize>()).sum();
    log(&format!("stage 2: {survivors} surviving events"));
    report("stage2", &["dedup", "emit events", "clicks", "alloc"], &s2.iter().map(|s| &s.prof).collect::<Vec<_>>());
    drop(s1);
    log("stage 1 memory freed");

    // Stage 3
    let names = interner.freeze();
    let mut order: Vec<usize> = (0..p2).collect();
    let weight = |q: usize| -> usize { s2.iter().map(|s| s.ev[q].len() + s.ck[q].len()).sum() };
    let weights: Vec<usize> = order.iter().map(|&q| weight(q)).collect();
    order.sort_by_key(|&q| std::cmp::Reverse(weights[q]));
    let ncamp_total = campaigns.ids.len() + 1;
    let c3 = C3 {
        s2: &s2,
        names: &names,
        cat_rank: &products.rank,
        base,
        first_day,
        ndays,
        organic: campaigns.ids.len(),
        order,
        next: AtomicUsize::new(0),
    };
    let mut w3 = par(threads, |_| stage3_worker(&c3, ncamp_total));
    let user_rows: usize = w3.iter().map(|w| w.rows.len()).sum();
    log(&format!("stage 3: {user_rows} user_daily rows"));
    report("stage3", &["group", "sessions+attrib"], &w3.iter().map(|w| &w.prof).collect::<Vec<_>>());
    drop(s2);
    log("stage 2 memory freed");

    // Stage 4
    let ud_dir = out.join("user_daily");
    let cd_dir = out.join("campaign_daily");
    fs::create_dir_all(&ud_dir).map_err(|e| format!("{}: {e}", ud_dir.display()))?;
    fs::create_dir_all(&cd_dir).map_err(|e| format!("{}: {e}", cd_dir.display()))?;
    let mut agg = Agg::new(ncamp_total * ndays);
    for w in &w3 {
        for (dst, src) in [(&mut agg.sess, &w.agg.sess), (&mut agg.purch, &w.agg.purch), (&mut agg.rev, &w.agg.rev), (&mut agg.assist, &w.agg.assist)] {
            dst.iter_mut().zip(src).for_each(|(a, b)| *a += b);
        }
    }
    let row_sets: Vec<Vec<UdRow>> = w3.iter_mut().map(|w| std::mem::take(&mut w.rows)).collect();
    let results = par(threads, |t| -> Res<()> {
        if let Some(rows) = row_sets.get(t) {
            if !rows.is_empty() || (t == 0 && user_rows == 0) {
                write_user_daily(&ud_dir.join(format!("part-{t:05}.parquet")), rows, &names, &products)?;
            }
        }
        if t == 0 {
            let n = write_campaign_daily(&cd_dir.join("part-00000.parquet"), &agg, &campaigns, first_day, ndays)?;
            log(&format!("campaign_daily: {n} rows"));
        }
        Ok(())
    });
    for r in results {
        r?;
    }
    log("done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.below(100) < pct
        }
    }

    fn catalogs() -> (Catalog, Campaigns) {
        let mut names: Vec<Vec<u8>> = vec![b"books".to_vec(), b"shoes".to_vec(), b"unknown".to_vec()];
        let mut lookup = Lookup::new(b"p");
        for i in 1..200u32 {
            lookup.insert_first(format!("p{i}").as_bytes(), i % 2);
        }
        lookup.insert_first(b"P7", 1);
        names.truncate(3);
        let cat = Catalog { lookup, names, rank: vec![0, 1, 2], unknown: 2 };
        let mut camps = Campaigns { lookup: Lookup::new(b"c_"), ids: Vec::new(), channels: Vec::new() };
        for i in 1..20u32 {
            camps.lookup.insert_first(format!("c_{i}").as_bytes(), i - 1);
            camps.ids.push(format!("c_{i}").into_bytes());
            camps.channels.push(b"search".to_vec());
        }
        (cat, camps)
    }

    const WS: i64 = 1_791_590_400_000; // 2026-10-10T00:00:00Z
    const WE: i64 = WS + 14 * DAY_MS;

    fn utc(ms: i64) -> String {
        let days = ms.div_euclid(DAY_MS);
        let secs = ms.rem_euclid(DAY_MS) / 1000;
        // civil_from_days (Hinnant)
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    }

    fn id(r: &mut Rng, prefix: char) -> String {
        const B36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let n = 1 + r.below(9) as usize;
        let mut s: String = std::iter::once(prefix).chain((0..n).map(|_| B36[r.below(36) as usize] as char)).collect();
        match r.below(40) {
            0 => s = s.to_uppercase(),
            1 => s = String::new(),
            2 => s.push(' '),
            3 => s.insert(0, ' '),
            4 => s.push('_'),
            5 => s.push_str("abcdefghij"),
            6 => s.push('\r'),
            _ => {}
        }
        s
    }

    fn ts(r: &mut Rng) -> String {
        let t = WS - 3 * DAY_MS + r.below((20 * DAY_MS) as u64) as i64;
        let t = if r.chance(70) { t - t.rem_euclid(1000) } else { t };
        match r.below(16) {
            0..=5 if t % 1000 == 0 => format!("{}Z", utc(t)),
            6 | 7 if t % 1000 == 0 => {
                let off = [-600i64, -240, 0, 60, 330, 720][r.below(6) as usize];
                let local = t + off * 60_000;
                format!("{}{}{:02}:{:02}", utc(local), if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60)
            }
            8 => format!("{:016}", t),
            9 => format!("{}z", utc(t)),
            10 => format!("{}.5", t),
            11 => format!("{}", utc(t).replace('T', " ")),
            12 => "2026-02-30T10:00:00Z".to_string(),
            13 => "2026-10-12T10:00:00+0200".to_string(),
            14 => String::new(),
            _ => format!("{t}"),
        }
    }

    fn quote(r: &mut Rng, s: String) -> String {
        match r.below(30) {
            0..=5 => format!("\"{s}\""),
            6 => format!("\"{s}"),
            7 => format!("{s}\""),
            8 => format!("\"\"{s}\""),
            _ => s,
        }
    }

    fn event_lines(r: &mut Rng, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..n {
            let kind = ["view", "cart", "purchase", "Purchase", "cart ", ""][[0, 0, 0, 1, 1, 2, 2, 2, 3, 4, 5][r.below(11) as usize]];
            let product = match r.below(12) {
                0 => String::new(),
                1 => "P7".to_string(),
                2 => " p1".to_string(),
                3 => "p0123".to_string(),
                _ => format!("p{}", 1 + r.below(250)),
            };
            let price = match r.below(12) {
                0 => String::new(),
                1 => "0250".to_string(),
                2 => "19.99".to_string(),
                3 => "10000001".to_string(),
                4 => "0".to_string(),
                _ => format!("{}", 1 + r.below(50_000)),
            };
            let fields = [id(r, 'e'), id(r, 'u'), ts(r), ts(r), kind.to_string(), product, price];
            let mut parts: Vec<String> = fields.into_iter().map(|f| quote(r, f)).collect();
            match r.below(40) {
                0 => {
                    parts.pop();
                }
                1 => parts.push("x".to_string()),
                2 => parts = vec![String::new()],
                _ => {}
            }
            out.extend_from_slice(parts.join(",").as_bytes());
            if r.chance(3) {
                out.push(b'\r');
            }
            out.push(b'\n');
        }
        out
    }

    fn click_lines(r: &mut Rng, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..n {
            let camp = match r.below(10) {
                0 => "C_3".to_string(),
                1 => String::new(),
                2 => "c_99".to_string(),
                _ => format!("c_{}", 1 + r.below(19)),
            };
            let fields = [id(r, 'k'), id(r, 'u'), camp, ts(r)];
            let mut parts: Vec<String> = fields.into_iter().map(|f| quote(r, f)).collect();
            if r.chance(3) {
                parts.push("x".to_string());
            }
            out.extend_from_slice(parts.join(",").as_bytes());
            out.push(b'\n');
        }
        out
    }

    fn link_lines(r: &mut Rng, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..n {
            let mut parts: Vec<String> = [id(r, 'u'), id(r, 'u')].into_iter().map(|f| quote(r, f)).collect();
            if r.chance(3) {
                parts.pop();
            }
            out.extend_from_slice(parts.join(",").as_bytes());
            out.push(b'\n');
        }
        out
    }

    fn ctx<'a>(cat: &'a Catalog, camps: &'a Campaigns, interner: &'a Interner) -> C1<'a> {
        C1 {
            files: &[],
            tasks: &[],
            next: AtomicUsize::new(0),
            links_left: AtomicUsize::new(0),
            link_pairs: Mutex::new(Vec::new()),
            interner,
            pmap: OnceLock::new(),
            products: cat,
            campaigns: camps,
            ws: WS,
            we: WE,
            base: WS - CLICK_REACH,
            p1_shift: 64 - 4,
            p1: 16,
            part_cap: 16,
        }
    }

    /// Fast path + generic fallback == generic path alone, on randomized dirty input.
    #[test]
    fn fast_paths_match_generic_paths() {
        let (cat, camps) = catalogs();
        let mut r = Rng(0x2545_F491_4F6C_DD1D);
        for round in 0..40 {
            let ev = event_lines(&mut r, 2000);
            let (i1, i2) = (Interner::default(), Interner::default());
            let (c1, c2) = (ctx(&cat, &camps, &i1), ctx(&cat, &camps, &i2));
            let mut a: Vec<Vec<Ev1>> = vec![Vec::new(); 16];
            let mut b: Vec<Vec<Ev1>> = vec![Vec::new(); 16];
            parse_events(&ev, &c1, &mut a);
            slow_events(&ev, &c2, &mut b);
            assert_eq!(a, b, "events, round {round}");
            assert!(a.iter().map(|v| v.len()).sum::<usize>() > 500);

            let ck = click_lines(&mut r, 2000);
            let (i1, i2) = (Interner::default(), Interner::default());
            let (c1, c2) = (ctx(&cat, &camps, &i1), ctx(&cat, &camps, &i2));
            let (mut a, mut b) = (Vec::new(), Vec::new());
            parse_clicks(&ck, &c1, &mut a);
            slow_clicks(&ck, &c2, &mut b);
            assert_eq!(a, b, "clicks, round {round}");

            let lk = link_lines(&mut r, 2000);
            let (i1, i2) = (Interner::default(), Interner::default());
            let (c1, c2) = (ctx(&cat, &camps, &i1), ctx(&cat, &camps, &i2));
            let (mut a, mut b) = (Vec::new(), Vec::new());
            parse_links(&lk, &c1, &mut a);
            slow_links(&lk, &c2, &mut b);
            assert_eq!(a, b, "links, round {round}");
        }
    }
}
