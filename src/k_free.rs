use crate::iface::*;
use core::arch::x86_64::*;

#[cfg(not(all(
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
)))]
compile_error!("k_free needs -C target-cpu=cascadelake (or newer)");

macro_rules! kernel {
    ($ty:ident, $name:literal, $mode:ident) => {
        pub struct $ty;
        impl Kernel for $ty {
            const NAME: &'static str = $name;
            fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_events::<S, $mode>(buf, sink) }
                sink.ev_flush();
            }
            fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_clicks::<S, $mode>(buf, sink) }
                sink.ck_flush();
            }
        }
    };
}
kernel!(Free, "free", PIPE);
kernel!(FreeN, "freen", CHAIN);

pub struct LfOnly;
impl Kernel for LfOnly {
    const NAME: &'static str = "lf";
    fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
        unsafe {
            let base = buf.as_ptr();
            let len = buf.len();
            let end = base.add(len);
            let b = sink.ev();
            let sink: *mut S = sink;
            let cx = &Cx { base, end, b, cache: (*b).scratch.as_mut_ptr() as *mut DateEnt, sink };
            let mut w = (*b).ts.as_mut_ptr().add((*b).n);
            let mut p = base;
            while p.wrapping_add(64) <= end {
                let r = ev_lf::<S>(cx, p, w);
                p = r.0 as *const u8;
                w = r.1 as *mut i64;
            }
            (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
            let mut pos = p.offset_from(base) as usize;
            while pos < len {
                pos = scan_event(buf, pos, &mut *sink);
            }
            (*sink).ev_flush();
        }
    }
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
        unsafe {
            let base = buf.as_ptr();
            let len = buf.len();
            let end = base.add(len);
            let b = sink.ck();
            let sink: *mut S = sink;
            let cx = &Cx { base, end, b, cache: (*b).scratch.as_mut_ptr() as *mut DateEnt, sink };
            let mut w = (*b).ts.as_mut_ptr().add((*b).n);
            let mut p = base;
            while p.wrapping_add(64) <= end {
                let r = cl_lf::<S>(cx, p, w);
                p = r.0 as *const u8;
                w = r.1 as *mut i64;
            }
            (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
            let mut pos = p.offset_from(base) as usize;
            while pos < len {
                pos = scan_click(buf, pos, &mut *sink);
            }
            (*sink).ck_flush();
        }
    }
}

pub struct LongOnly;
impl Kernel for LongOnly {
    const NAME: &'static str = "lng";
    fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
        unsafe {
            let b = sink.ev();
            let cache = (*b).scratch.as_mut_ptr() as *mut DateEnt;
            let (base, len) = (buf.as_ptr(), buf.len());
            let mut l = 0;
            while l < len {
                let mut r = FAIL;
                if l + 128 <= len {
                    r = ev_long::<S>(base, len, l, cache, sink);
                }
                if r == FAIL && l + 64 <= len {
                    r = ev_core::<S, true>(base, len, l, 0, cache, sink);
                }
                l = if r != FAIL { r + 1 } else { scan_event(buf, l, sink) };
            }
        }
        sink.ev_flush();
    }
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
        unsafe {
            let b = sink.ck();
            let cache = (*b).scratch.as_mut_ptr() as *mut DateEnt;
            let (base, len) = (buf.as_ptr(), buf.len());
            let mut l = 0;
            while l < len {
                let mut r = FAIL;
                if l + 128 <= len {
                    r = cl_long::<S>(base, len, l, cache, sink);
                }
                l = if r != FAIL { r + 1 } else { scan_click(buf, l, sink) };
            }
        }
        sink.ck_flush();
    }
}

#[cfg(feature = "stats")]
pub static REASONS: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
#[cfg(feature = "stats")]
pub const REASON_NAMES: [&str; 16] = [
    "ev-careful", "cl-careful", "ids-cold", "ids-malformed", "ts-cold", "ts-malformed", "ingest-cold", "kind-cold",
    "dec-cold", "price-quote", "price-long", "loop-exit1", "ev-slow", "cl-slow", "date-miss", "ev-redo",
];
#[cfg(feature = "stats")]
pub static LFC: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
#[inline(always)]
fn lfc(_r: usize) {
    #[cfg(feature = "stats")]
    LFC[_r].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}
#[inline(always)]
fn count(_r: usize) {
    #[cfg(feature = "stats")]
    REASONS[_r].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct IdShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lz: [u8; 16],
    orv: [u64; 2],
}

const fn id_shapes(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 128] {
    let mut t = [IdShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], orv: [0; 2] }; 128];
    let mut raw = 0;
    while raw < 64 {
        let mut q = 0;
        while q < 2 {
            let i = raw * 2 + q;
            let plus = 1 + 2 * q;
            let mut ok = false;
            if raw >= plus + 1 && raw <= plus + 8 {
                let nd = raw - plus;
                let mut j = 8 - nd;
                while j < 8 {
                    t[i].ctrl[dig + j] = (8 - q + j) as u8;
                    t[i].subv[dig + j] = b'0';
                    j += 1;
                }
                t[i].ctrl[chk] = (16 - raw + q) as u8;
                t[i].subv[chk] = prefix;
                if q == 1 {
                    t[i].ctrl[chk + 1] = 15;
                    t[i].subv[chk + 1] = b'"';
                }
                if nd > 1 {
                    t[i].lz[dig + 8 - nd] = 1;
                }
                ok = true;
            } else if allow_empty && raw == 2 * q {
                if q == 1 {
                    t[i].ctrl[chk + 1] = 15;
                    t[i].subv[chk + 1] = b'"';
                }
                t[i].orv[1] = u64::MAX;
                ok = true;
            }
            if !ok {
                t[i].subv[chk] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

const fn id_shapes_raw(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 64] {
    let both = id_shapes(prefix, dig, chk, allow_empty);
    let mut t = [both[1]; 64];
    let mut raw = 0;
    while raw < 64 {
        let q = if raw == 10 || raw == 11 || (raw == 2 && allow_empty) { 1 } else { 0 };
        t[raw] = both[raw * 2 + q];
        if q == 1 {
            t[raw].ctrl[chk + 2] = (16 - raw) as u8;
            t[raw].subv[chk + 2] = b'"';
        }
        raw += 1;
    }
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct JidRow {
    ctrl: [u8; 32],
    subv: [u8; 32],
    lz: [u8; 32],
    orv: [u64; 4],
}
const JID_ROWS: usize = 64 * 64;

const fn jid_rows() -> [JidRow; JID_ROWS] {
    let e = id_shapes_raw(b'e', 0, 8, false);
    let u = id_shapes_raw(b'u', 0, 8, true);
    let mut bad_u = u[0];
    bad_u.subv[8] = 1;
    bad_u.orv = [0; 2];
    let mut t = [JidRow { ctrl: [0; 32], subv: [0; 32], lz: [0; 32], orv: [0; 4] }; JID_ROWS];
    let mut c0 = 0;
    while c0 < 64 {
        let mut d = 0;
        while d < 64 {
            let (a, b) = (&e[c0], if d == 0 { &bad_u } else { &u[d - 1] });
            let r = &mut t[c0 * 64 + d];
            let mut i = 0;
            while i < 16 {
                r.ctrl[i] = a.ctrl[i];
                r.ctrl[16 + i] = b.ctrl[i];
                r.subv[i] = a.subv[i];
                r.subv[16 + i] = b.subv[i];
                r.lz[i] = a.lz[i];
                r.lz[16 + i] = b.lz[i];
                i += 1;
            }
            r.orv = [a.orv[1], 0, b.orv[1], 0];
            d += 1;
        }
        c0 += 1;
    }
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct DecShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lz: [u8; 16],
    _pad: [u8; 16],
}

const fn dec_shapes(prefix: &[u8]) -> [DecShape; 128] {
    let mut t = [DecShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], _pad: [0; 16] }; 128];
    let pl = prefix.len();
    let mut raw = 0;
    while raw < 64 {
        let mut q = 0;
        while q < 2 {
            let i = raw * 2 + q;
            let plus = pl + 2 * q;
            if raw >= plus + 1 && raw <= plus + 8 {
                let nd = raw - plus;
                let mut j = 8 - nd;
                while j < 8 {
                    t[i].ctrl[j] = (8 - q + j) as u8;
                    t[i].subv[j] = b'0';
                    j += 1;
                }
                let mut k = 0;
                while k < pl {
                    t[i].ctrl[8 + k] = (16 - raw + q + k) as u8;
                    t[i].subv[8 + k] = prefix[k];
                    k += 1;
                }
                if q == 1 {
                    t[i].ctrl[8 + pl] = 15;
                    t[i].subv[8 + pl] = b'"';
                }
                if nd > 1 {
                    t[i].lz[8 - nd] = 1;
                }
            } else {
                t[i].subv[8] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

const fn camp4_shapes() -> [DecShape; 128] {
    let mut t = [DecShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], _pad: [0; 16] }; 128];
    let prefix = *b"c_";
    let pl = 2;
    let mut raw = 0;
    while raw < 64 {
        let mut q = 0;
        while q < 2 {
            let i = raw * 2 + (1 - q);
            let plus = pl + 2 * q;
            if raw >= plus + 1 && raw <= plus + 4 {
                let nd = raw - plus;
                let mut j = 4 - nd;
                while j < 4 {
                    t[i].ctrl[j] = (12 - q + j) as u8;
                    t[i].subv[j] = b'0';
                    j += 1;
                }
                let mut k = 0;
                while k < pl {
                    t[i].ctrl[8 + k] = (16 - raw + q + k) as u8;
                    t[i].subv[8 + k] = prefix[k];
                    k += 1;
                }
                if q == 1 {
                    t[i].ctrl[8 + pl] = 15;
                    t[i].subv[8 + pl] = b'"';
                    t[i].ctrl[8 + pl + 1] = (16 - raw) as u8;
                    t[i].subv[8 + pl + 1] = b'"';
                }
                if nd > 1 {
                    t[i].lz[4 - nd] = 1;
                }
            } else {
                t[i].subv[8] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TsA {
    ctrl: [u8; 16],
    base: [u8; 16],
    lim: [u8; 16],
    w1: [i8; 16],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TsB {
    w2: [i16; 8],
    w3: [i16; 8],
    kmaskv: [u8; 16],
    limv: [i32; 4],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TsC {
    koff: i8,
    soff: i8,
    dmask: i16,
    me: i32,
    mf: [i32; 2],
}

const IDENT: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const TSA_INVALID: TsA = TsA { ctrl: IDENT, base: [0; 16], lim: [255; 16], w1: [0; 16] };
const TSB_INVALID: TsB = TsB { w2: [0; 8], w3: [0; 8], kmaskv: [0; 16], limv: [-1, i32::MAX, i32::MAX, i32::MAX] };
const TSC_INVALID: TsC = TsC { koff: -1, soff: -1, dmask: 0, me: 0, mf: [0, 0] };

const fn ts_f12(f2: bool, q: bool) -> (TsA, TsB, TsC) {
    let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
    let raw = 20 + 5 * f2 as usize + 2 * q as usize;
    let off = 11 + q as usize;
    let mut i = 0;
    while i < 16 {
        a.ctrl[i] = if off + i < raw { (off + i + 16 - raw) as u8 } else { 0x80 };
        i += 1;
    }
    let bs = *b"00:00:00";
    let l = [2u8, 9, 0, 5, 9, 0, 5, 9];
    let w = [10i8, 1, 0, 10, 1, 0, 10, 1];
    let mut i = 0;
    while i < 8 {
        a.base[i] = bs[i];
        a.lim[i] = l[i];
        a.w1[i] = w[i];
        i += 1;
    }
    let mut next = 9;
    if f2 {
        let b2 = *b"+00:00";
        let l2 = [2u8, 2, 9, 0, 5, 9];
        let w2 = [0i8, 10, 1, 0, 10, 1];
        let mut i = 0;
        while i < 6 {
            a.base[8 + i] = b2[i];
            a.lim[8 + i] = l2[i];
            a.w1[8 + i] = w2[i];
            i += 1;
        }
        next = 14;
        c.mf = [60_000, -60_000];
        c.soff = (off + 8) as i8 - raw as i8;
    } else {
        a.base[8] = b'Z';
        a.lim[8] = 0;
    }
    if q {
        a.base[next] = b'"';
        a.lim[next] = 0;
    }
    b.w2 = [60, 1, 60, 1, 1, 1, 1, 0];
    b.w3 = [60, 1, 60, 1, 0, 0, 0, 0];
    let mut i = if q { 0 } else { 1 };
    while i < 12 {
        b.kmaskv[i] = 0xFF;
        i += 1;
    }
    b.limv = [1430, i32::MAX, 23, i32::MAX];
    c.koff = q as i8 - 1;
    c.dmask = -1;
    c.me = 1000;
    (a, b, c)
}

const fn ts_rows() -> ([TsA; 64], [TsB; 64], [TsC; 64]) {
    let mut ta = [TSA_INVALID; 64];
    let mut tb = [TSB_INVALID; 64];
    let mut tc = [TSC_INVALID; 64];
    let mut n = 1;
    while n <= 16 {
        if n != 15 {
            let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
            let mut i = 16 - n;
            while i < 16 {
                a.base[i] = b'0';
                a.lim[i] = 9;
                a.w1[i] = if i % 2 == 0 { 10 } else { 1 };
                i += 1;
            }
            b.w2 = [100, 1, 100, 1, 100, 1, 100, 1];
            b.w3 = [10_000, 1, 10_000, 1, 0, 0, 0, 0];
            b.limv = [i32::MAX; 4];
            c.me = 100_000_000;
            c.mf = [1, 1];
            ta[n] = a;
            tb[n] = b;
            tc[n] = c;
        }
        n += 1;
    }
    let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
    a.base[1] = b'"';
    a.lim[1] = 0;
    a.base[15] = b'"';
    a.lim[15] = 0;
    let mut i = 2;
    while i < 15 {
        a.base[i] = b'0';
        a.lim[i] = 9;
        a.w1[i] = if i == 14 { 1 } else if i % 2 == 0 { 10 } else { 1 };
        i += 1;
    }
    b.w2 = [0, 1, 100, 1, 100, 1, 10, 1];
    b.w3 = [10_000, 1, 1000, 1, 0, 0, 0, 0];
    b.limv = [i32::MAX; 4];
    c.me = 10_000_000;
    c.mf = [1, 1];
    ta[15] = a;
    tb[15] = b;
    tc[15] = c;
    let mut k = 0;
    while k < 4 {
        let (f2, q) = (k & 1 == 1, k & 2 == 2);
        let (a, b, c) = ts_f12(f2, q);
        let raw = 20 + 5 * f2 as usize + 2 * q as usize;
        ta[raw] = a;
        tb[raw] = b;
        tc[raw] = c;
        k += 1;
    }
    (ta, tb, tc)
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjA {
    ctrl: [u8; 32],
    base: [u8; 32],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjB {
    lim: [u8; 32],
    w1: [i8; 32],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjC {
    w2: [i16; 16],
    w3: [i16; 16],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjD {
    limv: [i32; 8],
    kmask2: [u8; 16],
    kmask3: [u8; 16],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjE {
    kme: [i64; 4],
    kmf: [i64; 4],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjF {
    ksel: [u8; 32],
    koff1: i64,
    pmask: u64,
    pmask3: u64,
    koffe: i64,
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjRow {
    a: TjA,
    b: TjB,
    c: TjC,
    d: TjD,
    e: TjE,
    f: TjF,
    _pad: [u64; 16],
}
const _: () = assert!(core::mem::size_of::<TjRow>() == 512);

const fn tj_table() -> [TjRow; 128] {
    let (ja, jb, jc, jd, je, jf) = tj_rows();
    let mut t = [TjRow { a: ja[0], b: jb[0], c: jc[0], d: jd[0], e: je[0], f: jf[0], _pad: [0; 16] }; 128];
    let mut j = 0;
    while j < 128 {
        t[j] = TjRow { a: ja[j], b: jb[j], c: jc[j], d: jd[j], e: je[j], f: jf[j], _pad: [0; 16] };
        j += 1;
    }
    t
}

const fn tjl_table() -> [TjRow; 130] {
    let (ja, jb, jc, jd, je, jf) = tj_rows_g(ts_rows_l(), true);
    let mut t = [TjRow { a: ja[0], b: jb[0], c: jc[0], d: jd[0], e: je[0], f: jf[0], _pad: [0; 16] }; 130];
    let mut k = 2;
    while k < 130 {
        let j = k - 2;
        t[k] = TjRow { a: ja[j], b: jb[j], c: jc[j], d: jd[j], e: je[j], f: jf[j], _pad: [0; 16] };
        k += 1;
    }
    t
}
static TJL: [TjRow; 130] = tjl_table();

const ZLANE: TsA = TsA { ctrl: [0x80; 16], base: [0; 16], lim: [0; 16], w1: [0; 16] };
const fn xb(k: usize, raw: usize) -> u8 {
    assert!(k < raw && k + 16 >= raw);
    (k + 16 - raw) as u8
}
const fn ts_f2_l(q: bool) -> (TsA, TsB, TsC) {
    let (_, b, c) = ts_f12(true, q);
    let mut a = ZLANE;
    let raw = 25 + 2 * q as usize;
    let off = 11 + q as usize;
    let bs = *b"00:00:00";
    let l = [2u8, 9, 0, 5, 9, 0, 5, 9];
    let w = [10i8, 1, 0, 10, 1, 0, 10, 1];
    let mut i = 0;
    while i < 8 {
        a.ctrl[i] = xb(off + i, raw);
        a.base[i] = bs[i];
        a.lim[i] = l[i];
        a.w1[i] = w[i];
        i += 1;
    }
    a.ctrl[8] = xb(off + 8, raw);
    a.base[8] = b'+';
    a.lim[8] = 2;
    a.ctrl[9] = xb(off + 9, raw);
    a.base[9] = b'0';
    a.lim[9] = 2;
    a.w1[9] = 10;
    a.ctrl[10] = xb(off + 10, raw);
    a.base[10] = b'0';
    a.lim[10] = 9;
    a.w1[10] = 1;
    a.ctrl[11] = xb(off + 8, raw);
    a.base[11] = b'-';
    a.lim[11] = 0xFE;
    a.ctrl[12] = xb(off + 12, raw);
    a.base[12] = b'0';
    a.lim[12] = 5;
    a.w1[12] = 10;
    a.ctrl[13] = xb(off + 13, raw);
    a.base[13] = b'0';
    a.lim[13] = 9;
    a.w1[13] = 1;
    if q {
        a.ctrl[14] = xb(off + 14, raw);
        a.base[14] = b'"';
    }
    a.ctrl[15] = xb(off + 11, raw);
    a.base[15] = b':';
    (a, b, c)
}
const fn ts_f3_l(n: usize) -> (TsA, TsB, TsC) {
    let (mut a, mut b, mut c) = (ZLANE, TSB_INVALID, TSC_INVALID);
    assert!(n >= 1 && n <= 14);
    a.base[8] = 0xFF;
    a.lim[8] = 1;
    let k = if n < 7 { n } else { 7 };
    let mut t = 0;
    while t < n {
        let lane = if t < k { 15 - t } else { 7 - (t - k) };
        a.ctrl[lane] = (15 - t) as u8;
        a.base[lane] = b'0';
        a.lim[lane] = 9;
        a.w1[lane] = if lane % 2 == 0 { 10 } else { 1 };
        t += 1;
    }
    b.w2 = [100, 1, 100, 1, 100, 1, 100, 1];
    b.w3 = [10_000, 1, 10_000, 1, 0, 0, 0, 0];
    b.limv = [i32::MAX; 4];
    c.me = 10_000_000;
    c.mf = [1, 1];
    (a, b, c)
}
const fn ts_f3q_l() -> (TsA, TsB, TsC) {
    let (mut a, mut b, mut c) = (ZLANE, TSB_INVALID, TSC_INVALID);
    a.ctrl[0] = 1;
    a.base[0] = b'"';
    let w = [0i8, 1, 10, 1, 10, 1, 10, 1, 0, 1, 10, 1, 10, 1, 1, 0];
    let mut i = 1;
    while i < 15 {
        if i != 8 {
            a.ctrl[i] = (if i < 8 { i + 1 } else { i }) as u8;
            a.base[i] = b'0';
            a.lim[i] = 9;
            a.w1[i] = w[i];
        }
        i += 1;
    }
    a.base[8] = 0xFF;
    a.lim[8] = 1;
    a.ctrl[15] = 15;
    a.base[15] = b'"';
    b.w2 = [100, 1, 100, 1, 100, 1, 10, 1];
    b.w3 = [10_000, 1, 1000, 1, 0, 0, 0, 0];
    b.limv = [i32::MAX; 4];
    c.me = 1_000_000;
    c.mf = [1, 1];
    (a, b, c)
}
const fn ts_rows_l() -> ([TsA; 64], [TsB; 64], [TsC; 64]) {
    let mut ta = [TSA_INVALID; 64];
    let mut tb = [TSB_INVALID; 64];
    let mut tc = [TSC_INVALID; 64];
    let mut n = 1;
    while n <= 14 {
        let (a, b, c) = ts_f3_l(n);
        ta[n] = a;
        tb[n] = b;
        tc[n] = c;
        n += 1;
    }
    let (a, b, c) = ts_f3q_l();
    ta[15] = a;
    tb[15] = b;
    tc[15] = c;
    let mut k = 0;
    while k < 4 {
        let (f2, q) = (k & 1 == 1, k & 2 == 2);
        let (a, b, c) = if f2 { ts_f2_l(q) } else { ts_f12(false, q) };
        let raw = 20 + 5 * f2 as usize + 2 * q as usize;
        ta[raw] = a;
        tb[raw] = b;
        tc[raw] = c;
        k += 1;
    }
    (ta, tb, tc)
}

const fn tj_rows() -> ([TjA; 128], [TjB; 128], [TjC; 128], [TjD; 128], [TjE; 128], [TjF; 128]) {
    tj_rows_g(ts_rows(), false)
}
const fn tj_rows_g(r: ([TsA; 64], [TsB; 64], [TsC; 64]), lp: bool) -> ([TjA; 128], [TjB; 128], [TjC; 128], [TjD; 128], [TjE; 128], [TjF; 128]) {
    let (ta, tb, tc) = r;
    let mut ja = [TjA { ctrl: [0; 32], base: [0; 32] }; 128];
    let mut jb = [TjB { lim: [0; 32], w1: [0; 32] }; 128];
    let mut jc = [TjC { w2: [0; 16], w3: [0; 16] }; 128];
    let mut jd = [TjD { limv: [0; 8], kmask2: [0; 16], kmask3: [0; 16] }; 128];
    let mut je = [TjE { kme: [0; 4], kmf: [0; 4] }; 128];
    let mut jf = [TjF { ksel: [0; 32], koff1: 0, pmask: 0, pmask3: DATE_PEXT, koffe: 0 }; 128];
    let mut j = 0;
    while j < 128 {
        let rows = [j / 2, 20 + 2 * (j % 2)];
        let mut lane = 0;
        while lane < 2 {
            let (a, b) = (&ta[rows[lane]], &tb[rows[lane]]);
            let mut i = 0;
            while i < 16 {
                ja[j].ctrl[lane * 16 + i] = a.ctrl[i];
                ja[j].base[lane * 16 + i] = a.base[i];
                jb[j].lim[lane * 16 + i] = a.lim[i];
                jb[j].w1[lane * 16 + i] = a.w1[i];
                i += 1;
            }
            let mut i = 0;
            while i < 8 {
                jc[j].w2[lane * 8 + i] = b.w2[i];
                i += 1;
            }
            let mut i = 0;
            while i < 4 {
                if lane == 0 {
                    jc[j].w3[4 + i] = b.w3[i];
                } else if i < 2 {
                    jc[j].w3[8 + i] = b.w3[i];
                }
                i += 1;
            }
            let mut i = 0;
            while i < 4 {
                jd[j].limv[lane * 4 + i] = b.limv[i];
                i += 1;
            }
            lane += 1;
        }
        jd[j].kmask2 = tb[rows[0]].kmaskv;
        jd[j].kmask3 = tb[rows[1]].kmaskv;
        let c = &tc[rows[0]];
        let f2 = c.mf[0] == 60_000;
        je[j] = TjE { kme: [0, c.me as i64, 1000, 0], kmf: [0, if lp { if f2 { 60_000 } else { c.mf[0] as i64 } } else if f2 { 60_000 } else { -(c.mf[0] as i64) }, 0, 0] };
        if f2 {
            jf[j].ksel[8] = 2;
        }
        jf[j].koff1 = c.koff as i64 + 1;
        jf[j].koffe = c.koff as i64 - (j / 2) as i64;
        jf[j].pmask = if c.dmask != 0 { DATE_PEXT } else { 0 };
        j += 1;
    }
    (ja, jb, jc, jd, je, jf)
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow {
    cand: [[u8; 16]; 2],
    maskv: [u8; 16],
    k: [u32; 2],
}

const fn kind_row(c0: &[u8], k0: u32, c1: &[u8], k1: u32) -> KindRow {
    let mut r = KindRow { cand: [[0; 16]; 2], maskv: [0; 16], k: [k0, k1] };
    let mut i = 0;
    while i < c0.len() {
        r.cand[0][i] = c0[i];
        r.cand[1][i] = c1[i];
        r.maskv[i] = 0xFF;
        i += 1;
    }
    r
}

const fn kind_rows() -> [KindRow; 64] {
    let mut t = [KindRow { cand: [[b'\n'; 16]; 2], maskv: [0xFF; 16], k: [3, 3] }; 64];
    t[4] = kind_row(b"cart", 1, b"view", 0);
    t[6] = kind_row(b"\"cart\"", 1, b"\"view\"", 0);
    t[8] = kind_row(b"purchase", 2, b"purchase", 2);
    t[10] = kind_row(b"\"purchase\"", 2, b"\"purchase\"", 2);
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow2 {
    cand: [u8; 16],
    maskv: [u8; 16],
    pp: u64,
    _pad: [u64; 3],
}

const fn kind_rows2() -> [KindRow2; 128] {
    let k = kind_rows();
    let mut t = [KindRow2 { cand: [b'\n'; 16], maskv: [0xFF; 16], pp: 0, _pad: [0; 3] }; 128];
    let mut i = 0;
    while i < 128 {
        let (raw, f) = (i / 2, i % 2);
        t[i].cand = k[raw].cand[f];
        t[i].maskv = k[raw].maskv;
        t[i].pp = (k[raw].k[f] as u64) << 56 | (k[raw].k[f] == 2) as u64;
        i += 1;
    }
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TailRow {
    sel: u32,
    m: u32,
    pat: [u32; 4],
    pp: [u64; 4],
    adj: i64,
}
const TAIL_NO: u32 = u32::MAX;
const TAIL_K: usize = 72;
const TAIL_ROWS: usize = 63 * TAIL_K + 64;

const fn tail_up(x: u32, sh: usize) -> u32 {
    if x == TAIL_NO { TAIL_NO } else { x << sh }
}
const fn tail_rows() -> [TailRow; TAIL_ROWS] {
    let mut t = [TailRow { sel: 0, m: 0, pat: [TAIL_NO; 4], pp: [PP_SOME; 4], adj: 0 }; TAIL_ROWS];
    let mut i = 0;
    while i < TAIL_ROWS {
        t[i].adj = (i / TAIL_K + i % TAIL_K) as i64 - 32;
        i += 1;
    }
    let mut r5 = 0;
    while r5 < 64 {
        let mut r6 = 0;
        while r6 < 64 {
            if r5 + 1 + r6 <= 31 {
                let p5 = if r5 >= 2 { 1 | 1u32 << (r5 - 1) } else { TAIL_NO };
                let p6 = if r6 >= 2 { (1 | 1u32 << (r6 - 1)) << (r5 + 1) } else { TAIL_NO };
                let both = if r5 >= 2 && r6 >= 2 { p5 | p6 } else { TAIL_NO };
                let (e0, e1) = (if r5 == 0 { PP_NONE } else { PP_SOME }, if r5 == 2 { PP_NONE } else { PP_SOME });
                let sh = 32 - (r5 + 1 + r6);
                t[r5 * TAIL_K + r6] = TailRow { sel: (1 | 1 << (r5 + 1)) << sh, m: ((1u32 << (r5 + 1 + r6)) - 1) << sh, pat: [0, tail_up(p5, sh), tail_up(p6, sh), tail_up(both, sh)], pp: [e0, e1, e0, e1], adj: (r5 + r6) as i64 - 32 };
            }
            r6 += 1;
        }
        r5 += 1;
    }
    t
}

#[repr(C, align(64))]
struct Tables {
    tsa: [TsA; 64],
    tsb: [TsB; 64],
    tsc: [TsC; 64],
    tj: [TjRow; 128],
    kind: [KindRow; 64],
    kind2: [KindRow2; 128],
    tail: [TailRow; TAIL_ROWS],
    eid: [IdShape; 128],
    kid: [IdShape; 128],
    uid: [IdShape; 128],
    eid1: [IdShape; 64],
    jid: [JidRow; JID_ROWS],
    kid1: [IdShape; 64],
    uid1: [IdShape; 64],
    prod: [DecShape; 128],
    camp: [DecShape; 128],
    camp4: [DecShape; 128],
}

static T: Tables = Tables {
    tsa: ts_rows().0,
    tsb: ts_rows().1,
    tsc: ts_rows().2,
    tj: tj_table(),
    kind: kind_rows(),
    kind2: kind_rows2(),
    tail: tail_rows(),
    eid: id_shapes(b'e', 0, 8, false),
    kid: id_shapes(b'k', 0, 8, false),
    uid: id_shapes(b'u', 8, 0, true),
    eid1: id_shapes_raw(b'e', 0, 8, false),
    jid: jid_rows(),
    kid1: id_shapes_raw(b'k', 0, 8, false),
    uid1: id_shapes_raw(b'u', 8, 0, true),
    prod: dec_shapes(b"p"),
    camp: dec_shapes(b"c_"),
    camp4: camp4_shapes(),
};

#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct DateEnt {
    key: [u8; 12],
    strikes: u32,
    ms: [i64; 2],
}
const DATE_PEXT: u64 = 0x0F03;

const _: () = assert!(SCRATCH * 8 == 64 * core::mem::size_of::<DateEnt>() && SCRATCH_INIT == u64::from_le_bytes([b','; 8]));
const _: () = assert!(core::mem::offset_of!(EvBatch, scratch) % 32 == 0 && core::mem::offset_of!(CkBatch, scratch) % 32 == 0);
const _: () = assert!(core::mem::offset_of!(DateEnt, ms) == 16 && SCRATCH_ZERO.start == 2 && SCRATCH_ZERO.end == 4);

#[inline(always)]
fn date_idx(dd: u16) -> usize {
    unsafe { _pext_u64(dd as u64, DATE_PEXT) as usize }
}

#[cold]
#[inline(never)]
unsafe fn date_miss(kp: *const u8, quoted: bool, cache: *mut DateEnt) -> Option<i64> {
    count(14);
    unsafe {
        let k = core::slice::from_raw_parts(kp, 12);
        if quoted && k[0] != b'"' {
            return None;
        }
        let dig = |i: usize| -> Option<i64> { if k[i].is_ascii_digit() { Some((k[i] - b'0') as i64) } else { None } };
        if k[5] != b'-' || k[8] != b'-' || k[11] != b'T' {
            return None;
        }
        let (Some(y0), Some(y1), Some(y2), Some(y3), Some(m0), Some(m1), Some(d0), Some(d1)) =
            (dig(1), dig(2), dig(3), dig(4), dig(6), dig(7), dig(9), dig(10))
        else {
            return None;
        };
        let (y, m, d) = (y0 * 1000 + y1 * 100 + y2 * 10 + y3, m0 * 10 + m1, d0 * 10 + d1);
        if !(1..=12).contains(&m) || d < 1 || d > slow::days_in_month(y, m) {
            return None;
        }
        let ms = slow::days_from_civil(y, m, d) * 86_400_000;
        let ent = &mut *cache.add(date_idx(u16::from_le_bytes([k[9], k[10]])));
        if ent.key[1] == b',' || ent.strikes >= 3 {
            ent.key[0] = b'"';
            ent.key[1..12].copy_from_slice(&k[1..12]);
            ent.ms = [ms; 2];
            ent.strikes = 0;
        } else if ent.key[1..12] != k[1..12] {
            ent.strikes += 1;
        }
        Some(ms)
    }
}

#[inline(always)]
unsafe fn ld(p: *const u8) -> __m128i {
    unsafe { _mm_loadu_si128(p as *const __m128i) }
}
#[inline(always)]
unsafe fn rd16(p: *const u8) -> u16 {
    unsafe { core::ptr::read_unaligned(p as *const u16) }
}
#[inline(always)]
unsafe fn rd64(p: *const u8) -> u64 {
    unsafe { core::ptr::read_unaligned(p as *const u64) }
}
#[inline(always)]
unsafe fn store_hi<const OFF: usize>(p: *mut i64, v: __m128i) {
    unsafe { core::arch::asm!("vmovhps qword ptr [{p} + {off}], {v}", p = in(reg) p, off = const OFF, v = in(xmm_reg) v, options(nostack, preserves_flags)) };
}

#[inline(always)]
fn tz(mut x: u64) -> u64 {
    unsafe { core::arch::asm!("tzcnt {x}, {x}", x = inout(reg) x, options(pure, nomem, nostack)) };
    x
}
#[inline(always)]
fn tz2(x: u64) -> u64 {
    let o: u64;
    unsafe { core::arch::asm!("tzcnt {o}, {x}", o = lateout(reg) o, x = in(reg) x, options(pure, nomem, nostack)) };
    o
}
#[inline(always)]
fn blsr(mut x: u64) -> u64 {
    unsafe { core::arch::asm!("blsr {x}, {x}", x = inout(reg) x, options(pure, nomem, nostack)) };
    x
}

#[inline(always)]
fn bzhi(a: u64, i: u64) -> u64 {
    let o: u64;
    unsafe { core::arch::asm!("bzhi {o}, {a}, {i}", o = lateout(reg) o, a = in(reg) a, i = in(reg) i, options(pure, nomem, nostack)) };
    o
}

#[inline(always)]
fn lea3<const D: i32>(a: u64, b: u64) -> u64 {
    let o: u64;
    unsafe { core::arch::asm!("lea {o}, [{a} + {b} + {d}]", o = lateout(reg) o, a = in(reg) a, b = in(reg) b, d = const D, options(pure, nomem, nostack, preserves_flags)) };
    o
}
#[inline(always)]
unsafe fn ld_xor(a: *const u8, m: *const u8) -> __m128i {
    let o: __m128i;
    unsafe { core::arch::asm!("vmovdqu {o}, xmmword ptr [{a}]", "vpxor {o}, {o}, xmmword ptr [{m}]", o = out(xmm_reg) o, a = in(reg) a, m = in(reg) m, options(pure, readonly, nostack, preserves_flags)) };
    o
}
#[inline(always)]
unsafe fn and_m(v: __m128i, m: *const u8) -> __m128i {
    let o: __m128i;
    unsafe { core::arch::asm!("vpand {o}, {v}, xmmword ptr [{m}]", o = lateout(xmm_reg) o, v = in(xmm_reg) v, m = in(reg) m, options(pure, readonly, nostack, preserves_flags)) };
    o
}
#[inline(always)]
fn jid_off(c0: u64, c1: u64) -> u64 {
    let o: u64;
    unsafe { core::arch::asm!("imul {o}, {a}, 63", "add {o}, {b}", "shl {o}, 7", o = lateout(reg) o, a = in(reg) c0, b = in(reg) c1, options(pure, nomem, nostack)) };
    o
}
#[inline(always)]
fn add_sub<const D: i32>(a: u64, b: u64) -> u64 {
    let o: u64;
    unsafe { core::arch::asm!("lea {o}, [{a} + {d}]", "sub {o}, {b}", o = lateout(reg) o, a = in(reg) a, b = in(reg) b, d = const D, options(pure, nomem, nostack)) };
    o
}
#[inline(always)]
unsafe fn xor_lo(y: __m256i, a: *const u8, b: usize) -> __m128i {
    let o: __m128i;
    unsafe { core::arch::asm!("vpxor {o}, {y:x}, xmmword ptr [{a} + {b}]", o = lateout(xmm_reg) o, y = in(ymm_reg) y, a = in(reg) a, b = in(reg) b, options(pure, readonly, nostack, preserves_flags)) };
    o
}

#[inline(always)]
fn nz(v: __m128i) -> bool {
    unsafe { _mm_cvtsi128_si64(_mm_or_si128(v, _mm_unpackhi_epi64(v, v))) != 0 }
}

struct Masks {
    c: u64,
    q: u64,
    n: u64,
}

#[inline(always)]
unsafe fn masks(p: *const u8) -> Masks {
    unsafe { Masks { c: eq64(p, b','), q: eq64(p, b'"'), n: eq64(p, b'\n') } }
}

#[inline(always)]
unsafe fn eq64(p: *const u8, c: u8) -> u64 {
    let m: u64;
    unsafe {
        core::arch::asm!(
            "vpcmpeqb {a}, {c}, ymmword ptr [{p}]",
            "vpcmpeqb {b}, {c}, ymmword ptr [{p} + 32]",
            "kunpckdq {a}, {b}, {a}",
            a = out(kreg) m, b = out(kreg) _, c = in(ymm_reg) _mm256_set1_epi8(c as i8), p = in(reg) p,
            options(pure, readonly, nostack, preserves_flags)
        )
    };
    m
}

#[inline(always)]
fn hi32(v: __m256i, c: u8) -> u64 {
    unsafe { (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 as u64) << 32 }
}

#[inline(always)]
unsafe fn ts_date(rs: *const u8, raw: usize, cache: *mut DateEnt) -> (i64, __m128i, *const u8) {
    unsafe {
        let (b, c) = (T.tsb.get_unchecked(raw), T.tsc.get_unchecked(raw));
        let kp = rs.wrapping_offset(c.koff as isize);
        let ent = cache.add(date_idx(rd16(kp.wrapping_add(9))));
        let keybad = _mm_and_si128(_mm_xor_si128(ld(kp), ld(ent as *const u8)), ld(b.kmaskv.as_ptr()));
        ((*ent).ms[0], keybad, kp)
    }
}

#[inline(always)]
unsafe fn ts_time<const FULL: bool>(rend: *const u8, raw: usize, day_ms: i64) -> (i64, __m128i) {
    unsafe {
        let (a, b, c) = (T.tsa.get_unchecked(raw), T.tsb.get_unchecked(raw), T.tsc.get_unchecked(raw));
        let lane = _mm_shuffle_epi8(ld(rend.wrapping_sub(16)), ld(a.ctrl.as_ptr()));
        let d = _mm_sub_epi8(lane, ld(a.base.as_ptr()));
        let m1 = _mm_maddubs_epi16(d, ld(a.w1.as_ptr() as *const u8));
        let m2 = _mm_madd_epi16(m1, ld(b.w2.as_ptr() as *const u8));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), ld(b.w3.as_ptr() as *const u8));
        let bad = _mm_or_si128(_mm_subs_epu8(d, ld(a.lim.as_ptr())), _mm_cmpgt_epi32(m2, ld(b.limv.as_ptr() as *const u8)));
        let r = _mm_cvtsi128_si64(m3) as u64;
        let val = if FULL {
            let s = *rend.wrapping_offset(c.soff as isize);
            let mf = *c.mf.get_unchecked((s >> 1 & 1) as usize);
            (day_ms & c.dmask as i64) + (r as u32) as i64 * c.me as i64 + (r >> 32) as i64 * mf as i64
        } else {
            day_ms + (r as u32) as i64 * 1000
        };
        (val, bad)
    }
}

#[inline(always)]
unsafe fn ids2(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> (u64, u64, __m128i) {
    unsafe {
        let (ids, bad) = ids2v(xa, sa, xb, sb);
        (_mm_cvtsi128_si64(ids) as u64, _mm_extract_epi64::<1>(ids) as u64, bad)
    }
}

#[inline(always)]
unsafe fn ids2v(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> (__m128i, __m128i) {
    unsafe {
        let va = _mm_sub_epi8(_mm_shuffle_epi8(xa, ld(sa.ctrl.as_ptr())), ld(sa.subv.as_ptr()));
        let vb = _mm_sub_epi8(_mm_shuffle_epi8(xb, ld(sb.ctrl.as_ptr())), ld(sb.subv.as_ptr()));
        let d = _mm_blend_epi32::<0b1100>(va, vb);
        let chk = _mm_blend_epi32::<0b1100>(vb, va);
        let e = _mm_sub_epi8(d, _mm_set1_epi8(49));
        let notdig = _mm_subs_epu8(d, _mm_set1_epi8(9));
        let notlet = _mm_subs_epu8(e, _mm_set1_epi8(25));
        let val = _mm_min_epu8(d, _mm_sub_epi8(d, _mm_set1_epi8(39)));
        let lz = _mm_subs_epu8(_mm_or_si128(ld(sa.lz.as_ptr()), ld(sb.lz.as_ptr())), val);
        let bad = _mm_or_si128(_mm_or_si128(chk, lz), _mm_min_epu8(notdig, notlet));
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510));
        let ids = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        (_mm_or_si128(ids, ld(sb.orv.as_ptr() as *const u8)), bad)
    }
}

#[inline(always)]
unsafe fn ids2j(xa: __m128i, xb: __m128i, r: &JidRow) -> (__m256i, __m256i) {
    unsafe {
        let ld2 = |q: *const u8| _mm256_load_si256(q as *const __m256i);
        let x = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(xa), xb);
        let v = _mm256_sub_epi8(_mm256_shuffle_epi8(x, ld2(r.ctrl.as_ptr())), ld2(r.subv.as_ptr()));
        let e = _mm256_sub_epi8(v, _mm256_set1_epi8(49));
        let notdig = _mm256_subs_epu8(v, _mm256_set1_epi8(9));
        let notlet = _mm256_subs_epu8(e, _mm256_set1_epi8(25));
        let val = _mm256_min_epu8(v, _mm256_sub_epi8(v, _mm256_set1_epi8(39)));
        let lz = _mm256_subs_epu8(ld2(r.lz.as_ptr()), val);
        let cls = _mm256_ternarylogic_epi64::<0xF8>(_mm256_min_epu8(notdig, notlet), v, _mm256_set_epi64x(-1, 0, -1, 0));
        let m1 = _mm256_maddubs_epi16(val, _mm256_set1_epi16(0x0124));
        let m2 = _mm256_madd_epi16(m1, _mm256_set1_epi32(0x0001_0510));
        let hi: __m256i;
        core::arch::asm!("vpmuludq {o:y}, {a:y}, {b:y}", o = lateout(zmm_reg) hi, a = in(zmm_reg) m2, b = in(zmm_reg) _mm256_set1_epi64x(1_679_616), options(pure, nomem, nostack, preserves_flags));
        let ids = _mm256_add_epi64(hi, _mm256_srli_epi64::<32>(m2));
        (_mm256_or_si256(ids, ld2(r.orv.as_ptr() as *const u8)), _mm256_or_si256(cls, lz))
    }
}

#[inline(always)]
unsafe fn dec(x: __m128i, s: &DecShape) -> (u32, __m128i) {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let lz = _mm_subs_epu8(ld(s.lz.as_ptr()), v);
        let bad = _mm_or_si128(lz, _mm_subs_epu8(v, _mm_set_epi64x(0, 0x0909_0909_0909_0909)));
        let m1 = _mm_maddubs_epi16(v, _mm_set_epi64x(0, 0x010A_010A_010A_010A));
        let m2 = _mm_madd_epi16(m1, _mm_set_epi64x(0, 0x0001_0064_0001_0064));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), _mm_set_epi64x(0, 0x0001_2710));
        (_mm_cvtsi128_si32(m3) as u32, bad)
    }
}

#[inline(always)]
unsafe fn dec4(x: __m128i, s: &DecShape) -> (__m128i, __m128i) {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let lz = _mm_subs_epu8(ld(s.lz.as_ptr()), v);
        let bad = _mm_or_si128(lz, _mm_subs_epu8(v, _mm_set_epi64x(0, 0x0909_0909)));
        let m1 = _mm_maddubs_epi16(v, _mm_set_epi64x(0, 0x010A_010A));
        (_mm_madd_epi16(m1, _mm_set_epi64x(0, 0x0001_0064)), bad)
    }
}

#[inline(always)]
unsafe fn kind_of(rs: *const u8, raw: usize) -> (u32, __m128i) {
    unsafe {
        let kr = T.kind.get_unchecked(raw);
        let f = (*rs.wrapping_add(3) & 1) as usize;
        let bad = _mm_and_si128(_mm_xor_si128(ld(rs), ld(kr.cand.get_unchecked(f).as_ptr())), ld(kr.maskv.as_ptr()));
        (*kr.k.get_unchecked(f), bad)
    }
}

#[inline(always)]
unsafe fn price(end: *const u8, len: usize) -> Option<u32> {
    unsafe {
        if len == 0 {
            return Some(0);
        }
        if len > 8 {
            return None;
        }
        let keep = !0u64 << (8 * (8 - len));
        let y = (rd64(end.wrapping_sub(8)) ^ 0x3030_3030_3030_3030) & keep;
        if (y | y.wrapping_add(0x0606_0606_0606_0606)) & 0xF0F0_F0F0_F0F0_F0F0 != 0 {
            return Some(0);
        }
        let y = y.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
        let y = y.wrapping_mul(6_553_601) >> 16 & 0x0000_FFFF_0000_FFFF;
        let v = (y.wrapping_mul(42_949_672_960_001) >> 32) as u32;
        Some(if v.wrapping_sub(1) < 10_000_000 { v } else { 0 })
    }
}

#[cold]
#[inline(never)]
unsafe fn cold_ids(base: *const u8, len: usize, a0: usize, b0: usize, prefix: u8, a1: usize, b1: usize) -> Option<(u64, u64, [u64; 4])> {
    count(2);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (Some(v0), Some(v1)) = (slow::field_value(buf, a0, b0), slow::field_value(buf, a1, b1)) else {
        count(3);
        return None;
    };
    if v0.0 == v0.1 {
        return None;
    }
    let (a, ah, al) = slow::parse_idk(buf, v0, prefix);
    let (b, bh, bl) = slow::parse_idk(buf, v1, b'u');
    Some((a, b, [ah, al, bh, bl]))
}

#[cold]
#[inline(never)]
unsafe fn cold_ts(base: *const u8, len: usize, a: usize, b: usize, f1_only: bool) -> Option<i64> {
    count(if f1_only { 6 } else { 4 });
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let Some((s, e)) = slow::field_value(buf, a, b) else {
        count(5);
        return None;
    };
    Some(if f1_only { slow::parse_f1(&buf[s..e]) } else { slow::parse_ts(&buf[s..e]) })
}

#[cold]
#[inline(never)]
unsafe fn cold_kind(base: *const u8, len: usize, a: usize, b: usize) -> Option<u32> {
    count(7);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (s, e) = slow::field_value(buf, a, b)?;
    Some(slow::parse_kind(&buf[s..e]) as u32)
}

#[cold]
#[inline(never)]
unsafe fn cold_dec(base: *const u8, len: usize, a: usize, b: usize, product: bool) -> Option<u32> {
    count(8);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (s, e) = slow::field_value(buf, a, b)?;
    Some(if product { slow::parse_product(&buf[s..e]) } else { slow::parse_campaign(&buf[s..e]) })
}

const FAIL: usize = usize::MAX;

const OB: usize = 40;

#[inline(always)]
unsafe fn ts_careful<const FULL: bool>(rs: *const u8, raw: usize, cache: *mut DateEnt) -> Option<i64> {
    unsafe {
        let (mut day, keybad, kp) = ts_date(rs, raw, cache);
        if nz(keybad) {
            day = date_miss(kp, T.tsb.get_unchecked(raw).kmaskv[0] != 0, cache)?;
        }
        let (v, bad) = ts_time::<FULL>(rs.wrapping_add(raw), raw, day);
        if nz(bad) { None } else { Some(v) }
    }
}

macro_rules! or_cold {
    ($fast:expr, $cold:expr) => {
        match $fast {
            Some(v) => v,
            None => match $cold {
                Some(v) => v,
                None => return FAIL,
            },
        }
    };
}

#[inline(always)]
unsafe fn ev_core<S: EventSink, const CHAIN: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let lb = l + OB;
        let pb = p.wrapping_add(OB);
        let w1 = masks(p);
        let wb = masks(pb);
        let c0 = _tzcnt_u64(w1.c);
        let t = _blsr_u64(w1.c);
        let c1 = _tzcnt_u64(t);
        let c2 = _tzcnt_u64(_blsr_u64(t));
        let m = if CHAIN { _tzcnt_u64(wb.n) } else { e_in.wrapping_sub(lb) as u64 };
        let tb = _bzhi_u64(wb.c, m as u32);
        let c5 = 63 ^ _lzcnt_u64(tb);
        let t = _bzhi_u64(tb, c5 as u32);
        let c4 = 63 ^ _lzcnt_u64(t);
        let c3 = 63 ^ _lzcnt_u64(_bzhi_u64(t, c4 as u32));
        let head = (w1.c << (64 - OB)).count_ones() as u64;
        let tail = tb.count_ones() as u64;
        let nl_head = if CHAIN { w1.n << (64 - OB) } else { 0 };
        if (m > 63) | (nl_head != 0) | (head + tail != 6) | (tail < 3) | (c2 > 62) {
            return FAIL;
        }
        let (c0, c1, c2, c3, c4, c5, m) = (c0 as usize, c1 as usize, c2 as usize, c3 as usize, c4 as usize, c5 as usize, m as usize);
        let e = lb + m;
        let raw6 = m - c5 - 1;
        let pq = _bzhi_u64(wb.q >> (c5 + 1), raw6 as u32);
        let qb = pq & 1;
        if (CHAIN && e >= len) | (pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1))) {
            return FAIL;
        }
        let (rs2, raw2) = (p.wrapping_add(c1 + 1), c2 - c1 - 1);
        let (rs3, raw3) = (p.wrapping_add(c2 + 1), (OB + c3).wrapping_sub(c2 + 1));
        let row3 = 20 + 2 * (w1.q >> (c2 + 1) & 1) as usize;
        let (rs4, raw4) = (pb.wrapping_add(c3 + 1), c4 - c3 - 1);
        let se = T.eid.get_unchecked(c0 * 2 + (w1.q & 1) as usize);
        let su = T.uid.get_unchecked((c1 - c0 - 1) * 2 + (w1.q >> (c0 + 1) & 1) as usize);
        let sp = T.prod.get_unchecked((c5 - c4 - 1) * 2 + (wb.q >> (c4 + 1) & 1) as usize);
        let xe = ld(p.wrapping_add(c0).wrapping_sub(16));
        let xu = ld(p.wrapping_add(c1).wrapping_sub(16));
        let xp = ld(pb.wrapping_add(c5).wrapping_sub(16));

        let (eid, uid, kx, ts, ingest, kind, product);
        {
            count(0);
            (eid, uid, kx) = {
                let (a, b, bad) = ids2(xe, se, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b, [0; 4])) }, cold_ids(base, len, l, l + c0, b'e', l + c0 + 1, l + c1))
            };
            ts = or_cold!(ts_careful::<true>(rs2, raw2, cache), cold_ts(base, len, l + c1 + 1, l + c2, false));
            ingest = or_cold!(
                if raw3 == row3 { ts_careful::<false>(rs3, row3, cache) } else { None },
                cold_ts(base, len, l + c2 + 1, lb + c3, true)
            );
            kind = {
                let (k, bad) = kind_of(rs4, raw4);
                or_cold!(if nz(bad) { None } else { Some(k) }, cold_kind(base, len, lb + c3 + 1, lb + c4))
            };
            product = {
                let (v, bad) = dec(xp, sp);
                or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, lb + c4 + 1, lb + c5, true))
            };
        }
        let mut pr = 0;
        if kind == 2 {
            match price(pb.wrapping_add(m - qb as usize), raw6 - 2 * qb as usize) {
                Some(v) => pr = v,
                None => return FAIL,
            }
        }
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr, eh: kx[0], el: kx[1], uh: kx[2], ul: kx[3] });
        e
    }
}

#[inline(always)]
pub(crate) unsafe fn hex32(p: *const u8) -> Option<(u64, u64)> {
    unsafe {
        let x = _mm256_loadu_si256(p as *const __m256i);
        let d = _mm256_sub_epi8(x, _mm256_set1_epi8(b'0' as i8));
        let e = _mm256_sub_epi8(x, _mm256_set1_epi8(b'a' as i8));
        let bad = _mm256_min_epu8(_mm256_subs_epu8(d, _mm256_set1_epi8(9)), _mm256_subs_epu8(e, _mm256_set1_epi8(5)));
        if _mm256_testz_si256(bad, bad) == 0 {
            return None;
        }
        let val = _mm256_min_epu8(d, _mm256_add_epi8(e, _mm256_set1_epi8(10)));
        let m = _mm256_maddubs_epi16(val, _mm256_set1_epi16(0x0110));
        let w = _mm256_packus_epi16(m, m);
        let hi = (_mm256_extract_epi64::<0>(w) as u64).swap_bytes();
        let lo = (_mm256_extract_epi64::<2>(w) as u64).swap_bytes();
        Some((hi, lo))
    }
}

#[inline(always)]
unsafe fn id1<const HI: bool>(x: __m128i, s: &IdShape) -> (u64, __m128i) {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let e = _mm_sub_epi8(v, _mm_set1_epi8(49));
        let mn = _mm_min_epu8(_mm_subs_epu8(v, _mm_set1_epi8(9)), _mm_subs_epu8(e, _mm_set1_epi8(25)));
        let cls = if HI { _mm_blend_epi32::<0b1100>(v, mn) } else { _mm_blend_epi32::<0b1100>(mn, v) };
        let val = _mm_min_epu8(v, _mm_sub_epi8(v, _mm_set1_epi8(39)));
        let lz = _mm_subs_epu8(ld(s.lz.as_ptr()), val);
        let val = if HI { _mm_unpackhi_epi64(val, val) } else { val };
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510));
        let id = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        let orv = if HI { s.orv[1] } else { s.orv[0] };
        ((_mm_cvtsi128_si64(id) as u64) | orv, _mm_or_si128(cls, lz))
    }
}

