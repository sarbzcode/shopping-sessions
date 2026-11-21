//! A minimal Parquet writer for flat tables: one row group per file, PLAIN-encoded data pages (v1),
//! uncompressed, definition levels for optional columns as bit-packed RLE-hybrid runs.
//! Metadata is Thrift compact protocol (parquet.thrift field ids in comments).

pub const PAGE_ROWS: usize = 1 << 20;

// Thrift compact type ids
const CT_I32: u8 = 5;
const CT_I64: u8 = 6;
const CT_BINARY: u8 = 8;
const CT_LIST: u8 = 9;
const CT_STRUCT: u8 = 12;

// parquet.thrift enums
const TYPE_INT32: i32 = 1;
const TYPE_INT64: i32 = 2;
const TYPE_BYTE_ARRAY: i32 = 6;
const REQUIRED: i32 = 0;
const OPTIONAL: i32 = 1;
const CONVERTED_UTF8: i32 = 0;
const CONVERTED_DATE: i32 = 6;
const ENC_PLAIN: i32 = 0;
const ENC_RLE: i32 = 3;
const PAGE_DATA: i32 = 0;

struct Thrift {
    out: Vec<u8>,
    last: Vec<i16>,
}

impl Thrift {
    fn new() -> Self {
        Self { out: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }
    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let last = self.last.last_mut().unwrap();
        let delta = id - *last;
        if delta > 0 && delta <= 15 {
            self.out.push(((delta as u8) << 4) | ty);
        } else {
            self.out.push(ty);
            let z = ((id as i64) << 1) ^ ((id as i64) >> 63);
            let mut v = z as u64;
            while v >= 0x80 {
                self.out.push((v as u8) | 0x80);
                v >>= 7;
            }
            self.out.push(v as u8);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.field(id, CT_I32);
        self.zigzag(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.field(id, CT_I64);
        self.zigzag(v);
    }
    fn binf(&mut self, id: i16, b: &[u8]) {
        self.field(id, CT_BINARY);
        self.varint(b.len() as u64);
        self.out.extend_from_slice(b);
    }
    fn struct_begin(&mut self, id: i16) {
        self.field(id, CT_STRUCT);
        self.last.push(0);
    }
    /// The body of a struct that is a list element (no field header).
    fn elem_struct_begin(&mut self) {
        self.last.push(0);
    }
    fn struct_end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }
    fn list_begin(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, CT_LIST);
        if n < 15 {
            self.out.push(((n as u8) << 4) | elem);
        } else {
            self.out.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Utf8,
    Date,
    I32,
    I64,
}

pub struct Spec {
    pub name: &'static str,
    pub kind: Kind,
    pub optional: bool,
}

/// One column's data for one file.
pub enum Data<'a> {
    I32(&'a [i32]),
    I64(&'a [i64]),
    /// Concatenated bytes plus end offsets (value i is bytes[ends[i-1]..ends[i]]); `valid`
    /// (optional columns only) says which rows are non-null; null rows have empty ranges.
    Str { bytes: &'a [u8], ends: &'a [u32], valid: Option<&'a [bool]> },
}

impl Data<'_> {
    fn rows(&self) -> usize {
        match self {
            Data::I32(v) => v.len(),
            Data::I64(v) => v.len(),
            Data::Str { ends, .. } => ends.len(),
        }
    }
}

struct ChunkMeta {
    offset: i64,
    size: i64,
}

fn page_payload(spec: &Spec, data: &Data, lo: usize, hi: usize, out: &mut Vec<u8>) {
    out.clear();
    if spec.optional {
        // Definition levels: 4-byte length, then one bit-packed run (header = groups << 1 | 1).
        let valid = match data {
            Data::Str { valid: Some(v), .. } => &v[lo..hi],
            _ => panic!("optional column without validity"),
        };
        let groups = (hi - lo).div_ceil(8);
        let mut levels = Vec::with_capacity(groups + 10);
        let mut h = ((groups as u64) << 1) | 1;
        while h >= 0x80 {
            levels.push((h as u8) | 0x80);
            h >>= 7;
        }
        levels.push(h as u8);
        for g in 0..groups {
            let mut byte = 0u8;
            for bit in 0..8 {
                let r = g * 8 + bit;
                if r < valid.len() && valid[r] {
                    byte |= 1 << bit;
                }
            }
            levels.push(byte);
        }
        out.extend_from_slice(&(levels.len() as u32).to_le_bytes());
        out.extend_from_slice(&levels);
    }
    match data {
        Data::I32(v) => v[lo..hi].iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
        Data::I64(v) => v[lo..hi].iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
        Data::Str { bytes, ends, valid } => {
            for r in lo..hi {
                if let Some(v) = valid {
                    if !v[r] {
                        continue;
                    }
                }
                let s = if r == 0 { 0 } else { ends[r - 1] as usize };
                let e = ends[r] as usize;
                out.extend_from_slice(&((e - s) as u32).to_le_bytes());
                out.extend_from_slice(&bytes[s..e]);
            }
        }
    }
}

fn page_header(num_values: usize, payload_len: usize) -> Vec<u8> {
    let mut t = Thrift::new();
    t.i32f(1, PAGE_DATA); // type
    t.i32f(2, payload_len as i32); // uncompressed_page_size
    t.i32f(3, payload_len as i32); // compressed_page_size
    t.struct_begin(5); // data_page_header
    t.i32f(1, num_values as i32); // num_values
    t.i32f(2, ENC_PLAIN); // encoding
    t.i32f(3, ENC_RLE); // definition_level_encoding
    t.i32f(4, ENC_RLE); // repetition_level_encoding
    t.struct_end();
    t.out.push(0);
    t.out
}

fn physical(kind: Kind) -> i32 {
    match kind {
        Kind::Utf8 => TYPE_BYTE_ARRAY,
        Kind::Date | Kind::I32 => TYPE_INT32,
        Kind::I64 => TYPE_INT64,
    }
}

/// Builds a complete Parquet file in memory.
pub fn build(specs: &[Spec], columns: &[Data]) -> Vec<u8> {
    assert_eq!(specs.len(), columns.len());
    let rows = columns.first().map_or(0, |c| c.rows());
    assert!(columns.iter().all(|c| c.rows() == rows));
    let mut file = b"PAR1".to_vec();
    let mut metas = Vec::new();
    let mut payload = Vec::new();
    for (spec, data) in specs.iter().zip(columns) {
        let offset = file.len() as i64;
        let mut lo = 0;
        while lo < rows {
            let hi = (lo + PAGE_ROWS).min(rows);
            page_payload(spec, data, lo, hi, &mut payload);
            file.extend_from_slice(&page_header(hi - lo, payload.len()));
            file.extend_from_slice(&payload);
            lo = hi;
        }
        metas.push(ChunkMeta { offset, size: file.len() as i64 - offset });
    }

    let mut t = Thrift::new();
    t.i32f(1, 1); // version
    t.list_begin(2, CT_STRUCT, specs.len() + 1); // schema
    t.elem_struct_begin();
    t.binf(4, b"schema"); // name
    t.i32f(5, specs.len() as i32); // num_children
    t.struct_end();
    for spec in specs {
        t.elem_struct_begin();
        t.i32f(1, physical(spec.kind)); // type
        t.i32f(3, if spec.optional { OPTIONAL } else { REQUIRED }); // repetition_type
        t.binf(4, spec.name.as_bytes()); // name
        match spec.kind {
            Kind::Utf8 => {
                t.i32f(6, CONVERTED_UTF8); // converted_type
                t.struct_begin(10); // logicalType
                t.struct_begin(1); // STRING
                t.struct_end();
                t.struct_end();
            }
            Kind::Date => {
                t.i32f(6, CONVERTED_DATE);
                t.struct_begin(10);
                t.struct_begin(6); // DATE
                t.struct_end();
                t.struct_end();
            }
            Kind::I32 | Kind::I64 => {}
        }
        t.struct_end();
    }
    t.i64f(3, rows as i64); // num_rows
    let groups = usize::from(rows > 0);
    t.list_begin(4, CT_STRUCT, groups); // row_groups
    if rows > 0 {
        t.elem_struct_begin();
        t.list_begin(1, CT_STRUCT, specs.len()); // columns
        for (spec, meta) in specs.iter().zip(&metas) {
            t.elem_struct_begin();
            t.i64f(2, meta.offset); // file_offset
            t.struct_begin(3); // meta_data
            t.i32f(1, physical(spec.kind)); // type
            t.list_begin(2, CT_I32, 2); // encodings
            t.zigzag(ENC_PLAIN as i64);
            t.zigzag(ENC_RLE as i64);
            t.list_begin(3, CT_BINARY, 1); // path_in_schema
            t.varint(spec.name.len() as u64);
            t.out.extend_from_slice(spec.name.as_bytes());
            t.i32f(4, 0); // codec UNCOMPRESSED
            t.i64f(5, rows as i64); // num_values
            t.i64f(6, meta.size); // total_uncompressed_size
            t.i64f(7, meta.size); // total_compressed_size
            t.i64f(9, meta.offset); // data_page_offset
            t.struct_end();
            t.struct_end();
        }
        t.i64f(2, metas.iter().map(|m| m.size).sum()); // total_byte_size
        t.i64f(3, rows as i64); // num_rows
        t.struct_end();
    }
    t.binf(6, b"shopping-sessions"); // created_by
    t.out.push(0);

    file.extend_from_slice(&t.out);
    file.extend_from_slice(&(t.out.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    file
}
