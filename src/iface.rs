pub const ID_EMPTY: u64 = u64::MAX;
pub const ID_ODD: u64 = 1 << 63;
pub const ID_LONG: u64 = 1 << 62;
pub const LONG_MASK: u64 = ID_LONG - 1;
pub const ID_C_SHORT: u8 = 0;
pub const ID_C_LONG: u8 = 1;
pub const ID_C_ODD: u8 = 2;
pub const ID_C_EMPTY: u8 = 3;
#[inline(always)]
pub fn id_class(w: u64) -> u8 {
    if w == ID_EMPTY {
        ID_C_EMPTY
    } else if w & ID_ODD != 0 {
        ID_C_ODD
    } else if w & ID_LONG != 0 {
        ID_C_LONG
    } else {
        ID_C_SHORT
    }
}
#[inline(always)]
pub fn id_is_long(w: u64) -> bool {
    w >> 62 == 1
}
#[inline(always)]
pub fn long_word(_hi: u64, lo: u64) -> u64 {
    ID_LONG | (lo & LONG_MASK)
}
#[inline]
pub fn long_key(digits: &[u8]) -> Option<(u64, u64)> {
    if digits.len() != 32 {
        return None;
    }
    let mut k = [0u64; 2];
    for (i, &b) in digits.iter().enumerate() {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => return None,
        };
        k[i / 16] = k[i / 16] << 4 | d as u64;
    }
    Some((k[0], k[1]))
}