#[inline(always)]
unsafe fn id_any(p: *const u8, s: usize, raw: usize, q: usize, shapes: &[IdShape; 128], prefix: u8) -> Option<(u64, u64, u64)> {
    unsafe {
        let f = p.add(s);
        if raw <= 11 {
            let x = ld(f.wrapping_add(raw).wrapping_sub(16));
            let sh = shapes.get_unchecked(raw * 2 + q);
            let (v, bad) = if prefix == b'u' { id1::<true>(x, sh) } else { id1::<false>(x, sh) };
            if nz(bad) { None } else { Some((v, 0, 0)) }
        } else if raw == 33 && *f == prefix {
            let (hi, lo) = hex32(f.add(1))?;
            Some((long_word(hi, lo), hi, lo))
        } else if raw == 35 && *f == b'"' && *f.add(1) == prefix && *f.add(34) == b'"' {
            let (hi, lo) = hex32(f.add(2))?;
            Some((long_word(hi, lo), hi, lo))
        } else {
            None
        }
    }
}

#[inline(always)]
unsafe fn ids_long(base: *const u8, len: usize, l: usize, c0: usize, c1: usize, qm: u128, prefix: u8, se: &[IdShape; 128]) -> Option<(u64, u64, [u64; 4])> {
    unsafe {
        let p = base.add(l);
        let raw1 = c1 - c0 - 1;
        let a = id_any(p, 0, c0, (qm & 1) as usize, se, prefix);
        let b = id_any(p, c0 + 1, raw1, (qm >> (c0 + 1) & 1) as usize, &T.uid, b'u');
        match (a, b) {
            (Some((a, ah, al)), Some((b, bh, bl))) => Some((a, b, [ah, al, bh, bl])),
            _ => cold_ids(base, len, l, l + c0, prefix, l + c0 + 1, l + c1),
        }
    }
}

#[inline(never)]
unsafe fn ev_long<S: EventSink>(base: *const u8, len: usize, l: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe {
        let p = base.add(l);
        let (w0, w1) = (masks(p), masks(p.add(64)));
        let nm = w0.n as u128 | (w1.n as u128) << 64;
        if nm == 0 {
            return FAIL;
        }
        let m = nm.trailing_zeros() as usize;
        let below = (1u128 << m) - 1;
        let cm = (w0.c as u128 | (w1.c as u128) << 64) & below;
        let qm = (w0.q as u128 | (w1.q as u128) << 64) & below;
        if cm.count_ones() != 6 || l + m >= len {
            return FAIL;
        }
        let mut t = cm;
        let mut c = [0usize; 6];
        for ci in c.iter_mut() {
            *ci = t.trailing_zeros() as usize;
            t &= t - 1;
        }
        let [c0, c1, c2, c3, c4, c5] = c;
        let e = l + m;
        let (raw2, raw3, raw4, raw5, raw6) = (c2 - c1 - 1, c3 - c2 - 1, c4 - c3 - 1, c5 - c4 - 1, m - c5 - 1);
        if (raw2 >= 64) | (raw4 >= 64) | (raw5 >= 64) {
            return FAIL;
        }
        let pq = (qm >> (c5 + 1)) as u64 & ((1u64 << raw6.min(63)) - 1) | if raw6 >= 64 { 1 << 63 } else { 0 };
        let qb = pq & 1;
        if pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1)) {
            return FAIL;
        }
        count(0);
        let (eid, uid, kx) = match ids_long(base, len, l, c0, c1, qm, b'e', &T.eid) {
            Some(v) => v,
            None => return FAIL,
        };
        let ts = or_cold!(ts_careful::<true>(p.add(c1 + 1), raw2, cache), cold_ts(base, len, l + c1 + 1, l + c2, false));
        let row3 = 20 + 2 * (qm >> (c2 + 1) & 1) as usize;
        let ingest = or_cold!(
            if raw3 == row3 { ts_careful::<false>(p.add(c2 + 1), row3, cache) } else { None },
            cold_ts(base, len, l + c2 + 1, l + c3, true)
        );
        let kind = {
            let (k, bad) = kind_of(p.add(c3 + 1), raw4);
            or_cold!(if nz(bad) { None } else { Some(k) }, cold_kind(base, len, l + c3 + 1, l + c4))
        };
        let product = {
            let sp = T.prod.get_unchecked(raw5 * 2 + (qm >> (c4 + 1) & 1) as usize);
            let (v, bad) = dec(ld(p.add(c5).wrapping_sub(16)), sp);
            or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, l + c4 + 1, l + c5, true))
        };
        let mut pr = 0;
        if kind == 2 {
            match price(p.add(m - qb as usize), raw6 - 2 * qb as usize) {
                Some(v) => pr = v,
                None => return FAIL,
            }
        }
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr, eh: kx[0], el: kx[1], uh: kx[2], ul: kx[3] });
        e
    }
}

#[inline(never)]
unsafe fn cl_long<S: ClickSink>(base: *const u8, len: usize, l: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe {
        let p = base.add(l);
        let (w0, w1) = (masks(p), masks(p.add(64)));
        let nm = w0.n as u128 | (w1.n as u128) << 64;
        if nm == 0 {
            return FAIL;
        }
        let m = nm.trailing_zeros() as usize;
        let below = (1u128 << m) - 1;
        let cm = (w0.c as u128 | (w1.c as u128) << 64) & below;
        let qm = (w0.q as u128 | (w1.q as u128) << 64) & below;
        if cm.count_ones() != 3 || l + m >= len {
            return FAIL;
        }
        let c0 = cm.trailing_zeros() as usize;
        let t = cm & (cm - 1);
        let c1 = t.trailing_zeros() as usize;
        let c2 = (t & (t - 1)).trailing_zeros() as usize;
        let (raw2, raw3) = (c2 - c1 - 1, m - c2 - 1);
        if (raw2 >= 64) | (raw3 >= 64) {
            return FAIL;
        }
        let e = l + m;
        count(1);
        let (cid, uid, kx) = match ids_long(base, len, l, c0, c1, qm, b'k', &T.kid) {
            Some(v) => v,
            None => return FAIL,
        };
        let campaign = {
            let sc = T.camp.get_unchecked(raw2 * 2 + (qm >> (c1 + 1) & 1) as usize);
            let (v, bad) = dec(ld(p.add(c2).wrapping_sub(16)), sc);
            or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, l + c1 + 1, l + c2, false))
        };
        let ts = or_cold!(ts_careful::<true>(p.add(c2 + 1), raw3, cache), cold_ts(base, len, l + c2 + 1, e, false));
        sink.click(&ClRow { cid, uid, campaign, ts, ch: kx[0], cl: kx[1], uh: kx[2], ul: kx[3] });
        e
    }
}

