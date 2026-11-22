//! Minimal Parquet writer: one row group, one PLAIN uncompressed v1 data page per column.

pub const T_INT32: i32 = 1;
pub const T_INT64: i32 = 2;
pub const T_BYTES: i32 = 6;

#[derive(Clone, Copy, PartialEq)]
pub enum Logical {
    None,
    Str,
    Date,
}

pub struct Col {
    pub name: &'static str,
    pub ptype: i32,
    pub optional: bool,
    pub logical: Logical,
    pub body: Vec<u8>, // page body (levels + values)
}

struct Tc {
    b: Vec<u8>,
    last: i16,
    stack: Vec<i16>,
}

fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

impl Tc {
    fn new() -> Tc {
        Tc { b: Vec::new(), last: 0, stack: Vec::new() }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn hdr(&mut self, id: i16, ty: u8) {
        let d = id - self.last;
        if (1..=15).contains(&d) {
            self.b.push(((d as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.varint(zz(id as i64));
        }
        self.last = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.hdr(id, 5);
        self.varint(zz(v as i64));
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.hdr(id, 6);
        self.varint(zz(v));
    }
    fn bin(&mut self, id: i16, s: &[u8]) {
        self.hdr(id, 8);
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
    fn st(&mut self, id: i16) {
        self.hdr(id, 12);
        self.stack.push(self.last);
        self.last = 0;
    }
    fn elem_st(&mut self) {
        self.stack.push(self.last);
        self.last = 0;
    }
    fn en(&mut self) {
        self.b.push(0);
        self.last = self.stack.pop().unwrap();
    }
    fn list(&mut self, id: i16, ety: u8, n: usize) {
        self.hdr(id, 9);
        if n < 15 {
            self.b.push(((n as u8) << 4) | ety);
        } else {
            self.b.push(0xF0 | ety);
            self.varint(n as u64);
        }
    }
    fn i32e(&mut self, v: i32) {
        self.varint(zz(v as i64));
    }
    fn bine(&mut self, s: &[u8]) {
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
}

pub fn i64_page(vals: impl Iterator<Item = i64>, n: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(n * 8);
    for v in vals {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

pub fn i32_page(vals: impl Iterator<Item = i32>, n: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(n * 4);
    for v in vals {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

pub fn str_page<'a>(vals: impl Iterator<Item = &'a [u8]>, est: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(est);
    for v in vals {
        b.extend_from_slice(&(v.len() as u32).to_le_bytes());
        b.extend_from_slice(v);
    }
    b
}

/// Optional string column: def levels (bit width 1, one bit-packed run) + values of non-nulls.
pub fn opt_str_page<'a>(vals: impl Iterator<Item = Option<&'a [u8]>>, n: usize) -> Vec<u8> {
    let groups = (n + 7) / 8;
    let mut levels = vec![0u8; groups];
    let mut data = Vec::new();
    for (i, v) in vals.enumerate() {
        if let Some(s) = v {
            levels[i >> 3] |= 1 << (i & 7);
            data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            data.extend_from_slice(s);
        }
    }
    let mut hdr = Vec::new();
    let mut h = ((groups as u64) << 1) | 1;
    while h >= 0x80 {
        hdr.push((h as u8) | 0x80);
        h >>= 7;
    }
    hdr.push(h as u8);
    let rl = hdr.len() + levels.len();
    let mut b = Vec::with_capacity(4 + rl + data.len());
    b.extend_from_slice(&(rl as u32).to_le_bytes());
    b.extend_from_slice(&hdr);
    b.extend_from_slice(&levels);
    b.extend_from_slice(&data);
    b
}

/// A column whose page body is given in pieces (written one after the other, never concatenated).
pub struct ColP<'a> {
    pub name: &'static str,
    pub ptype: i32,
    pub optional: bool,
    pub logical: Logical,
    pub parts: Vec<&'a [u8]>,
}

pub fn write_file(path: &std::path::Path, num_rows: usize, cols: Vec<Col>) -> std::io::Result<()> {
    let parts: Vec<ColP> = cols.iter().map(|c| ColP { name: c.name, ptype: c.ptype, optional: c.optional, logical: c.logical, parts: vec![&c.body[..]] }).collect();
    write_file_parts(path, num_rows, &parts)
}

/// Slowest (create, write, flush + close) of any output file, in ns (shown by FAST_VERBOSE).
pub static WSTAT: [std::sync::atomic::AtomicU64; 3] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

pub fn write_file_parts(path: &std::path::Path, num_rows: usize, cols: &[ColP]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::Ordering::Relaxed;
    let t0 = std::time::Instant::now();
    let file = std::fs::File::create(path)?;
    let t1 = std::time::Instant::now();
    WSTAT[0].fetch_max((t1 - t0).as_nanos() as u64, Relaxed);
    let mut f = std::io::BufWriter::with_capacity(1 << 16, file);
    f.write_all(b"PAR1")?;
    let mut pos = 4usize;
    // (offset, total_size) per column
    let mut meta = Vec::new();
    for c in cols {
        let blen: usize = c.parts.iter().map(|p| p.len()).sum();
        let mut h = Tc::new();
        h.i32(1, 0); // DATA_PAGE
        h.i32(2, blen as i32);
        h.i32(3, blen as i32);
        h.st(5);
        h.i32(1, num_rows as i32);
        h.i32(2, 0); // PLAIN
        h.i32(3, 3); // RLE
        h.i32(4, 3); // RLE
        h.en();
        h.b.push(0); // end PageHeader
        f.write_all(&h.b)?;
        for p in &c.parts {
            f.write_all(p)?;
        }
        meta.push((pos as i64, (h.b.len() + blen) as i64));
        pos += h.b.len() + blen;
    }
    let mut t = Tc::new();
    t.i32(1, 1);
    t.list(2, 12, cols.len() + 1);
    t.elem_st();
    t.bin(4, b"schema");
    t.i32(5, cols.len() as i32);
    t.en();
    for c in cols.iter() {
        t.elem_st();
        t.i32(1, c.ptype);
        t.i32(3, if c.optional { 1 } else { 0 });
        t.bin(4, c.name.as_bytes());
        match c.logical {
            Logical::Str => {
                t.i32(6, 0);
                t.st(10);
                t.st(1);
                t.en();
                t.en();
            }
            Logical::Date => {
                t.i32(6, 6);
                t.st(10);
                t.st(6);
                t.en();
                t.en();
            }
            Logical::None => {}
        }
        t.en();
    }
    t.i64(3, num_rows as i64);
    t.list(4, 12, 1);
    t.elem_st();
    t.list(1, 12, cols.len());
    let mut total = 0i64;
    for (c, &(off, sz)) in cols.iter().zip(meta.iter()) {
        total += sz;
        t.elem_st();
        t.i64(2, off);
        t.st(3);
        t.i32(1, c.ptype);
        t.list(2, 5, 2);
        t.i32e(0);
        t.i32e(3);
        t.list(3, 8, 1);
        t.bine(c.name.as_bytes());
        t.i32(4, 0); // UNCOMPRESSED
        t.i64(5, num_rows as i64);
        t.i64(6, sz);
        t.i64(7, sz);
        t.i64(9, off);
        t.en();
        t.en();
    }
    t.i64(2, total);
    t.i64(3, num_rows as i64);
    t.en();
    t.bin(6, b"sessions-fast");
    t.b.push(0); // end FileMetaData
    let flen = t.b.len() as u32;
    f.write_all(&t.b)?;
    f.write_all(&flen.to_le_bytes())?;
    f.write_all(b"PAR1")?;
    f.flush()?;
    let t2 = std::time::Instant::now();
    WSTAT[1].fetch_max((t2 - t1).as_nanos() as u64, Relaxed);
    drop(f);
    WSTAT[2].fetch_max(t2.elapsed().as_nanos() as u64, Relaxed);
    Ok(())
}
