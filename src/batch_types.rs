
pub const ID_EMPTY: u64 = u64::MAX;
pub const ID_ODD: u64 = 1 << 63;
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
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClRow {
    pub cid: u64,
    pub uid: u64,
    pub campaign: u32,
    pub ts: i64,
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
}
#[repr(C, align(512))]
pub struct CkBatch {
    pub ts: [i64; NB],
    pub camp: [u64; NB],
    pub cid: [u64; NB],
    pub uid: [u64; NB],
    pub scratch: [u64; SCRATCH],
    pub n: usize,
}
const _: () = assert!(BATCH_ALIGN == 512 && core::mem::offset_of!(EvBatch, ts) == 0 && core::mem::offset_of!(CkBatch, ts) == 0);
impl EvBatch {
    pub fn new() -> Box<EvBatch> {
        let mut b: Box<EvBatch> = unsafe { Box::new(core::mem::zeroed()) };
        b.scratch = [SCRATCH_INIT; SCRATCH];
        b.scratch[SCRATCH_ZERO].fill(0);
        b
    }
    #[inline(always)]
    pub fn row(&self, i: usize) -> EvRow {
        let pp = self.pp[i];
        EvRow { eid: self.eid[i], uid: self.uid[i], ts: self.ts[i], ingest: self.ing[i], kind: (pp >> 56) as u8 & 3, product: pp as u32, price: (pp >> 32) as u32 & 0xFF_FFFF }
    }
}
impl CkBatch {
    pub fn new() -> Box<CkBatch> {
        let mut b: Box<CkBatch> = unsafe { Box::new(core::mem::zeroed()) };
        b.scratch = [SCRATCH_INIT; SCRATCH];
        b.scratch[SCRATCH_ZERO].fill(0);
        b
    }
    #[inline(always)]
    pub fn row(&self, i: usize) -> ClRow {
        ClRow { cid: self.cid[i], uid: self.uid[i], campaign: self.camp[i] as u32, ts: self.ts[i] }
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
            (*b).n = i + 1;
            if i + 1 == NB {
                self.ck_flush();
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