struct Cx<B, S> {
    base: *const u8,
    end: *const u8,
    b: *mut B,
    cache: *mut DateEnt,
    sink: *mut S,
}

const FB: usize = 32;

#[derive(Clone, Copy)]
struct EvMasks {
    w1c: u64,
    wbc: u64,
    wn: u64,
}

#[inline(always)]
unsafe fn ev_masks(p: *const u8) -> EvMasks {
    unsafe {
        let (w1c, wbc, wn): (u64, u64, u64);
        const _: () = assert!(FB == 32);
        core::arch::asm!(
            "vmovdqu {v1}, ymmword ptr [{p} + 32]",
            "vmovdqu {v2}, ymmword ptr [{p} + 64]",
            "vpcmpeqb {a}, {c}, ymmword ptr [{p}]",
            "vpcmpeqb {b}, {c}, {v1}",
            "vpcmpeqb {y}, {c}, {v2}",
            "kunpckdq {a}, {b}, {a}",
            "kunpckdq {y}, {y}, {b}",
            "vpcmpeqb {x}, {n}, {v1}",
            "vpcmpeqb {b}, {n}, {v2}",
            "kunpckdq {x}, {b}, {x}",
            a = out(kreg) w1c, b = out(kreg) _, x = out(kreg) wn, y = out(kreg) wbc, v1 = out(ymm_reg) _, v2 = out(ymm_reg) _,
            c = in(ymm_reg) _mm256_set1_epi8(b',' as i8), n = in(ymm_reg) _mm256_set1_epi8(b'\n' as i8), p = in(reg) p,
            options(pure, readonly, nostack, preserves_flags)
        );
        EvMasks { w1c, wbc, wn }
    }
}
impl EvMasks {
    #[inline(always)]
    fn m(&self) -> u64 {
        tz(self.wn)
    }
    #[inline(always)]
    fn next(&self, p: *const u8) -> *const u8 {
        lea3::<{ FB as i32 + 1 }>(p as u64, self.m()) as *const u8
    }
    #[inline(always)]
    fn here(self) -> EvMasks {
        EvMasks { w1c: here_k(self.w1c), wbc: here_k(self.wbc), wn: here_k(self.wn) }
    }
}

#[unsafe(naked)]
unsafe extern "C" fn keep_k() {
    core::arch::naked_asm!(
        "sub rsp, 72",
        "kmovq qword ptr [rsp], k0",
        "kmovq qword ptr [rsp + 8], k1",
        "kmovq qword ptr [rsp + 16], k2",
        "kmovq qword ptr [rsp + 24], k3",
        "kmovq qword ptr [rsp + 32], k4",
        "kmovq qword ptr [rsp + 40], k5",
        "kmovq qword ptr [rsp + 48], k6",
        "kmovq qword ptr [rsp + 56], k7",
        "vzeroupper",
        "call rax",
        "kmovq k0, qword ptr [rsp]",
        "kmovq k1, qword ptr [rsp + 8]",
        "kmovq k2, qword ptr [rsp + 16]",
        "kmovq k3, qword ptr [rsp + 24]",
        "kmovq k4, qword ptr [rsp + 32]",
        "kmovq k5, qword ptr [rsp + 40]",
        "kmovq k6, qword ptr [rsp + 48]",
        "kmovq k7, qword ptr [rsp + 56]",
        "add rsp, 72",
        "ret",
    )
}
macro_rules! kcall {
    ($f:expr, $a0:expr, $a1:expr, $a2:expr, $a3:expr) => {{
        core::hint::cold_path();
        let (r0, r1): (u64, u64);
        core::arch::asm!(
            "call {t}",
            t = sym keep_k,
            inout("rax") $f as usize as u64 => r0,
            inout("rdi") $a0 => _,
            inout("rsi") $a1 => _,
            inout("rdx") $a2 => r1,
            inout("rcx") $a3 => _,
            lateout("r8") _, lateout("r9") _, lateout("r10") _, lateout("r11") _,
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _, out("zmm12") _, out("zmm13") _, out("zmm14") _, out("zmm15") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _, out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _, out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
        );
        (r0, r1)
    }};
}
#[repr(C)]
struct Pair(u64, u64);

#[cold]
#[inline(never)]
unsafe extern "C" fn flush_ev<S: EventSink>(sink: *mut S) {
    unsafe { (*sink).ev_flush() }
}
#[cold]
#[inline(never)]
unsafe extern "C" fn flush_ck<S: ClickSink>(sink: *mut S) {
    unsafe { (*sink).ck_flush() }
}

#[cold]
#[inline(never)]
unsafe extern "C" fn ev_cold<S: EventSink>(cx: *const Cx<EvBatch, S>, p: *const u8, s: *mut i64) -> Pair {
    unsafe {
        let cx = &*cx;
        let b = cx.b;
        let (base, len) = (cx.base, cx.end.offset_from(cx.base) as usize);
        let l = p.offset_from(base) as usize;
        let sink = &mut *cx.sink;
        (*b).n = s.offset_from((*b).ts.as_ptr()) as usize;
        let mut r = FAIL;
        if l + 128 <= len {
            r = ev_long::<S>(base, len, l, cx.cache, sink);
        }
        if r == FAIL {
            r = ev_core::<S, true>(base, len, l, 0, cx.cache, sink);
        }
        let next = if r != FAIL { r + 1 } else { scan_event(core::slice::from_raw_parts(base, len), l, sink) };
        Pair(base.wrapping_add(next) as u64, (*b).ts.as_mut_ptr().add((*b).n) as u64)
    }
}

#[inline(never)]
unsafe extern "C" fn ev_lf<S: EventSink>(cx: *const Cx<EvBatch, S>, p: *const u8, s: *mut i64) -> Pair {
    unsafe {
        let cxr = &*cx;
        macro_rules! cold {
            ($n:expr) => {{
                lfc($n);
                return ev_cold::<S>(cx, p, s);
            }};
        }
        const PP: usize = core::mem::offset_of!(EvBatch, pp);
        let rl = cxr.end.offset_from(p) as usize;
        let n0 = eq64(p, b'\n');
        let m = if n0 != 0 {
            tz(n0)
        } else {
            let n1 = eq64(p.add(64), b'\n');
            if n1 != 0 {
                64 + tz(n1)
            } else {
                if rl < 128 {
                    cold!(0);
                }
                let n2 = eq64(p.add(128), b'\n');
                if n2 == 0 {
                    cold!(1);
                }
                128 + tz(n2)
            }
        } as usize;
        let pe = p.add(m);
        if pe >= cxr.end {
            cold!(2);
        }
        let c0 = tz(eq64(p, b',')) as usize;
        let c1 = c0 + 1 + tz(eq64(p.add(c0 + 1), b',')) as usize;
        let c2 = c1 + 1 + tz(eq64(p.add(c1.min(rl - 1) + 1), b',')) as usize;
        let tb = eq64(pe.wrapping_sub(64), b',');
        let z = _lzcnt_u64(tb) as usize;
        let t = tb.wrapping_shl(z as u32) << 1;
        let r5 = _lzcnt_u64(t) as usize;
        let t = t.wrapping_shl(r5 as u32) << 1;
        let r4 = _lzcnt_u64(t) as usize;
        let r6 = z;
        if (tb.count_ones() < 3) | (c2 >= m) {
            cold!(3);
        }
        let c3 = m.wrapping_sub(r6 + r5 + r4 + 3);
        let c4 = c3 + r4 + 1;
        let (raw1, raw2, raw3) = (c1 - c0 - 1, c2 - c1 - 1, c3.wrapping_sub(c2 + 1));
        let u3 = raw3.wrapping_sub(20);
        if (raw2 >= 64) | (r4 >= 64) | (r5 >= 64) | (r6 >= 64) | (u3 & !2 != 0) | (c1 >= m) {
            cold!(4);
        }
        let q0 = (*p == b'"') as usize;
        let q1 = (*p.add(c0 + 1) == b'"') as usize;
        let (eid, eh, el, uid, uh, ul);
        let mut bad = false;
        if c0 <= 11 {
            let (v, b) = id1::<false>(ld(p.add(c0).wrapping_sub(16)), T.eid.get_unchecked(c0 * 2 + q0));
            (eid, eh, el) = (v, 0, 0);
            bad |= nz(b);
        } else {
            if (c0 != 33 + 2 * q0) | (*p.add(q0) != b'e') | (*p.add(c0 - 1) != if q0 == 1 { b'"' } else { *p.add(c0 - 1) }) {
                cold!(5);
            }
            match hex32(p.add(q0 + 1)) {
                Some((h, l)) => (eid, eh, el) = (long_word(h, l), h, l),
                None => cold!(6),
            }
        }
        if raw1 <= 11 {
            let (v, b) = id1::<true>(ld(p.add(c1).wrapping_sub(16)), T.uid.get_unchecked(raw1 * 2 + q1));
            (uid, uh, ul) = (v, 0, 0);
            bad |= nz(b);
        } else {
            let f = p.add(c0 + 1);
            if (raw1 != 33 + 2 * q1) | (*f.add(q1) != b'u') | (*p.add(c1 - 1) != if q1 == 1 { b'"' } else { *p.add(c1 - 1) }) {
                cold!(7);
            }
            match hex32(f.add(q1 + 1)) {
                Some((h, l)) => (uid, uh, ul) = (long_word(h, l), h, l),
                None => cold!(8),
            }
        }
        if bad {
            cold!(9);
        }
        let tr = T.tail.get_unchecked(r5 * TAIL_K + r6);
        let q32 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(pe.wrapping_sub(32) as *const __m256i), _mm256_set1_epi8(b'"' as i8))) as u32;
        let qw = q32 & tr.m;
        let qi = _pext_u32(qw, tr.sel) as usize;
        if qw != *tr.pat.get_unchecked(qi) {
            cold!(10);
        }
        let rs4 = p.add(c3 + 1);
        let kr = T.kind2.get_unchecked(r4 * 2 + (*rs4.add(3) & 1) as usize);
        let bad_kind = _mm_and_si128(_mm_xor_si128(ld(rs4), ld(kr.cand.as_ptr())), ld(kr.maskv.as_ptr()));
        let q3 = u3 >> 1;
        let jo = (q3 + 2 * (c2 - c1)) << 9;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (ja, jb, jc, jd, je, jf) = (&tj.a, &tj.b, &tj.c, &tj.d, &tj.e, &tj.f);
        let ld2 = |q: *const u8| _mm256_load_si256(q as *const __m256i);
        let cache = cxr.cache;
        let e2 = p.add(c2);
        let (p1, ko) = (p.add(c1), jf.koff1 as usize);
        let y2 = ld2(cache.add(_pext_u64(rd16(p1.wrapping_add(ko).wrapping_add(9)) as u64, jf.pmask) as usize) as *const u8);
        let bad_k2 = _mm_and_si128(xor_lo(y2, p1, ko), ld(jd.kmask2.as_ptr()));
        let y3 = ld2(cache.add(_pext_u64(rd16(e2.wrapping_add(q3).wrapping_add(9)) as u64, jf.pmask3) as usize) as *const u8);
        let bad_k3 = _mm_and_si128(xor_lo(y3, e2, q3), ld(jd.kmask3.as_ptr()));
        let x = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(ld(e2.wrapping_sub(16))), ld(rs4.wrapping_sub(17)));
        let days = _mm256_permute2x128_si256::<0x31>(y2, y3);
        let d = _mm256_sub_epi8(_mm256_shuffle_epi8(x, ld2(ja.ctrl.as_ptr())), ld2(ja.base.as_ptr()));
        let m1 = _mm256_maddubs_epi16(d, ld2(jb.w1.as_ptr() as *const u8));
        let m2 = _mm256_madd_epi16(m1, ld2(jc.w2.as_ptr() as *const u8));
        let m3 = _mm256_madd_epi16(_mm256_packus_epi32(m2, m2), ld2(jc.w3.as_ptr() as *const u8));
        let (bad_ta, bad_tb) = (_mm256_subs_epu8(d, ld2(jb.lim.as_ptr())), _mm256_cmpgt_epi32(m2, ld2(jd.limv.as_ptr() as *const u8)));
        let t = _mm256_mul_epu32(m3, ld2(je.kme.as_ptr() as *const u8));
        let f = _mm256_mul_epi32(_mm256_srli_epi64::<32>(m3), ld2(je.kmf.as_ptr() as *const u8));
        let ksub = _mm256_testn_epi64_mask(d, ld2(jf.ksel.as_ptr()));
        let r = _mm256_mask_sub_epi64(_mm256_add_epi64(t, f), ksub, t, f);
        let r = _mm256_add_epi64(r, days);
        *(s.byte_add(PP) as *mut u64) = *tr.pp.get_unchecked(qi) | kr.pp;
        *(s.byte_add(core::mem::offset_of!(EvBatch, eid)) as *mut u64) = eid;
        *(s.byte_add(core::mem::offset_of!(EvBatch, uid)) as *mut u64) = uid;
        *(s.byte_add(core::mem::offset_of!(EvBatch, ehi)) as *mut u64) = eh;
        *(s.byte_add(core::mem::offset_of!(EvBatch, elo)) as *mut u64) = el;
        *(s.byte_add(core::mem::offset_of!(EvBatch, uhi)) as *mut u64) = uh;
        *(s.byte_add(core::mem::offset_of!(EvBatch, ulo)) as *mut u64) = ul;
        store_hi::<0>(s, _mm256_castsi256_si128(r));
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(EvBatch, ing)) as *mut __m128i, _mm256_extracti128_si256::<1>(r));
        let other = _mm256_or_si256(_mm256_zextsi128_si256(_mm_or_si128(bad_k3, bad_kind)), _mm256_and_si256(_mm256_or_si256(bad_ta, bad_tb), _mm256_set_epi64x(-1, -1, 0, 0)));
        let all = _mm256_ternarylogic_epi64::<0xFE>(bad_ta, bad_tb, _mm256_or_si256(other, _mm256_zextsi128_si256(bad_k2)));
        let np = pe.add(1);
        if _mm256_testz_si256(all, all) == 0 {
            if (_mm256_testz_si256(other, other) == 0) || !fix_slot(cxr, p, c1 + 1, c2, s) {
                cold!(11);
            }
        }
        if *(s as *const u8).add(PP + 7) & 2 != 0 && ev_purchase(p, np, s) as u8 == 0 {
            cold!(12);
        }
        let s1 = s.add(1);
        if (s1 as usize) & (BATCH_ALIGN - 1) == 0 {
            (*cxr.b).n = NB;
            (*cxr.sink).ev_flush();
            return Pair(np as u64, (*cxr.b).ts.as_mut_ptr() as u64);
        }
        Pair(np as u64, s1 as u64)
    }
}

#[cold]
#[inline(never)]
unsafe fn fix_ts(base: *const u8, len: usize, a: usize, b: usize, cache: *mut DateEnt) -> Option<i64> {
    unsafe {
        if core::slice::from_raw_parts(base.add(a), b - a).contains(&b'\n') {
            return None;
        }
        match ts_careful::<true>(base.add(a), b - a, cache) {
            Some(v) => Some(v),
            None => cold_ts(base, len, a, b, false),
        }
    }
}

#[cold]
#[inline(never)]
unsafe fn fix_slot<B, S>(cx: &Cx<B, S>, p: *const u8, a: usize, b: usize, s: *mut i64) -> bool {
    unsafe {
        let l = p.offset_from(cx.base) as usize;
        match fix_ts(cx.base, cx.end.offset_from(cx.base) as usize, l + a, l + b, cx.cache) {
            Some(v) => {
                *s = v;
                true
            }
            None => false,
        }
    }
}

#[derive(Clone, Copy)]
struct EvSt {
    jo: usize,
    x: __m256i,
    days: __m256i,
    bad2: __m256i,
    bad: __m256i,
}

#[inline(always)]
unsafe fn ev_s<S: EventSink, const TAIL: bool>(cx: &Cx<EvBatch, S>, p: *const u8, k: EvMasks, s: *mut i64) -> Option<EvSt> {
    unsafe {
        const PP: usize = core::mem::offset_of!(EvBatch, pp);
        let EvMasks { w1c, wbc, wn } = k;
        let m = tz(wn);
        let pb = p.wrapping_add(FB);
        let c0 = tz2(w1c);
        let t = blsr(w1c);
        let c1 = tz2(t);
        let c2 = tz(blsr(t));
        let tb = bzhi(wbc, m);
        let z = _lzcnt_u64(tb);
        let r6 = lea3::<-64>(m, z);
        let t = tb.wrapping_shl(z as u32) << 1;
        let r5 = _lzcnt_u64(t);
        let t = t.wrapping_shl(r5 as u32) << 1;
        let r4 = _lzcnt_u64(t);
        let zr = z.wrapping_add(r5);
        let i4 = lea3::<1>(zr, r4) ^ 63;
        let u3 = add_sub::<{ FB as i32 - 22 }>(i4, c2);
        if (m > 63) | (TAIL && pb.wrapping_add(m as usize) >= cx.end) | (c2 > 62) {
            return None;
        }
        core::arch::asm!("test {u}, -3", "jnz {fail}", u = in(reg) u3, fail = label { return None; }, options(nomem, nostack));
        let tr = &*((T.tail.as_ptr() as *const u8).add((here(r5 * TAIL_K as u64 + r6) as usize) << 6) as *const TailRow);
        let q32 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(pb.wrapping_add(m as usize).wrapping_sub(32) as *const __m256i), _mm256_set1_epi8(b'"' as i8))) as u32;
        let qw = q32 & tr.m;
        let qi = _pext_u32(qw, tr.sel) as usize;
        if qw != *tr.pat.get_unchecked(qi) {
            return None;
        }
        let (c0, c1, c2, r4) = (c0 as usize, c1 as usize, c2 as usize, r4 as usize);
        let rs4 = pb.wrapping_add(i4 as usize);
        let kr = &*((T.kind2.as_ptr() as *const u8).add((here((r4 * 2 + (*rs4.wrapping_add(3) & 1) as usize) as u64) as usize) << 6) as *const KindRow2);
        let bad_kind = _mm_and_si128(_mm_xor_si128(ld(rs4), ld(kr.cand.as_ptr())), ld(kr.maskv.as_ptr()));
        *(s.byte_add(PP) as *mut u64) = *tr.pp.get_unchecked(qi) | kr.pp;
        let ro = jid_off(c0 as u64, c1 as u64) as usize;
        let jr = &*((T.jid.as_ptr() as *const u8).add(ro) as *const JidRow);
        let (ids, bad_i) = ids2j(ld(p.wrapping_add(c0).wrapping_sub(16)), ld(p.wrapping_add(c1).wrapping_sub(16)), jr);
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(EvBatch, eid)) as *mut __m128i, _mm256_castsi256_si128(ids));
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(EvBatch, uid)) as *mut __m128i, _mm256_extracti128_si256::<1>(ids));
        let q3 = (u3 >> 1) as usize;
        let jo = (here((q3 + 2 * (c2 - c1)) as u64) as usize) << 9;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (jd, jf) = (&tj.d, &tj.f);
        let ld2 = |q: *const u8| _mm256_load_si256(q as *const __m256i);
        let cache = cx.cache;
        let e2 = p.wrapping_add(c2);
        let (p1, ko) = (p.wrapping_add(c1), jf.koff1 as usize);
        let y2 = ld2(cache.add(_pext_u64(rd16(p1.wrapping_add(ko).wrapping_add(9)) as u64, jf.pmask) as usize) as *const u8);
        let bad_k2 = _mm_and_si128(xor_lo(y2, p1, ko), ld(jd.kmask2.as_ptr()));
        let y3 = ld2(cache.add(_pext_u64(rd16(e2.wrapping_add(q3).wrapping_add(9)) as u64, jf.pmask3) as usize) as *const u8);
        let bad_k3 = _mm_and_si128(xor_lo(y3, e2, q3), ld(jd.kmask3.as_ptr()));
        let x = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(ld(e2.wrapping_sub(16))), ld(rs4.wrapping_sub(17)));
        let bad = _mm256_or_si256(bad_i, _mm256_zextsi128_si256(_mm_or_si128(bad_k3, bad_kind)));
        Some(EvSt { jo, x, days: _mm256_permute2x128_si256::<0x31>(y2, y3), bad2: _mm256_or_si256(bad, _mm256_zextsi128_si256(bad_k2)), bad })
    }
}

#[cold]
#[inline(never)]
unsafe extern "C" fn ev_fix<S: EventSink>(cx: *const Cx<EvBatch, S>, p: *const u8, s: *mut i64) -> u64 {
    unsafe {
        let t = _blsr_u64(eq64(p, b','));
        fix_slot(&*cx, p, t.trailing_zeros() as usize + 1, _blsr_u64(t).trailing_zeros() as usize, s) as u64
    }
}

#[inline(always)]
unsafe fn ev_d<S: EventSink>(cx: &Cx<EvBatch, S>, p: *const u8, st: EvSt, np: *const u8, w: &mut *mut i64, lim: *const u8) -> *const u8 {
    unsafe {
        let s = *w;
        const A: usize = BATCH_ALIGN;
        const PP: usize = core::mem::offset_of!(EvBatch, pp);
        macro_rules! cold {
            () => {{
                let (r, nw) = kcall!(ev_cold::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<EvBatch, S> as u64, p as u64, s as u64, 0u64);
                *w = nw as *mut i64;
                return tag_far(r as *const u8, lim);
            }};
        }
        let jo = st.jo;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (ja, jb, jc, jd, je, jf) = (&tj.a, &tj.b, &tj.c, &tj.d, &tj.e, &tj.f);
        let ld2 = |q: *const u8| _mm256_load_si256(q as *const __m256i);
        let d = _mm256_sub_epi8(_mm256_shuffle_epi8(st.x, ld2(ja.ctrl.as_ptr())), ld2(ja.base.as_ptr()));
        let m1 = _mm256_maddubs_epi16(d, ld2(jb.w1.as_ptr() as *const u8));
        let m2 = _mm256_madd_epi16(m1, ld2(jc.w2.as_ptr() as *const u8));
        let m3 = _mm256_madd_epi16(_mm256_packus_epi32(m2, m2), ld2(jc.w3.as_ptr() as *const u8));
        let (bad_ta, bad_tb) = (_mm256_subs_epu8(d, ld2(jb.lim.as_ptr())), _mm256_cmpgt_epi32(m2, ld2(jd.limv.as_ptr() as *const u8)));
        let t = _mm256_mul_epu32(m3, ld2(je.kme.as_ptr() as *const u8));
        let f = _mm256_mul_epi32(_mm256_srli_epi64::<32>(m3), ld2(je.kmf.as_ptr() as *const u8));
        let ksub = _mm256_testn_epi64_mask(d, ld2(jf.ksel.as_ptr()));
        let r = _mm256_mask_sub_epi64(_mm256_add_epi64(t, f), ksub, t, f);
        let r = _mm256_add_epi64(r, st.days);
        store_hi::<0>(s, _mm256_castsi256_si128(r));
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(EvBatch, ing)) as *mut __m128i, _mm256_extracti128_si256::<1>(r));
        let all = _mm256_ternarylogic_epi64::<0xFE>(bad_ta, bad_tb, st.bad2);
        if _mm256_testz_si256(all, all) == 0 {
            let bad_tt = _mm256_or_si256(bad_ta, bad_tb);
            let other = _mm256_or_si256(st.bad, _mm256_and_si256(bad_tt, _mm256_set_epi64x(-1, -1, 0, 0)));
            if _mm256_testz_si256(other, other) == 0 {
                cold!();
            }
            if kcall!(ev_fix::<S> as unsafe extern "C" fn(_, _, _) -> u64, cx as *const Cx<EvBatch, S> as u64, p as u64, s as u64, 0u64).0 as u8 == 0 {
                cold!();
            }
        }
        if core::ptr::read_volatile((s as *const u8).add(PP + 7)) & 2 != 0
            && kcall!(ev_purchase as unsafe extern "C" fn(_, _, _) -> u64, p as u64, np as u64, s as u64, 0u64).0 as u8 == 0
        {
            cold!();
        }
        *w = s.add(1);
        if (s.add(1) as usize) & (A - 1) == 0 {
            (*cx.b).n = NB;
            kcall!(flush_ev::<S> as unsafe extern "C" fn(_), cx.sink as u64, 0u64, 0u64, 0u64);
            *w = (*cx.b).ts.as_mut_ptr();
            return tag_far(np, lim);
        }
        np
    }
}

#[inline(always)]
unsafe fn ev_fast<S: EventSink, const TAIL: bool>(cx: &Cx<EvBatch, S>, p: *const u8, k: EvMasks, np: *const u8, w: &mut *mut i64) -> *const u8 {
    unsafe {
        match ev_s::<S, TAIL>(cx, p, k, *w) {
            Some(st) => ev_d(cx, p, st, np, w, NO_LIM),
            None => {
                let (r, nw) = kcall!(ev_lf::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<EvBatch, S> as u64, p as u64, *w as u64, 0u64);
                *w = nw as *mut i64;
                r as *const u8
            }
        }
    }
}

#[cold]
#[inline(never)]
unsafe extern "C" fn ev_purchase(p: *const u8, np: *const u8, s: *mut i64) -> u64 {
    unsafe {
        let _ = p;
        let pe = np.wrapping_sub(1);
        let pt = pe.wrapping_sub(32);
        let tw = _mm256_loadu_si256(pt as *const __m256i);
        let q = hi32(tw, b'"') >> 32;
        let tb = hi32(tw, b',');
        let r6 = _lzcnt_u64(tb);
        let r5 = _lzcnt_u64(tb.wrapping_shl(r6 as u32) << 1);
        let i6 = 32 - r6;
        let i5 = i6.wrapping_sub(r5).wrapping_sub(1);
        let qb = (q.wrapping_shr(i6 as u32) & 1) as usize;
        let qb5 = (q.wrapping_shr(i5 as u32) & 1) as usize;
        let (r5, r6) = (r5 as usize, r6 as usize);
        let Some(pr) = price(pe.wrapping_sub(qb), r6 - 2 * qb) else { return 0 };
        let (prod, bad_p) = dec(ld(pt.wrapping_add(i6 as usize).wrapping_sub(17)), T.prod.get_unchecked(r5 * 2 + qb5));
        if nz(bad_p) {
            return 0;
        }
        *(s.byte_add(core::mem::offset_of!(EvBatch, pp)) as *mut u64) = prod as u64 | (pr as u64) << 32 | 2 << 56 | if pr == 0 { PP_INVALID } else { 0 };
        1
    }
}

#[derive(Clone, Copy)]
struct ClMasks {
    c: u64,
    q: u64,
}

#[inline(always)]
unsafe fn cl_masks(p: *const u8) -> (ClMasks, u64) {
    unsafe {
        let w = masks(p);
        (ClMasks { c: w.c, q: w.q }, tz(w.n))
    }
}

#[inline(always)]
unsafe fn cl_core<S: ClickSink, const CHAIN: bool, const CAREFUL: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    w: &ClMasks,
    n_in: u64,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let c0 = tz(w.c);
        let t = blsr(w.c);
        let c1 = tz(t);
        let t = blsr(t);
        let c2 = tz(t);
        let t = blsr(t);
        let n = if CHAIN { n_in } else { (e_in - l) as u64 };
        if (n > if CHAIN { 63 } else { 64 }) | (c2 >= n) | (_bzhi_u64(t, n as u32) != 0) {
            return FAIL;
        }
        let (c0, c1, c2, n) = (c0 as usize, c1 as usize, c2 as usize, n as usize);
        let e = l + n;
        let (sk, su) = if CAREFUL {
            (T.kid.get_unchecked(c0 * 2 + (w.q & 1) as usize), T.uid.get_unchecked((c1 - c0 - 1) * 2 + (w.q >> (c0 + 1) & 1) as usize))
        } else {
            (T.kid1.get_unchecked(c0), T.uid1.get_unchecked(c1 - c0 - 1))
        };
        let sc = T.camp.get_unchecked((c2 - c1 - 1) * 2 + (w.q >> (c1 + 1) & 1) as usize);
        let xk = ld(p.wrapping_add(c0).wrapping_sub(16));
        let xu = ld(p.wrapping_add(c1).wrapping_sub(16));
        let xc = ld(p.wrapping_add(c2).wrapping_sub(16));
        let (rs3, raw3) = (p.wrapping_add(c2 + 1), n - c2 - 1);
        let (cid, uid, kx, campaign, ts);
        {
            count(1);
            (cid, uid, kx) = {
                let (a, b, bad) = ids2(xk, sk, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b, [0; 4])) }, cold_ids(base, len, l, l + c0, b'k', l + c0 + 1, l + c1))
            };
            campaign = {
                let (v, bad) = dec(xc, sc);
                or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, l + c1 + 1, l + c2, false))
            };
            ts = or_cold!(ts_careful::<true>(rs3, raw3, cache), cold_ts(base, len, l + c2 + 1, e, false));
        }
        sink.click(&ClRow { cid, uid, campaign, ts, ch: kx[0], cl: kx[1], uh: kx[2], ul: kx[3] });
        e
    }
}

#[derive(Clone, Copy)]
struct CkMasks {
    c: u64,
    n: u64,
}