pub const TS_BAD: i64 = i64::MIN;
pub const TS_HUGE: i64 = i64::MAX;
pub const PROD_EMPTY: u32 = u32::MAX;
pub const PROD_ODD: u32 = u32::MAX - 1;
pub const CAMP_EMPTY: u32 = u32::MAX;
pub const CAMP_ODD: u32 = u32::MAX - 1;
pub const KIND_VIEW: u8 = 0;
pub const KIND_CART: u8 = 1;
pub const KIND_PURCHASE: u8 = 2;
pub const KIND_OTHER: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EvRow {
    pub eid: u64,
    pub uid: u64,
    pub ts: i64,
    pub ingest: i64,
    pub kind: u8,
    pub product: u32,
    pub price: u32,
    pub eh: u64,
    pub el: u64,
    pub uh: u64,
    pub ul: u64,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClRow {
    pub cid: u64,
    pub uid: u64,
    pub campaign: u32,
    pub ts: i64,
    pub ch: u64,
    pub cl: u64,
    pub uh: u64,
    pub ul: u64,
}

pub const NB: usize = 64;

pub const PP_INVALID: u64 = 1 << 63;
pub const PP_SOME: u64 = PROD_ODD as u64;
pub const PP_NONE: u64 = PP_INVALID | PROD_EMPTY as u64;

#[inline(always)]
pub fn ev_pp(kind: u8, product: u32, price: u32) -> u64 {
    let k = (kind as u64) << 56;
    if kind == KIND_PURCHASE {
        let inv = if product == PROD_EMPTY || price == 0 { PP_INVALID } else { 0 };
        product as u64 | (price as u64) << 32 | k | inv
    } else if product == PROD_EMPTY {
        PP_NONE | k
    } else {
        PP_SOME | k | if kind > KIND_PURCHASE { PP_INVALID } else { 0 }
    }
}

pub const BATCH_ALIGN: usize = NB * 8;
pub const SCRATCH: usize = 256;
pub const SCRATCH_INIT: u64 = 0x2C2C_2C2C_2C2C_2C2C;
pub const SCRATCH_ZERO: core::ops::Range<usize> = 2..4;
pub const SCRATCH_UNMATCH: (usize, u64) = (48 * 4 + 1, ((b',' ^ b'-') as u64) << 8);

#[repr(C, align(512))]
pub struct EvBatch {
    pub ts: [i64; NB],
    pub pp: [u64; NB],
    pub eid: [u64; NB],
    pub uid: [u64; NB],
    pub ing: [i64; NB],
    pub _pad: [u64; 8],
    pub scratch: [u64; SCRATCH],
    pub n: usize,
    pub _pad2: [u64; 7],
    pub ehi: [u64; NB],
    pub elo: [u64; NB],
    pub uhi: [u64; NB],
    pub ulo: [u64; NB],
    pub _pad3: [u64; 8],
}
#[repr(C, align(512))]
pub struct CkBatch {
    pub ts: [i64; NB],
    pub camp: [u64; NB],
    pub cid: [u64; NB],
    pub uid: [u64; NB],
    pub scratch: [u64; SCRATCH],
    pub n: usize,
    pub _pad2: [u64; 7],
    pub chi: [u64; NB],
    pub clo: [u64; NB],
    pub uhi: [u64; NB],
    pub ulo: [u64; NB],
    pub _pad3: [u64; 8],
}
const _: () = assert!(core::mem::offset_of!(EvBatch, ehi) % 64 == 0 && core::mem::offset_of!(CkBatch, chi) % 64 == 0);
const _: () = assert!(BATCH_ALIGN == 512 && core::mem::offset_of!(EvBatch, ts) == 0 && core::mem::offset_of!(CkBatch, ts) == 0);
impl EvBatch {
    pub fn new() -> Box<EvBatch> {
        let mut b: Box<EvBatch> = unsafe { Box::new(core::mem::zeroed()) };
        b.scratch = [SCRATCH_INIT; SCRATCH];
        b.scratch[SCRATCH_ZERO].fill(0);
        b.scratch[SCRATCH_UNMATCH.0] ^= SCRATCH_UNMATCH.1;
        b
    }
    #[inline(always)]
    pub fn row(&self, i: usize) -> EvRow {
        let pp = self.pp[i];
        let (e, u) = (self.eid[i], self.uid[i]);
        let (eh, el) = if id_is_long(e) { (self.ehi[i], self.elo[i]) } else { (0, 0) };
        let (uh, ul) = if id_is_long(u) { (self.uhi[i], self.ulo[i]) } else { (0, 0) };
        EvRow { eid: e, uid: u, ts: self.ts[i], ingest: self.ing[i], kind: (pp >> 56) as u8 & 3, product: pp as u32, price: (pp >> 32) as u32 & 0xFF_FFFF, eh, el, uh, ul }
    }
}
impl CkBatch {
    pub fn new() -> Box<CkBatch> {
        let mut b: Box<CkBatch> = unsafe { Box::new(core::mem::zeroed()) };
        b.scratch = [SCRATCH_INIT; SCRATCH];
        b.scratch[SCRATCH_ZERO].fill(0);
        b.scratch[SCRATCH_UNMATCH.0] ^= SCRATCH_UNMATCH.1;
        b
    }
    #[inline(always)]
    pub fn row(&self, i: usize) -> ClRow {
        let (c, u) = (self.cid[i], self.uid[i]);
        let (ch, cl) = if id_is_long(c) { (self.chi[i], self.clo[i]) } else { (0, 0) };
        let (uh, ul) = if id_is_long(u) { (self.uhi[i], self.ulo[i]) } else { (0, 0) };
        ClRow { cid: c, uid: u, campaign: self.camp[i] as u32, ts: self.ts[i], ch, cl, uh, ul }
    }
}

pub trait EventSink {
    fn ev(&mut self) -> *mut EvBatch;
    fn ev_flush(&mut self);
    #[inline(always)]
    fn event(&mut self, r: &EvRow) {
        unsafe {
            let b = self.ev();
            let i = (*b).n;
            *(*b).eid.get_unchecked_mut(i) = r.eid;
            *(*b).uid.get_unchecked_mut(i) = r.uid;
            *(*b).ts.get_unchecked_mut(i) = r.ts;
            *(*b).ing.get_unchecked_mut(i) = r.ingest;
            *(*b).pp.get_unchecked_mut(i) = ev_pp(r.kind, r.product, r.price);
            *(*b).ehi.get_unchecked_mut(i) = r.eh;
            *(*b).elo.get_unchecked_mut(i) = r.el;
            *(*b).uhi.get_unchecked_mut(i) = r.uh;
            *(*b).ulo.get_unchecked_mut(i) = r.ul;
            (*b).n = i + 1;
            if i + 1 == NB {
                self.ev_flush();
            }
        }
    }
}
pub trait ClickSink {
    fn ck(&mut self) -> *mut CkBatch;
    fn ck_flush(&mut self);
    #[inline(always)]
    fn click(&mut self, r: &ClRow) {
        unsafe {
            let b = self.ck();
            let i = (*b).n;
            *(*b).cid.get_unchecked_mut(i) = r.cid;
            *(*b).uid.get_unchecked_mut(i) = r.uid;
            *(*b).ts.get_unchecked_mut(i) = r.ts;
            *(*b).camp.get_unchecked_mut(i) = r.campaign as u64;
            *(*b).chi.get_unchecked_mut(i) = r.ch;
            *(*b).clo.get_unchecked_mut(i) = r.cl;
            *(*b).uhi.get_unchecked_mut(i) = r.uh;
            *(*b).ulo.get_unchecked_mut(i) = r.ul;
            (*b).n = i + 1;
            if i + 1 == NB {
                self.ck_flush();
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LkRow {
    pub a: u64,
    pub b: u64,
    pub ah: u64,
    pub al: u64,
    pub bh: u64,
    pub bl: u64,
}
#[repr(C, align(512))]
pub struct LkBatch {
    pub a: [u64; NB],
    pub b: [u64; NB],
    pub ahi: [u64; NB],
    pub alo: [u64; NB],
    pub bhi: [u64; NB],
    pub blo: [u64; NB],
    pub n: usize,
}
impl LkBatch {
    pub fn new() -> Box<LkBatch> {
        unsafe { Box::new(core::mem::zeroed()) }
    }
    #[inline(always)]
    pub fn row(&self, i: usize) -> LkRow {
        let (a, b) = (self.a[i], self.b[i]);
        let (ah, al) = if id_is_long(a) { (self.ahi[i], self.alo[i]) } else { (0, 0) };
        let (bh, bl) = if id_is_long(b) { (self.bhi[i], self.blo[i]) } else { (0, 0) };
        LkRow { a, b, ah, al, bh, bl }
    }
}
pub trait LinkSink {
    fn lk(&mut self) -> *mut LkBatch;
    fn lk_flush(&mut self);
    #[inline(always)]
    fn link(&mut self, r: &LkRow) {
        unsafe {
            let b = self.lk();
            let i = (*b).n;
            *(*b).a.get_unchecked_mut(i) = r.a;
            *(*b).b.get_unchecked_mut(i) = r.b;
            *(*b).ahi.get_unchecked_mut(i) = r.ah;
            *(*b).alo.get_unchecked_mut(i) = r.al;
            *(*b).bhi.get_unchecked_mut(i) = r.bh;
            *(*b).blo.get_unchecked_mut(i) = r.bl;
            (*b).n = i + 1;
            if i + 1 == NB {
                self.lk_flush();
            }
        }
    }
}

pub trait Kernel {
    const NAME: &'static str;
    fn events<S: EventSink>(buf: &[u8], sink: &mut S);
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S);
}

#[inline(always)]
pub fn odd_id(offset: usize, len: usize) -> u64 {
    ID_ODD | ((offset as u64) << 8) | (len.min(255) as u64)
}
#[inline(always)]
pub fn id_is_odd(id: u64) -> bool {
    id != ID_EMPTY && id & ID_ODD != 0
}
#[inline(always)]
pub fn odd_offset(id: u64) -> usize {
    ((id & !ID_ODD) >> 8) as usize
}
#[inline(always)]
pub fn odd_len(id: u64) -> usize {
    (id & 255) as usize
}
