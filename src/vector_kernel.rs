
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

#[cfg(feature = "stats")]
pub static REASONS: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
#[cfg(feature = "stats")]
pub const REASON_NAMES: [&str; 16] = [
    "ev-careful", "cl-careful", "ids-cold", "ids-malformed", "ts-cold", "ts-malformed", "ingest-cold", "kind-cold",
    "dec-cold", "price-quote", "price-long", "", "ev-slow", "cl-slow", "date-miss", "",
];
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
    _pad: i64,
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

const fn tj_rows() -> ([TjA; 128], [TjB; 128], [TjC; 128], [TjD; 128], [TjE; 128], [TjF; 128]) {
    let (ta, tb, tc) = ts_rows();
    let mut ja = [TjA { ctrl: [0; 32], base: [0; 32] }; 128];
    let mut jb = [TjB { lim: [0; 32], w1: [0; 32] }; 128];
    let mut jc = [TjC { w2: [0; 16], w3: [0; 16] }; 128];
    let mut jd = [TjD { limv: [0; 8], kmask2: [0; 16], kmask3: [0; 16] }; 128];
    let mut je = [TjE { kme: [0; 4], kmf: [0; 4] }; 128];
    let mut jf = [TjF { ksel: [0; 32], koff1: 0, pmask: 0, pmask3: DATE_PEXT, _pad: 0 }; 128];
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
        je[j] = TjE { kme: [0, c.me as i64, 1000, 0], kmf: [0, if f2 { 60_000 } else { -(c.mf[0] as i64) }, 0, 0] };
        if f2 {
            jf[j].ksel[8] = 2;
        }
        jf[j].koff1 = c.koff as i64 + 1;
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
        t[i].pp = (k[raw].k[f] as u64) << 56;
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
    _pad: u64,
}
const TAIL_NO: u32 = u32::MAX;
const TAIL_K: usize = 72;
const TAIL_ROWS: usize = 63 * TAIL_K + 64;

const fn tail_up(x: u32, sh: usize) -> u32 {
    if x == TAIL_NO { TAIL_NO } else { x << sh }
}
const fn tail_rows() -> [TailRow; TAIL_ROWS] {
    let mut t = [TailRow { sel: 0, m: 0, pat: [TAIL_NO; 4], pp: [PP_SOME; 4], _pad: 0 }; TAIL_ROWS];
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
                t[r5 * TAIL_K + r6] = TailRow { sel: (1 | 1 << (r5 + 1)) << sh, m: ((1u32 << (r5 + 1 + r6)) - 1) << sh, pat: [0, tail_up(p5, sh), tail_up(p6, sh), tail_up(both, sh)], pp: [e0, e1, e0, e1], _pad: 0 };
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
unsafe fn cold_ids(base: *const u8, len: usize, a0: usize, b0: usize, prefix: u8, a1: usize, b1: usize) -> Option<(u64, u64)> {
    count(2);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (Some(v0), Some(v1)) = (slow::field_value(buf, a0, b0), slow::field_value(buf, a1, b1)) else {
        count(3);
        return None;
    };
    if v0.0 == v0.1 {
        return None;
    }
    Some((slow::parse_id(buf, v0, prefix), slow::parse_id(buf, v1, b'u')))
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

        let (eid, uid, ts, ingest, kind, product);
        {
            count(0);
            (eid, uid) = {
                let (a, b, bad) = ids2(xe, se, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b)) }, cold_ids(base, len, l, l + c0, b'e', l + c0 + 1, l + c1))
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
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr });
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
        let r = ev_core::<S, true>(base, len, l, 0, cx.cache, sink);
        let next = if r != FAIL { r + 1 } else { scan_event(core::slice::from_raw_parts(base, len), l, sink) };
        Pair(base.wrapping_add(next) as u64, (*b).ts.as_mut_ptr().add((*b).n) as u64)
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
        _mm_storeu_si128(s.byte_add(core::mem::offset_of!(EvBatch, ing)) as *mut __m128i, _mm256_extracti128_si256::<1>(r));
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
                let (r, nw) = kcall!(ev_cold::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<EvBatch, S> as u64, p as u64, *w as u64, 0u64);
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
        let (cid, uid, campaign, ts);
        {
            count(1);
            (cid, uid) = {
                let (a, b, bad) = ids2(xk, sk, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b)) }, cold_ids(base, len, l, l + c0, b'k', l + c0 + 1, l + c1))
            };
            campaign = {
                let (v, bad) = dec(xc, sc);
                or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, l + c1 + 1, l + c2, false))
            };
            ts = or_cold!(ts_careful::<true>(rs3, raw3, cache), cold_ts(base, len, l + c2 + 1, e, false));
        }
        sink.click(&ClRow { cid, uid, campaign, ts });
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
        let (w, nl) = cl_masks(p);
        let r = cl_core::<S, true, true>(base, len, l, 0, &w, nl, cx.cache, sink);
        let next = if r != FAIL { r + 1 } else { scan_click(core::slice::from_raw_parts(base, len), l, sink) };
        Pair(base.wrapping_add(next) as u64, (*b).ts.as_mut_ptr().add((*b).n) as u64)
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
                let (r, nw) = kcall!(cl_cold::<S> as unsafe extern "C" fn(_, _, _) -> Pair, cx as *const Cx<CkBatch, S> as u64, p as u64, *w as u64, 0u64);
                *w = nw as *mut i64;
                r as *const u8
            }
        }
    }
}