#[inline(always)]
unsafe fn ck_masks(p: *const u8) -> CkMasks {
    unsafe {
        let (c, n): (u64, u64);
        core::arch::asm!(
            "vpcmpeqb {a}, {c}, ymmword ptr [{p}]",
            "vpcmpeqb {b}, {c}, ymmword ptr [{p} + 32]",
            "kunpckdq {a}, {b}, {a}",
            "vpcmpeqb {x}, {n}, ymmword ptr [{p}]",
            "vpcmpeqb {b}, {n}, ymmword ptr [{p} + 32]",
            "kunpckdq {x}, {b}, {x}",
            a = out(kreg) c, b = out(kreg) _, x = out(kreg) n,
            c = in(ymm_reg) _mm256_set1_epi8(b',' as i8), n = in(ymm_reg) _mm256_set1_epi8(b'\n' as i8), p = in(reg) p,
            options(pure, readonly, nostack, preserves_flags)
        );
        CkMasks { c, n }
    }
}
impl CkMasks {
    #[inline(always)]
    fn m(&self) -> u64 {
        tz(self.n)
    }
    #[inline(always)]
    fn next(&self, p: *const u8) -> *const u8 {
        lea3::<1>(p as u64, self.m()) as *const u8
    }
    #[inline(always)]
    fn here(self) -> CkMasks {
        if CK_KREG {
            CkMasks { c: here_k(self.c), n: here_k(self.n) }
        } else {
            CkMasks { c: here(self.c), n: here(self.n) }
        }
    }
}
const CK_KREG: bool = true;

#[cold]
#[inline(never)]
unsafe extern "C" fn cl_cold<S: ClickSink>(cx: *const Cx<CkBatch, S>, p: *const u8, s: *mut i64) -> Pair {
    unsafe {
        let cx = &*cx;
        let b = cx.b;
        let (base, len) = (cx.base, cx.end.offset_from(cx.base) as usize);
        let l = p.offset_from(base) as usize;
        let sink = &mut *cx.sink;
        (*b).n = s.offset_from((*b).ts.as_ptr()) as usize;
        let mut r = FAIL;
        if l + 128 <= len {
            r = cl_long::<S>(base, len, l, cx.cache, sink);
        }
        if r == FAIL {
            let (w, nl) = cl_masks(p);
            r = cl_core::<S, true, true>(base, len, l, 0, &w, nl, cx.cache, sink);
        }
        let next = if r != FAIL { r + 1 } else { scan_click(core::slice::from_raw_parts(base, len), l, sink) };
        Pair(base.wrapping_add(next) as u64, (*b).ts.as_mut_ptr().add((*b).n) as u64)
    }
}

#[inline(never)]
unsafe extern "C" fn cl_lf<S: ClickSink>(cx: *const Cx<CkBatch, S>, p: *const u8, s: *mut i64) -> Pair {
    unsafe {
        let cxr = &*cx;
        macro_rules! cold {
            () => {{
                return cl_cold::<S>(cx, p, s);
            }};
        }
        let rl = cxr.end.offset_from(p) as usize;
        let n0 = eq64(p, b'\n');
        let m = if n0 != 0 { tz(n0) } else { 64 + tz(eq64(p.add(64), b'\n')) } as usize;
        let pe = p.add(m);
        if (m >= 128) | (pe >= cxr.end) {
            cold!();
        }
        let c0 = tz(eq64(p, b',')) as usize;
        let c1 = c0 + 1 + tz(eq64(p.add(c0 + 1), b',')) as usize;
        let c2 = c1 + 1 + tz(eq64(p.add(c1.min(rl - 1) + 1), b',')) as usize;
        let c3 = c2 + 1 + tz(eq64(p.add(c2.min(rl - 1) + 1), b',')) as usize;
        let (raw1, raw2, raw3) = (c1 - c0 - 1, c2.wrapping_sub(c1 + 1), m.wrapping_sub(c2 + 1));
        if (c2 >= m) | (c3 < m) | (raw2 >= 63) | (raw3 >= 63) {
            cold!();
        }
        let q0 = (*p == b'"') as usize;
        let q1 = (*p.add(c0 + 1) == b'"') as usize;
        let (cid, ch, cl, uid, uh, ul);
        let mut bad = false;
        if c0 <= 11 {
            let (v, b) = id1::<false>(ld(p.add(c0).wrapping_sub(16)), T.kid.get_unchecked(c0 * 2 + q0));
            (cid, ch, cl) = (v, 0, 0);
            bad |= nz(b);
        } else {
            if (c0 != 33 + 2 * q0) | (*p.add(q0) != b'k') | (*p.add(c0 - 1) != if q0 == 1 { b'"' } else { *p.add(c0 - 1) }) {
                cold!();
            }
            match hex32(p.add(q0 + 1)) {
                Some((h, l)) => (cid, ch, cl) = (long_word(h, l), h, l),
                None => cold!(),
            }
        }
        if raw1 <= 11 {
            let (v, b) = id1::<true>(ld(p.add(c1).wrapping_sub(16)), T.uid.get_unchecked(raw1 * 2 + q1));
            (uid, uh, ul) = (v, 0, 0);
            bad |= nz(b);
        } else {
            let f = p.add(c0 + 1);
            if (raw1 != 33 + 2 * q1) | (*f.add(q1) != b'u') | (*p.add(c1 - 1) != if q1 == 1 { b'"' } else { *p.add(c1 - 1) }) {
                cold!();
            }
            match hex32(f.add(q1 + 1)) {
                Some((h, l)) => (uid, uh, ul) = (long_word(h, l), h, l),
                None => cold!(),
            }
        }
        let sc = T.camp4.get_unchecked(raw2 * 2 + (*p.add(c1 + 1) & 1) as usize);
        let e2 = p.add(c2);
        let (cv, bad_c) = dec4(ld(e2.wrapping_sub(16)), sc);
        let jo = (m - c2) << 10;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (ja, jb, jc, jd, je, jf) = (&tj.a, &tj.b, &tj.c, &tj.d, &tj.e, &tj.f);
        let kp = e2.wrapping_offset(jf.koff1 as isize);
        let ent = cxr.cache.add(_pext_u64(rd16(kp.wrapping_add(9)) as u64, jf.pmask) as usize);
        let bad_k = _mm_and_si128(_mm_xor_si128(ld(kp), ld(ent as *const u8)), ld(jd.kmask2.as_ptr()));
        let d = _mm_sub_epi8(_mm_shuffle_epi8(ld(pe.wrapping_sub(16)), ld(ja.ctrl.as_ptr())), ld(ja.base.as_ptr()));
        let m1 = _mm_maddubs_epi16(d, ld(jb.w1.as_ptr() as *const u8));
        let m2 = _mm_madd_epi16(m1, ld(jc.w2.as_ptr() as *const u8));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), ld(jc.w3.as_ptr() as *const u8));
        let bad_t = _mm_or_si128(_mm_subs_epu8(d, ld(jb.lim.as_ptr())), _mm_cmpgt_epi32(m2, ld(jd.limv.as_ptr() as *const u8)));
        let t = _mm_mul_epu32(m3, ld(je.kme.as_ptr() as *const u8));
        let f = _mm_mul_epi32(_mm_srli_epi64::<32>(m3), ld(je.kmf.as_ptr() as *const u8));
        let ksub = _mm_testn_epi64_mask(d, ld(jf.ksel.as_ptr()));
        let r = _mm_mask_sub_epi64(_mm_add_epi64(t, f), ksub, t, f);
        let r = _mm_add_epi64(r, ld((ent as *const u8).add(16)));
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(CkBatch, camp)) as *mut __m128i, cv);
        *(s.byte_add(core::mem::offset_of!(CkBatch, cid)) as *mut u64) = cid;
        *(s.byte_add(core::mem::offset_of!(CkBatch, uid)) as *mut u64) = uid;
        *(s.byte_add(core::mem::offset_of!(CkBatch, chi)) as *mut u64) = ch;
        *(s.byte_add(core::mem::offset_of!(CkBatch, clo)) as *mut u64) = cl;
        *(s.byte_add(core::mem::offset_of!(CkBatch, uhi)) as *mut u64) = uh;
        *(s.byte_add(core::mem::offset_of!(CkBatch, ulo)) as *mut u64) = ul;
        store_hi::<0>(s, r);
        let np = pe.add(1);
        if bad | nz(_mm_or_si128(bad_c, _mm_or_si128(bad_k, bad_t))) {
            if bad || nz(bad_c) || !fix_slot(cxr, p, c2 + 1, m, s) {
                cold!();
            }
        }
        let s1 = s.add(1);
        if (s1 as usize) & (BATCH_ALIGN - 1) == 0 {
            (*cxr.b).n = NB;
            (*cxr.sink).ck_flush();
            return Pair(np as u64, (*cxr.b).ts.as_mut_ptr() as u64);
        }
        Pair(np as u64, s1 as u64)
    }
}

#[derive(Clone, Copy)]
struct ClSt {
    jo: usize,
    x: __m128i,
    days: __m128i,
    bad_k: __m128i,
    bad: __m128i,
}

#[inline(always)]
unsafe fn cl_s<S: ClickSink, const TAIL: bool>(cx: &Cx<CkBatch, S>, p: *const u8, k: CkMasks, s: *mut i64) -> Option<ClSt> {
    unsafe {
        let CkMasks { c: wc, n } = k;
        let nl = tz(n);
        let c0 = tz(wc);
        let t = blsr(wc);
        let c1 = tz(t);
        let t = blsr(t);
        let c2 = tz(t);
        let t = blsr(t);
        if (nl > 63) | (c2 >= nl) | (bzhi(t, nl) != 0) {
            return None;
        }
        let (c0, c1, c2, nl) = (c0 as usize, c1 as usize, c2 as usize, nl as usize);
        let co = (here(((c2 - c1) * 2 + (*p.wrapping_add(c1 + 1) & 1) as usize) as u64) as usize) << 6;
        let sc = &*((T.camp4.as_ptr() as *const u8).add(co).sub(128) as *const DecShape);
        let e2 = p.wrapping_add(c2);
        let (cv, bad_c) = dec4(ld(e2.wrapping_sub(16)), sc);
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(CkBatch, camp)) as *mut __m128i, cv);
        let su = &*((T.uid1.as_ptr() as *const u8).add(here(((c1 - c0) << 6) as u64) as usize).sub(64) as *const IdShape);
        let (ids, bad_id) = ids2v(ld(p.wrapping_add(c0).wrapping_sub(16)), T.kid1.get_unchecked(c0), ld(p.wrapping_add(c1).wrapping_sub(16)), su);
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(CkBatch, cid)) as *mut __m128i, ids);
        store_hi::<{ core::mem::offset_of!(CkBatch, uid) }>(s, ids);
        let jo = (here((nl - c2) as u64) as usize) << 10;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (jd, jf) = (&tj.d, &tj.f);
        let kp = e2.wrapping_offset(jf.koff1 as isize);
        let ent = cx.cache.add(_pext_u64(rd16(kp.wrapping_add(9)) as u64, jf.pmask) as usize);
        let bad_k = _mm_and_si128(_mm_xor_si128(ld(kp), ld(ent as *const u8)), ld(jd.kmask2.as_ptr()));
        Some(ClSt { jo, x: ld(p.wrapping_add(nl).wrapping_sub(16)), days: ld((ent as *const u8).add(16)), bad_k, bad: _mm_or_si128(bad_id, bad_c) })
    }
}

#[cold]
#[inline(never)]
unsafe extern "C" fn cl_fix<S: ClickSink>(cx: *const Cx<CkBatch, S>, p: *const u8, np: *const u8, s: *mut i64) -> u64 {
    unsafe {
        let t = _blsr_u64(_blsr_u64(masks(p).c));
        fix_slot(&*cx, p, t.trailing_zeros() as usize + 1, np.offset_from(p) as usize - 1, s) as u64
    }
}

#[inline(always)]
unsafe fn cl_d<S: ClickSink>(cx: &Cx<CkBatch, S>, p: *const u8, st: ClSt, np: *const u8, w: &mut *mut i64) -> *const u8 {
    unsafe {
        let s = *w;
        const A: usize = BATCH_ALIGN;
        macro_rules! cold {
            () => {{
                let (r, nw) = kcall!(cl_cold::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<CkBatch, S> as u64, p as u64, s as u64, 0u64);
                *w = nw as *mut i64;
                return r as *const u8;
            }};
        }
        let jo = st.jo;
        let tj = &*((T.tj.as_ptr() as *const u8).add(jo).sub(2 * 512) as *const TjRow);
        let (ja, jb, jc, jd, je, jf) = (&tj.a, &tj.b, &tj.c, &tj.d, &tj.e, &tj.f);
        let d = _mm_sub_epi8(_mm_shuffle_epi8(st.x, ld(ja.ctrl.as_ptr())), ld(ja.base.as_ptr()));
        let m1 = _mm_maddubs_epi16(d, ld(jb.w1.as_ptr() as *const u8));
        let m2 = _mm_madd_epi16(m1, ld(jc.w2.as_ptr() as *const u8));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), ld(jc.w3.as_ptr() as *const u8));
        let bad_t = _mm_or_si128(_mm_subs_epu8(d, ld(jb.lim.as_ptr())), _mm_cmpgt_epi32(m2, ld(jd.limv.as_ptr() as *const u8)));
        let t = _mm_mul_epu32(m3, ld(je.kme.as_ptr() as *const u8));
        let f = _mm_mul_epi32(_mm_srli_epi64::<32>(m3), ld(je.kmf.as_ptr() as *const u8));
        let ksub = _mm_testn_epi64_mask(d, ld(jf.ksel.as_ptr()));
        let r = _mm_mask_sub_epi64(_mm_add_epi64(t, f), ksub, t, f);
        let r = _mm_add_epi64(r, st.days);
        store_hi::<0>(s, r);
        if nz(_mm_or_si128(st.bad, _mm_or_si128(st.bad_k, bad_t))) {
            if nz(st.bad) {
                cold!();
            }
            if kcall!(cl_fix::<S> as unsafe extern "C" fn(_, _, _, _) -> u64, cx as *const Cx<CkBatch, S> as u64, p as u64, np as u64, s as u64).0 as u8 == 0 {
                cold!();
            }
        }
        *w = s.add(1);
        if (s.add(1) as usize) & (A - 1) == 0 {
            (*cx.b).n = NB;
            kcall!(flush_ck::<S> as unsafe extern "C" fn(_), cx.sink as u64, 0u64, 0u64, 0u64);
            *w = (*cx.b).ts.as_mut_ptr();
        }
        np
    }
}

#[inline(always)]
unsafe fn cl_fast<S: ClickSink, const TAIL: bool>(cx: &Cx<CkBatch, S>, p: *const u8, k: CkMasks, np: *const u8, w: &mut *mut i64) -> *const u8 {
    unsafe {
        match cl_s::<S, TAIL>(cx, p, k, *w) {
            Some(st) => cl_d(cx, p, st, np, w),
            None => {
                let (r, nw) = kcall!(cl_lf::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<CkBatch, S> as u64, p as u64, *w as u64, 0u64);
                *w = nw as *mut i64;
                r as *const u8
            }
        }
    }
}

const PIPE: u8 = 0;
const PFD: usize = 1024;
const CHAIN: u8 = 1;

const TAG: usize = 1 << 63;
const NO_LIM: *const u8 = usize::MAX as *const u8;
#[inline(always)]
fn tag_far(r: *const u8, lim: *const u8) -> *const u8 {
    if r > lim { (r as usize | TAG) as *const u8 } else { r }
}

