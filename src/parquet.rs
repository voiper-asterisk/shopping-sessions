//! A minimal Parquet writer: flat schema, uncompressed, data page v1,
//! PLAIN values (the caller hands in already-encoded PLAIN bytes) and an
//! optional definition-level bitmap for one nullable string column.
#![allow(dead_code)]

use std::io::Write;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Str,
    Date,
    I32,
    I64,
}

pub struct ColSpec {
    pub name: &'static str,
    pub kind: Kind,
    pub nullable: bool,
}

/// One column chunk's bytes for a row group.
pub struct ColData<'a> {
    /// PLAIN-encoded values (non-null ones only).
    pub values: &'a [u8],
    /// For nullable columns: one bit per row, LSB first, 1 = present. Must hold
    /// at least `ceil(rows / 8)` bytes, padding bits zero.
    pub defs: Option<&'a [u8]>,
}

struct ChunkMeta {
    offset: u64,
    size: u64,
    num_values: u64,
}

struct GroupMeta {
    rows: u64,
    chunks: Vec<ChunkMeta>,
}

pub struct Writer<W: Write> {
    out: W,
    pos: u64,
    cols: &'static [ColSpec],
    groups: Vec<GroupMeta>,
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
fn zigzag(out: &mut Vec<u8>, v: i64) {
    varint(out, ((v << 1) ^ (v >> 63)) as u64);
}

/// Thrift compact protocol struct writer.
struct Thrift<'a> {
    out: &'a mut Vec<u8>,
    last: Vec<i16>,
}
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BIN: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl<'a> Thrift<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Thrift { out, last: vec![0] }
    }
    fn field(&mut self, id: i16, ty: u8) {
        let last = self.last.last_mut().unwrap();
        let delta = id - *last;
        if delta > 0 && delta <= 15 {
            self.out.push(((delta as u8) << 4) | ty);
        } else {
            self.out.push(ty);
            zigzag(self.out, id as i64);
        }
        *last = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, T_I32);
        zigzag(self.out, v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, T_I64);
        zigzag(self.out, v);
    }
    fn str(&mut self, id: i16, s: &[u8]) {
        self.field(id, T_BIN);
        varint(self.out, s.len() as u64);
        self.out.extend_from_slice(s);
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, T_LIST);
        self.list_header(elem, n);
    }
    fn list_header(&mut self, elem: u8, n: usize) {
        if n < 15 {
            self.out.push(((n as u8) << 4) | elem);
        } else {
            self.out.push(0xF0 | elem);
            varint(self.out, n as u64);
        }
    }
    fn begin(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.last.push(0);
    }
    /// A struct that is a list element (no field header).
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W, cols: &'static [ColSpec]) -> std::io::Result<Self> {
        out.write_all(b"PAR1")?;
        Ok(Writer { out, pos: 4, cols, groups: Vec::new() })
    }

    /// Writes one row group; `data[i]` belongs to `cols[i]`.
    pub fn row_group(&mut self, rows: usize, data: &[ColData]) -> std::io::Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let mut chunks = Vec::with_capacity(data.len());
        let mut head = Vec::with_capacity(64);
        for (spec, col) in self.cols.iter().zip(data) {
            // Definition levels: 4-byte length, then one bit-packed run.
            let mut levels = Vec::new();
            if spec.nullable {
                let defs = col.defs.expect("nullable column needs defs");
                let groups = (rows + 7) / 8;
                let mut run = Vec::with_capacity(groups + 8);
                varint(&mut run, ((groups as u64) << 1) | 1);
                run.extend_from_slice(&defs[..groups]);
                levels.extend_from_slice(&(run.len() as u32).to_le_bytes());
                levels.extend_from_slice(&run);
            }
            let page_size = levels.len() + col.values.len();
            head.clear();
            {
                let mut t = Thrift::new(&mut head);
                t.i32(1, 0); // DATA_PAGE
                t.i32(2, page_size as i32);
                t.i32(3, page_size as i32);
                t.begin(5);
                t.i32(1, rows as i32);
                t.i32(2, 0); // PLAIN
                t.i32(3, 3); // RLE
                t.i32(4, 3); // RLE
                t.end();
                t.end();
            }
            let offset = self.pos;
            self.out.write_all(&head)?;
            self.out.write_all(&levels)?;
            self.out.write_all(col.values)?;
            let size = (head.len() + page_size) as u64;
            self.pos += size;
            chunks.push(ChunkMeta { offset, size, num_values: rows as u64 });
        }
        self.groups.push(GroupMeta { rows: rows as u64, chunks });
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        let mut meta = Vec::with_capacity(1024);
        {
            let mut t = Thrift::new(&mut meta);
            t.i32(1, 1); // version
            t.list(2, T_STRUCT, self.cols.len() + 1);
            // root
            t.begin_elem();
            t.str(4, b"schema");
            t.i32(5, self.cols.len() as i32);
            t.end();
            for c in self.cols {
                t.begin_elem();
                t.i32(
                    1,
                    match c.kind {
                        Kind::Str => 6,
                        Kind::Date | Kind::I32 => 1,
                        Kind::I64 => 2,
                    },
                );
                t.i32(3, if c.nullable { 1 } else { 0 });
                t.str(4, c.name.as_bytes());
                match c.kind {
                    Kind::Str => {
                        t.i32(6, 0); // UTF8
                        t.begin(10);
                        t.begin(1); // STRING
                        t.end();
                        t.end();
                    }
                    Kind::Date => {
                        t.i32(6, 6); // DATE
                        t.begin(10);
                        t.begin(6); // DATE
                        t.end();
                        t.end();
                    }
                    _ => {}
                }
                t.end();
            }
            let total_rows: u64 = self.groups.iter().map(|g| g.rows).sum();
            t.i64(3, total_rows as i64);
            t.list(4, T_STRUCT, self.groups.len());
            for g in &self.groups {
                t.begin_elem();
                t.list(1, T_STRUCT, g.chunks.len());
                let mut total = 0u64;
                for (c, spec) in g.chunks.iter().zip(self.cols) {
                    total += c.size;
                    t.begin_elem();
                    t.i64(2, c.offset as i64);
                    t.begin(3);
                    t.i32(
                        1,
                        match spec.kind {
                            Kind::Str => 6,
                            Kind::Date | Kind::I32 => 1,
                            Kind::I64 => 2,
                        },
                    );
                    t.list(2, T_I32, 2);
                    zigzag(t.out, 0); // PLAIN
                    zigzag(t.out, 3); // RLE
                    t.list(3, T_BIN, 1);
                    varint(t.out, spec.name.len() as u64);
                    t.out.extend_from_slice(spec.name.as_bytes());
                    t.i32(4, 0); // UNCOMPRESSED
                    t.i64(5, c.num_values as i64);
                    t.i64(6, c.size as i64);
                    t.i64(7, c.size as i64);
                    t.i64(9, c.offset as i64);
                    t.end();
                    t.end();
                }
                t.i64(2, total as i64);
                t.i64(3, g.rows as i64);
                t.end();
            }
            t.str(6, b"govinda");
            t.end();
        }
        self.out.write_all(&meta)?;
        self.out.write_all(&(meta.len() as u32).to_le_bytes())?;
        self.out.write_all(b"PAR1")?;
        self.out.flush()
    }
}

pub const USER_DAILY: &[ColSpec] = &[
    ColSpec { name: "user_id", kind: Kind::Str, nullable: false },
    ColSpec { name: "day", kind: Kind::Date, nullable: false },
    ColSpec { name: "sessions", kind: Kind::I32, nullable: false },
    ColSpec { name: "events", kind: Kind::I32, nullable: false },
    ColSpec { name: "revenue_cents", kind: Kind::I64, nullable: false },
    ColSpec { name: "top_category", kind: Kind::Str, nullable: true },
];

pub const CAMPAIGN_DAILY: &[ColSpec] = &[
    ColSpec { name: "campaign_id", kind: Kind::Str, nullable: false },
    ColSpec { name: "channel", kind: Kind::Str, nullable: false },
    ColSpec { name: "day", kind: Kind::Date, nullable: false },
    ColSpec { name: "sessions", kind: Kind::I64, nullable: false },
    ColSpec { name: "purchasing_sessions", kind: Kind::I64, nullable: false },
    ColSpec { name: "revenue_cents", kind: Kind::I64, nullable: false },
    ColSpec { name: "assisted_sessions", kind: Kind::I64, nullable: false },
];