const PIPE: u8 = 0;
const PFD: usize = 512;
const UNROLL_EV: usize = 1;
const CHAIN: u8 = 1;

const TAG: usize = 1 << 63;
const NO_LIM: *const u8 = usize::MAX as *const u8;
#[inline(always)]
fn tag_far(r: *const u8, lim: *const u8) -> *const u8 {
    if r > lim { (r as usize | TAG) as *const u8 } else { r }
}
#[inline(always)]
fn untag(r: *const u8) -> *const u8 {
    (r as usize & !TAG) as *const u8
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

macro_rules! pipe_stages {
    ($S:ident, $cx:ident, $p:ident, $w:ident, $end:ident, $len:ident, $masks:ident, $adv:expr, $line:ident, $s:ident, $d:ident, $cold:ident, $unroll:expr) => {
        const REACH: usize = 3 * ($adv + 64) + 2;
        const FAR: usize = (NB + 1) * ($adv + 64 + 1);
        if $len >= REACH + FAR {
            let lim_far = $end.sub(REACH + FAR);
            'outer: while $p <= lim_far {
                let k0 = $masks($p);
                let mut p1 = k0.next($p);
                let Some(mut st) = $s::<$S, false>($cx, $p, k0, $w) else {
                    let (r, nw) = kcall!($cold::<$S> as unsafe extern "C" fn(_, _, _) -> Pair, $cx as *const _ as u64, $p as u64, $w as u64, 0u64);
                    $w = nw as *mut i64;
                    $p = r as *const u8;
                    continue 'outer;
                };
                let mut k1 = $masks(p1).here();
                macro_rules! step {
                    () => {{
                        let p2 = k1.next(p1);
                        let k2 = $masks(p2).here();
                        _mm_prefetch::<_MM_HINT_T0>($p.wrapping_add(PFD) as *const i8);
                        let r = $d($cx, $p, st, p1, &mut $w, lim_far);
                        if r != p1 {
                            $p = untag(r);
                            continue 'outer;
                        }
                        $p = r;
                        let Some(stn) = $s::<$S, false>($cx, $p, k1, $w) else {
                            let (r, nw) = kcall!($cold::<$S> as unsafe extern "C" fn(_, _, _) -> Pair, $cx as *const _ as u64, $p as u64, $w as u64, 0u64);
                            $w = nw as *mut i64;
                            $p = r as *const u8;
                            continue 'outer;
                        };
                        st = stn;
                        p1 = p2;
                        k1 = k2;
                    }};
                }
                loop {
                    step!();
                    if $unroll >= 2 {
                        step!();
                    }
                    if $unroll >= 3 {
                        step!();
                    }
                }
            }
        }
    };
}