#[inline(always)]
fn here(mut v: u64) -> u64 {
    unsafe { core::arch::asm!("/* {0} */", inout(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}
#[inline(always)]
fn here_k(mut v: u64) -> u64 {
    unsafe { core::arch::asm!("/* {0} */", inout(kreg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[cold]
#[inline(never)]
fn scan_event<S: EventSink>(buf: &[u8], pos: usize, sink: &mut S) -> usize {
    count(12);
    let e = match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i,
        None => buf.len(),
    };
    slow::event(buf, pos, e, sink);
    e + 1
}
#[cold]
#[inline(never)]
fn scan_click<S: ClickSink>(buf: &[u8], pos: usize, sink: &mut S) -> usize {
    count(13);
    let e = match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i,
        None => buf.len(),
    };
    slow::click(buf, pos, e, sink);
    e + 1
}

macro_rules! runner {
    ($name:ident, $sink:ident, $get:ident, $line:ident, $masks:ident, $adv:expr, $scan:ident) => {
        #[inline(never)]
        unsafe fn $name<S: $sink, const MODE: u8>(buf: &[u8], sink: &mut S) {
            unsafe {
                let base = buf.as_ptr();
                let len = buf.len();
                let end = base.add(len);
                let b = sink.$get();
                let sink: *mut S = sink;
                let cx = &Cx { base, end, b, cache: (*b).scratch.as_mut_ptr() as *mut DateEnt, sink };
                let mut w = (*b).ts.as_mut_ptr().add((*b).n);
                let mut p = base;
                if MODE == PIPE {
                    const REACH: usize = 3 * ($adv + 64) + 2 + 3 * 162 + 160;
                    const FAR: usize = (NB + 1) * ($adv + 64 + 1) + (NB + 2) * 162;
                    if len >= REACH + FAR {
                        let lim = end.sub(REACH + FAR);
                        while p <= lim {
                            let (code, np, nw) = ev_loop::<S>(p, w, b, cx as *const Cx<EvBatch, S>, lim, sink);
                            p = np;
                            w = nw;
                            if code == 1 {
                                count(11);
                                let k = $masks(p);
                                p = $line::<S, false>(cx, p, k, k.next(p), &mut w);
                            }
                        }
                    }
                }
                {
                    const REACH: usize = 2 * ($adv + 64) + 1;
                    if len >= REACH {
                        let lim = end.sub(REACH);
                        while p <= lim {
                            let k = $masks(p);
                            _mm_prefetch::<_MM_HINT_T0>(p.wrapping_add(PFD) as *const i8);
                            p = $line::<S, false>(cx, p, k, k.next(p), &mut w);
                        }
                    }
                }
                let (base, end, b) = (core::ptr::read_volatile(&cx.base), core::ptr::read_volatile(&cx.end), core::ptr::read_volatile(&cx.b));
                while p.wrapping_add(64) <= end {
                    let k = $masks(p);
                    p = $line::<S, true>(cx, p, k, k.next(p), &mut w);
                }
                (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
                let buf = core::slice::from_raw_parts(base, end.offset_from(base) as usize);
                let mut pos = p.offset_from(base) as usize;
                while pos < buf.len() {
                    pos = $scan(buf, pos, &mut *core::ptr::read_volatile(&cx.sink));
                }
            }
        }
    };
}
runner!(run_events, EventSink, ev, ev_fast, ev_masks, OB, scan_event);

#[repr(C, align(128))]
#[derive(Clone, Copy)]
struct EjRow {
    ctrl: [u8; 32],
    subv: [u8; 32],
    _pad: [u8; 64],
}
const EJ_E: usize = 32 * 8 + 32 + 16 + 1;
const EJ_ROWS: usize = EJ_E + 81;
const fn ej_lane(r: &mut EjRow, o: usize, s: &IdShape, sh: i32) {
    assert!(s.ctrl[7] == 0x80 && s.subv[7] == 0 && s.lz[7] == 0);
    let mut i = 0;
    while i < 16 {
        let c = s.ctrl[i];
        r.ctrl[o + i] = if c == 0x80 {
            0x80
        } else {
            let n = c as i32 + sh;
            assert!(n >= 0 && n <= 15);
            n as u8
        };
        r.subv[o + i] = s.subv[i];
        i += 1;
    }
    let mut i = 0;
    while i < 16 {
        if s.lz[i] != 0 {
            assert!(i >= 8 && r.ctrl[o + i] != 0x80);
            r.ctrl[o + 7] = r.ctrl[o + i];
            r.subv[o + 7] = b'1';
        }
        i += 1;
    }
}
const fn ej_rows() -> [EjRow; EJ_ROWS] {
    let e = id_shapes_raw(b'e', 8, 0, false);
    let u = id_shapes_raw(b'u', 8, 0, true);
    let mut bad = EjRow { ctrl: [0x80; 32], subv: [0; 32], _pad: [0; 64] };
    bad.subv[0] = 1;
    bad.subv[16] = 1;
    let mut t = [bad; EJ_ROWS];
    let mut used = [false; EJ_ROWS];
    let mut c0 = 0;
    while c0 <= 32 {
        let mut d = 7;
        while d <= 15 {
            let c1 = c0 + d + 1;
            let mut r = EjRow { ctrl: [0x80; 32], subv: [0; 32], _pad: [0; 64] };
            ej_lane(&mut r, 0, &e[c0], 0);
            if u[d].orv[1] == 0 {
                ej_lane(&mut r, 16, &u[d], d as i32 - 15);
                assert!(r.ctrl[16 + 6] == 0x80 && r.subv[16 + 6] == 0);
                r.ctrl[16 + 6] = (d + 1) as u8;
                r.subv[16 + 6] = b',';
            } else {
                r.subv[16] = 1;
            }
            let ix = c0 * 8 + c1;
            assert!(!used[ix]);
            used[ix] = true;
            t[ix] = r;
            d += 1;
        }
        c0 += 1;
    }
    let mut c0 = 0;
    while c0 < 32 {
        let mut r = EjRow { ctrl: [0x80; 32], subv: [0; 32], _pad: [0; 64] };
        ej_lane(&mut r, 0, &e[c0], 0);
        assert!(!used[EJ_E + c0]);
        t[EJ_E + c0] = r;
        c0 += 1;
    }
    let mut q0 = 0;
    while q0 < 2 {
        let c0p = 9 + 2 * q0;
        let mut c1p = c0p + 1;
        while c1p <= 32 {
            let ix = EJ_E + 8 * c0p + c1p - 40;
            let mut r = EjRow { ctrl: [0x80; 32], subv: [0; 32], _pad: [0; 64] };
            let mut good = false;
            let d = c1p - c0p - 1;
            if d >= 7 && d <= 15 && u[d].orv[1] == 0 {
                ej_lane(&mut r, 16, &u[d], d as i32 - 15);
                assert!(r.ctrl[16 + 6] == 0x80 && r.subv[16 + 6] == 0);
                r.ctrl[16 + 6] = (d + 1) as u8;
                r.subv[16 + 6] = b',';
                good = true;
            }
            if good {
                assert!(!used[ix]);
                used[ix] = true;
                t[ix] = r;
            } else if !used[ix] {
                r.subv[16] = 1;
                t[ix] = r;
            }
            c1p += 1;
        }
        q0 += 1;
    }
    t
}
static EJ: [EjRow; EJ_ROWS] = ej_rows();

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow3 {
    cand: [u8; 16],
    maskv: [u8; 16],
    pp: u64,
    tlk: i64,
    pthr: u64,
    isp: u64,
}
const fn kind3(t: &mut [KindRow3; 256], s: &[u8], kind: u64) {
    let n = s.len();
    assert!(n + 2 <= 16);
    assert!(t[s[1] as usize].isp == 0 && t[s[1] as usize].pthr == u64::MAX && t[s[1] as usize].pp == 0);
    let mut r = KindRow3 { cand: [0; 16], maskv: [0; 16], pp: kind << 56, tlk: n as i64 - 40 - TL2_SOFF as i64 - (kind as i64) * TL2_P as i64, pthr: 0, isp: 0 };
    r.cand[0] = b',';
    let mut i = 0;
    while i < n {
        r.cand[1 + i] = s[i];
        i += 1;
    }
    r.cand[n + 1] = b',';
    let mut i = 0;
    while i < n + 2 {
        r.maskv[i] = 0xFF;
        i += 1;
    }
    if kind == 2 {
        r.pthr = u64::MAX;
        r.isp = 1;
    }
    t[s[1] as usize] = r;
}
const fn kind_rows3() -> [KindRow3; 256] {
    let mut t = [KindRow3 { cand: [0; 16], maskv: [0; 16], pp: 0, tlk: 4 - 40 - TL2_SOFF as i64 - 2 * TL2_P as i64, pthr: u64::MAX, isp: 0 }; 256];
    kind3(&mut t, b"view", 0);
    kind3(&mut t, b"cart", 1);
    kind3(&mut t, b"purchase", 2);
    kind3(&mut t, b"\"view\"", 0);
    kind3(&mut t, b"\"cart\"", 1);
    kind3(&mut t, b"\"purchase\"", 2);
    t
}
static K3: [KindRow3; 256] = kind_rows3();

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TailRow2 {
    m: u64,
    sel: u32,
    _p0: u32,
    pat: [u64; 4],
    pp: u64,
    _p1: u64,
}
const TL2_P: usize = 72;
const TL2_S: usize = 3 * TL2_P;
const TL2_SOFF: usize = 36;
const TL2_R6: usize = 65;
const fn tail_rows2() -> [TailRow2; TL2_R6 * TL2_S] {
    let mut t = [TailRow2 { m: 0, sel: 0, _p0: 0, pat: [1; 4], pp: 0, _p1: 0 }; TL2_R6 * TL2_S];
    let mut pl = 0;
    while pl < 3 {
    let mut r6 = 0;
    while r6 < 32 {
        let mut s = r6;
        while s <= 30 {
            let r5 = s - r6;
            let n = s + 1;
            let sh = 32 - n;
            let qm = ((1u32 << n) - 1) << sh;
            let p5 = if r5 >= 2 { 1 | 1u32 << (r5 - 1) } else { TAIL_NO };
            let p6 = if r6 >= 2 { (1 | 1u32 << (r6 - 1)) << (r5 + 1) } else { TAIL_NO };
            let both = if r5 >= 2 && r6 >= 2 { p5 | p6 } else { TAIL_NO };
            let mut pat32 = [0, tail_up(p5, sh), tail_up(p6, sh), tail_up(both, sh)];
            if r5 == 2 {
                pat32[1] = TAIL_NO;
                pat32[3] = TAIL_NO;
            }
            let cpart = (1u64 << (31 - r6)) << 32;
            let mut pat = [1u64; 4];
            let mut i = 0;
            while i < 4 {
                if pat32[i] != TAIL_NO {
                    pat[i] = pat32[i] as u64 | cpart;
                }
                i += 1;
            }
            let pp = if pl == 2 { 2 << 56 } else if r5 == 0 { PP_NONE | (pl as u64) << 56 } else { PP_SOME | (pl as u64) << 56 };
            t[r6 * TL2_S + pl * TL2_P + s + TL2_SOFF] = TailRow2 { m: qm as u64 | (qm as u64) << 32, sel: (1 | 1u32 << (r5 + 1)) << sh, _p0: 0, pat, pp, _p1: 0 };
            s += 1;
        }
        r6 += 1;
    }
    pl += 1;
    }
    t
}
static TL2: [TailRow2; TL2_R6 * TL2_S] = tail_rows2();

#[repr(C, align(64))]
struct EvConst {
    e: [u8; 32],
    diglim: [u8; 32],
    letlim: [u8; 32],
    m39: [u8; 32],
    w124: [u16; 16],
    w510: [u32; 8],
    m36: [u64; 4],
    comma: [u8; 32],
    nl: [u8; 32],
    quote: [u8; 32],
    pidx: [u64; 4],
    xidx: [u64; 4],
}
const fn ev_const() -> EvConst {
    let mut c = EvConst { e: [0; 32], diglim: [0; 32], letlim: [0; 32], m39: [217; 32], w124: [0x0124; 16], w510: [0x0001_0510; 8], m36: [1_679_616; 4], comma: [b','; 32], nl: [b'\n'; 32], quote: [b'"'; 32], pidx: [1, 5, 2, 6], xidx: [0, 1, 6, 7] };
    let mut i = 0;
    while i < 32 {
        let k = i % 16;
        if k == 7 {
            c.e[i] = 208;
            c.diglim[i] = 8;
            c.letlim[i] = 25;
        } else if k >= 8 {
            c.e[i] = 207;
            c.diglim[i] = 9;
            c.letlim[i] = 25;
        }
        i += 1;
    }
    c
}
static EVC: EvConst = ev_const();
const _: () = assert!(
    core::mem::offset_of!(EvConst, m39) == 96
        && core::mem::offset_of!(EvConst, w124) == 128
        && core::mem::offset_of!(EvConst, w510) == 160
        && core::mem::offset_of!(EvConst, m36) == 192
        && core::mem::offset_of!(EvConst, comma) == 224
        && core::mem::offset_of!(EvConst, nl) == 256
        && core::mem::offset_of!(EvConst, quote) == 288
        && core::mem::offset_of!(EvConst, pidx) == 320
        && core::mem::offset_of!(EvConst, xidx) == 352
        && core::mem::offset_of!(TailRow, adj) == 56
        && core::mem::size_of::<EjRow>() == 128
        && core::mem::offset_of!(EjRow, subv) == 32
        && core::mem::size_of::<KindRow3>() == 64
        && core::mem::offset_of!(KindRow3, maskv) == 16
        && core::mem::offset_of!(KindRow3, pp) == 32
        && core::mem::offset_of!(KindRow3, tlk) == 40
        && core::mem::offset_of!(KindRow3, pthr) == 48
        && core::mem::offset_of!(KindRow3, isp) == 56
        && core::mem::size_of::<TailRow2>() == 64
        && core::mem::offset_of!(TailRow2, sel) == 8
        && core::mem::offset_of!(TailRow2, pat) == 16
        && core::mem::offset_of!(TailRow2, pp) == 48
        && TL2_S * 64 == 13824
        && (PP_SOME as u32) != 0
        && (PP_NONE as u32) != 0
        && TL2_SOFF == 36
        && core::mem::size_of::<TjRow>() == 512
        && core::mem::offset_of!(TjRow, b) == 64
        && core::mem::offset_of!(TjRow, c) == 128
        && core::mem::offset_of!(TjRow, d) == 192
        && core::mem::offset_of!(TjRow, e) == 256
        && core::mem::offset_of!(TjRow, f) == 320
        && core::mem::offset_of!(TjD, kmask2) == 32
        && core::mem::offset_of!(TjD, kmask3) == 48
        && core::mem::offset_of!(TjE, kmf) == 32
        && core::mem::offset_of!(TjF, koff1) == 32
        && core::mem::offset_of!(TjF, pmask) == 40
        && core::mem::offset_of!(TjF, pmask3) == 48
        && core::mem::offset_of!(TjF, koffe) == 56
        && core::mem::offset_of!(TjB, w1) == 32
        && core::mem::offset_of!(TjC, w3) == 32
        && core::mem::offset_of!(TjA, base) == 32
        && core::mem::size_of::<TailRow>() == 64
        && core::mem::offset_of!(TailRow, m) == 4
        && core::mem::offset_of!(TailRow, pat) == 8
        && core::mem::offset_of!(TailRow, pp) == 24
        && core::mem::size_of::<DateEnt>() == 32
        && core::mem::offset_of!(EvBatch, ts) == 0
        && core::mem::offset_of!(EvBatch, pp) == 512
        && BATCH_ALIGN == 512
        && TAIL_K == 72
        && FB == 32
        && NB == 64
);
const E_TAIL: usize = core::mem::offset_of!(Tables, tail);
const E_PP: usize = core::mem::offset_of!(EvBatch, pp);
const E_EID: usize = core::mem::offset_of!(EvBatch, eid);
const E_UID: usize = core::mem::offset_of!(EvBatch, uid);
const E_ING: usize = core::mem::offset_of!(EvBatch, ing);
const E_SCR: usize = core::mem::offset_of!(EvBatch, scratch) - 512;
const E_N: usize = core::mem::offset_of!(EvBatch, n) - 512;

#[cold]
#[inline(never)]
unsafe extern "C" fn ev_redo<S: EventSink>(cx: *const Cx<EvBatch, S>, p: *const u8, s: *mut i64) -> Pair {
    unsafe {
        count(15);
        let mut w = s;
        let k = ev_masks(p);
        let np = ev_fast::<S, false>(&*cx, p, k, k.next(p), &mut w);
        Pair(np as u64, w as u64)
    }
}

macro_rules! ev_consts {
    () => {
        concat!(
            "vmovdqa ymm15, ymmword ptr [rip + {EVC} + 224]\n",
            "vmovdqa ymm14, ymmword ptr [rip + {EVC} + 256]\n",
            "vmovdqa ymm13, ymmword ptr [rip + {EVC} + 288]\n",
            "vmovdqa64 ymm16, ymmword ptr [rip + {EVC} + 352]\n",
            "vmovdqa64 ymm24, ymmword ptr [rip + {EVC} + 0]\n",
            "vmovdqa64 ymm25, ymmword ptr [rip + {EVC} + 32]\n",
            "vmovdqa64 ymm26, ymmword ptr [rip + {EVC} + 64]\n",
            "vmovdqa64 ymm28, ymmword ptr [rip + {EVC} + 320]\n",
            "vmovdqa64 ymm19, ymmword ptr [rip + {LXC} + 224]\n",
            "xor r11d, r11d\n",
            "mov eax, 0xffe\n",
            "kmovd k5, eax\n",
            "mov eax, 12\n",
            "kmovd k1, eax\n",
        )
    };
}
macro_rules! ev_save_b {
    () => {
        concat!(
            "mov qword ptr [rsp + 40], rcx\n",
            "vmovdqu ymmword ptr [rsp + 448], ymm12\n",
            "vmovdqu ymmword ptr [rsp + 64], ymm11\n",
            "vmovdqu ymmword ptr [rsp + 96], ymm10\n",
            "vmovdqu64 ymmword ptr [rsp + 128], ymm27\n",
            "vmovdqu64 ymmword ptr [rsp + 160], ymm29\n",
            "vmovdqu64 ymmword ptr [rsp + 192], ymm17\n",
            "vmovdqu64 ymmword ptr [rsp + 224], ymm18\n",
            "vmovdqu64 ymmword ptr [rsp + 352], ymm22\n",
            "vmovdqu64 ymmword ptr [rsp + 384], ymm23\n",
            "kmovq qword ptr [rsp + 416], k7\n",
            "kmovq qword ptr [rsp + 424], k4\n",
        )
    };
}
macro_rules! ev_restore_b {
    () => {
        concat!(
            "mov rcx, qword ptr [rsp + 40]\n",
            "vmovdqu ymm12, ymmword ptr [rsp + 448]\n",
            "vmovdqu ymm11, ymmword ptr [rsp + 64]\n",
            "vmovdqu ymm10, ymmword ptr [rsp + 96]\n",
            "vmovdqu64 ymm27, ymmword ptr [rsp + 128]\n",
            "vmovdqu64 ymm29, ymmword ptr [rsp + 160]\n",
            "vmovdqu64 ymm17, ymmword ptr [rsp + 192]\n",
            "vmovdqu64 ymm18, ymmword ptr [rsp + 224]\n",
            "vmovdqu64 ymm22, ymmword ptr [rsp + 352]\n",
            "vmovdqu64 ymm23, ymmword ptr [rsp + 384]\n",
            "kmovq k7, qword ptr [rsp + 416]\n",
            "kmovq k4, qword ptr [rsp + 424]\n",
            ev_consts!(),
        )
    };
}
macro_rules! ev_next {
    () => {
        concat!(
            "vpcmpeqb k0, ymm14, ymmword ptr [rdi + 64]\n",
            "vpcmpeqb k6, ymm14, ymmword ptr [rdi + 96]\n",
            "kunpckdq k0, k6, k0\n",
            "kmovq rax, k0\n",
            "tzcnt rax, rax\n",
            "jnc 127f\n",
            "vpcmpeqb k0, ymm14, ymmword ptr [rdi + 128]\n",
            "kmovd eax, k0\n",
            "tzcnt eax, eax\n",
            "add eax, 64\n",
            "127:\n",
            "lea rdi, [rdi + rax + 65]\n",
        )
    };
}
macro_rules! ev_mk0 {
    () => {
        concat!(
            "kmovq rax, k7\n",
            "tzcnt rax, rax\n",
        )
    };
}
macro_rules! ev_mk {
    ($a:literal, $b:literal, $cl:literal, $ch:literal) => {
        concat!(
            "lea ", $a, ", [", $b, " + rax + 65]\n",
            "vpcmpeqb k7, ymm14, ymmword ptr [", $a, " + 64]\n",
            "vpcmpeqb k6, ymm14, ymmword ptr [", $a, " + 96]\n",
            "kunpckdq k7, k6, k7\n",
            "vpcmpeqb ", $cl, ", ymm15, ymmword ptr [", $a, "]\n",
            "vpcmpeqb ", $ch, ", ymm15, ymmword ptr [", $a, " + 32]\n",
            "prefetcht0 byte ptr [", $a, " + {PFD}]\n",
        )
    };
}
macro_rules! ev_dec {
    ($jo:literal, $x:literal, $d:literal, $b2:literal, $all:literal, $r:literal) => {
        concat!(
            "vpshufb ymm0, ", $x, ", ymmword ptr [{TJL} + ", $jo, " + 32768]\n",
            "vpsubb ymm1, ymm0, ymmword ptr [{TJL} + ", $jo, " + 32800]\n",
            "vpmaddubsw ymm0, ymm1, ymmword ptr [{TJL} + ", $jo, " + 32864]\n",
            "vpmaddwd ymm2, ymm0, ymmword ptr [{TJL} + ", $jo, " + 32896]\n",
            "vpackusdw ymm0, ymm2, ymm2\n",
            "vpmaddwd ymm0, ymm0, ymmword ptr [{TJL} + ", $jo, " + 32928]\n",
            "vpsubusb ", $all, ", ymm1, ymmword ptr [{TJL} + ", $jo, " + 32832]\n",
            "vpcmpgtd ymm2, ymm2, ymmword ptr [{TJL} + ", $jo, " + 32960]\n",
            "vpmuludq ymm5, ymm0, ymmword ptr [{TJL} + ", $jo, " + 33024]\n",
            "vpsrlq ymm0, ymm0, 32\n",
            "vpsignd ymm0, ymm0, ymm1\n",
            "vpmuldq ymm0, ymm0, ymmword ptr [{TJL} + ", $jo, " + 33056]\n",
            "vpaddq ymm5, ymm5, ", $d, "\n",
            "vpaddq ", $r, ", ymm5, ymm0\n",
            "vpternlogq ", $all, ", ymm2, ", $b2, ", 0xfe\n",
        )
    };
}
macro_rules! ev_hd {
    ($b:literal, $cl:literal, $ch:literal) => {
        concat!(
            "vpmovmskb ecx, ", $cl, "\n",
            "vpmovmskb edx, ", $ch, "\n",
            "tzcnt edx, edx\n",
            "lea rsi, [", $b, " + rdx + 32]\n",
            "tzcnt r8d, ecx\n",
            "blsr ecx, ecx\n",
            "tzcnt r9d, ecx\n",
        )
    };
}
macro_rules! ev_st {
    ($b:literal, $jo:literal, $x:literal, $d:literal, $b2:literal, $b2x:literal, $v:literal, $ids:expr, $fail:literal, $plab:literal, $pret:literal, $ppst:literal, $lng:literal, $rj:literal) => {
        concat!(
            $lng,
            "cmp eax, 31\n",
            "ja ", $fail, "f\n",
            "vmovdqu ymm3, ymmword ptr [", $b, " + r8 - 16]\n",
            "lea r8, [r9 + r8*8]\n",
            "shl r8, 7\n",
            "vpshufb ymm3, ymm3, ymmword ptr [{EJ} + r8]\n",
            "vpsubb ", $v, ", ymm3, ymmword ptr [{EJ} + r8 + 32]\n",
            "vpaddb ymm4, ", $v, ", ymm24\n",
            "vpsubusb ymm4, ymm4, ymm26\n",
            "vpsubusb ymm5, ", $v, ", ymm25\n",
            "vpminub ymm5, ymm5, ymm4\n",
            $rj, ":\n",
            $ids,
            "cmp byte ptr [rsi + 1], 34\n",
            "sete r11b\n",
            "lea ", $jo, ", [r11 + rdx*2]\n",
            "shl ", $jo, ", 9\n",
            "lea rdx, [rsi + r11*2]\n",
            "movzx edi, byte ptr [rdx + 23]\n",
            "shl edi, 6\n",
            "vmovdqu ymm6, ymmword ptr [", $b, " + rax + 32]\n",
            "add rax, ", $b, "\n",
            "vpcmpeqb k2, ymm13, ymm6\n",
            "vpcmpeqb ymm1, ymm14, ymm6\n",
            "vpcmpeqb k3, ymm15, ymm6\n",
            "kunpckdq k3, k3, k2\n",
            "kmovq r10, k3\n",
            "lzcnt r9, r10\n",
            "sub rax, rdx\n",
            "sub rax, qword ptr [{K3} + rdi + 40]\n",
            "shl rax, 6\n",
            "imul r9, r9, 13824\n",
            "add r9, rax\n",
            "mov rcx, qword ptr [{TL2} + r9 + 48]\n",
            "and r10, qword ptr [{TL2} + r9]\n",
            "pext r8d, r10d, dword ptr [{TL2} + r9 + 8]\n",
            "mov r8, qword ptr [{TL2} + r9 + r8*8 + 16]\n",
            "cmp r10, r8\n",
            "jne ", $fail, "f\n",

            "vmovdqu xmm6, xmmword ptr [rdx + 21]\n",
            "vpxor xmm6, xmm6, xmmword ptr [{K3} + rdi]\n",
            "vpand xmm6, xmm6, xmmword ptr [{K3} + rdi + 16]\n",
            "mov r8, qword ptr [{TJL} + ", $jo, " + 33144]\n",
            "add r8, rsi\n",
            "movzx r9d, word ptr [r8 + 9]\n",
            "pext r9, r9, qword ptr [{TJL} + ", $jo, " + 33128]\n",
            "shl r9d, 5\n",
            "vmovdqa ymm2, ymmword ptr [r15 + r9 + {SCR}]\n",
            "vpxor xmm3, xmm2, xmmword ptr [r8]\n",
            "vpandq ", $b2x, ", xmm3, xmmword ptr [{TJL} + ", $jo, " + 32992]\n",
            "movzx r10d, word ptr [rsi + r11 + 9]\n",
            "pext r10, r10, qword ptr [{TJL} + ", $jo, " + 33136]\n",
            "shl r10d, 5\n",
            "vmovdqa ymm4, ymmword ptr [r15 + r10 + {SCR}]\n",
            "vpsubb xmm7{{k5}}{{z}}, xmm4, xmmword ptr [rsi + r11]\n",
            "vpternlogq ymm7, ymm6, ymm5, 0xfe\n",
            "vpternlogq ", $b2, ", ymm7, ymm1, 0xfe\n",
            "vmovdqu64 ", $x, ", ymmword ptr [rsi - 16]\n",
            "vpermt2q ", $x, ", ymm16, ymmword ptr [rdx - 11]\n",
            "vshufi64x2 ", $d, ", ymm2, ymm4, 3\n",
            "test ecx, ecx\n",
            "jz ", $plab, "f\n",
            $pret, ":\n",
            $ppst,
        )
    };
}
macro_rules! ev_ids_ab {
    () => {
        "vpunpckhqdq ymm3, ymm22, ymm3\nvpaddb ymm4, ymm3, ymmword ptr [rip + {EVC} + 96]\nvpminub ymm3, ymm3, ymm4\nvpmaddubsw ymm3, ymm3, ymmword ptr [rip + {EVC} + 128]\nvpmaddwd ymm3, ymm3, ymmword ptr [rip + {EVC} + 160]\nvpmuludq ymm4, ymm3, ymmword ptr [rip + {EVC} + 192]\nvpsrlq ymm3, ymm3, 32\nvpaddq ymm3, ymm3, ymm4\nvmovdqu64 ymm3 {{k4}}, ymmword ptr [rsp + 256]\nvmovdqu xmmword ptr [r15 + rbx + {EID}], xmm3\nvmovdqu64 ymmword ptr [r15 + rbx + {UID} - 16] {{k1}}, ymm3\n"
    };
}
macro_rules! ev_retry {
    ($lab:literal, $b:literal, $cl:literal, $a:literal, $jo:literal, $x:literal, $d:literal, $b2:literal, $b2x:literal, $v:literal, $ids:expr, $real:literal, $pl:literal, $pr:literal, $ret:literal) => {
        concat!(
            $lab, ":\n",
            "vpmovmskb ecx, ", $cl, "\n",
            "tzcnt r8d, ecx\n",
            "blsr ecx, ecx\n",
            "tzcnt r9d, ecx\n",
            "blsr ecx, ecx\n",
            "jz ", $real, "f\n",
            "tzcnt edx, ecx\n",
            "vpcmpeqb ymm0, ymm14, ymmword ptr [", $b, " + 64]\n",
            "vpmovmskb eax, ymm0\n",
            "tzcnt eax, eax\n",
            "cmp eax, edx\n",
            "jae ", $real, "f\n",
            "lea rsi, [", $b, " + rdx]\n",
            "sub rdx, r9\n",
            "sub rdx, 32\n",
            ev_st!($b, $jo, $x, $d, $b2, $b2x, $v, $ids, $real, $pl, $pr, "", "", "39"),
            "jmp ", $ret, "b\n",
            ev_pur!($pl, $pr, $real, $a, $b, ev_save_b, ev_restore_b),
        )
    };
}
macro_rules! ev_hx {
    ($src:expr, $o:literal, $t1:literal, $t2:literal, $t3:literal) => {
        ev_hx!($src, $o, $t1, $t2, $t3, "ymm5")
    };
    ($src:expr, $o:literal, $t1:literal, $t2:literal, $t3:literal, $bad:literal) => {
        concat!(
            "vmovdqu64 ", $o, ", ymmword ptr ", $src, "\n",
            "vpaddb ", $t1, ", ", $o, ", ymmword ptr [rip + {LXC}]\n",
            "vpaddb ", $t2, ", ", $o, ", ymmword ptr [rip + {LXC} + 32]\n",
            "vpaddb ", $t3, ", ", $o, ", ymmword ptr [rip + {LXC} + 64]\n",
            "vpminub ", $o, ", ", $t1, ", ", $t3, "\n",
            "vpsubusb ", $t1, ", ", $t1, ", ymmword ptr [rip + {LXC} + 96]\n",
            "vpsubusb ", $t2, ", ", $t2, ", ymmword ptr [rip + {LXC} + 128]\n",
            "vpminub ", $t1, ", ", $t1, ", ", $t2, "\n",
            "vporq ", $bad, ", ", $bad, ", ", $t1, "\n",
            "vpmaddubsw ", $o, ", ", $o, ", ymmword ptr [rip + {LXC} + 160]\n",
            "vpshufb ", $o, ", ", $o, ", ymmword ptr [rip + {LXC} + 192]\n",
        )
    };
}
macro_rules! ev_lng {
    ($lab:literal, $b:literal, $v:literal, $fail:literal, $rj:literal, $lane:literal, $sd:literal, $chk:literal, $stash:literal, $chkll:literal, $ll:literal, $a:literal, $acl:literal, $ach:literal) => {
        concat!(
            $lab, ":\n",
            "vpcmpeqb ymm0, ymm15, ymmword ptr [", $b, " + 24]\n",
            "vpcmpeqb ymm1, ymm15, ymmword ptr [", $b, " + 56]\n",
            "vpmovmskb edi, ymm0\n",
            "vpmovmskb edx, ymm1\n",
            "tzcnt edx, edx\n",
            "lea rsi, [", $b, " + rdx + 56]\n",
            "tzcnt r10d, edi\n",
            "blsr edi, edi\n",
            "tzcnt r9d, edi\n",
            "lea edi, [r9 + r10*8 - 40]\n",
            "lea ecx, [r10 + 24]\n",
            "cmp r8d, 32\n",
            "cmovne r9d, r10d\n",
            "cmovne edi, r8d\n",
            "cmovne ecx, r8d\n",
            "setne r8b\n",
            "add r8d, r8d\n",
            "cmp r9d, 31\n",
            "ja ", $ll, "f\n",
            "cmp eax, 24\n",
            "jb ", $fail, "f\n",
            "cmp eax, 55\n",
            "ja ", $fail, "f\n",
            "sub rdx, r9\n",
            "lea r9, [", $b, " + rcx + 1]\n",
            "test r8d, r8d\n",
            "cmovz r9, ", $b, "\n",
            "lea r10, [", $b, " + r10 - 9]\n",
            "sub r10, r9\n",
            "test r10d, -3\n",
            "jnz ", $fail, "f\n",
            "shr r10d, 1\n",
            "vmovdqu ymm3, ymmword ptr [", $b, " + rcx - 16]\n",
            "shl edi, 7\n",
            "vpshufb ymm3, ymm3, ymmword ptr [{EJ} + rdi + {EJE7}]\n",
            "vpsubb ", $v, ", ymm3, ymmword ptr [{EJ} + rdi + {EJE7} + 32]\n",
            "vpaddb ymm4, ", $v, ", ymm24\n",
            "vpsubusb ymm4, ymm4, ymm26\n",
            "vpsubusb ymm5, ", $v, ", ymm25\n",
            "vpminub ymm5, ymm5, ymm4\n",
            "lea ecx, [r8*8 + 101]\n",
            "cmp byte ptr [r9 + r10], cl\n",
            "jne ", $fail, "f\n",
            "cmp byte ptr [r9], 34\n",
            "sete cl\n",
            "cmp byte ptr [r9 + r10*2 + 32], 34\n",
            "sete dil\n",
            "and cl, dil\n",
            "cmp cl, r10b\n",
            "jne ", $fail, "f\n",
            ev_hx!("[r9 + r10 + 1]", "ymm0", "ymm1", "ymm2", "ymm6"),
            "vextracti128 xmm1, ymm0, 1\n",
            $chk,
            "vmovq qword ptr [r15 + rbx + {EHI} + ", $sd, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 512 + ", $sd, "], xmm1\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 1024 + ", $sd, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 1536 + ", $sd, "], xmm1\n",
            "129:\n",
            "vpternlogq xmm1, xmm19, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vmovq qword ptr [rsp + r8*8 + 256 + 8*", $lane, "], xmm1\n",
            "kmovb k2, byte ptr [{LXC} + 288 + 4*", $lane, " + r8]\n",
            "korb k4, k4, k2\n",
            "jmp ", $rj, "b\n",
            $stash,
            $ll, ":\n",
            "test r8d, r8d\n",
            "jnz ", $fail, "f\n",
            "cmp r9d, 32\n",
            "jne ", $fail, "f\n",
            $chkll,
            "cmp eax, 48\n",
            "jb ", $fail, "f\n",
            "cmp eax, 64\n",
            "jb 128f\n",
            "vpcmpeqb ymm0, ymm14, ymmword ptr [", $b, " + 128]\n",
            "vpmovmskb ecx, ymm0\n",
            "tzcnt ecx, ecx\n",
            "cmp ecx, 15\n",
            "ja ", $fail, "f\n",
            "lea eax, [rcx + 64]\n",
            "lea ", $a, ", [", $b, " + rax + 65]\n",
            "vpcmpeqb k7, ymm14, ymmword ptr [", $a, " + 64]\n",
            "vpcmpeqb k6, ymm14, ymmword ptr [", $a, " + 96]\n",
            "kunpckdq k7, k6, k7\n",
            "vpcmpeqb ", $acl, ", ymm15, ymmword ptr [", $a, "]\n",
            "vpcmpeqb ", $ach, ", ymm15, ymmword ptr [", $a, " + 32]\n",
            "prefetcht0 byte ptr [", $a, " + {PFD}]\n",
            "128:\n",
            "vpcmpeqb ymm0, ymm15, ymmword ptr [", $b, " + 48]\n",
            "vpcmpeqb ymm1, ymm15, ymmword ptr [", $b, " + 80]\n",
            "vpmovmskb ecx, ymm0\n",
            "vpmovmskb edx, ymm1\n",
            "tzcnt edx, edx\n",
            "lea rsi, [", $b, " + rdx + 80]\n",
            "tzcnt ecx, ecx\n",
            "cmp ecx, 31\n",
            "ja ", $fail, "f\n",
            "sub rdx, rcx\n",
            "lea r9d, [r10 - 9]\n",
            "test r9d, -3\n",
            "jnz ", $fail, "f\n",
            "shr r9d, 1\n",
            "lea r8, [", $b, " + r10 + 25]\n",
            "sub ecx, r10d\n",
            "sub ecx, 10\n",
            "test ecx, -3\n",
            "jnz ", $fail, "f\n",
            "shr ecx, 1\n",
            "cmp byte ptr [", $b, " + r9], 101\n",
            "jne ", $fail, "f\n",
            "cmp byte ptr [", $b, "], 34\n",
            "sete dil\n",
            "cmp byte ptr [", $b, " + r9*2 + 32], 34\n",
            "sete r10b\n",
            "and dil, r10b\n",
            "cmp dil, r9b\n",
            "jne ", $fail, "f\n",
            "cmp byte ptr [r8 + rcx], 117\n",
            "jne ", $fail, "f\n",
            "cmp byte ptr [r8], 34\n",
            "sete dil\n",
            "cmp byte ptr [r8 + rcx*2 + 32], 34\n",
            "sete r10b\n",
            "and dil, r10b\n",
            "cmp dil, cl\n",
            "jne ", $fail, "f\n",
            "vpxor xmm5, xmm5, xmm5\n",
            "vpxord ", $v, ", ", $v, ", ", $v, "\n",
            ev_hx!(concat!("[", $b, " + r9 + 1]"), "ymm0", "ymm1", "ymm2", "ymm6"),
            ev_hx!("[r8 + rcx + 1]", "ymm7", "ymm1", "ymm2", "ymm6"),
            "vextracti128 xmm1, ymm0, 1\n",
            "vextracti128 xmm2, ymm7, 1\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + ", $sd, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 512 + ", $sd, "], xmm1\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 1024 + ", $sd, "], xmm7\n",
            "vmovq qword ptr [r15 + rbx + {EHI} + 1536 + ", $sd, "], xmm2\n",
            "vpternlogq xmm1, xmm19, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vpternlogq xmm2, xmm19, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vmovq qword ptr [rsp + 256 + 8*", $lane, "], xmm1\n",
            "vmovq qword ptr [rsp + 272 + 8*", $lane, "], xmm2\n",
            "kmovb k2, byte ptr [{LXC} + 296 + ", $lane, "]\n",
            "korb k4, k4, k2\n",
            "jmp ", $rj, "b\n",
        )
    };
}
macro_rules! ev_pur {
    ($plab:literal, $pret:literal, $fail:literal, $a:literal, $b:literal, $save:ident, $restore:ident) => {
        concat!(
            $plab, ":\n",
            "cmp qword ptr [{K3} + rdi + 56], 0\n",
            "je ", $fail, "f\n",
            "vmovdqu ymm0, ymmword ptr [", $a, " - 33]\n",
            "vpcmpeqb ymm1, ymm15, ymm0\n",
            "vpmovmskb r8d, ymm1\n",
            "vpcmpeqb ymm0, ymm13, ymm0\n",
            "vpmovmskb r9d, ymm0\n",
            "lzcnt edx, r8d\n",
            "shlx r8d, r8d, edx\n",
            "add r8d, r8d\n",
            "lzcnt r8d, r8d\n",
            "cmp r8d, 31\n",
            "ja ", $fail, "f\n",
            "mov esi, 32\n",
            "sub esi, edx\n",
            "shrx r10, r9, rsi\n",
            "and r10d, 1\n",
            "mov ecx, esi\n",
            "sub ecx, r8d\n",
            "dec ecx\n",
            "shrx rcx, r9, rcx\n",
            "and ecx, 1\n",
            "vmovdqu xmm0, xmmword ptr [", $a, " + rsi - 50]\n",
            "lea r8, [rcx + r8*2]\n",
            "shl r8, 6\n",
            "vpshufb xmm0, xmm0, xmmword ptr [{T} + r8 + {PROD}]\n",
            "vpsubb xmm0, xmm0, xmmword ptr [{T} + r8 + {PROD} + 16]\n",
            "vmovdqu xmm1, xmmword ptr [{T} + r8 + {PROD} + 32]\n",
            "vpsubusb xmm1, xmm1, xmm0\n",
            "vpsubusb xmm2, xmm0, xmmword ptr [rip + {PUC}]\n",
            "vpor xmm1, xmm1, xmm2\n",
            "vptest xmm1, xmm1\n",
            "jnz ", $fail, "f\n",
            "vpmaddubsw xmm0, xmm0, xmmword ptr [rip + {PUC} + 16]\n",
            "vpmaddwd xmm0, xmm0, xmmword ptr [rip + {PUC} + 32]\n",
            "vpackusdw xmm0, xmm0, xmm0\n",
            "vpmaddwd xmm0, xmm0, xmmword ptr [rip + {PUC} + 48]\n",
            "lea rdi, [r10 + r10]\n",
            "sub rdx, rdi\n",
            "jz 46f\n",
            "cmp rdx, 8\n",
            "ja ", $fail, "f\n",
            "neg r10\n",
            "mov rdi, qword ptr [", $a, " + r10 - 9]\n",
            "shl edx, 3\n",
            "neg edx\n",
            "mov r10, -1\n",
            "shlx r10, r10, rdx\n",
            "movabs r9, 0x3030303030303030\n",
            "xor rdi, r9\n",
            "and rdi, r10\n",
            "movabs r9, 0x0606060606060606\n",
            "lea r10, [rdi + r9]\n",
            "or r10, rdi\n",
            "movabs r9, 0xF0F0F0F0F0F0F0F0\n",
            "test r10, r9\n",
            "jnz 46f\n",
            "imul rdi, rdi, 2561\n",
            "shr rdi, 8\n",
            "movabs r9, 0x00FF00FF00FF00FF\n",
            "and rdi, r9\n",
            "imul rdi, rdi, 6553601\n",
            "shr rdi, 16\n",
            "movabs r9, 0x0000FFFF0000FFFF\n",
            "and rdi, r9\n",
            "movabs r9, 42949672960001\n",
            "imul rdi, r9\n",
            "shr rdi, 32\n",
            "lea r9d, [rdi - 1]\n",
            "cmp r9d, 10000000\n",
            "jae 46f\n",
            "vmovd ecx, xmm0\n",
            "shl rdi, 32\n",
            "or rcx, rdi\n",
            "bts rcx, 57\n",
            "jmp ", $pret, "b\n",
            "46:\n",
            "vmovd ecx, xmm0\n",
            "movabs rdi, 0x8200000000000000\n",
            "or rcx, rdi\n",
            "jmp ", $pret, "b\n",
        )
    };
}

#[repr(C, align(64))]
struct PuConst([u64; 8]);
#[repr(C, align(64))]
struct LxConst {
    d: [u8; 32],
    t: [u8; 32],
    u: [u8; 32],
    n9: [u8; 32],
    n5: [u8; 32],
    w: [u16; 16],
    sh: [u8; 32],
    mask: [u64; 4],
    long: [u64; 4],
    kt: [u8; 32],
}
const fn lx_const() -> LxConst {
    let mut c = LxConst { d: [0xD0; 32], t: [0x9F; 32], u: [0xA9; 32], n9: [9; 32], n5: [5; 32], w: [0x0110; 16], sh: [0x80; 32], mask: [LONG_MASK; 4], long: [ID_LONG; 4], kt: [0; 32] };
    c.kt[0] = 1;
    c.kt[2] = 4;
    c.kt[4] = 2;
    c.kt[6] = 8;
    c.kt[8] = 5;
    c.kt[9] = 10;
    c.kt[16] = 1;
    c.kt[17] = 4;
    c.kt[18] = b'k';
    c.kt[19] = b'u';
    let mut i = 0;
    while i < 8 {
        c.sh[i] = (14 - 2 * i) as u8;
        c.sh[16 + i] = (14 - 2 * i) as u8;
        i += 1;
    }
    c
}
static LXC: LxConst = lx_const();
const _: () = assert!(
    core::mem::offset_of!(LxConst, w) == 160
        && core::mem::offset_of!(LxConst, sh) == 192
        && core::mem::offset_of!(LxConst, mask) == 224
        && core::mem::offset_of!(LxConst, long) == 256
        && core::mem::offset_of!(LxConst, kt) == 288
        && core::mem::offset_of!(EvBatch, uhi) == core::mem::offset_of!(EvBatch, ehi) + 1024
        && core::mem::offset_of!(EvBatch, elo) == core::mem::offset_of!(EvBatch, ehi) + 512
        && core::mem::offset_of!(EvBatch, ulo) == core::mem::offset_of!(EvBatch, ehi) + 1536
);
const E_EHI: usize = core::mem::offset_of!(EvBatch, ehi);
static PUC: PuConst = PuConst([0x0909_0909_0909_0909, 0, 0x010A_010A_010A_010A, 0, 0x0001_0064_0001_0064, 0, 0x0001_2710, 0]);
const E_PROD: usize = core::mem::offset_of!(Tables, prod);
const _: () = assert!(PP_INVALID == 1 << 63 && core::mem::size_of::<DecShape>() == 64);

#[inline(never)]
unsafe fn ev_loop<S: EventSink>(p: *const u8, w: *mut i64, b: *mut EvBatch, cx: *const Cx<EvBatch, S>, lim: *const u8, sink: *mut S) -> (u64, *const u8, *mut i64) {
    let (code, np, nw): (u64, *const u8, *mut i64);
    unsafe {
        if (w as usize) & 8 != 0 {
            let r = ev_redo::<S>(cx, p, w);
            return (0, r.0 as *const u8, r.1 as *mut i64);
        }
        core::arch::asm!(
            "push rbx",
            "push rbp",
            "mov rax, rsp",
            "and rsp, -64",
            "sub rsp, 512",
            "mov qword ptr [rsp + 504], rax",
            "mov qword ptr [rsp + 8], rcx",
            "mov qword ptr [rsp + 24], r8",
            "mov qword ptr [rsp + 32], r9",
            "lea r15, [rdx + 512]",
            "mov rbx, rsi",
            "sub rbx, r15",
            "mov rbp, rdi",
            "mov qword ptr [rsp], rbp",
            "94:",
            ev_consts!(),
            "kxorw k4, k4, k4",
            "mov byte ptr [rsp + 328], 0",
            "vpcmpeqb k7, ymm14, ymmword ptr [rbp + 64]",
            "vpcmpeqb k6, ymm14, ymmword ptr [rbp + 96]",
            "kunpckdq k7, k6, k7",
            "vpcmpeqb ymm9, ymm15, ymmword ptr [rbp]",
            "vpcmpeqb ymm8, ymm15, ymmword ptr [rbp + 32]",
            ev_mk0!(),
            ev_hd!("rbp", "ymm9", "ymm8"),
            ev_mk!("r12", "rbp", "ymm11", "ymm10"),
            ev_st!("rbp", "r13", "ymm29", "ymm17", "ymm18", "xmm18", "ymm22", "", "40", "52", "53", "", "jc 131f\nsub rdx, r9\n", "132"),
            "jmp 22f",
            ".p2align 5",
            "22:",
            "mov qword ptr [r15 + rbx + {PP}], rcx",
            ev_mk0!(),
            ev_hd!("r12", "ymm11", "ymm10"),
            ev_mk!("rbp", "r12", "ymm9", "ymm8"),
            ev_dec!("r13", "ymm29", "ymm17", "ymm18", "ymm23", "ymm31"),
            ev_st!("r12", "r14", "ymm30", "ymm20", "ymm21", "xmm21", "ymm3",
                ev_ids_ab!(),
                "41", "54", "55", "mov qword ptr [r15 + rbx + {PP} + 8], rcx\n", "jc 133f\nsub rdx, r9\n", "134"),
            "kxorw k4, k4, k4",
            ev_mk0!(),
            ev_hd!("rbp", "ymm9", "ymm8"),
            ev_mk!("r12", "rbp", "ymm11", "ymm10"),
            ev_dec!("r14", "ymm30", "ymm20", "ymm21", "ymm27", "ymm4"),
            "vpermt2q ymm31, ymm28, ymm4",
            "vmovdqu64 xmmword ptr [r15 + rbx], xmm31",
            "vmovdqu64 ymmword ptr [r15 + rbx + {ING} - 16] {{k1}}, ymm31",
            ev_st!("rbp", "r13", "ymm29", "ymm17", "ymm18", "xmm18", "ymm22", "", "42", "56", "57", "", "jc 135f\nsub rdx, r9\n", "136"),
            "vpord ymm3, ymm27, ymm23",
            "vptest ymm3, ymm3",
            "jnz 75f",
            "23:",
            "mov qword ptr [rsp], rbp",
            "add rbx, 16",
            "jnz 22b",
            ev_save_b!(),
            "mov qword ptr [r15 + {NOFF}], 64",
            "mov rdi, qword ptr [rsp + 32]",
            "vzeroupper",
            "call {FL}",
            ev_restore_b!(),
            "mov rbx, -512",
            "cmp byte ptr [rsp + 328], 0",
            "jne 138f",
            "139:",
            "cmp rbp, qword ptr [rsp + 24]",
            "jbe 22b",
            "mov rdi, rbp",
            "lea rsi, [r15 - 512]",
            "xor eax, eax",
            "jmp 99f",
            "138:",
            "mov byte ptr [rsp + 328], 0",
            "mov rdi, qword ptr [rsp + 304]",
            "mov rsi, qword ptr [rsp + 312]",
            "mov qword ptr [r15 + {EHI} - 512], rdi",
            "mov qword ptr [r15 + {EHI}], rsi",
            "mov qword ptr [r15 + {EHI} + 512], rdi",
            "mov qword ptr [r15 + {EHI} + 1024], rsi",
            "jmp 139b",
            ev_lng!("131", "rbp", "ymm22", "40", "132", "0", "0", "", "", "", "142", "r12", "ymm11", "ymm10"),
            ev_lng!("133", "r12", "ymm3", "41", "134", "1", "8", "", "", "", "143", "rbp", "ymm9", "ymm8"),
            ev_lng!("135", "rbp", "ymm22", "42", "136", "0", "16", "cmp rbx, -16\nje 137f\n", "137:\nvmovq qword ptr [rsp + 304], xmm0\nvmovq qword ptr [rsp + 312], xmm1\nmov byte ptr [rsp + 328], 1\njmp 129b\n", "cmp rbx, -16\nje 42f\n", "144", "r12", "ymm11", "ymm10"),
            ev_pur!("52", "53", "40", "r12", "rbp", ev_save_b, ev_restore_b),
            ev_pur!("54", "55", "41", "rbp", "r12", ev_save_b, ev_restore_b),
            ev_pur!("56", "57", "42", "r12", "rbp", ev_save_b, ev_restore_b),
            ev_retry!("40", "rbp", "ymm9", "r12", "r13", "ymm29", "ymm17", "ymm18", "xmm18", "ymm22", "", "72", "43", "44", "53"),
            ev_retry!("41", "r12", "ymm11", "rbp", "r14", "ymm30", "ymm20", "ymm21", "xmm21", "ymm3", ev_ids_ab!(), "73", "58", "59", "55"),
            ev_retry!("42", "rbp", "ymm9", "r12", "r13", "ymm29", "ymm17", "ymm18", "xmm18", "ymm22", "", "74", "60", "61", "57"),
            "72:",
            "mov rdi, rbp",
            "lea rsi, [r15 + rbx]",
            "jmp 90f",
            "73:",
            "vmovdqa64 ymm0, ymm23",
            "vptest ymm0, ymm0",
            "jnz 78f",
            "vpunpckhqdq ymm3, ymm22, ymm22",
            "vpaddb ymm4, ymm3, ymmword ptr [rip + {EVC} + 96]",
            "vpminub ymm3, ymm3, ymm4",
            "vpmaddubsw ymm3, ymm3, ymmword ptr [rip + {EVC} + 128]",
            "vpmaddwd ymm3, ymm3, ymmword ptr [rip + {EVC} + 160]",
            "vpmuludq ymm4, ymm3, ymmword ptr [rip + {EVC} + 192]",
            "vpsrlq ymm3, ymm3, 32",
            "vpaddq ymm3, ymm3, ymm4",
            "vmovdqu64 ymm3 {{k4}}, ymmword ptr [rsp + 256]",
            "vmovq qword ptr [r15 + rbx + {EID}], xmm3",
            "vextracti128 xmm0, ymm3, 1",
            "vmovq qword ptr [r15 + rbx + {UID}], xmm0",
            "vmovhps qword ptr [r15 + rbx], xmm31",
            "vextracti32x4 xmm0, ymm31, 1",
            "vmovq qword ptr [r15 + rbx + {ING}], xmm0",
            "mov rdi, r12",
            "lea rsi, [r15 + rbx + 8]",
            "jmp 90f",
            "78:",
            "mov rdi, qword ptr [rsp]",
            "lea rsi, [r15 + rbx]",
            "jmp 90f",
            "74:",
            "vmovdqa64 ymm0, ymm23",
            "vptest ymm0, ymm0",
            "jnz 78b",
            "vptestmq k0, ymm27, ymm27",
            "kortestb k0, k0",
            "jnz 79f",
            "mov rdi, rbp",
            "lea rsi, [r15 + rbx + 16]",
            "cmp rbx, -16",
            "jne 90f",
            "mov qword ptr [r15 + {NOFF}], 64",
            "mov rdi, qword ptr [rsp + 32]",
            "vzeroupper",
            "call {FL}",
            "mov rdi, rbp",
            "lea rsi, [r15 - 512]",
            "jmp 90f",
            "79:",
            "mov rdi, qword ptr [rsp]",
            ev_next!(),
            "lea rsi, [r15 + rbx + 8]",
            "jmp 90f",
            "90:",
            "mov rdx, rsi",
            "mov rsi, rdi",
            "mov rdi, qword ptr [rsp + 8]",
            "vzeroupper",
            "call {LF}",
            "91:",
            "cmp rax, qword ptr [rsp + 24]",
            "ja 92f",
            "test dl, 8",
            "jz 93f",
            "mov rsi, rax",
            "mov rdi, qword ptr [rsp + 8]",
            "vzeroupper",
            "call {REDO}",
            "jmp 91b",
            "92:",
            "mov rdi, rax",
            "mov rsi, rdx",
            "xor eax, eax",
            "jmp 99f",
            "93:",
            "mov rbp, rax",
            "mov rbx, rdx",
            "sub rbx, r15",
            "mov qword ptr [rsp], rbp",
            "jmp 94b",
            "75:",
            ev_save_b!(),
            "mov rdi, qword ptr [rsp]",
            ev_next!(),
            "mov qword ptr [rsp + 16], rdi",
            "mov word ptr [rsp + 48], 0",
            "vmovdqa64 ymm0, ymm23",
            "vptest ymm0, ymm0",
            "jz 76f",
            "mov rdi, qword ptr [rsp]",
            "lea rsi, [r15 + rbx]",
            "call 70f",
            "mov byte ptr [rsp + 48], al",
            "76:",
            "vmovdqa64 ymm0, ymm27",
            "vptest ymm0, ymm0",
            "jz 77f",
            "mov rdi, qword ptr [rsp + 16]",
            "lea rsi, [r15 + rbx + 8]",
            "call 70f",
            "mov byte ptr [rsp + 49], al",
            "77:",
            "vmovdqa64 ymm0, ymm23",
            "vptest ymm0, ymm0",
            "jz 82f",
            "cmp byte ptr [rsp + 48], 2",
            "je 88f",
            "mov rdi, qword ptr [rsp]",
            "vmovdqu ymm0, ymmword ptr [rip + {EVC} + 256]",
            "vpcmpeqb k0, ymm0, ymmword ptr [rdi + 64]",
            "vpcmpeqb k6, ymm0, ymmword ptr [rdi + 96]",
            "kunpckdq k0, k6, k0",
            "kmovq rax, k0",
            "tzcnt rax, rax",
            "jnc 127f",
            "vpcmpeqb k0, ymm0, ymmword ptr [rdi + 128]",
            "kmovd eax, k0",
            "tzcnt eax, eax",
            "add eax, 64",
            "127:",
            "lea rax, [rdi + rax + 65]",
            "cmp rax, qword ptr [rsp + 16]",
            "jne 88f",
            "cmp byte ptr [rsp + 48], 0",
            "je 81f",
            "mov rsi, rdi",
            "mov rdi, qword ptr [rsp + 8]",
            "lea rdx, [r15 + rbx]",
            "vzeroupper",
            "call {FIX}",
            "test al, al",
            "jnz 81f",
            "88:",
            "mov rdi, qword ptr [rsp + 8]",
            "mov rsi, qword ptr [rsp]",
            "lea rdx, [r15 + rbx]",
            "vzeroupper",
            "call {REDO}",
            "cmp rax, qword ptr [rsp + 16]",
            "jne 84f",
            "lea rcx, [r15 + rbx + 8]",
            "cmp rdx, rcx",
            "jne 84f",
            "mov rdi, qword ptr [rsp]",
            "vmovdqu ymm0, ymmword ptr [rip + {EVC} + 256]",
            "vpcmpeqb k0, ymm0, ymmword ptr [rdi + 64]",
            "vpcmpeqb k6, ymm0, ymmword ptr [rdi + 96]",
            "kunpckdq k0, k6, k0",
            "kmovq rax, k0",
            "tzcnt rax, rax",
            "jnc 127f",
            "vpcmpeqb k0, ymm0, ymmword ptr [rdi + 128]",
            "kmovd eax, k0",
            "tzcnt eax, eax",
            "add eax, 64",
            "127:",
            "lea rax, [rdi + rax + 65]",
            "cmp rax, qword ptr [rsp + 16]",
            "jne 86f",
            "81:",
            "vmovdqu64 ymm0, ymmword ptr [rsp + 128]",
            "vptest ymm0, ymm0",
            "jz 83f",
            "82:",
            "cmp byte ptr [rsp + 49], 1",
            "jb 83f",
            "ja 89f",
            "mov rdi, qword ptr [rsp + 8]",
            "mov rsi, qword ptr [rsp + 16]",
            "lea rdx, [r15 + rbx + 8]",
            "vzeroupper",
            "call {FIX}",
            "test al, al",
            "jnz 83f",
            "89:",
            "mov rdi, qword ptr [rsp + 8]",
            "mov rsi, qword ptr [rsp + 16]",
            "lea rdx, [r15 + rbx + 8]",
            "vzeroupper",
            "call {REDO}",
            "cmp rax, rbp",
            "jne 84f",
            "test dl, 8",
            "jnz 84f",
            "mov rbx, rdx",
            "sub rbx, r15",
            "cmp rbx, -512",
            "jne 141f",
            "cmp byte ptr [rsp + 328], 0",
            "je 141f",
            "mov byte ptr [rsp + 328], 0",
            "mov rdi, qword ptr [rsp + 304]",
            "mov rsi, qword ptr [rsp + 312]",
            "mov qword ptr [r15 + {EHI} - 512], rdi",
            "mov qword ptr [r15 + {EHI}], rsi",
            "mov qword ptr [r15 + {EHI} + 512], rdi",
            "mov qword ptr [r15 + {EHI} + 1024], rsi",
            "141:",
            "cmp rbp, qword ptr [rsp + 24]",
            "ja 85f",
            "sub rbx, 16",
            "83:",
            ev_restore_b!(),
            "jmp 23b",
            "70:",
            "mov qword ptr [rsp + 488], r13",
            "mov qword ptr [rsp + 496], r14",
            "mov qword ptr [rsp + 504], rsi",
            "mov byte ptr [rsp + 64], 0",
            "mov r14, rdi",
            "vpcmpeqb ymm8, ymm15, ymmword ptr [r14]",
            "vpcmpeqb ymm9, ymm15, ymmword ptr [r14 + 32]",
            ev_hd!("r14", "ymm8", "ymm9"),
            "jc 68f",
            "sub rdx, r9",
            "blsr ecx, ecx",
            "jnz 68f",
            "vpmovmskb eax, ymm9",
            "test eax, eax",
            "jz 68f",
            "lea r10, [r9 - 1]",
            "cmp r10, r8",
            "je 65f",
            "lea r10, [r9 - 3]",
            "cmp r10, r8",
            "jne 64f",
            "cmp word ptr [r14 + r8 + 1], 0x2222",
            "jne 64f",
            "65:",
            "imul r9, r8, -7",
            "add r9, {EJE}",
            "mov byte ptr [rsp + 64], 1",
            "64:",
            "lea r10, [r9 + r8*8]",
            "shl r10, 7",
            "vmovdqu ymm3, ymmword ptr [r14 + r8 - 16]",
            "vpshufb ymm3, ymm3, ymmword ptr [{EJ} + r10]",
            "vpsubb ymm4, ymm3, ymmword ptr [{EJ} + r10 + 32]",
            "vpaddb ymm3, ymm4, ymm24",
            "vpsubusb ymm3, ymm3, ymm26",
            "vpsubusb ymm4, ymm4, ymm25",
            "vpminub ymm4, ymm4, ymm3",
            "vptest ymm4, ymm4",
            "jnz 68f",
            "71:",
            "vpcmpeqb ymm0, ymm14, ymmword ptr [r14 + 64]",
            "vpmovmskb eax, ymm0",
            "tzcnt eax, eax",
            "vpxor xmm10, xmm10, xmm10",
            ev_st!("r14", "r13", "ymm8", "ymm9", "ymm10", "xmm0", "ymm11", "", "68", "66", "67", "", "", "39"),
            "vmovdqa xmm12, xmm0",
            "vpunpckhqdq ymm1, ymm11, ymm11",
            "vpaddb ymm2, ymm1, ymmword ptr [rip + {EVC} + 96]",
            "vpminub ymm1, ymm1, ymm2",
            "vpmaddubsw ymm1, ymm1, ymmword ptr [rip + {EVC} + 128]",
            "vpmaddwd ymm1, ymm1, ymmword ptr [rip + {EVC} + 160]",
            "vpmuludq ymm2, ymm1, ymmword ptr [rip + {EVC} + 192]",
            "vpsrlq ymm1, ymm1, 32",
            "vpaddq ymm1, ymm1, ymm2",
            "mov r10, qword ptr [rsp + 504]",
            "vmovq qword ptr [r10 + {EID}], xmm1",
            "vextracti128 xmm2, ymm1, 1",
            "vmovq qword ptr [r10 + {UID}], xmm2",
            "vpxord ymm19, ymm19, ymm19",
            ev_dec!("r13", "ymm8", "ymm9", "ymm19", "ymm11", "ymm3"),
            "mov rsi, qword ptr [rsp + 504]",
            "vextracti128 xmm2, ymm3, 1",
            "vmovq qword ptr [rsi + {ING}], xmm2",
            "vextracti128 xmm0, ymm11, 1",
            "vpor ymm0, ymm0, ymm10",
            "vptest ymm0, ymm0",
            "jnz 68f",
            "cmp byte ptr [rsp + 64], 0",
            "je 62f",
            "mov qword ptr [rsi + {UID}], -1",
            "62:",
            "vpor xmm0, xmm11, xmm12",
            "mov eax, 1",
            "vptest xmm0, xmm0",
            "jnz 69f",
            "vmovhps qword ptr [rsi], xmm3",
            "xor eax, eax",
            "jmp 69f",
            "66:",
            "cmp rax, qword ptr [{K3} + rdi + 48]",
            "jne 68f",
            "jmp 67b",
            "68:",
            "mov eax, 2",
            "69:",
            "mov r13, qword ptr [rsp + 488]",
            "mov r14, qword ptr [rsp + 496]",
            "ret",
            "84:",
            "mov rdi, rax",
            "mov rsi, rdx",
            "xor eax, eax",
            "jmp 99f",
            "86:",
            "mov rdi, qword ptr [rsp + 16]",
            "mov rsi, rdx",
            "xor eax, eax",
            "jmp 99f",
            "85:",
            "mov rdi, rbp",
            "lea rsi, [r15 + rbx]",
            "xor eax, eax",
            "99:",
            "mov rsp, qword ptr [rsp + 504]",
            "pop rbp",
            "pop rbx",
            inout("rdi") p => np,
            inout("rsi") w => nw,
            inout("rdx") b => _,
            inout("rcx") cx => _,
            inout("r8") lim => _,
            inout("r9") sink => _,
            out("rax") code,
            out("r10") _, out("r11") _, out("r12") _, out("r13") _, out("r14") _, out("r15") _,
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _, out("zmm12") _, out("zmm13") _, out("zmm14") _, out("zmm15") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _, out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _, out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
            out("k0") _, out("k1") _, out("k2") _, out("k3") _, out("k4") _, out("k5") _, out("k6") _, out("k7") _,
            T = sym T, TJL = sym TJL, EVC = sym EVC, EJ = sym EJ, K3 = sym K3, TL2 = sym TL2, PUC = sym PUC, FL = sym flush_ev::<S>, REDO = sym ev_redo::<S>, FIX = sym ev_fix::<S>, LF = sym ev_lf::<S>,
            PROD = const E_PROD, EJE = const EJ_E, LXC = sym LXC, EJE7 = const EJ_E * 128, EHI = const E_EHI,
            PP = const E_PP, EID = const E_EID, UID = const E_UID, ING = const E_ING, SCR = const E_SCR, NOFF = const E_N, PFD = const PFD,
        );
    }
    (code, np, nw)
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct CtsRow {
    ctrl: [u8; 16],
    base: [u8; 16],
    lim: [u8; 16],
    w1: [i8; 16],
    w2: [i16; 8],
    w3: [i16; 8],
    limv: [i32; 4],
    kmask2: [u8; 16],
    kme: [i64; 2],
    kmf: [i64; 2],
    ksel: [u8; 16],
    koff1: i64,
    pmask: u64,
    _pad: [u64; 8],
}
const CTS_ROWS: usize = 65;
const fn cts_rows() -> [CtsRow; CTS_ROWS] {
    let (ja, jb, jc, jd, je, jf) = tj_rows();
    let mut t = [CtsRow { ctrl: [0; 16], base: [0; 16], lim: [0; 16], w1: [0; 16], w2: [0; 8], w3: [0; 8], limv: [0; 4], kmask2: [0; 16], kme: [0; 2], kmf: [0; 2], ksel: [0; 16], koff1: 0, pmask: 0, _pad: [0; 8] }; CTS_ROWS];
    let mut r = 0;
    while r < CTS_ROWS {
        let j = 2 * if r == 0 { 63 } else { r - 1 };
        let mut i = 0;
        while i < 16 {
            t[r].ctrl[i] = ja[j].ctrl[i];
            t[r].base[i] = ja[j].base[i];
            t[r].lim[i] = jb[j].lim[i];
            t[r].w1[i] = jb[j].w1[i];
            t[r].kmask2[i] = jd[j].kmask2[i];
            t[r].ksel[i] = jf[j].ksel[i];
            i += 1;
        }
        let mut i = 0;
        while i < 8 {
            t[r].w2[i] = jc[j].w2[i];
            t[r].w3[i] = jc[j].w3[i];
            i += 1;
        }
        let mut i = 0;
        while i < 4 {
            t[r].limv[i] = jd[j].limv[i];
            i += 1;
        }
        t[r].kme = [je[j].kme[0], je[j].kme[1]];
        t[r].kmf = [je[j].kmf[0], je[j].kmf[1]];
        t[r].koff1 = jf[j].koff1;
        t[r].pmask = jf[j].pmask;
        r += 1;
    }
    let mut q = 0;
    while q < 2 {
        let raw = 25 + 2 * q;
        let r = raw + 1;
        let old = t[r];
        let src = 16 - raw as i32;
        let b0 = 11 + q as i32;
        let mut row = old;
        row.ctrl = [0x80; 16];
        row.base = [0; 16];
        row.lim = [255; 16];
        row.w1 = [0; 16];
        let bs = *b"00:00:00";
        let l = [2u8, 9, 0, 5, 9, 0, 5, 9];
        let w = [10i8, 1, 0, 10, 1, 0, 10, 1];
        let mut i = 0;
        while i < 8 {
            row.ctrl[i] = (src + b0 + i as i32) as u8;
            row.base[i] = bs[i];
            row.lim[i] = l[i];
            row.w1[i] = w[i];
            i += 1;
        }
        let f = [(9, b'0', 2u8, 10i8), (10, b'0', 9, 1), (12, b'0', 5, 10), (13, b'0', 9, 1), (11, b':', 0, 0)];
        let mut i = 0;
        while i < 5 {
            row.ctrl[8 + i] = (src + b0 + f[i].0) as u8;
            row.base[8 + i] = f[i].1;
            row.lim[8 + i] = f[i].2;
            row.w1[8 + i] = f[i].3;
            i += 1;
        }
        if q == 1 {
            row.ctrl[13] = 15;
            row.base[13] = b'"';
            row.lim[13] = 0;
        }
        row.ctrl[14] = (src + b0 + 8) as u8;
        row.base[14] = b'+';
        row.lim[14] = 2;
        row.w1[14] = 1;
        row.ctrl[15] = (src + b0 + 8) as u8;
        row.base[15] = b'-';
        row.lim[15] = 255;
        row.w1[15] = 1;
        row.w2 = [60, 1, 60, 1, 60, 1, 0, 1];
        row.limv = [1430, i32::MAX, 1439, 255];
        row.w3 = [0, 0, 0, 0, 60, 1, 1, 0];
        row.ksel = [0; 16];
        row.ksel[15] = 0x80;
        row.kmf = [0, 60_000];
        row.lim[15] = 254;
        assert!(old.kme[1] == 1000 && old.kmf[1] == 60_000 && old.koff1 == q as i64 && old.pmask == DATE_PEXT && old.ksel[8] == 2 && old.w3[4] == 60 && old.w3[6] == 60);
        t[r] = row;
        q += 1;
    }
    let mut q = 0;
    while q < 2 {
        let mut n = 1;
        while n <= 16 {
            let raw = n + 2 * q;
            let r = raw + 1;
            if q == 0 && n == 16 {
                let old = t[r];
                assert!(old.kme[1] == 100_000_000 && old.pmask == 0 && old.koff1 == 0);
                let mut row = old;
                row.ctrl = [0x80; 16];
                row.base = [0; 16];
                row.lim = [0; 16];
                row.w1 = [0; 16];
                row.w2 = [0; 8];
                row.w3 = [0; 8];
                row.limv = [-1, i32::MAX, i32::MAX, i32::MAX];
                row.kme = [0, 0];
                row.kmf = [0, 0];
                t[r] = row;
            } else if (q == 0 && n <= 14) || (q == 1 && n == 13) {
                let old = t[r];
                assert!(old.kmf[1] == -1 && old.pmask == 0 && old.koff1 == 0 && old.kmask2[0] == 0 && old.kmask2[11] == 0);
                let mut row = old;
                row.ctrl = [0x80; 16];
                row.base = [0; 16];
                row.lim = [0; 16];
                row.w1 = [0; 16];
                row.w2 = [0; 8];
                row.w3 = [0; 8];
                row.limv = [i32::MAX; 4];
                let src = 16 - raw;
                row.base[15] = 255;
                row.lim[15] = 1;
                let p0 = 15 - n;
                let mut i = 0;
                while i < n {
                    row.ctrl[p0 + i] = (src + q + i) as u8;
                    row.base[p0 + i] = b'0';
                    row.lim[p0 + i] = 9;
                    i += 1;
                }
                if q == 1 {
                    row.ctrl[0] = src as u8;
                    row.base[0] = b'"';
                    row.ctrl[1] = 15;
                    row.base[1] = b'"';
                }
                let mut len = [0usize; 4];
                let mut g = 0;
                while g < 4 {
                    let mut e = 0;
                    let mut p = 4 * g;
                    while p < 4 * g + 4 {
                        if p >= p0 && p < 15 {
                            len[g] += 1;
                            e = p;
                        }
                        p += 1;
                    }
                    let mut k = 2 * g;
                    while k < 2 * g + 2 {
                        let mut m = 0;
                        let mut any = false;
                        let mut p = 2 * k;
                        while p < 2 * k + 2 {
                            if p >= p0 && p < 15 {
                                m = p;
                                any = true;
                            }
                            p += 1;
                        }
                        if any {
                            let mut p = 2 * k;
                            while p < 2 * k + 2 {
                                if p >= p0 && p < 15 {
                                    row.w1[p] = if m == p { 1 } else { 10 };
                                }
                                p += 1;
                            }
                            row.w2[k] = [1i16, 10, 100, 1000][e - m];
                        }
                        k += 1;
                    }
                    g += 1;
                }
                let p10 = [1i16, 10, 100, 1000, 10_000];
                row.w3 = [0, 0, 0, 0, p10[len[1]], 1, p10[len[3]], 1];
                let mut kme = 1i64;
                let mut i = 0;
                while i < len[2] + len[3] {
                    kme *= 10;
                    i += 1;
                }
                row.kme = [0, kme];
                row.kmf = [0, 1];
                assert!(len[0] + len[1] + len[2] + len[3] == n && len[0] + len[1] <= 8 && len[2] + len[3] <= 7);
                t[r] = row;
            }
            n += 1;
        }
        q += 1;
    }
    t
}
static CTS: [CtsRow; CTS_ROWS] = cts_rows();

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct CampRow {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lim: [u8; 16],
    _pad: [u8; 16],
}
const CAMPB_ROWS: usize = 320;
const CAMPB_OFF: usize = 64 * 64;
const fn campb_rows() -> [CampRow; CAMPB_ROWS] {
    let mut bad = CampRow { ctrl: [0x80; 16], subv: [0; 16], lim: [0; 16], _pad: [0; 16] };
    bad.subv[8] = 1;
    let mut t = [bad; CAMPB_ROWS];
    let mut q = 0;
    while q < 2 {
        let mut nd = 1;
        while nd <= 4 {
            let raw = 2 + nd + 2 * q;
            let mut s = CampRow { ctrl: [0x80; 16], subv: [0; 16], lim: [0; 16], _pad: [0; 16] };
            let fs = 15 - raw;
            let ds = 15 - q - nd;
            let mut j = 0;
            while j < 4 {
                s.lim[j] = 9;
                j += 1;
            }
            let mut j = 0;
            while j < nd {
                s.ctrl[4 - nd + j] = (ds + j) as u8;
                s.subv[4 - nd + j] = b'0';
                j += 1;
            }
            s.ctrl[8] = (fs + q) as u8;
            s.subv[8] = b'c';
            s.ctrl[9] = (fs + q + 1) as u8;
            s.subv[9] = b'_';
            s.ctrl[10] = 15;
            s.subv[10] = b',';
            if q == 1 {
                s.ctrl[11] = 14;
                s.subv[11] = b'"';
                s.ctrl[12] = fs as u8;
                s.subv[12] = b'"';
            }
            s.ctrl[13] = (fs - 1) as u8;
            s.subv[13] = b',';
            if nd > 1 {
                s.ctrl[14] = ds as u8;
                s.subv[14] = b'1';
                s.lim[14] = 8;
            }
            let b = if q == 1 { b'"' } else { b'c' } as usize;
            let idx = b + 64 - 2 * (raw + 1);
            assert!(t[idx].subv[8] == 1);
            t[idx] = s;
            nd += 1;
        }
        q += 1;
    }
    t
}
static CAMPB: [CampRow; CAMPB_ROWS] = campb_rows();

const CTJ_CLASSES: usize = 8;
const fn ctj_shape_idx(k: usize) -> usize {
    let q = k / 4;
    let nd = k % 4 + 1;
    let raw = 2 + nd + 2 * q;
    let b = if q == 1 { b'"' } else { b'c' } as usize;
    b + 64 - 2 * (raw + 1)
}

#[repr(C, align(128))]
#[derive(Clone, Copy)]
struct CjRow {
    ctrl: [u8; 32],
    subv: [u8; 32],
    thr: [i64; 4],
    _pad: [u8; 32],
}
const fn cj_thr(lz: &[u8; 16]) -> i64 {
    let mut thr = -1i64;
    let mut j = 0;
    while j < 8 {
        if lz[j] != 0 {
            assert!(thr == -1 && lz[j] == 1 && j < 7);
            let mut p = 1i64;
            let mut k = 0;
            while k < 7 - j {
                p *= 36;
                k += 1;
            }
            thr = p - 1;
        }
        j += 1;
    }
    let mut j = 8;
    while j < 16 {
        assert!(lz[j] == 0);
        j += 1;
    }
    thr
}
const CJ_O: usize = 32 * 8 + 32 + 1;
const CJ_ROWS: usize = CJ_O + 81;
const fn cj_rows() -> [CjRow; CJ_ROWS] {
    let k = id_shapes_raw(b'k', 0, 8, false);
    let u = id_shapes_raw(b'u', 0, 8, true);
    let mut bad = CjRow { ctrl: [0x80; 32], subv: [0; 32], thr: [-1; 4], _pad: [0; 32] };
    bad.subv[8] = 1;
    bad.subv[24] = 1;
    let mut t = [bad; CJ_ROWS];
    let mut c0 = 0;
    while c0 < 32 {
        let mut c1 = c0 + 1;
        while c1 < 32 {
            let d = c1 - c0 - 1;
            let a = &k[c0];
            let b = &u[d];
            if a.subv[8] != b'k' || b.orv[1] != 0 || d > 15 || b.subv[8] != b'u' {
                c1 += 1;
                continue;
            }
            let r = &mut t[c0 * 8 + c1];
            assert!(r.subv[8] == 1 && r.subv[24] == 1);
            let mut i = 0;
            while i < 16 {
                r.ctrl[i] = a.ctrl[i];
                r.subv[i] = a.subv[i];
                i += 1;
            }
            r.thr[0] = cj_thr(&a.lz);
            {
                r.thr[2] = cj_thr(&b.lz);
                let mut i = 0;
                while i < 16 {
                    let c = b.ctrl[i];
                    r.ctrl[16 + i] = if c == 0x80 {
                        0x80
                    } else {
                        let n = c as i32 + d as i32 - 15;
                        assert!(n >= 1 && n <= 15);
                        n as u8
                    };
                    r.subv[16 + i] = b.subv[i];
                    i += 1;
                }
            }
            c1 += 1;
        }
        c0 += 1;
    }
    let mut c0 = 0;
    while c0 < 32 {
        let a = &k[c0];
        if a.subv[8] == b'k' {
            let r = &mut t[CJ_O + c0];
            let mut i = 0;
            while i < 16 {
                r.ctrl[i] = a.ctrl[i];
                r.subv[i] = a.subv[i];
                r.ctrl[16 + i] = 0x80;
                r.subv[16 + i] = 0;
                i += 1;
            }
            r.thr[0] = cj_thr(&a.lz);
            r.thr[2] = -1;
        }
        c0 += 1;
    }
    let mut q0 = 0;
    while q0 < 2 {
        let c0 = 9 + 2 * q0;
        let mut c1 = c0 + 1;
        while c1 < 32 {
            let d = c1 - c0 - 1;
            let b = &u[d];
            let ix = CJ_O + 8 * c0 + c1 - 40;
            if b.orv[1] == 0 && d <= 15 && b.subv[8] == b'u' && !(c0 == 11 && c1 <= 15) {
                let r = &mut t[ix];
                assert!(r.subv[8] == 1 && r.subv[24] == 1);
                let mut i = 0;
                while i < 16 {
                    r.ctrl[i] = 0x80;
                    r.subv[i] = 0;
                    let c = b.ctrl[i];
                    r.ctrl[16 + i] = if c == 0x80 {
                        0x80
                    } else {
                        let n = c as i32 + d as i32 - 15;
                        assert!(n >= 1 && n <= 15);
                        n as u8
                    };
                    r.subv[16 + i] = b.subv[i];
                    i += 1;
                }
                r.thr[0] = -1;
                r.thr[2] = cj_thr(&b.lz);
            }
            c1 += 1;
        }
        q0 += 1;
    }
    t
}
static CJ: [CjRow; CJ_ROWS] = cj_rows();

#[repr(C, align(64))]
struct ClConst {
    comma: [u8; 32],
    nl: [u8; 32],
    e: [u8; 32],
    letlim: [u8; 32],
    diglim: [u8; 32],
    m39: [u8; 32],
    w1: [u16; 16],
    w2: [u32; 8],
    w3: [u64; 4],
    cw1: [u8; 16],
    cw2: [u32; 4],
    ksel: [u64; 2],
    lm0: [u64; 4],
    lm1: [u64; 4],
}
const fn cl_const() -> ClConst {
    let mut c = ClConst { comma: [b','; 32], nl: [b'\n'; 32], e: [0; 32], letlim: [0; 32], diglim: [0; 32], m39: [217; 32], w1: [0x0124; 16], w2: [0x0001_0510; 8], w3: [1_679_616; 4], cw1: [0; 16], cw2: [0; 4], ksel: [0, 0x80 << 56], lm0: [u64::MAX, u64::MAX, 0, 0], lm1: [0, 0, u64::MAX, u64::MAX] };
    let mut i = 0;
    while i < 32 {
        if i % 16 < 8 {
            c.e[i] = 207;
            c.letlim[i] = 25;
            c.diglim[i] = 9;
        }
        i += 1;
    }
    c.cw1 = [10, 1, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    c.cw2 = [0x0001_0064, 0, 0, 0];
    c
}
static CLC: ClConst = cl_const();
const _: () = assert!(
    core::mem::size_of::<CjRow>() == 128
        && core::mem::size_of::<CampRow>() == 64
        && core::mem::offset_of!(ClConst, nl) == 32
        && core::mem::offset_of!(ClConst, e) == 64
        && core::mem::offset_of!(ClConst, letlim) == 96
        && core::mem::offset_of!(ClConst, diglim) == 128
        && core::mem::offset_of!(ClConst, m39) == 160
        && core::mem::offset_of!(ClConst, w1) == 192
        && core::mem::offset_of!(ClConst, w2) == 224
        && core::mem::offset_of!(ClConst, w3) == 256
        && core::mem::offset_of!(ClConst, cw1) == 288
        && core::mem::offset_of!(ClConst, cw2) == 304
        && core::mem::offset_of!(ClConst, ksel) == 320
        && core::mem::offset_of!(ClConst, lm0) == 336
        && core::mem::offset_of!(ClConst, lm1) == 368
);

const C_CAMP: usize = core::mem::offset_of!(CkBatch, camp);
const C_CHI: usize = core::mem::offset_of!(CkBatch, chi);
const _: () = assert!(
    core::mem::offset_of!(CkBatch, clo) == core::mem::offset_of!(CkBatch, chi) + 512
        && core::mem::offset_of!(CkBatch, uhi) == core::mem::offset_of!(CkBatch, chi) + 1024
        && core::mem::offset_of!(CkBatch, ulo) == core::mem::offset_of!(CkBatch, chi) + 1536
);
const C_CID: usize = core::mem::offset_of!(CkBatch, cid);
const C_UID: usize = core::mem::offset_of!(CkBatch, uid);
const C_SCR: usize = core::mem::offset_of!(CkBatch, scratch) - 512;
const _: () = assert!(
    core::mem::offset_of!(CkBatch, ts) == 0
        && core::mem::offset_of!(CkBatch, camp) == 512
        && NB * 8 == 512
        && core::mem::size_of::<CtsRow>() == 256
        && core::mem::offset_of!(CtsRow, base) == 16
        && core::mem::offset_of!(CtsRow, lim) == 32
        && core::mem::offset_of!(CtsRow, w1) == 48
        && core::mem::offset_of!(CtsRow, w2) == 64
        && core::mem::offset_of!(CtsRow, w3) == 80
        && core::mem::offset_of!(CtsRow, limv) == 96
        && core::mem::offset_of!(CtsRow, kmask2) == 112
        && core::mem::offset_of!(CtsRow, kme) == 128
        && core::mem::offset_of!(CtsRow, kmf) == 144
        && core::mem::offset_of!(CtsRow, ksel) == 160
        && core::mem::offset_of!(CtsRow, koff1) == 176
        && core::mem::offset_of!(CtsRow, pmask) == 184
        && core::mem::size_of::<DecShape>() == 64
        && core::mem::offset_of!(DecShape, subv) == 16
        && core::mem::offset_of!(DecShape, lz) == 32
        && core::mem::size_of::<IdShape>() == 64
        && core::mem::offset_of!(IdShape, subv) == 16
        && core::mem::offset_of!(IdShape, lz) == 32
        && core::mem::offset_of!(IdShape, orv) == 48
        && core::mem::size_of::<DateEnt>() == 32
        && core::mem::offset_of!(DateEnt, ms) == 16
);

macro_rules! cl_ms {
    ($ab:tt, $a:literal, $b:literal, $cin:literal, $nlin:literal, $cout:literal, $nlout:literal, $bad:literal, $badx:literal, $ids:literal, $lab:literal, $rj:literal) => {
        concat!(
            "lea ", $a, ", [", $b, " + ", $nlin, " + 33]\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $a, "]\n",
            "vpmovmskb ", $cout, ", ymm0\n",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [", $a, " + 32]\n",
            "vpmovmskb ", $nlout, ", ymm1\n",
            "tzcnt ", $nlout, ", ", $nlout, "\n",
            "prefetcht0 byte ptr [", $a, " + {PFD}]\n",
            "tzcnt r8d, ", $cin, "\n",
            "blsr ", $cin, ", ", $cin, "\n",
            "tzcnt r9d, ", $cin, "\n",
            "jc ", $lab, "f\n",
            "blsr ", $cin, ", ", $cin, "\n",
            "tzcnt r10d, ", $cin, "\n",
            "vmovdqu ymm3, ymmword ptr [", $b, " + r8 - 16]\n",
            "movzx edx, byte ptr [", $b, " + r9 + 1]\n",
            "lea r8d, [r9 + r8*8]\n",
            "shl r8d, 7\n",
            "vpshufb ymm3, ymm3, ymmword ptr [{CJ} + r8]\n",
            "vpsubb ymm5, ymm3, ymmword ptr [{CJ} + r8 + 32]\n",
            "vpaddb ymm4, ymm5, ymm16\n",
            "vpsubusb ymm4, ymm4, ymm17\n",
            "vpsubusb ymm6, ymm5, ymm18\n",
            "vpminub ymm4, ymm4, ymm6\n",
            "vpaddb ymm6, ymm5, ymm19\n",
            "vpminub ymm5, ymm5, ymm6\n",
            "vpmaddubsw ymm5, ymm5, ymm20\n",
            "vpmaddwd ymm5, ymm5, ymm21\n",
            "vpmuludq ymm1, ymm5, ymm22\n",
            "vpsrlq ymm5, ymm5, 32\n",
            "vpaddq ", $ids, ", ymm1, ymm5\n",
            "vpcmpgtq ymm6, ", $ids, ", ymmword ptr [{CJ} + r8 + 64]\n",
            $rj, ":\n",
            "sub r9, r10\n",
            "lea rdx, [rdx + r9*2]\n",
            cl_ms!(@camp $ab),
            "lea rdx, [", $b, " + r10]\n",
            cl_ms!(@src $ab, $a),
            "sub r10, ", $nlin, "\n",
            cl_ms!(@row $ab),
            "movzx r9d, word ptr [rdx + 9]\n",
            cl_ms!(@pext $ab),
            "shl r9d, 5\n",
            "vmovdqu64 ", $badx, ", xmmword ptr [r15 + r9 + {SCR}]\n",
            "vpxorq ", $badx, ", ", $badx, ", xmmword ptr [rdx]\n",
            cl_ms!(@kmask $ab, $badx),
            "vpternlogq ", $bad, ", ymm4, ymm6, 0xfd\n",
            cl_ms!(@days $ab),
        )
    };
    (@camp a) => { "mov edi, dword ptr [{CCA} + rdx*4 + 256]\n" };
    (@camp b) => { concat!("mov r8d, dword ptr [{CCB} + rdx*4 + 256]\n", "add rdi, r8\n") };
    (@src a, $a:literal) => { concat!("vmovdqu64 xmm8, xmmword ptr [", $a, " - 17]\n", "vmovdqu64 xmm11, xmmword ptr [rdx - 15]\n") };
    (@src b, $a:literal) => { concat!("vmovdqu64 ymm8{{k3}}, ymmword ptr [", $a, " - 33]\n", "vmovdqu64 ymm11{{k3}}, ymmword ptr [rdx - 31]\n") };
    (@row a) => { concat!("imul rsi, r10, -{TS} * 65\n", "add rdx, qword ptr [{TJ} + rsi + {TDA} + 320]\n") };
    (@row b) => { concat!("imul r14, r10, -{TS}\n", "add r14, rsi\n", "add rdx, qword ptr [{TJ} + r14 + {TD} + 336]\n") };
    (@pext a) => { "pext r9, r9, qword ptr [{TJ} + rsi + {TDA} + 328]\n" };
    (@pext b) => { "pext r9, r9, qword ptr [{TJ} + r14 + {TD} + 344]\n" };
    (@kmask a, $badx:literal) => { concat!("vpandq ", $badx, ", ", $badx, ", xmmword ptr [{TJ} + rsi + {TDA} + 288]\n") };
    (@kmask b, $badx:literal) => { concat!("vpandq ", $badx, ", ", $badx, ", xmmword ptr [{TJ} + r14 + {TD} + 304]\n") };
    (@days a) => { "vmovdqu64 xmm9, xmmword ptr [r15 + r9 + {SCR} + 16]\n" };
    (@days b) => { "vmovdqu64 ymm9{{k3}}, ymmword ptr [r15 + r9 + {SCR}]\n" };
}
macro_rules! cl_lng {
    ($lab:literal, $a:literal, $b:literal, $cin:literal, $cin64:literal, $cin8:literal, $nlin:literal, $cout:literal, $nlout:literal, $ids:literal, $rj:literal, $so:literal, $fl:literal, $chk:literal, $stash:literal, $chkll:literal, $ll:literal) => {
        concat!(
            $lab, ":\n",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [", $b, " + 64]\n",
            "vpmovmskb edx, ymm1\n",
            "tzcnt edx, edx\n",
            "add edx, 32\n",
            "cmp ", $nlin, ", 32\n",
            "cmovb rdx, ", $nlin, "\n",
            "lea ", $a, ", [", $b, " + rdx + 33]\n",
            "mov ", $nlin, ", rdx\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $a, "]\n",
            "vpmovmskb ", $cout, ", ymm0\n",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [", $a, " + 32]\n",
            "vpmovmskb ", $nlout, ", ymm1\n",
            "tzcnt ", $nlout, ", ", $nlout, "\n",
            "prefetcht0 byte ptr [", $a, " + {PFD}]\n",
            "lea r10d, [", $nlin, " - 24]\n",
            "cmp r10d, 31\n",
            "ja ", $ll, "f\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $b, " + 24]\n",
            "vpmovmskb edx, ymm0\n",
            "tzcnt r10d, edx\n",
            "blsr edx, edx\n",
            "tzcnt r9d, edx\n",
            "blsr edx, edx\n",
            "tzcnt edx, edx\n",
            "lea r14d, [r9 + r10*8 - 40]\n",
            "lea ", $cin, ", [r10 + 24]\n",
            "cmp r8d, 32\n",
            "cmovne edx, r9d\n",
            "cmovne r9d, r10d\n",
            "cmovne r14d, r8d\n",
            "cmovne ", $cin, ", r8d\n",
            "setne r8b\n",
            "cmp r9d, 31\n",
            "ja ", $ll, "f\n",
            "vmovdqu ymm3, ymmword ptr [", $b, " + ", $cin64, " - 16]\n",
            "shl r14d, 7\n",
            "vpshufb ymm3, ymm3, ymmword ptr [{CJ} + r14 + {CJO7}]\n",
            "vpsubb ymm5, ymm3, ymmword ptr [{CJ} + r14 + {CJO7} + 32]\n",
            "vpaddb ymm4, ymm5, ymm16\n",
            "vpsubusb ymm4, ymm4, ymm17\n",
            "vpsubusb ymm6, ymm5, ymm18\n",
            "vpminub ymm4, ymm4, ymm6\n",
            "vpaddb ymm6, ymm5, ymm19\n",
            "vpminub ymm5, ymm5, ymm6\n",
            "vpmaddubsw ymm5, ymm5, ymm20\n",
            "vpmaddwd ymm5, ymm5, ymm21\n",
            "vpmuludq ymm1, ymm5, ymm22\n",
            "vpsrlq ymm5, ymm5, 32\n",
            "vpaddq ", $ids, ", ymm1, ymm5\n",
            "vpcmpgtq ymm6, ", $ids, ", ymmword ptr [{CJ} + r14 + {CJO7} + 64]\n",
            "lea r14, [", $b, " + ", $cin64, " + 1]\n",
            "test r8d, r8d\n",
            "cmovz r14, ", $b, "\n",
            "kmovb k5, byte ptr [{LXC} + 304 + r8]\n",
            "movzx r8d, byte ptr [{LXC} + 306 + r8]\n",
            "lea ", $cin64, ", [", $b, " + r10 - 9]\n",
            "sub ", $cin64, ", r14\n",
            "test ", $cin, ", -3\n",
            "jnz ", $fl, "f\n",
            "shr ", $cin, ", 1\n",
            "cmp byte ptr [r14 + ", $cin64, "], r8b\n",
            "jne ", $fl, "f\n",
            "movzx r10d, byte ptr [r14]\n",
            "shl r10d, 8\n",
            "mov r10b, byte ptr [r14 + ", $cin64, "*2 + 32]\n",
            "cmp r10d, 0x2222\n",
            "sete r10b\n",
            "cmp r10b, ", $cin8, "\n",
            "jne ", $fl, "f\n",
            ev_hx!(concat!("[r14 + ", $cin64, " + 1]"), "ymm0", "ymm1", "ymm2", "ymm27", "ymm4"),
            "vextracti128 xmm1, ymm0, 1\n",
            $chk,
            "vmovq qword ptr [r15 + rbx + {CHI} + ", $so, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 512 + ", $so, "], xmm1\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 1024 + ", $so, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 1536 + ", $so, "], xmm1\n",
            "129:\n",
            "vpternlogq xmm1, xmm29, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vpbroadcastq ymm1, xmm1\n",
            "vmovdqa64 ", $ids, " {{k5}}, ymm1\n",
            "lea r10d, [rdx + 24]\n",
            "add r9d, 24\n",
            "movzx edx, byte ptr [", $b, " + r9 + 1]\n",
            "jmp ", $rj, "b\n",
            $stash,
            $ll, ":\n",
            "test r8d, -33\n",
            "jnz ", $fl, "f\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $b, " + 24]\n",
            "vpmovmskb edx, ymm0\n",
            "tzcnt r10d, edx\n",
            "blsr edx, edx\n",
            "jnz ", $fl, "f\n",
            $chkll,
            "cmp ", $nlin, ", 48\n",
            "jb ", $fl, "f\n",
            "cmp ", $nlin, ", 64\n",
            "jb 128f\n",
            "vpcmpeqb ymm0, ymm12, ymmword ptr [", $b, " + 96]\n",
            "vpmovmskb edx, ymm0\n",
            "tzcnt edx, edx\n",
            "cmp edx, 15\n",
            "ja ", $fl, "f\n",
            "lea ", $nlin, ", [rdx + 64]\n",
            "lea ", $a, ", [", $b, " + rdx + 97]\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $a, "]\n",
            "vpmovmskb ", $cout, ", ymm0\n",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [", $a, " + 32]\n",
            "vpmovmskb ", $nlout, ", ymm1\n",
            "tzcnt ", $nlout, ", ", $nlout, "\n",
            "prefetcht0 byte ptr [", $a, " + {PFD}]\n",
            "128:\n",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [", $b, " + 48]\n",
            "vpmovmskb edx, ymm0\n",
            "tzcnt r9d, edx\n",
            "blsr edx, edx\n",
            "tzcnt edx, edx\n",
            "cmp r9d, 31\n",
            "ja ", $fl, "f\n",
            "lea r8d, [r10 - 9]\n",
            "test r8d, -3\n",
            "jnz ", $fl, "f\n",
            "shr r8d, 1\n",
            "lea r14, [", $b, " + r10 + 25]\n",
            "mov ", $cin, ", r9d\n",
            "sub ", $cin, ", r10d\n",
            "sub ", $cin, ", 10\n",
            "test ", $cin, ", -3\n",
            "jnz ", $fl, "f\n",
            "shr ", $cin, ", 1\n",
            "cmp byte ptr [", $b, " + r8], 107\n",
            "jne ", $fl, "f\n",
            "movzx r10d, byte ptr [", $b, "]\n",
            "shl r10d, 8\n",
            "mov r10b, byte ptr [", $b, " + r8*2 + 32]\n",
            "cmp r10d, 0x2222\n",
            "sete r10b\n",
            "cmp r10b, r8b\n",
            "jne ", $fl, "f\n",
            "cmp byte ptr [r14 + ", $cin64, "], 117\n",
            "jne ", $fl, "f\n",
            "movzx r10d, byte ptr [r14]\n",
            "shl r10d, 8\n",
            "mov r10b, byte ptr [r14 + ", $cin64, "*2 + 32]\n",
            "cmp r10d, 0x2222\n",
            "sete r10b\n",
            "cmp r10b, ", $cin8, "\n",
            "jne ", $fl, "f\n",
            "vpxor xmm4, xmm4, xmm4\n",
            "vpcmpeqd ymm6, ymm6, ymm6\n",
            ev_hx!(concat!("[", $b, " + r8 + 1]"), "ymm0", "ymm1", "ymm2", "ymm27", "ymm4"),
            ev_hx!(concat!("[r14 + ", $cin64, " + 1]"), "ymm28", "ymm1", "ymm2", "ymm27", "ymm4"),
            "vextracti128 xmm1, ymm0, 1\n",
            "vextracti32x4 xmm2, ymm28, 1\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + ", $so, "], xmm0\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 512 + ", $so, "], xmm1\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 1024 + ", $so, "], xmm28\n",
            "vmovq qword ptr [r15 + rbx + {CHI} + 1536 + ", $so, "], xmm2\n",
            "vpternlogq xmm1, xmm29, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vpternlogq xmm2, xmm29, xmmword ptr [rip + {LXC} + 256], 0xEA\n",
            "vinserti32x4 ", $ids, ", ymm1, xmm2, 1\n",
            "lea r10d, [rdx + 48]\n",
            "add r9d, 48\n",
            "movzx edx, byte ptr [", $b, " + r9 + 1]\n",
            "jmp ", $rj, "b\n",
            $fl, ":\n",
            "vpcmpeqd ymm4, ymm4, ymm4\n",
            "mov r9d, 32\n",
            "mov r10d, 32\n",
            "mov ", $nlin, ", 32\n",
            "movzx edx, byte ptr [", $b, " + 33]\n",
            "jmp ", $rj, "b\n",
        )
    };
}
macro_rules! cl_dp {
    ($o:literal, $v:literal) => {
        concat!(
            "vpshufb ymm0, ymm8, ymmword ptr [{TJ} + r14 + {TD}]\n",
            "vpsubb ymm0, ymm0, ymmword ptr [{TJ} + r14 + {TD} + 32]\n",
            "vpmaddubsw ymm1, ymm0, ymmword ptr [{TJ} + r14 + {TD} + 96]\n",
            "vpmaddwd ymm1, ymm1, ymmword ptr [{TJ} + r14 + {TD} + 128]\n",
            "vpackusdw ymm2, ymm1, ymm1\n",
            "vpmaddwd ymm2, ymm2, ymmword ptr [{TJ} + r14 + {TD} + 160]\n",
            "vpsubusb ", $v, ", ymm0, ymmword ptr [{TJ} + r14 + {TD} + 64]\n",
            "vpcmpgtd ymm5, ymm1, ymmword ptr [{TJ} + r14 + {TD} + 192]\n",
            "vpmuludq ymm4, ymm2, ymmword ptr [{TJ} + r14 + {TD} + 224]\n",
            "vpsignd ymm2, ymm2, ymm0\n",
            "vpsrlq ymm2, ymm2, 32\n",
            "vpmuldq ymm2, ymm2, ymmword ptr [{TJ} + r14 + {TD} + 256]\n",
            "vpaddq ymm4, ymm4, ymm9\n",
            "vpaddq ymm4, ymm4, ymm2\n",
            "vpshufb ymm6, ymm11, ymmword ptr [{CJ2} + rdi]\n",
            "vpsubb ymm6, ymm6, ymmword ptr [{CJ2} + rdi + 32]\n",
            "vpsubusb ymm2, ymm6, ymmword ptr [{CJ2} + rdi + 64]\n",
            "vpmaddubsw ymm6, ymm6, ymm25\n",
            "vpmaddwd ymm6, ymm6, ymm26\n",
            "vpternlogq ", $v, ", ymm5, ymm2, 0xfe\n",
            "vmovhps qword ptr [r15 + rbx + ", $o, " - 32], xmm4\n",
            "vmovdqu64 ymmword ptr [r15 + rbx + ", $o, " - 48]{{k4}}, ymm4\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " - 32 + {CAMPO}], xmm6\n",
            "vmovdqu64 ymmword ptr [r15 + rbx + ", $o, " - 40 + {CAMPO}]{{k2}}, ymm6\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " - 32 + {CIDO}], xmm15\n",
            "vmovdqu64 ymmword ptr [r15 + rbx + ", $o, " - 48 + {UIDO}]{{k2}}, ymm15\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " - 24 + {CIDO}], xmm7\n",
            "vmovdqu64 ymmword ptr [r15 + rbx + ", $o, " - 40 + {UIDO}]{{k2}}, ymm7\n",
        )
    };
}

#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct Tj2Row {
    ctrl: [u8; 32],
    base: [u8; 32],
    lim: [u8; 32],
    w1: [i8; 32],
    w2: [i16; 16],
    w3: [i16; 16],
    limv: [i32; 8],
    kme: [i64; 4],
    kmf: [i64; 4],
    kmask_a: [u8; 16],
    kmask_b: [u8; 16],
    koff_a: i64,
    pmask_a: u64,
    koff_b: i64,
    pmask_b: u64,
}
const TJ2_ROWS: usize = CTS_ROWS * CTS_ROWS;
const TJ2_SIZE: usize = 352;
const fn tj2_rows() -> [Tj2Row; TJ2_ROWS] {
    let cts = cts_rows();
    let z = Tj2Row { ctrl: [0; 32], base: [0; 32], lim: [0; 32], w1: [0; 32], w2: [0; 16], w3: [0; 16], limv: [0; 8], kme: [0; 4], kmf: [0; 4], kmask_a: [0; 16], kmask_b: [0; 16], koff_a: 0, pmask_a: 0, koff_b: 0, pmask_b: 0 };
    let mut t = [z; TJ2_ROWS];
    let mut ia = 0;
    while ia < CTS_ROWS {
        let mut ib = 0;
        while ib < CTS_ROWS {
            let o = &mut t[ia * CTS_ROWS + ib];
            let mut l = 0;
            while l < 2 {
                let s = if l == 0 { &cts[ia] } else { &cts[ib] };
                let mut i = 0;
                while i < 16 {
                    o.ctrl[16 * l + i] = s.ctrl[i];
                    o.base[16 * l + i] = s.base[i];
                    o.lim[16 * l + i] = s.lim[i];
                    o.w1[16 * l + i] = s.w1[i];
                    i += 1;
                }
                let mut i = 0;
                while i < 8 {
                    o.w2[8 * l + i] = s.w2[i];
                    o.w3[8 * l + i] = s.w3[i];
                    i += 1;
                }
                let mut i = 0;
                while i < 4 {
                    o.limv[4 * l + i] = s.limv[i];
                    i += 1;
                }
                o.kme[2 * l] = s.kme[0];
                o.kme[2 * l + 1] = s.kme[1];
                o.kmf[2 * l] = s.kmf[0];
                o.kmf[2 * l + 1] = s.kmf[1];
                l += 1;
            }
            o.kmask_a = cts[ia].kmask2;
            o.kmask_b = cts[ib].kmask2;
            o.koff_a = cts[ia].koff1;
            o.pmask_a = cts[ia].pmask;
            o.koff_b = cts[ib].koff1;
            o.pmask_b = cts[ib].pmask;
            ib += 1;
        }
        ia += 1;
    }
    t
}
static TJ2: [Tj2Row; TJ2_ROWS] = tj2_rows();
const _: () = assert!(
    core::mem::size_of::<Tj2Row>() == TJ2_SIZE
        && core::mem::offset_of!(Tj2Row, base) == 32
        && core::mem::offset_of!(Tj2Row, lim) == 64
        && core::mem::offset_of!(Tj2Row, w1) == 96
        && core::mem::offset_of!(Tj2Row, w2) == 128
        && core::mem::offset_of!(Tj2Row, w3) == 160
        && core::mem::offset_of!(Tj2Row, limv) == 192
        && core::mem::offset_of!(Tj2Row, kme) == 224
        && core::mem::offset_of!(Tj2Row, kmf) == 256
        && core::mem::offset_of!(Tj2Row, kmask_a) == 288
        && core::mem::offset_of!(Tj2Row, kmask_b) == 304
        && core::mem::offset_of!(Tj2Row, koff_a) == 320
        && core::mem::offset_of!(Tj2Row, pmask_a) == 328
        && core::mem::offset_of!(Tj2Row, koff_b) == 336
        && core::mem::offset_of!(Tj2Row, pmask_b) == 344
);