macro_rules! runner {
    ($name:ident, $sink:ident, $get:ident, $line:ident, $masks:ident, $adv:expr, $scan:ident, $pipe:ident, $s:ident, $d:ident, $cold:ident, $unroll:expr) => {
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
                    $pipe!(S, cx, p, w, end, len, $masks, $adv, $line, $s, $d, $cold, $unroll);
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
runner!(run_events, EventSink, ev, ev_fast, ev_masks, OB, scan_event, pipe_stages, ev_s, ev_d, ev_cold, UNROLL_EV);


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
        row.kmf = [0, -60_000];
        assert!(old.kme[1] == 1000 && old.kmf[1] == 60_000 && old.koff1 == q as i64 && old.pmask == DATE_PEXT && old.ksel[8] == 2 && old.w3[4] == 60 && old.w3[6] == 60);
        t[r] = row;
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

#[repr(C, align(128))]
#[derive(Clone, Copy)]
struct CjRow {
    ctrl: [u8; 32],
    subv: [u8; 32],
    lz: [u8; 32],
    _pad: [u8; 32],
}
const CJ_ROWS: usize = 34 * 32;
const fn cj_rows() -> [CjRow; CJ_ROWS] {
    let k = id_shapes_raw(b'k', 0, 8, false);
    let u = id_shapes_raw(b'u', 0, 8, true);
    let mut bad = CjRow { ctrl: [0x80; 32], subv: [0; 32], lz: [0; 32], _pad: [0; 32] };
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
            let r = &mut t[c0 * 32 + c1];
            let mut i = 0;
            while i < 16 {
                r.ctrl[i] = a.ctrl[i];
                r.subv[i] = a.subv[i];
                r.lz[i] = a.lz[i];
                i += 1;
            }
            if b.orv[1] == 0 && d <= 15 {
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
                    r.lz[16 + i] = b.lz[i];
                    i += 1;
                }
            }
            c1 += 1;
        }
        c0 += 1;
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
}
const fn cl_const() -> ClConst {
    let mut c = ClConst { comma: [b','; 32], nl: [b'\n'; 32], e: [0; 32], letlim: [0; 32], diglim: [0; 32], m39: [217; 32], w1: [0x0124; 16], w2: [0x0001_0510; 8], w3: [1_679_616; 4], cw1: [0; 16], cw2: [0; 4], ksel: [0, 0x80 << 56] };
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
);

const C_CAMP: usize = core::mem::offset_of!(CkBatch, camp);
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
    ($a:literal, $b:literal, $cin:literal, $nlin:literal, $cout:literal, $nlout:literal, $row:literal, $x:literal, $days:literal, $bad:literal, $badx:literal, $camp:literal, $ids:literal) => {
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
            "blsr ", $cin, ", ", $cin, "\n",
            "tzcnt r10d, ", $cin, "\n",
            "vmovdqu ymm3, ymmword ptr [", $b, " + r8 - 16]\n",
            "movzx edx, byte ptr [", $b, " + r9 + 1]\n",
            "vmovdqu xmm0, xmmword ptr [", $b, " + r10 - 15]\n",
            "shl r8d, 12\n",
            "imul edi, r9d, 128\n",
            "add edi, r8d\n",
            "sub r9, r10\n",
            "lea rdx, [rdx + r9*2]\n",
            "shl rdx, 6\n",
            "vpshufb xmm0, xmm0, xmmword ptr [{CB} + rdx + {CBO}]\n",
            "vpsubb xmm0, xmm0, xmmword ptr [{CB} + rdx + {CBO} + 16]\n",
            "vpsubusb xmm2, xmm0, xmmword ptr [{CB} + rdx + {CBO} + 32]\n",
            "vpmaddubsw xmm0, xmm0, xmm25\n",
            "vpmaddwd ", $camp, ", xmm0, xmm26\n",
            "vpshufb ymm3, ymm3, ymmword ptr [{CJ} + rdi]\n",
            "vpsubb ymm5, ymm3, ymmword ptr [{CJ} + rdi + 32]\n",
            "vpaddb ymm4, ymm5, ymm16\n",
            "vpsubusb ymm4, ymm4, ymm17\n",
            "vpsubusb ymm6, ymm5, ymm18\n",
            "vpminub ymm4, ymm4, ymm6\n",
            "vpaddb ymm6, ymm5, ymm19\n",
            "vpminub ymm5, ymm5, ymm6\n",
            "vmovdqa ymm6, ymmword ptr [{CJ} + rdi + 64]\n",
            "vpsubusb ymm6, ymm6, ymm5\n",
            "vpternlogq ymm4, ymm6, ymm2, 0xfe\n",
            "vpmaddubsw ymm5, ymm5, ymm20\n",
            "vpmaddwd ymm5, ymm5, ymm21\n",
            "vpmuludq ymm1, ymm5, ymm22\n",
            "vpsrlq ymm5, ymm5, 32\n",
            "vpaddq ", $ids, ", ymm1, ymm5\n",
            "lea rdx, [", $b, " + r10]\n",
            "sub r10, ", $nlin, "\n",
            "imul ", $row, ", r10, -256\n",
            "add rdx, qword ptr [{CTS} + ", $row, " + 8192 + 176]\n",
            "movzx r9d, word ptr [rdx + 9]\n",
            "pext r9, r9, qword ptr [{CTS} + ", $row, " + 8192 + 184]\n",
            "shl r9d, 5\n",
            "vmovdqu64 ", $badx, ", xmmword ptr [r15 + r9 + {SCR}]\n",
            "vpxorq ", $badx, ", ", $badx, ", xmmword ptr [rdx]\n",
            "vpternlogq ", $bad, ", ymm4, ymmword ptr [{CTS} + ", $row, " + 8192 + 112], 0xec\n",
            "vmovdqu64 ", $days, ", xmmword ptr [r15 + r9 + {SCR} + 16]\n",
            "vmovdqu64 ", $x, ", xmmword ptr [", $a, " - 17]\n",
        )
    };
}
macro_rules! cl_d {
    ($o:literal, $lim:literal, $limx:literal, $row:literal, $x:literal, $days:literal, $bad:literal, $camp:literal, $idsx:literal, $uid:literal) => {
        concat!(
            "vpshufb xmm0, ", $x, ", xmmword ptr [{CTS} + ", $row, " + 8192]\n",
            "vpsubb xmm0, xmm0, xmmword ptr [{CTS} + ", $row, " + 8192 + 16]\n",
            "vpmaddubsw xmm1, xmm0, xmmword ptr [{CTS} + ", $row, " + 8192 + 48]\n",
            "vpmaddwd xmm1, xmm1, xmmword ptr [{CTS} + ", $row, " + 8192 + 64]\n",
            "vpackusdw xmm2, xmm1, xmm1\n",
            "vpmaddwd xmm2, xmm2, xmmword ptr [{CTS} + ", $row, " + 8192 + 80]\n",
            "vpsubusb ", $limx, ", xmm0, xmmword ptr [{CTS} + ", $row, " + 8192 + 32]\n",
            "vpcmpgtd xmm1, xmm1, xmmword ptr [{CTS} + ", $row, " + 8192 + 96]\n",
            "vptestnmq k1, xmm0, xmm24\n",
            "vpmuludq xmm4, xmm2, xmmword ptr [{CTS} + ", $row, " + 8192 + 128]\n",
            "vpsrlq xmm2, xmm2, 32\n",
            "vpmuldq xmm2, xmm2, xmmword ptr [{CTS} + ", $row, " + 8192 + 144]\n",
            "vpsubq xmm2{{k1}}, xmm23, xmm2\n",
            "vpaddq xmm4, xmm4, ", $days, "\n",
            "vpaddq xmm4, xmm4, xmm2\n",
            $uid, "\n",
            "vmovhps qword ptr [r15 + rbx + ", $o, "], xmm4\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " + {CAMPO}], ", $camp, "\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " + {CIDO}], ", $idsx, "\n",
            "vmovq qword ptr [r15 + rbx + ", $o, " + {UIDO}], xmm5\n",
            "vpternlogq ", $lim, ", ymm1, ", $bad, ", 0xfe\n",
        )
    };
}