#[repr(C, align(128))]
#[derive(Clone, Copy)]
struct Cj2Row {
    ctrl: [u8; 32],
    subv: [u8; 32],
    lim: [u8; 32],
    _pad: [u8; 32],
}
const fn cj2_rows() -> [Cj2Row; CTJ_CLASSES * CTJ_CLASSES] {
    let camp = campb_rows();
    let mut t = [Cj2Row { ctrl: [0; 32], subv: [0; 32], lim: [0; 32], _pad: [0; 32] }; CTJ_CLASSES * CTJ_CLASSES];
    let mut ka = 0;
    while ka < CTJ_CLASSES {
        let mut kb = 0;
        while kb < CTJ_CLASSES {
            let o = &mut t[ka * CTJ_CLASSES + kb];
            let mut l = 0;
            while l < 2 {
                let c = &camp[ctj_shape_idx(if l == 0 { ka } else { kb })];
                assert!(c.subv[8] == b'c' && c.subv[10] == b',');
                let mut i = 0;
                while i < 16 {
                    o.ctrl[16 * l + i] = c.ctrl[i];
                    o.subv[16 * l + i] = c.subv[i];
                    o.lim[16 * l + i] = c.lim[i];
                    i += 1;
                }
                l += 1;
            }
            kb += 1;
        }
        ka += 1;
    }
    t
}
static CJ2: [Cj2Row; CTJ_CLASSES * CTJ_CLASSES] = cj2_rows();
const fn cj2_class(stride: u32) -> [u32; CAMPB_ROWS] {
    let mut t = [0u32; CAMPB_ROWS];
    let mut seen = [false; CAMPB_ROWS];
    let mut k = 0;
    while k < CTJ_CLASSES {
        let i = ctj_shape_idx(k);
        assert!(!seen[i]);
        seen[i] = true;
        t[i] = k as u32 * stride;
        k += 1;
    }
    t
}
static CJ2_A: [u32; CAMPB_ROWS] = cj2_class(128 * CTJ_CLASSES as u32);
static CJ2_B: [u32; CAMPB_ROWS] = cj2_class(128);
const _: () = assert!(core::mem::size_of::<Cj2Row>() == 128);

const CL_REACH: usize = (NB + 3) * 113 + 160;

#[repr(C)]
struct ClFlush {
    f: unsafe extern "C" fn(*mut u8),
    sink: *mut u8,
    lim: *const u8,
    lf: unsafe extern "C" fn(*const u8, *const u8, *mut i64) -> Pair,
    cx: *const u8,
}
const _: () = assert!(core::mem::offset_of!(ClFlush, lim) == 16 && core::mem::offset_of!(ClFlush, lf) == 24 && core::mem::offset_of!(ClFlush, cx) == 32);
const C_N: usize = core::mem::offset_of!(CkBatch, n) - 512;
macro_rules! cl_consts {
    () => {
        concat!(
            "vmovdqa ymm13, ymmword ptr [{K}]\n",
            "vmovdqa ymm12, ymmword ptr [{K} + 32]\n",
            "vmovdqa64 ymm16, ymmword ptr [{K} + 64]\n",
            "vmovdqa64 ymm17, ymmword ptr [{K} + 96]\n",
            "vmovdqa64 ymm18, ymmword ptr [{K} + 128]\n",
            "vmovdqa64 ymm19, ymmword ptr [{K} + 160]\n",
            "vmovdqa64 ymm20, ymmword ptr [{K} + 192]\n",
            "vmovdqa64 ymm21, ymmword ptr [{K} + 224]\n",
            "vmovdqa64 ymm22, ymmword ptr [{K} + 256]\n",
            "vbroadcasti32x4 ymm25, xmmword ptr [{K} + 288]\n",
            "vbroadcasti32x4 ymm26, xmmword ptr [{K} + 304]\n",
            "vmovdqa64 ymm29, ymmword ptr [rip + {LXC} + 224]\n",
            "mov eax, 4\n",
            "kmovw k2, eax\n",
            "mov eax, 12\n",
            "kmovw k3, eax\n",
            "mov eax, 8\n",
            "kmovw k4, eax\n",
        )
    };
}
#[inline(never)]
unsafe fn cl_loop(p: *const u8, w: *mut i64, b512: *mut u8, fl: &ClFlush) -> (u64, *const u8, *mut i64) {
    let (code, np, nw): (u64, *const u8, *mut i64);
    unsafe {
        core::arch::asm!(
            "push rbx",
            "push rbp",
            "sub rsp, 176",
            "mov qword ptr [rsp + 16], rcx",
            "mov r15, rdx",
            "71:",
            "mov byte ptr [rsp + 8], 0",
            "mov rbx, rsi",
            "sub rbx, r15",
            "add rbx, 32",
            cl_consts!(),
            "mov rbp, rdi",
            "mov qword ptr [rsp], rbp",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [rbp]",
            "vpmovmskb eax, ymm0",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rbp + 32]",
            "vpmovmskb ecx, ymm1",
            "tzcnt ecx, ecx",
            cl_ms!(a, "r12", "rbp", "eax", "rcx", "r11d", "r13d", "ymm10", "xmm10", "ymm15", "151", "161"),
            ".p2align 5",
            "2:",
            cl_ms!(b, "rbp", "r12", "r11d", "r13", "eax", "ecx", "ymm30", "xmm30", "ymm7", "152", "162"),
            cl_dp!("0", "ymm14"),
            cl_ms!(a, "r12", "rbp", "eax", "rcx", "r11d", "r13d", "ymm23", "xmm23", "ymm15", "153", "163"),
            cl_ms!(b, "rbp", "r12", "r11d", "r13", "eax", "ecx", "ymm24", "xmm24", "ymm7", "154", "164"),
            cl_dp!("16", "ymm3"),
            "vpternlogq ymm24, ymm3, ymm14, 0xfe",
            "vpternlogq ymm24, ymm10, ymm30, 0xfe",
            "vporq ymm24, ymm24, ymm23",
            "vptestmq k1, ymm24, ymm24",
            "kortestb k1, k1",
            "jnz 90f",
            "mov qword ptr [rsp], rbp",
            cl_ms!(a, "r12", "rbp", "eax", "rcx", "r11d", "r13d", "ymm10", "xmm10", "ymm15", "155", "165"),
            "add rbx, 32",
            "jle 2b",
            "jmp 95f",
            "90:",
            "mov eax, 1",
            "xor ecx, ecx",
            "vptest ymm10, ymm10",
            "jnz 92f",
            "vptest ymm14, ymmword ptr [{K} + 336]",
            "jnz 92f",
            "inc ecx",
            "vptestmq k1, ymm30, ymm30",
            "kortestb k1, k1",
            "jnz 92f",
            "vptest ymm14, ymmword ptr [{K} + 368]",
            "jnz 92f",
            "inc ecx",
            "vptestmq k1, ymm23, ymm23",
            "kortestb k1, k1",
            "jnz 92f",
            "vptest ymm3, ymmword ptr [{K} + 336]",
            "jnz 92f",
            "inc ecx",
            "92:",
            "lea rsi, [r15 + rbx - 32]",
            "lea rsi, [rsi + rcx*8]",
            "mov rdi, qword ptr [rsp]",
            "test ecx, ecx",
            "jz 96f",
            "94:",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rdi + 32]",
            "vpmovmskb r8d, ymm1",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rdi + 64]",
            "vpmovmskb r9d, ymm1",
            "shl r9, 32",
            "or r8, r9",
            "tzcnt r8, r8",
            "jnc 126f",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rdi + 96]",
            "vpmovmskb r9d, ymm1",
            "tzcnt r9d, r9d",
            "lea r8, [r9 + 64]",
            "126:",
            "lea rdi, [rdi + r8 + 33]",
            "dec ecx",
            "jnz 94b",
            "96:",
            "mov rdx, rsi",
            "mov rsi, rdi",
            "mov rax, qword ptr [rsp + 16]",
            "mov rdi, qword ptr [rax + 32]",
            "vzeroupper",
            "call qword ptr [rax + 24]",
            "mov rcx, qword ptr [rsp + 16]",
            "mov rdi, rax",
            "mov rsi, rdx",
            "cmp rax, qword ptr [rcx + 16]",
            "ja 97f",
            "mov rcx, rdx",
            "sub rcx, r15",
            "add rcx, 32",
            "jle 71b",
            "97:",
            "mov eax, 3",
            "jmp 6f",
            "95:",
            "mov rax, qword ptr [rsp + 16]",
            "cmp rbp, qword ptr [rax + 16]",
            "ja 80f",
            "lea rcx, [rbx - 32]",
            "sar rcx, 3",
            "add rcx, {NBV}",
            "mov qword ptr [r15 + {CN}], rcx",
            "mov qword ptr [rsp + 24], rsi",
            "mov qword ptr [rsp + 32], r11",
            "mov qword ptr [rsp + 40], rdi",
            "vmovdqu64 xmmword ptr [rsp + 48], xmm8",
            "vmovdqu64 xmmword ptr [rsp + 64], xmm9",
            "vmovdqu64 xmmword ptr [rsp + 80], xmm11",
            "vmovdqu64 ymmword ptr [rsp + 112], ymm10",
            "vmovdqu64 ymmword ptr [rsp + 144], ymm15",
            "mov rdi, qword ptr [rax + 8]",
            "vzeroupper",
            "call qword ptr [rax]",
            "mov rsi, qword ptr [rsp + 24]",
            "mov r11, qword ptr [rsp + 32]",
            "mov rdi, qword ptr [rsp + 40]",
            "vmovdqu64 xmm8, xmmword ptr [rsp + 48]",
            "vmovdqu64 xmm9, xmmword ptr [rsp + 64]",
            "vmovdqu64 xmm11, xmmword ptr [rsp + 80]",
            "vmovdqu64 ymm10, ymmword ptr [rsp + 112]",
            "vmovdqu64 ymm15, ymmword ptr [rsp + 144]",
            cl_consts!(),
            "mov rbx, 32 - {NBV} * 8",
            "cmp byte ptr [rsp + 8], 0",
            "je 2b",
            "mov byte ptr [rsp + 8], 0",
            "mov r8, qword ptr [rsp + 96]",
            "mov r9, qword ptr [rsp + 104]",
            "mov qword ptr [r15 + {CHI} - 512], r8",
            "mov qword ptr [r15 + {CHI}], r9",
            "mov qword ptr [r15 + {CHI} + 512], r8",
            "mov qword ptr [r15 + {CHI} + 1024], r9",
            "jmp 2b",
            cl_lng!("151", "r12", "rbp", "eax", "rax", "al", "rcx", "r11d", "r13d", "ymm15", "161", "-32", "171", "", "", "", "181"),
            cl_lng!("152", "rbp", "r12", "r11d", "r11", "r11b", "r13", "eax", "ecx", "ymm7", "162", "-24", "172", "", "", "", "182"),
            cl_lng!("153", "r12", "rbp", "eax", "rax", "al", "rcx", "r11d", "r13d", "ymm15", "163", "-16", "173", "", "", "", "183"),
            cl_lng!("154", "rbp", "r12", "r11d", "r11", "r11b", "r13", "eax", "ecx", "ymm7", "164", "-8", "174", "", "", "", "184"),
            cl_lng!("155", "r12", "rbp", "eax", "rax", "al", "rcx", "r11d", "r13d", "ymm15", "165", "0", "175", "cmp rbx, -32\njg 156f\n", "156:\nvmovq qword ptr [rsp + 96], xmm0\nvmovq qword ptr [rsp + 104], xmm1\nmov byte ptr [rsp + 8], 1\njmp 129b\n", "cmp rbx, -32\njg 175f\n", "185"),
            "80:",
            "mov rdi, rbp",
            "mov eax, 2",
            "lea rsi, [r15 + rbx - 32]",
            "6:",
            "add rsp, 176",
            "pop rbp",
            "pop rbx",
            inout("rdi") p => np,
            inout("rsi") w => nw,
            inout("rdx") b512 => _,
            out("rax") code,
            inout("rcx") fl as *const ClFlush => _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _, out("r12") _, out("r13") _, out("r14") _, out("r15") _,
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _, out("zmm12") _, out("zmm13") _, out("zmm14") _, out("zmm15") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _, out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _, out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
            out("k1") _, out("k2") _, out("k3") _, out("k4") _,
            TJ = sym TJ2, CJ2 = sym CJ2, CCA = sym CJ2_A, CCB = sym CJ2_B, CJ = sym CJ, K = sym CLC,
            TS = const TJ2_SIZE, TD = const 32 * (CTS_ROWS + 1) * TJ2_SIZE, TDA = const 32 * CTS_ROWS * TJ2_SIZE,
            CAMPO = const C_CAMP, CIDO = const C_CID, UIDO = const C_UID, SCR = const C_SCR, PFD = const PFD,
            CN = const C_N, NBV = const NB,
            LXC = sym LXC, CHI = const C_CHI, CJO7 = const CJ_O * 128,
            clobber_abi("C"),
        );
    }
    (code, np, nw)
}

#[inline(never)]
unsafe fn run_clicks<S: ClickSink, const MODE: u8>(buf: &[u8], sink: &mut S) {
    unsafe {
        let base = buf.as_ptr();
        let len = buf.len();
        let end = base.add(len);
        let b = sink.ck();
        let sink: *mut S = sink;
        let cx = &Cx { base, end, b, cache: (*b).scratch.as_mut_ptr() as *mut DateEnt, sink };
        let mut w = (*b).ts.as_mut_ptr().add((*b).n);
        let mut p = base;
        if MODE == PIPE && len >= CL_REACH {
            let enter = end.sub(CL_REACH);
            let b512 = (*b).camp.as_mut_ptr() as *mut u8;
            let fl = ClFlush {
                f: core::mem::transmute::<unsafe extern "C" fn(*mut S), unsafe extern "C" fn(*mut u8)>(flush_ck::<S>),
                sink: sink as *mut u8,
                lim: enter,
                lf: core::mem::transmute::<unsafe extern "C" fn(*const Cx<CkBatch, S>, *const u8, *mut i64) -> Pair, unsafe extern "C" fn(*const u8, *const u8, *mut i64) -> Pair>(cl_lf::<S>),
                cx: cx as *const Cx<CkBatch, S> as *const u8,
            };
            let k = ck_masks(p);
            p = cl_fast::<S, false>(cx, p, k, k.next(p), &mut w);
            while p <= enter {
                if w.offset_from((*b).ts.as_ptr()) as usize > NB - 4 {
                    (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
                    (*sink).ck_flush();
                    w = (*b).ts.as_mut_ptr();
                }
                let (code, np, nw) = cl_loop(p, w, b512, &fl);
                p = np;
                w = nw;
                if code == 2 {
                    (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
                    (*sink).ck_flush();
                    w = (*b).ts.as_mut_ptr();
                } else if code == 1 {
                    let k = ck_masks(p);
                    p = cl_fast::<S, false>(cx, p, k, k.next(p), &mut w);
                }
            }
        }
        if MODE == CHAIN {
            const REACH: usize = 2 * 64 + 1;
            if len >= REACH {
                let lim = end.sub(REACH);
                while p <= lim {
                    let k = ck_masks(p);
                    _mm_prefetch::<_MM_HINT_T0>(p.wrapping_add(PFD) as *const i8);
                    p = cl_fast::<S, false>(cx, p, k, k.next(p), &mut w);
                }
            }
        }
        while p.wrapping_add(64) <= end {
            let k = ck_masks(p);
            p = cl_fast::<S, true>(cx, p, k, k.next(p), &mut w);
        }
        (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
        let mut pos = p.offset_from(base) as usize;
        while pos < buf.len() {
            pos = scan_click(buf, pos, &mut *sink);
        }
    }
}

#[inline(always)]
fn find_nl(buf: &[u8], mut i: usize) -> usize {
    unsafe {
        let nl = _mm256_set1_epi8(b'\n' as i8);
        while i + 32 <= buf.len() {
            let v = _mm256_loadu_si256(buf.as_ptr().add(i) as *const __m256i);
            let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, nl)) as u32;
            if m != 0 {
                return i + m.trailing_zeros() as usize;
            }
            i += 32;
        }
    }
    while i < buf.len() && buf[i] != b'\n' {
        i += 1;
    }
    i
}

pub fn links<S: LinkSink>(buf: &[u8], sink: &mut S) {
    let len = buf.len();
    let mut s = 0;
    unsafe {
        let base = buf.as_ptr();
        while s + 64 <= len {
            let p = base.add(s);
            let x0 = _mm256_loadu_si256(p as *const __m256i);
            let x1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
            let m = |c: u8| -> u64 {
                let c = _mm256_set1_epi8(c as i8);
                (_mm256_movemask_epi8(_mm256_cmpeq_epi8(x0, c)) as u32 as u64) | (crate::v3b::opq(_mm256_movemask_epi8(_mm256_cmpeq_epi8(x1, c)) as u32) as u64) << 32
            };
            let nl = m(b'\n');
            if nl == 0 {
                let e = find_nl(buf, s);
                link_line(buf, s, e, sink);
                s = e + 1;
                continue;
            }
            let e = nl.trailing_zeros() as usize;
            let lm = (1u64 << e) - 1;
            link_fast(buf, s, e, m(b',') & lm, m(b'"') & lm, sink);
            s += e + 1;
        }
    }
    while s < len {
        let e = find_nl(buf, s);
        link_line(buf, s, e, sink);
        s = e + 1;
    }
    sink.lk_flush();
}

#[inline(always)]
unsafe fn b36x2(xa: u64, xb: u64) -> (u64, u64, u32) {
    unsafe {
        let v = _mm_set_epi64x(xb as i64, xa as i64);
        let t = _mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8));
        let bad = _mm_min_epu8(_mm_subs_epu8(t, _mm_set1_epi8(9)), _mm_subs_epu8(_mm_sub_epi8(t, _mm_set1_epi8(49)), _mm_set1_epi8(25)));
        let badm = (_mm_movemask_epi8(_mm_cmpeq_epi8(bad, _mm_setzero_si128())) as u32) ^ 0xFFFF;
        let val = _mm_min_epu8(t, _mm_sub_epi8(t, _mm_set1_epi8(39)));
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510));
        let id = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        (_mm_cvtsi128_si64(id) as u64, _mm_extract_epi64::<1>(id) as u64, badm)
    }
}

#[inline(always)]
unsafe fn b36src(p: *const u8, vs: usize, l: usize) -> u64 {
    unsafe {
        let x = (p.wrapping_add(vs + l).wrapping_sub(8) as *const u64).read_unaligned();
        let keep = !0u64 << (8 * (9 - l));
        (x & keep) | (0x3030_3030_3030_3030 & !keep)
    }
}

#[inline(always)]
fn idk_u(buf: &[u8], vs: usize, l: usize) -> (u64, u64, u64) {
    unsafe {
        let p = buf.as_ptr();
        let c0 = *p.add(vs);
        if l == 33 && c0 == b'u' {
            if let Some((hi, lo)) = hex32(p.add(vs + 1)) {
                return (long_word(hi, lo), hi, lo);
            }
        } else if (2..=9).contains(&l) && c0 == b'u' && !(l > 2 && *p.add(vs + 1) == b'0') {
            let (v, _, bad) = b36x2(b36src(p, vs, l), 0x3030_3030_3030_3030);
            if bad == 0 {
                return (v, 0, 0);
            }
        }
        (odd_id(vs, l), 0, 0)
    }
}

#[inline(always)]
fn link_fast<S: LinkSink>(buf: &[u8], s: usize, e: usize, cm: u64, qm: u64, sink: &mut S) {
    let k = cm.trailing_zeros() as usize;
    let qa = qm & 1;
    let qb = qm.wrapping_shr(k as u32 + 1) & 1;
    let exp = (qa * (1 | 1u64.wrapping_shl(k as u32).wrapping_shr(1))) | (qb * (1u64.wrapping_shl(k as u32 + 1) | 1u64.wrapping_shl(e as u32).wrapping_shr(1)));
    let bad = (cm.count_ones() != 1) as u64 | (qa & (k < 2) as u64) | (qb & (e < k + 3) as u64) | (qm != exp) as u64;
    if bad != 0 {
        return link_slow(buf, s, s + e, sink);
    }
    let (qa, qb) = (qa as usize, qb as usize);
    let (va, la) = (s + qa, k - 2 * qa);
    let (vb, lb) = (s + k + 1 + qb, e - k - 1 - 2 * qb);
    if la == 0 || lb == 0 {
        return;
    }
    unsafe {
        let p = buf.as_ptr();
        if (2..=9).contains(&la) && (2..=9).contains(&lb) {
            let ok = (*p.add(va) == b'u') & (*p.add(vb) == b'u') & !(la > 2 && *p.add(va + 1) == b'0') & !(lb > 2 && *p.add(vb + 1) == b'0');
            let (a, b, bad) = b36x2(b36src(p, va, la), b36src(p, vb, lb));
            if ok && bad == 0 {
                sink.link(&LkRow { a, b, ah: 0, al: 0, bh: 0, bl: 0 });
                return;
            }
        }
    }
    let (wa, ah, al) = idk_u(buf, va, la);
    let (wb, bh, bl) = idk_u(buf, vb, lb);
    sink.link(&LkRow { a: wa, b: wb, ah, al, bh, bl });
}

#[inline(always)]
fn link_line<S: LinkSink>(buf: &[u8], s: usize, e: usize, sink: &mut S) {
    let line = &buf[s..e];
    let mut comma = usize::MAX;
    let mut n_comma = 0;
    let mut quote = false;
    for (i, &b) in line.iter().enumerate() {
        if b == b',' {
            n_comma += 1;
            comma = i;
        }
        quote |= b == b'"';
    }
    if quote || n_comma != 1 {
        return link_slow(buf, s, e, sink);
    }
    let (a, b) = ((s, s + comma), (s + comma + 1, e));
    if a.0 == a.1 || b.0 == b.1 {
        return;
    }
    let (wa, ah, al) = slow::parse_idk(buf, a, b'u');
    let (wb, bh, bl) = slow::parse_idk(buf, b, b'u');
    sink.link(&LkRow { a: wa, b: wb, ah, al, bh, bl });
}

#[cold]
#[inline(never)]
fn link_slow<S: LinkSink>(buf: &[u8], s: usize, e: usize, sink: &mut S) {
    let Some(v) = slow::split_line::<2>(buf, s, e) else { return };
    if v[0].0 == v[0].1 || v[1].0 == v[1].1 {
        return;
    }
    let (wa, ah, al) = slow::parse_idk(buf, v[0], b'u');
    let (wb, bh, bl) = slow::parse_idk(buf, v[1], b'u');
    sink.link(&LkRow { a: wa, b: wb, ah, al, bh, bl });
}

mod slow {
    use crate::iface::*;

    pub fn field_value(buf: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
        let raw = &buf[s..e];
        let quotes = raw.iter().filter(|&&b| b == b'"').count();
        if quotes == 0 {
            return Some((s, e));
        }
        if quotes == 2 && raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
            return Some((s + 1, e - 1));
        }
        None
    }

    pub fn split_line<const N: usize>(buf: &[u8], start: usize, end: usize) -> Option<[(usize, usize); N]> {
        if start == end {
            return None;
        }
        let mut out = [(0usize, 0usize); N];
        let mut count = 0;
        let mut field_start = start;
        let mut i = start;
        loop {
            if i == end || buf[i] == b',' {
                if count == N {
                    return None;
                }
                out[count] = field_value(buf, field_start, i)?;
                count += 1;
                if i == end {
                    break;
                }
                field_start = i + 1;
            }
            i += 1;
        }
        if count != N {
            return None;
        }
        Some(out)
    }

    pub fn parse_idk(buf: &[u8], span: (usize, usize), prefix: u8) -> (u64, u64, u64) {
        let (s, e) = span;
        let v = &buf[s..e];
        if v.is_empty() {
            return (ID_EMPTY, 0, 0);
        }
        if v.len() == 33 && v[0] == prefix {
            if let Some((hi, lo)) = unsafe { super::hex32(v.as_ptr().add(1)) } {
                return (long_word(hi, lo), hi, lo);
            }
        }
        (parse_short(buf, span, prefix), 0, 0)
    }

    fn parse_short(buf: &[u8], span: (usize, usize), prefix: u8) -> u64 {
        let (s, e) = span;
        let v = &buf[s..e];
        let digits = &v[1..];
        let canonical = v[0] == prefix
            && (1..=8).contains(&digits.len())
            && digits.iter().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
            && !(digits[0] == b'0' && digits.len() > 1);
        if !canonical {
            return odd_id(s, v.len());
        }
        let mut value: u64 = 0;
        for &b in digits {
            let d = if b.is_ascii_digit() { b - b'0' } else { b - b'a' + 10 };
            value = value * 36 + d as u64;
        }
        value
    }

    fn canonical_decimal(d: &[u8]) -> Option<u32> {
        if !(1..=8).contains(&d.len()) || !d.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if d[0] == b'0' && d.len() > 1 {
            return None;
        }
        Some(d.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32))
    }

    pub fn parse_product(v: &[u8]) -> u32 {
        if v.is_empty() {
            return PROD_EMPTY;
        }
        if v[0] != b'p' {
            return PROD_ODD;
        }
        canonical_decimal(&v[1..]).unwrap_or(PROD_ODD)
    }

    pub fn parse_campaign(v: &[u8]) -> u32 {
        if v.is_empty() {
            return CAMP_EMPTY;
        }
        if !v.starts_with(b"c_") {
            return CAMP_ODD;
        }
        canonical_decimal(&v[2..]).unwrap_or(CAMP_ODD)
    }

    pub fn parse_kind(v: &[u8]) -> u8 {
        match v {
            b"view" => KIND_VIEW,
            b"cart" => KIND_CART,
            b"purchase" => KIND_PURCHASE,
            _ => KIND_OTHER,
        }
    }

    fn parse_price(v: &[u8]) -> u32 {
        if v.is_empty() || !v.iter().all(|b| b.is_ascii_digit()) {
            return 0;
        }
        let first_nonzero = v.iter().position(|&b| b != b'0').unwrap_or(v.len());
        let significant = &v[first_nonzero..];
        if significant.len() > 8 {
            return 0;
        }
        let value = significant.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32);
        if (1..=10_000_000).contains(&value) { value } else { 0 }
    }

    fn is_leap(y: i64) -> bool {
        y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
    }

    pub fn days_in_month(y: i64, m: i64) -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => if is_leap(y) { 29 } else { 28 },
            _ => 0,
        }
    }

    pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn digits_value(v: &[u8]) -> Option<i64> {
        if v.iter().all(|b| b.is_ascii_digit()) {
            Some(v.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64))
        } else {
            None
        }
    }

    fn civil_ms(v: &[u8]) -> Option<i64> {
        if v.len() != 19 || v[4] != b'-' || v[7] != b'-' || v[10] != b'T' || v[13] != b':' || v[16] != b':' {
            return None;
        }
        let y = digits_value(&v[0..4])?;
        let mo = digits_value(&v[5..7])?;
        let d = digits_value(&v[8..10])?;
        let h = digits_value(&v[11..13])?;
        let mi = digits_value(&v[14..16])?;
        let s = digits_value(&v[17..19])?;
        if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
            return None;
        }
        Some(days_from_civil(y, mo, d) * 86_400_000 + ((h * 60 + mi) * 60 + s) * 1000)
    }

    pub fn parse_f1(v: &[u8]) -> i64 {
        if v.len() == 20 && v[19] == b'Z' {
            if let Some(ms) = civil_ms(&v[..19]) {
                return ms;
            }
        }
        TS_BAD
    }

    pub fn parse_ts(v: &[u8]) -> i64 {
        if !v.is_empty() && v.iter().all(|b| b.is_ascii_digit()) {
            let mut value: u128 = 0;
            for &b in v {
                value = (value * 10 + (b - b'0') as u128).min(u64::MAX as u128);
            }
            return value.min(i64::MAX as u128) as i64;
        }
        if v.len() == 20 {
            return parse_f1(v);
        }
        if v.len() == 25 && (v[19] == b'+' || v[19] == b'-') && v[22] == b':' {
            let Some(local) = civil_ms(&v[..19]) else { return TS_BAD };
            let (Some(oh), Some(om)) = (digits_value(&v[20..22]), digits_value(&v[23..25])) else {
                return TS_BAD;
            };
            if oh > 23 || om > 59 {
                return TS_BAD;
            }
            let offset = (oh * 60 + om) * 60_000;
            return if v[19] == b'+' { local - offset } else { local + offset };
        }
        TS_BAD
    }

    #[cold]
    #[inline(never)]
    pub fn event<S: EventSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<7>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let kind = parse_kind(val(4));
        let (eid, eh, el) = parse_idk(buf, v[0], b'e');
        let (uid, uh, ul) = parse_idk(buf, v[1], b'u');
        sink.event(&EvRow {
            eid,
            uid,
            ts: parse_ts(val(2)),
            ingest: parse_f1(val(3)),
            kind,
            product: parse_product(val(5)),
            price: if kind == KIND_PURCHASE { parse_price(val(6)) } else { 0 },
            eh,
            el,
            uh,
            ul,
        });
    }

    #[cold]
    #[inline(never)]
    pub fn click<S: ClickSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<4>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let (cid, ch, cl) = parse_idk(buf, v[0], b'k');
        let (uid, uh, ul) = parse_idk(buf, v[1], b'u');
        sink.click(&ClRow { cid, uid, campaign: parse_campaign(val(2)), ts: parse_ts(val(3)), ch, cl, uh, ul });
    }
}