const CL_REACH: usize = (NB + 3) * 65 + 64;

#[inline(never)]
unsafe fn cl_loop(p: *const u8, w: *mut i64, b512: *mut u8) -> (u64, *const u8, *mut i64) {
    let (code, np, nw): (u64, *const u8, *mut i64);
    unsafe {
        core::arch::asm!(
            "push rbx",
            "push rbp",
            "sub rsp, 24",
            "mov r15, rdx",
            "mov rbx, rsi",
            "sub rbx, r15",
            "vmovdqa ymm13, ymmword ptr [{K}]",
            "vmovdqa ymm12, ymmword ptr [{K} + 32]",
            "vmovdqa64 ymm16, ymmword ptr [{K} + 64]",
            "vmovdqa64 ymm17, ymmword ptr [{K} + 96]",
            "vmovdqa64 ymm18, ymmword ptr [{K} + 128]",
            "vmovdqa64 ymm19, ymmword ptr [{K} + 160]",
            "vmovdqa64 ymm20, ymmword ptr [{K} + 192]",
            "vmovdqa64 ymm21, ymmword ptr [{K} + 224]",
            "vmovdqa64 ymm22, ymmword ptr [{K} + 256]",
            "vmovdqa64 xmm25, xmmword ptr [{K} + 288]",
            "vmovdqa64 xmm26, xmmword ptr [{K} + 304]",
            "vmovdqa64 xmm24, xmmword ptr [{K} + 320]",
            "vpxord xmm23, xmm23, xmm23",
            "vpxor xmm14, xmm14, xmm14",
            "test bl, 8",
            "jnz 70f",
            "mov rbp, rdi",
            "mov qword ptr [rsp + 16], rbp",
            "mov qword ptr [rsp + 8], 1",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [rbp]",
            "vpmovmskb eax, ymm0",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rbp + 32]",
            "vpmovmskb ecx, ymm1",
            "tzcnt ecx, ecx",
            cl_ms!("r12", "rbp", "eax", "rcx", "r11d", "r13d", "rsi", "xmm8", "xmm9", "ymm10", "xmm10", "xmm11", "ymm27"),
            "jmp 2f",
            "70:",
            "mov r12, rdi",
            "mov qword ptr [rsp + 16], r12",
            "sub rbx, 8",
            "mov qword ptr [rsp + 8], rbx",
            "vpcmpeqb ymm0, ymm13, ymmword ptr [r12]",
            "vpmovmskb r11d, ymm0",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [r12 + 32]",
            "vpmovmskb r13d, ymm1",
            "tzcnt r13d, r13d",
            cl_ms!("rbp", "r12", "r11d", "r13", "eax", "ecx", "r14", "xmm28", "xmm29", "ymm30", "xmm30", "xmm31", "ymm7"),
            "jmp 3f",
            ".p2align 5",
            "2:",
            cl_ms!("rbp", "r12", "r11d", "r13", "eax", "ecx", "r14", "xmm28", "xmm29", "ymm30", "xmm30", "xmm31", "ymm7"),
            cl_d!("0", "ymm14", "xmm14", "rsi", "xmm8", "xmm9", "ymm10", "xmm11", "xmm27", "vextracti32x4 xmm5, ymm27, 1"),
            "3:",
            cl_ms!("r12", "rbp", "eax", "rcx", "r11d", "r13d", "rsi", "xmm8", "xmm9", "ymm10", "xmm10", "xmm11", "ymm27"),
            cl_d!("8", "ymm3", "xmm3", "r14", "xmm28", "xmm29", "ymm30", "xmm31", "xmm7", "vextracti128 xmm5, ymm7, 1"),
            "vpor ymm3, ymm3, ymm14",
            "vptest ymm3, ymm3",
            "jnz 90f",
            "mov qword ptr [rsp + 16], rbp",
            "add rbx, 16",
            "jnz 2b",
            "mov rdi, rbp",
            "mov eax, 2",
            "lea rsi, [r15 + rbx]",
            "jmp 6f",
            "90:",
            "mov rdi, qword ptr [rsp + 16]",
            "mov eax, 1",
            "vptest ymm14, ymm14",
            "jnz 91f",
            "lea rsi, [r15 + rbx + 8]",
            "cmp rbx, qword ptr [rsp + 8]",
            "je 6f",
            "vpcmpeqb ymm1, ymm12, ymmword ptr [rdi + 32]",
            "vpmovmskb ecx, ymm1",
            "tzcnt ecx, ecx",
            "lea rdi, [rdi + rcx + 33]",
            "jmp 6f",
            "91:",
            "lea rsi, [r15 + rbx]",
            "6:",
            "add rsp, 24",
            "pop rbp",
            "pop rbx",
            inout("rdi") p => np,
            inout("rsi") w => nw,
            inout("rdx") b512 => _,
            out("rax") code,
            out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _, out("r12") _, out("r13") _, out("r14") _, out("r15") _,
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _, out("zmm12") _, out("zmm13") _, out("zmm14") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _, out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _, out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
            out("k1") _,
            CTS = sym CTS, CB = sym CAMPB, CJ = sym CJ, K = sym CLC,
            CBO = const CAMPB_OFF,
            CAMPO = const C_CAMP, CIDO = const C_CID, UIDO = const C_UID, SCR = const C_SCR, PFD = const PFD,
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
            while p <= enter {
                let (code, np, nw) = cl_loop(p, w, b512);
                p = np;
                w = nw;
                if code == 2 {
                    (*b).n = NB;
                    (*sink).ck_flush();
                    w = (*b).ts.as_mut_ptr();
                } else {
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

    fn split_line<const N: usize>(buf: &[u8], start: usize, end: usize) -> Option<[(usize, usize); N]> {
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

    pub fn parse_id(buf: &[u8], span: (usize, usize), prefix: u8) -> u64 {
        let (s, e) = span;
        let v = &buf[s..e];
        if v.is_empty() {
            return ID_EMPTY;
        }
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
        sink.event(&EvRow {
            eid: parse_id(buf, v[0], b'e'),
            uid: parse_id(buf, v[1], b'u'),
            ts: parse_ts(val(2)),
            ingest: parse_f1(val(3)),
            kind,
            product: parse_product(val(5)),
            price: if kind == KIND_PURCHASE { parse_price(val(6)) } else { 0 },
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
        sink.click(&ClRow {
            cid: parse_id(buf, v[0], b'k'),
            uid: parse_id(buf, v[1], b'u'),
            campaign: parse_campaign(val(2)),
            ts: parse_ts(val(3)),
        });
    }
}
