use crate::pqw::*;
use crate::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn die(what: &str) -> ! {
    eprintln!("[v2b] write error: {what}");
    unsafe { sys::_exit(1) }
}

#[derive(Clone, Copy)]
struct View {
    keys: *const u32,
    day: *const i32,
    ses: *const i32,
    evs: *const i32,
    rev: *const i32,
    cat: *const u8,
    names: *const u8,
    nnames: usize,
    noff: *const u32,
    nnoff: usize,
    rows: usize,
}
unsafe impl Send for View {}
unsafe impl Sync for View {}
fn view(c: &crate::core::Core) -> View {
    View {
        keys: c.keys.as_ptr(),
        day: c.day.as_ptr(),
        ses: c.ses.as_ptr(),
        evs: c.evs.as_ptr(),
        rev: c.rev.as_ptr(),
        cat: c.cat.as_ptr(),
        names: c.names.as_ptr(),
        nnames: c.names.len(),
        noff: c.names_off.as_ptr(),
        nnoff: c.names_off.len(),
        rows: c.day.len(),
    }
}

fn write_piece(g: &G, t: usize, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], kbuf: &mut Vec<u32>) {
    write_rows(&view(&g.t(t).core), a, b, path, cats_off, cats_data, kbuf)
}

#[derive(Default)]
struct Split {
    k: Vec<u32>,
    d: Vec<i32>,
    s: Vec<i32>,
    e: Vec<i32>,
    r: Vec<i32>,
    c: Vec<u8>,
    b: Vec<u8>,
    t: Vec<u32>,
}

const STRIDE: usize = SUB_ROWS + SUB_ROWS / 2 + 16 + crate::env_u32(option_env!("XL_SPAD_DEF"), 0) as usize;
thread_local! {
    static SPLIT: std::cell::RefCell<Split> = std::cell::RefCell::new(Split::default());
    static UFILE: std::cell::RefCell<Option<UserFile>> = const { std::cell::RefCell::new(None) };
}

fn end_user_file() {
    if let Some(f) = UFILE.with(|f| f.borrow_mut().take()) {
        if f.finish().is_err() {
            die("user_daily");
        }
    }
}

const UF_LIMIT: usize = (crate::env_u32(option_env!("XL_UFL_DEF"), 1024) as usize) << 10;

const S3_LIMIT: usize = (crate::env_u32(option_env!("XL_S3L_DEF"), 192) as usize) << 10;
thread_local! {
    static S3_SMALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
const S3_MIN: usize = crate::env_u32(option_env!("XL_S3MIN_DEF"), SUB_ROWS as u32) as usize;
struct S3Ctx {
    ud: std::path::PathBuf,
    cd: std::path::PathBuf,
    cats_off: Vec<u32>,
    cats_data: Vec<u8>,
}
static S3CTX: std::sync::OnceLock<S3Ctx> = std::sync::OnceLock::new();
static S3PREP: std::sync::Once = std::sync::Once::new();
struct S3Slots([std::cell::UnsafeCell<Option<UserFile>>; 16]);
unsafe impl Sync for S3Slots {}
static S3FILE: S3Slots = S3Slots([const { std::cell::UnsafeCell::new(None) }; 16]);

fn ctx_new(g: &G, outdir: &str) -> S3Ctx {
    let mut cats_off = vec![0u32];
    let mut cats_data = Vec::new();
    for c in &g.cats {
        cats_data.extend_from_slice(c);
        cats_off.push(cats_data.len() as u32);
    }
    S3Ctx { ud: Path::new(outdir).join("user_daily"), cd: Path::new(outdir).join("campaign_daily"), cats_off, cats_data }
}

pub fn s3_init(g: &G, outdir: &str) {
    if std::env::var("XL_S3").is_ok_and(|v| v == "0") {
        return;
    }
    let _ = S3CTX.set(ctx_new(g, outdir));
    let f = std::env::var("XL_S3F").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(S3F_DEF as usize).min(100);
    S3_UPTO.store(g.nb * f / 100, Ordering::Relaxed);
    #[cfg(feature = "tl")]
    crate::pqw::prof::ON.store(std::env::var("XL_WPROF").is_ok_and(|v| v == "1"), Ordering::Relaxed);
}

fn s3_prep(c: &S3Ctx) {
    S3PREP.call_once(|| {
        if std::fs::create_dir_all(&c.ud).is_err() || std::fs::create_dir_all(&c.cd).is_err() {
            die("cannot create the output folders");
        }
        clear_stale(&c.ud);
        clear_stale(&c.cd);
    });
}

#[inline(always)]
pub fn s3_bucket(g: &G, tid: usize, c: &mut crate::core::Core) {
    if c.day.len() >= c.s3w + S3_MIN && g.next.load(Ordering::Relaxed) < S3_UPTO.load(Ordering::Relaxed) {
        s3_write(tid, c);
    }
}
static S3_UPTO: AtomicUsize = AtomicUsize::new(0);
const S3F_DEF: u32 = crate::env_u32(option_env!("XL_S3F_DEF"), 65);

#[cold]
#[inline(never)]
fn s3_write(tid: usize, c: &mut crate::core::Core) {
    let Some(ctx) = S3CTX.get() else { return };
    if tid >= 16 {
        return;
    }
    let (a, b) = (c.s3w, c.day.len());
    if c.keys.len() != b || c.ses.len() != b || c.evs.len() != b || c.rev.len() != b || c.cat.len() != b {
        crate::sys::fallback("row columns differ in length");
    }
    let _p0 = crate::pqw::prof::sw(18);
    s3_prep(ctx);
    crate::pqw::prof::sw(crate::pqw::prof::MISC);
    let slot = unsafe { &mut *S3FILE.0[tid].get() };
    S3_SMALL.with(|c| c.set(true));
    write_split_to(&view(c), a, b, &ctx.ud.join(format!("part-s{tid}.parquet")), &ctx.cats_off, &ctx.cats_data, slot);
    S3_SMALL.with(|c| c.set(false));
    crate::pqw::prof::sw(_p0);
    c.s3w = b;
}

const SPLIT_ROWS: bool = crate::env_u32(option_env!("XL_SPLIT_DEF"), 1) != 0;

#[allow(clippy::too_many_arguments)]
unsafe fn split_rows(keys: *mut u32, day: *mut i32, ses: *mut i32, evs: *mut i32, rev: *mut i32, cat: *mut u8, n: usize, tk: *mut u32, td: *mut i32, ts: *mut i32, te: *mut i32, tr: *mut i32, tc: *mut u8) -> (usize, usize) {
    use std::arch::x86_64::*;
    let (mut iz, mut ip, mut i) = (0usize, 0usize, 0usize);
    let zero = _mm256_setzero_si256();
    let ff = _mm_set1_epi8(-1);
    type V = __m256i;
    while i + 8 <= n {
        let k = _mm256_loadu_si256(keys.add(i) as *const V);
        let d = _mm256_loadu_si256(day.add(i) as *const V);
        let s = _mm256_loadu_si256(ses.add(i) as *const V);
        let e = _mm256_loadu_si256(evs.add(i) as *const V);
        let r = _mm256_loadu_si256(rev.add(i) as *const V);
        let c = _mm_loadl_epi64(cat.add(i) as *const __m128i);
        let z: u8 = _mm256_cmpeq_epi32_mask(r, zero) & (_mm_cmpeq_epi8_mask(c, ff) as u8);
        let p: u8 = !z;
        _mm256_storeu_si256(keys.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, k));
        _mm256_storeu_si256(day.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, d));
        _mm256_storeu_si256(ses.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, s));
        _mm256_storeu_si256(evs.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, e));
        _mm256_storeu_si256(tk.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, k));
        _mm256_storeu_si256(td.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, d));
        _mm256_storeu_si256(ts.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, s));
        _mm256_storeu_si256(te.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, e));
        _mm256_storeu_si256(tr.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, r));
        let c32 = _mm256_maskz_compress_epi32(p, _mm256_cvtepu8_epi32(c));
        _mm_storel_epi64(tc.add(ip) as *mut __m128i, _mm256_cvtepi32_epi8(c32));
        iz += z.count_ones() as usize;
        ip += p.count_ones() as usize;
        i += 8;
    }
    while i < n {
        let (k, d, se, e, r, c) = (*keys.add(i), *day.add(i), *ses.add(i), *evs.add(i), *rev.add(i), *cat.add(i));
        let z = (r == 0 && c == u8::MAX) as usize;
        *keys.add(iz) = k;
        *day.add(iz) = d;
        *ses.add(iz) = se;
        *evs.add(iz) = e;
        *tk.add(ip) = k;
        *td.add(ip) = d;
        *ts.add(ip) = se;
        *te.add(ip) = e;
        *tr.add(ip) = r;
        *tc.add(ip) = c;
        iz += z;
        ip += 1 - z;
        i += 1;
    }
    (iz, ip)
}

const PERM: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut z = 0;
    while z < 256 {
        let (mut v, mut k, mut j) = (0u64, 0, 0);
        while j < 8 {
            if z >> j & 1 == 1 { v |= (j as u64) << (8 * k); k += 1; }
            j += 1;
        }
        j = 0;
        while j < 8 {
            if z >> j & 1 == 0 { v |= (j as u64) << (8 * k); k += 1; }
            j += 1;
        }
        t[z] = v;
        z += 1;
    }
    t
};

const PERMR: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut z = 0;
    while z < 256 {
        let (mut v, mut k, mut j) = (0u64, 0, 0);
        while j < 8 {
            if z >> j & 1 == 1 { v |= (j as u64) << (8 * k); k += 1; }
            j += 1;
        }
        j = 8;
        while j > 0 {
            j -= 1;
            if z >> j & 1 == 0 { v |= (j as u64) << (8 * k); k += 1; }
        }
        t[z] = v;
        z += 1;
    }
    t
};

#[allow(clippy::too_many_arguments)]
unsafe fn split_rev(keys: *const u32, day: *const i32, ses: *const i32, evs: *const i32, rev: *const i32, cat: *const u8, n: usize, t: *mut u32, tc: *mut u8) -> (usize, usize, UStats) {
    use std::arch::x86_64::*;
    type V = __m256i;
    let (tk, td, ts, te, tr) = (t, t.add(STRIDE) as *mut i32, t.add(2 * STRIDE) as *mut i32, t.add(3 * STRIDE) as *mut i32, t.add(4 * STRIDE) as *mut i32);
    let (mut iz, mut ip, mut i) = (0usize, 0usize, 0usize);
    let zero = _mm256_setzero_si256();
    let ff = _mm_set1_epi8(-1);
    let (mut kmin, mut kmax) = (_mm256_set1_epi32(-1), zero);
    let (mut dmin, mut dmax) = (_mm256_set1_epi32(i32::MAX), _mm256_set1_epi32(i32::MIN));
    let (mut smin, mut smax, mut emin, mut emax) = (dmin, dmax, dmin, dmax);
    let mut m = 0usize;
    while i + 16 <= n {
        let k = _mm256_loadu_si256(keys.add(i) as *const V);
        let d = _mm256_loadu_si256(day.add(i) as *const V);
        let s = _mm256_loadu_si256(ses.add(i) as *const V);
        let e = _mm256_loadu_si256(evs.add(i) as *const V);
        let r = _mm256_loadu_si256(rev.add(i) as *const V);
        let c = _mm_loadl_epi64(cat.add(i) as *const __m128i);
        kmin = _mm256_min_epu32(kmin, k);
        kmax = _mm256_max_epu32(kmax, k);
        dmin = _mm256_min_epi32(dmin, d);
        dmax = _mm256_max_epi32(dmax, d);
        smin = _mm256_min_epi32(smin, s);
        smax = _mm256_max_epi32(smax, s);
        emin = _mm256_min_epi32(emin, e);
        emax = _mm256_max_epi32(emax, e);
        let rz = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpeq_epi32(r, zero))) as u32;
        let cn = (_mm_movemask_epi8(_mm_cmpeq_epi8(c, ff)) as u32) & 0xff;
        let z = rz & cn;
        let cz = z.count_ones() as usize;
        m += 8 - cn.count_ones() as usize;
        let pv = _mm_loadl_epi64(PERMR.as_ptr().add(z as usize) as *const __m128i);
        let idx = _mm256_cvtepu8_epi32(pv);
        let kq = _mm256_permutevar8x32_epi32(k, idx);
        let dq = _mm256_permutevar8x32_epi32(d, idx);
        let sq = _mm256_permutevar8x32_epi32(s, idx);
        let eq = _mm256_permutevar8x32_epi32(e, idx);
        _mm256_storeu_si256(tk.add(iz) as *mut V, kq);
        _mm256_storeu_si256(td.add(iz) as *mut V, dq);
        _mm256_storeu_si256(ts.add(iz) as *mut V, sq);
        _mm256_storeu_si256(te.add(iz) as *mut V, eq);
        let at = n - ip - 8;
        _mm256_storeu_si256(tk.add(at) as *mut V, kq);
        _mm256_storeu_si256(td.add(at) as *mut V, dq);
        _mm256_storeu_si256(ts.add(at) as *mut V, sq);
        _mm256_storeu_si256(te.add(at) as *mut V, eq);
        _mm256_storeu_si256(tr.add(at) as *mut V, _mm256_permutevar8x32_epi32(r, idx));
        _mm_storel_epi64(tc.add(at) as *mut __m128i, _mm_shuffle_epi8(c, pv));
        iz += cz;
        ip += 8 - cz;
        i += 8;
    }
    let mut a = [[0i32; 8]; 8];
    for (j, v) in [kmin, kmax, dmin, dmax, smin, smax, emin, emax].into_iter().enumerate() {
        _mm256_storeu_si256(a[j].as_mut_ptr() as *mut V, v);
    }
    let mut st = UStats { kmin: u32::MAX, kmax: 0, dmin: i32::MAX, dmax: i32::MIN, smin: i32::MAX, smax: i32::MIN, emin: i32::MAX, emax: i32::MIN, m: 0 };
    for l in 0..8 {
        st.kmin = st.kmin.min(a[0][l] as u32);
        st.kmax = st.kmax.max(a[1][l] as u32);
        st.dmin = st.dmin.min(a[2][l]);
        st.dmax = st.dmax.max(a[3][l]);
        st.smin = st.smin.min(a[4][l]);
        st.smax = st.smax.max(a[5][l]);
        st.emin = st.emin.min(a[6][l]);
        st.emax = st.emax.max(a[7][l]);
    }
    while i < n {
        let (k, d, se, e, r, c) = (*keys.add(i), *day.add(i), *ses.add(i), *evs.add(i), *rev.add(i), *cat.add(i));
        st.kmin = st.kmin.min(k);
        st.kmax = st.kmax.max(k);
        st.dmin = st.dmin.min(d);
        st.dmax = st.dmax.max(d);
        st.smin = st.smin.min(se);
        st.smax = st.smax.max(se);
        st.emin = st.emin.min(e);
        st.emax = st.emax.max(e);
        m += (c != u8::MAX) as usize;
        if r == 0 && c == u8::MAX {
            *tk.add(iz) = k;
            *td.add(iz) = d;
            *ts.add(iz) = se;
            *te.add(iz) = e;
            iz += 1;
        } else {
            let at = n - ip - 1;
            *tk.add(at) = k;
            *td.add(at) = d;
            *ts.add(at) = se;
            *te.add(at) = e;
            *tr.add(at) = r;
            *tc.add(at) = c;
            ip += 1;
        }
        i += 1;
    }
    st.m = m;
    (iz, ip, st)
}

const SREV: bool = crate::env_u32(option_env!("XL_SREV_DEF"), 1) != 0;

#[allow(clippy::too_many_arguments)]
unsafe fn split_perm(keys: *mut u32, day: *mut i32, ses: *mut i32, evs: *mut i32, rev: *const i32, cat: *const u8, n: usize, t: *mut u32, tc: *mut u8) -> (usize, usize, UStats) {
    use std::arch::x86_64::*;
    let (tk, td, ts, te, tr) = (t, t.add(STRIDE) as *mut i32, t.add(2 * STRIDE) as *mut i32, t.add(3 * STRIDE) as *mut i32, t.add(4 * STRIDE) as *mut i32);
    type V = __m256i;
    let (mut iz, mut ip, mut i) = (0usize, 0usize, 0usize);
    let zero = _mm256_setzero_si256();
    let ff = _mm_set1_epi8(-1);
    let (mut kmin, mut kmax) = (_mm256_set1_epi32(-1), zero);
    let (mut dmin, mut dmax) = (_mm256_set1_epi32(i32::MAX), _mm256_set1_epi32(i32::MIN));
    let (mut smin, mut smax, mut emin, mut emax) = (dmin, dmax, dmin, dmax);
    let mut m = 0usize;
    while i + 8 <= n {
        let k = _mm256_loadu_si256(keys.add(i) as *const V);
        let d = _mm256_loadu_si256(day.add(i) as *const V);
        let s = _mm256_loadu_si256(ses.add(i) as *const V);
        let e = _mm256_loadu_si256(evs.add(i) as *const V);
        let r = _mm256_loadu_si256(rev.add(i) as *const V);
        let c = _mm_loadl_epi64(cat.add(i) as *const __m128i);
        kmin = _mm256_min_epu32(kmin, k);
        kmax = _mm256_max_epu32(kmax, k);
        dmin = _mm256_min_epi32(dmin, d);
        dmax = _mm256_max_epi32(dmax, d);
        smin = _mm256_min_epi32(smin, s);
        smax = _mm256_max_epi32(smax, s);
        emin = _mm256_min_epi32(emin, e);
        emax = _mm256_max_epi32(emax, e);
        let rz = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpeq_epi32(r, zero))) as u32;
        let cn = (_mm_movemask_epi8(_mm_cmpeq_epi8(c, ff)) as u32) & 0xff;
        let z = rz & cn;
        let cz = z.count_ones() as usize;
        m += 8 - cn.count_ones() as usize;
        let pv = _mm_loadl_epi64(PERM.as_ptr().add(z as usize) as *const __m128i);
        let idx = _mm256_cvtepu8_epi32(pv);
        _mm256_storeu_si256(keys.add(iz) as *mut V, _mm256_permutevar8x32_epi32(k, idx));
        _mm256_storeu_si256(day.add(iz) as *mut V, _mm256_permutevar8x32_epi32(d, idx));
        _mm256_storeu_si256(ses.add(iz) as *mut V, _mm256_permutevar8x32_epi32(s, idx));
        _mm256_storeu_si256(evs.add(iz) as *mut V, _mm256_permutevar8x32_epi32(e, idx));
        let pv2 = _mm_loadl_epi64(PERM.as_ptr().add((255 - z) as usize) as *const __m128i);
        let idx2 = _mm256_cvtepu8_epi32(pv2);
        _mm256_storeu_si256(tk.add(ip) as *mut V, _mm256_permutevar8x32_epi32(k, idx2));
        _mm256_storeu_si256(td.add(ip) as *mut V, _mm256_permutevar8x32_epi32(d, idx2));
        _mm256_storeu_si256(ts.add(ip) as *mut V, _mm256_permutevar8x32_epi32(s, idx2));
        _mm256_storeu_si256(te.add(ip) as *mut V, _mm256_permutevar8x32_epi32(e, idx2));
        _mm256_storeu_si256(tr.add(ip) as *mut V, _mm256_permutevar8x32_epi32(r, idx2));
        _mm_storel_epi64(tc.add(ip) as *mut __m128i, _mm_shuffle_epi8(c, pv2));
        iz += cz;
        ip += 8 - cz;
        i += 8;
    }
    let mut a = [[0i32; 8]; 8];
    for (j, v) in [kmin, kmax, dmin, dmax, smin, smax, emin, emax].into_iter().enumerate() {
        _mm256_storeu_si256(a[j].as_mut_ptr() as *mut V, v);
    }
    let mut st = UStats { kmin: u32::MAX, kmax: 0, dmin: i32::MAX, dmax: i32::MIN, smin: i32::MAX, smax: i32::MIN, emin: i32::MAX, emax: i32::MIN, m: 0 };
    for l in 0..8 {
        st.kmin = st.kmin.min(a[0][l] as u32);
        st.kmax = st.kmax.max(a[1][l] as u32);
        st.dmin = st.dmin.min(a[2][l]);
        st.dmax = st.dmax.max(a[3][l]);
        st.smin = st.smin.min(a[4][l]);
        st.smax = st.smax.max(a[5][l]);
        st.emin = st.emin.min(a[6][l]);
        st.emax = st.emax.max(a[7][l]);
    }
    while i < n {
        let (k, d, se, e, r, c) = (*keys.add(i), *day.add(i), *ses.add(i), *evs.add(i), *rev.add(i), *cat.add(i));
        st.kmin = st.kmin.min(k);
        st.kmax = st.kmax.max(k);
        st.dmin = st.dmin.min(d);
        st.dmax = st.dmax.max(d);
        st.smin = st.smin.min(se);
        st.smax = st.smax.max(se);
        st.emin = st.emin.min(e);
        st.emax = st.emax.max(e);
        m += (c != u8::MAX) as usize;
        if r == 0 && c == u8::MAX {
            *keys.add(iz) = k;
            *day.add(iz) = d;
            *ses.add(iz) = se;
            *evs.add(iz) = e;
            iz += 1;
        } else {
            *tk.add(ip) = k;
            *td.add(ip) = d;
            *ts.add(ip) = se;
            *te.add(ip) = e;
            *tr.add(ip) = r;
            *tc.add(ip) = c;
            ip += 1;
        }
        i += 1;
    }
    st.m = m;
    (iz, ip, st)
}

const PSPLIT: bool = crate::env_u32(option_env!("XL_PSPLIT_DEF"), 1) != 0;

fn write_split(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8]) {
    UFILE.with(|f| write_split_to(v, a, b, path, cats_off, cats_data, &mut f.borrow_mut()))
}

fn write_split_to(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], uf: &mut Option<UserFile>) {
    let mut x = a;
    while x < b {
        let y = if b - x > SUB_ROWS + SUB_ROWS / 2 { x + SUB_ROWS } else { b };
        write_split_group(v, x, y, path, cats_off, cats_data, uf);
        x = y;
    }
}

const SUB_ROWS: usize = crate::env_u32(option_env!("XL_SUB_DEF"), 4096) as usize;

fn write_split_group(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], f: &mut Option<UserFile>) {
    use std::slice::from_raw_parts as sl;
    let n = b - a;
    SPLIT.with(|s| {
        let s = &mut *s.borrow_mut();
        macro_rules! fit {
            ($($f:ident)*) => { $( s.$f.clear(); s.$f.reserve(n + 16); )* };
        }
        fit!(c);
        let _pv = crate::pqw::prof::sw(crate::pqw::prof::SPLIT);
        let mut st = None;
        let mut cols: Option<(*const u32, *const i32, *const i32, *const i32, *const i32, *const u8)> = None;
        let head = if PSPLIT && SREV && n + 8 <= STRIDE && s.c.capacity() >= n + 8 { unsafe {
            if s.t.capacity() < 5 * STRIDE {
                s.t = Vec::with_capacity(5 * STRIDE);
            }
            let tp = s.t.as_mut_ptr();
            let tc = s.c.as_mut_ptr();
            let (iz, ip, s1) = split_rev(v.keys.add(a), v.day.add(a), v.ses.add(a), v.evs.add(a), v.rev.add(a), v.cat.add(a), n, tp, tc);
            if iz + ip != n {
                die("split");
            }
            st = Some(s1);
            std::ptr::write_bytes(tp.add(4 * STRIDE), 0, iz);
            std::ptr::write_bytes(tc, u8::MAX, iz);
            cols = Some((tp as *const u32, tp.add(STRIDE) as *const i32, tp.add(2 * STRIDE) as *const i32, tp.add(3 * STRIDE) as *const i32, tp.add(4 * STRIDE) as *const i32, tc as *const u8));
            iz
        } } else if PSPLIT && n + 8 <= STRIDE && s.c.capacity() >= n + 8 { unsafe {
            if s.t.capacity() < 5 * STRIDE {
                s.t = Vec::with_capacity(5 * STRIDE);
            }
            let (keys, day, ses, evs, rev, cat) = (v.keys.add(a) as *mut u32, v.day.add(a) as *mut i32, v.ses.add(a) as *mut i32, v.evs.add(a) as *mut i32, v.rev.add(a) as *mut i32, v.cat.add(a) as *mut u8);
            let tp = s.t.as_mut_ptr();
            let (iz, ip, s1) = split_perm(keys, day, ses, evs, rev, cat, n, tp, s.c.as_mut_ptr());
            debug_assert_eq!(iz + ip, n);
            if iz + ip != n {
                die("split");
            }
            st = Some(s1);
            std::ptr::write_bytes(rev, 0, iz);
            std::ptr::write_bytes(cat, u8::MAX, iz);
            std::ptr::copy_nonoverlapping(tp, keys.add(iz), ip);
            std::ptr::copy_nonoverlapping(tp.add(STRIDE) as *const i32, day.add(iz), ip);
            std::ptr::copy_nonoverlapping(tp.add(2 * STRIDE) as *const i32, ses.add(iz), ip);
            std::ptr::copy_nonoverlapping(tp.add(3 * STRIDE) as *const i32, evs.add(iz), ip);
            std::ptr::copy_nonoverlapping(tp.add(4 * STRIDE) as *const i32, rev.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.c.as_ptr(), cat.add(iz), ip);
            iz
        } } else { unsafe {
            fit!(k d s e r);
            let (keys, day, ses, evs, rev, cat) = (v.keys.add(a) as *mut u32, v.day.add(a) as *mut i32, v.ses.add(a) as *mut i32, v.evs.add(a) as *mut i32, v.rev.add(a) as *mut i32, v.cat.add(a) as *mut u8);
            let (iz, ip) = split_rows(keys, day, ses, evs, rev, cat, n, s.k.as_mut_ptr(), s.d.as_mut_ptr(), s.s.as_mut_ptr(), s.e.as_mut_ptr(), s.r.as_mut_ptr(), s.c.as_mut_ptr());
            debug_assert_eq!(iz + ip, n);
            std::ptr::write_bytes(rev, 0, iz);
            std::ptr::write_bytes(cat, u8::MAX, iz);
            std::ptr::copy_nonoverlapping(s.k.as_ptr(), keys.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.d.as_ptr(), day.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.s.as_ptr(), ses.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.e.as_ptr(), evs.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.r.as_ptr(), rev.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.c.as_ptr(), cat.add(iz), ip);
            iz
        } };
    crate::pqw::prof::sw(_pv);
    let (ck, cd, cs, ce, cr, cc) = cols.unwrap_or(unsafe { (v.keys.add(a), v.day.add(a), v.ses.add(a), v.evs.add(a), v.rev.add(a), v.cat.add(a)) });
    let u = unsafe {
        UserCols {
            names: StrCol { offsets: sl(v.noff, v.nnoff), data: sl(v.names, v.nnames) },
            keys: sl(ck, n),
            day: sl(cd, n),
            ses: sl(cs, n),
            evs: sl(ce, n),
            rev: sl(cr, n),
            cat: sl(cc, n),
            cats: StrCol { offsets: cats_off, data: cats_data },
            head,
            st,
        }
    };
    if f.is_none() {
        let r = if S3_SMALL.with(|c| c.get()) { UserFile::create_cap(path, S3_LIMIT, S3_LIMIT + (64 << 10)) } else { UserFile::create(path, UF_LIMIT) };
        match r {
            Ok(x) => *f = Some(x),
            Err(_) => die("user_daily"),
        }
    }
    if f.as_mut().unwrap().add(&u, &mut s.b).is_err() {
        die("user_daily");
    }
    });
}

fn write_rows(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], kbuf: &mut Vec<u32>) {
    use std::slice::from_raw_parts as sl;
    let n = b - a;
    if SPLIT_ROWS && n > 0 && RPG == 0 {
        return write_split(v, a, b, path, cats_off, cats_data);
    }
    let keys = unsafe { sl(v.keys.add(a), n) };
    let _ = &kbuf;
    let (names, noff) = unsafe { (sl(v.names, v.nnames), sl(v.noff, v.nnoff)) };
    let (no, lo): (&[u32], u32) = if a == b { (&[], 0) } else { (noff, 0) };
    let cols = unsafe {
        [
            Column { name: "user_id", data: Data::Dict { dict: StrCol { offsets: no, data: names }, keys: Keys::U32Off(keys, lo), nullable: false, plain: false } },
            Column { name: "day", data: Data::Date(sl(v.day.add(a), n), IntEnc::Dense) },
            Column { name: "sessions", data: Data::I32(sl(v.ses.add(a), n), IntEnc::Dense) },
            Column { name: "events", data: Data::I32(sl(v.evs.add(a), n), IntEnc::Auto) },
            Column { name: "revenue_cents", data: Data::I32(sl(v.rev.add(a), n), IntEnc::Plain) },
            Column { name: "top_category", data: Data::Dict { dict: StrCol { offsets: cats_off, data: cats_data }, keys: Keys::U8(sl(v.cat.add(a), n)), nullable: true, plain: false } },
        ]
    };
    let opts = if RPG == 0 { Opts::default() } else { Opts { rows_per_group: RPG } };
    if write_file(path, &cols, &opts, Method::Stream(1 << 20)).is_err() {
        die("user_daily");
    }
}

fn write_campaign(g: &G, dir: &Path) {
    let nd = g.nd;
    let n = (g.nc + 1) * nd;
    let mut agg = vec![[0u64; 4]; n];
    for tid in 0..g.nt {
        let c = &g.t(tid).core;
        for i in 0..n {
            agg[i][0] += c.agg[i][0] as u64;
            agg[i][3] += c.agg[i][1] as u64;
            agg[i][1] += c.aggp[i][0];
            agg[i][2] += c.aggp[i][1];
        }
    }
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][4] = crate::sys::tl::now();
    }
    let (mut id_off, mut id_data, mut ch_off, mut ch_data) = (vec![0u32], Vec::new(), vec![0u32], Vec::new());
    let mut day = Vec::new();
    let mut v: [Vec<i64>; 4] = Default::default();
    for cn in 0..=g.nc {
        for d in 0..nd {
            let a = agg[cn * nd + d];
            if a[0] == 0 && a[3] == 0 {
                continue;
            }
            if cn == g.nc {
                id_data.extend_from_slice(b"organic");
                ch_data.extend_from_slice(b"organic");
            } else {
                let mut d = [0u8; 24];
                let (mut i, mut x) = (d.len(), cn);
                loop {
                    i -= 1;
                    d[i] = b'0' + (x % 10) as u8;
                    x /= 10;
                    if x == 0 {
                        break;
                    }
                }
                i -= 2;
                d[i] = b'c';
                d[i + 1] = b'_';
                id_data.extend_from_slice(&d[i..]);
                ch_data.extend_from_slice(&g.channels[g.camp[cn] as usize]);
            }
            id_off.push(id_data.len() as u32);
            ch_off.push(ch_data.len() as u32);
            day.push(g.day0 + d as i32);
            for k in 0..4 {
                v[k].push(a[k] as i64);
            }
        }
    }
    let empty: &[u32] = &[];
    let rows = day.len();
    let cols = [
        Column { name: "campaign_id", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &id_off }, data: &id_data }) },
        Column { name: "channel", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &ch_off }, data: &ch_data }) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&v[0], IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&v[1], IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&v[2], IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&v[3], IntEnc::Plain) },
    ];
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][5] = crate::sys::tl::now();
    }
    if write_file(&dir.join("part-0.parquet"), &cols, &Opts::default(), Method::Whole).is_err() {
        die("campaign_daily");
    }
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][6] = crate::sys::tl::now();
    }
    if g.check {
        let (mut us, mut ur) = (0i64, 0i64);
        for tid in 0..g.nt {
            let c = &g.t(tid).core;
            us += c.ses.as_slice().iter().map(|&x| x as i64).sum::<i64>();
            ur += c.rev.as_slice().iter().map(|&x| x as i64).sum::<i64>();
        }
        let cs: i64 = v[0].iter().sum();
        let cr: i64 = v[2].iter().sum();
        let ca: i64 = v[3].iter().sum();
        let organic: i64 = (0..nd).map(|d| agg[g.nc * nd + d][0] as i64).sum();
        eprintln!("[v2b] check: sessions user={us} campaign={cs} revenue user={ur} campaign={cr} assisted={ca} credited_non_organic={}", cs - organic);
        if us != cs || ur != cr || ca < cs - organic {
            eprintln!("[v2b] CHECK FAILED");
            unsafe { sys::_exit(5) }
        }
    }
}

fn write_campaign_part(g: &G, path: &Path, lo: usize, hi: usize) -> [i64; 4] {
    let nd = g.nd;
    let n = (hi - lo) * nd;
    let _pv = crate::pqw::prof::sw(16);
    let mut agg: Vec<[u64; 4]> = Vec::with_capacity(n);
    unsafe {
        std::ptr::write_bytes(agg.as_mut_ptr(), 0, n);
        agg.set_len(n);
    }
    for tid in 0..g.nt {
        let c = &g.t(tid).core;
        let base = lo * nd;
        if n == 0 {
            break;
        }
        let (pa, pp) = (&c.agg[base + n - 1] as *const [u32; 2], &c.aggp[base + n - 1] as *const [u64; 2]);
        unsafe {
            let (pa, pp) = (pa.sub(n - 1), pp.sub(n - 1));
            for i in 0..n {
                let (a, p) = (*pa.add(i), *pp.add(i));
                let s = agg.get_unchecked_mut(i);
                s[0] += a[0] as u64;
                s[1] += p[0];
                s[2] += p[1];
                s[3] += a[1] as u64;
            }
        }
    }
    crate::pqw::prof::sw(_pv);
    let chmax = g.channels.iter().map(|c| c.len()).max().unwrap_or(0).max(7);
    let (mut id_off, mut ch_off) = (Vec::<u32>::with_capacity(n + 1), Vec::<u32>::with_capacity(n + 1));
    let mut id_data = Vec::<u8>::with_capacity(n * 24 + 64);
    let mut ch_data = Vec::<u8>::with_capacity(n * chmax + 64);
    let mut day = Vec::<i32>::with_capacity(n);
    let mut v: [Vec<i64>; 4] = [Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n)];
    let mut organic = 0i64;
    let mut rows = 0usize;
    unsafe {
        let (io, co, ip, cp, dp) = (id_off.as_mut_ptr(), ch_off.as_mut_ptr(), id_data.as_mut_ptr(), ch_data.as_mut_ptr(), day.as_mut_ptr());
        let vp = [v[0].as_mut_ptr(), v[1].as_mut_ptr(), v[2].as_mut_ptr(), v[3].as_mut_ptr()];
        *io = 0;
        *co = 0;
        let (mut il, mut cl) = (0usize, 0usize);
        for cn in lo..hi {
            let row0 = (cn - lo) * nd;
            if (0..nd).all(|d| { let a = agg.get_unchecked(row0 + d); a[0] == 0 && a[3] == 0 }) {
                continue;
            }
            let mut id = [0u8; 32];
            let mut ch = [0u8; 32];
            let (idn, chs): (usize, &[u8]) = if cn == g.nc {
                id[..7].copy_from_slice(b"organic");
                (7, b"organic")
            } else {
                let mut d = [0u8; 24];
                let (mut i, mut x) = (d.len(), cn);
                loop {
                    i -= 1;
                    d[i] = b'0' + (x % 10) as u8;
                    x /= 10;
                    if x == 0 {
                        break;
                    }
                }
                i -= 2;
                d[i] = b'c';
                d[i + 1] = b'_';
                id[..24 - i].copy_from_slice(&d[i..]);
                (24 - i, &g.channels[g.camp[cn] as usize][..])
            };
            let chn = chs.len();
            let short = chn <= 32;
            if short {
                ch[..chn].copy_from_slice(chs);
            }
            for d in 0..nd {
                let a = *agg.get_unchecked(row0 + d);
                if a[0] == 0 && a[3] == 0 {
                    continue;
                }
                if cn == g.nc {
                    organic += a[0] as i64;
                }
                (ip.add(il) as *mut [u8; 32]).write_unaligned(id);
                il += idn;
                if short {
                    (cp.add(cl) as *mut [u8; 32]).write_unaligned(ch);
                } else {
                    std::ptr::copy_nonoverlapping(chs.as_ptr(), cp.add(cl), chn);
                }
                cl += chn;
                rows += 1;
                *io.add(rows) = il as u32;
                *co.add(rows) = cl as u32;
                *dp.add(rows - 1) = g.day0 + d as i32;
                for k in 0..4 {
                    *vp[k].add(rows - 1) = a[k] as i64;
                }
            }
        }
        id_off.set_len(rows + 1);
        ch_off.set_len(rows + 1);
        id_data.set_len(il);
        ch_data.set_len(cl);
        day.set_len(rows);
        for k in 0..4 {
            v[k].set_len(rows);
        }
    }
    let empty: &[u32] = &[];
    let cols = [
        Column { name: "campaign_id", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &id_off }, data: &id_data }) },
        Column { name: "channel", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &ch_off }, data: &ch_data }) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&v[0], IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&v[1], IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&v[2], IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&v[3], IntEnc::Plain) },
    ];
    let _pv = crate::pqw::prof::sw(17);
    if (rows > 0 || lo == 0) && write_file(path, &cols, &Opts::default(), Method::Whole).is_err() {
        die("campaign_daily");
    }
    crate::pqw::prof::sw(_pv);
    [v[0].iter().sum(), v[2].iter().sum(), v[3].iter().sum(), organic]
}

fn clear_stale(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().as_encoded_bytes().ends_with(b".parquet") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn stage5(g: &mut G, outdir: &str) {
    let ud = Path::new(outdir).join("user_daily");
    let cd = Path::new(outdir).join("campaign_daily");
    if std::fs::create_dir_all(&ud).is_err() || std::fs::create_dir_all(&cd).is_err() {
        die("cannot create the output folders");
    }
    clear_stale(&ud);
    clear_stale(&cd);
    let mut cats_off = vec![0u32];
    let mut cats_data = Vec::new();
    for c in &g.cats {
        cats_data.extend_from_slice(c);
        cats_off.push(cats_data.len() as u32);
    }
    let nt = g.nt;
    let mut base = vec![0usize; nt + 1];
    for t in 0..nt {
        base[t + 1] = base[t] + g.t(t).core.day.len();
    }
    let total = base[nt];
    let np = (2 * nt).min(total).max(1);
    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    for p in 0..np {
        let (ga, gb) = (total * p / np, total * (p + 1) / np);
        for t in 0..nt {
            let (a, b) = (ga.max(base[t]), gb.min(base[t + 1]));
            if a < b {
                pieces.push((t, a - base[t], b - base[t]));
            }
        }
    }
    if pieces.is_empty() {
        pieces.push((0, 0, 0));
    }
    let gr: &G = g;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (pr, nx) = (&pieces, &next);
    sys::run_par(nt, |_| {
        let mut kbuf = Vec::new();
        loop {
            let i = nx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i == 0 {
                write_campaign(gr, &cd);
            } else if i <= pr.len() {
                let (t, a, b) = pr[i - 1];
                write_piece(gr, t, a, b, &ud.join(format!("part-{}.parquet", i - 1)), &cats_off, &cats_data, &mut kbuf);
            } else {
                break;
            }
        }
        end_user_file();
    });
}

fn cut(rows: &[(usize, usize)], np: usize) -> Vec<(usize, usize, usize)> {
    let mut base = vec![0usize; rows.len() + 1];
    for (t, &(a, b)) in rows.iter().enumerate() {
        base[t + 1] = base[t] + (b - a);
    }
    let total = base[rows.len()];
    let np = np.min(total).max(1);
    let mut v = Vec::new();
    for p in 0..np {
        let (ga, gb) = (total * p / np, total * (p + 1) / np);
        for t in 0..rows.len() {
            let (a, b) = (ga.max(base[t]), gb.min(base[t + 1]));
            if a < b {
                v.push((t, rows[t].0 + a - base[t], rows[t].0 + b - base[t]));
            }
        }
    }
    v
}

const RPG: usize = crate::env_u32(option_env!("XL_RPG_DEF"), 0) as usize;

fn cut_guided(rows: &[(usize, usize)], nt: usize) -> Vec<(usize, usize, usize)> {
    let total: usize = rows.iter().map(|&(a, b)| b - a).sum();
    let min = (total / (8 * nt)).max(4096);
    let mut v = Vec::new();
    let mut rem = total;
    let (mut t, mut at) = (0usize, rows.first().map_or(0, |r| r.0));
    while rem > 0 {
        let mut want = (rem / (2 * nt)).max(min).min(rem);
        rem -= want;
        while want > 0 {
            while at >= rows[t].1 {
                t += 1;
                at = rows[t].0;
            }
            let take = want.min(rows[t].1 - at);
            v.push((t, at, at + take));
            at += take;
            want -= take;
        }
    }
    v
}

pub static REGION_UNMAP: AtomicBool = AtomicBool::new(true);

pub fn stage45(g: &mut G, outdir: &str) {
    crate::complex::stage4_pre(g);
    let own;
    let ctx = match S3CTX.get() {
        Some(c) => c,
        None => {
            own = ctx_new(g, outdir);
            &own
        }
    };
    s3_prep(ctx);
    let (ud, cd, cats_off, cats_data) = (&ctx.ud, &ctx.cd, &ctx.cats_off[..], &ctx.cats_data[..]);
    let nt = g.nt;
    let v3: Vec<View> = (0..nt).map(|t| view(&g.t(t).core)).collect();
    let s3w: Vec<usize> = (0..nt).map(|t| g.t(t).core.s3w).collect();
    if (0..nt).any(|t| s3w[t] > v3[t].rows) {
        die("s3w");
    }
    let s3_any = (0..nt).any(|t| s3w[t] > 0);
    let pieces3 = cut_guided(&(0..nt).map(|t| (s3w[t], v3[t].rows)).collect::<Vec<_>>(), nt);
    let order = crate::complex::shard_order(g);
    g.next.store(0, Ordering::Relaxed);
    let gr: &G = g;
    let gr_nc_plus1 = |g: &G| g.nc + 1;
    let (ca_done, sh_next, sh_done, pq, fq) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let (ns, np) = (order.len(), pieces3.len());
    let nf = 2 * nt;
    let csum: Vec<std::sync::Mutex<[i64; 4]>> = (0..nt).map(|_| std::sync::Mutex::new([0; 4])).collect();
    let ncp = gr_nc_plus1(g);
    let rg_len = gr.files.last().map_or(0, |f| f.off + f.len);
    let rg_todo = AtomicBool::new(REGION_UNMAP.load(Ordering::Relaxed) && rg_len > 0 && !gr.region.is_null());
    #[cfg(feature = "tl")]
    let (pw0, pt0) = (std::time::Instant::now(), crate::sys::tl::now());
    #[cfg(feature = "tl")]
    {
        crate::pqw::prof::ON.store(std::env::var("XL_WPROF").is_ok_and(|v| v == "1"), Ordering::Relaxed);
        crate::pqw::prof::INSTR.store(std::env::var("XL_WINSTR").is_ok_and(|v| v == "1"), Ordering::Relaxed);
    }
    run_par(nt, |tid| {
        if tid < 16 {
            if let Some(f) = unsafe { (*S3FILE.0[tid].get()).take() } {
                UFILE.with(|u| {
                    let mut u = u.borrow_mut();
                    if let Some(old) = u.replace(f) {
                        if old.finish().is_err() {
                            die("user_daily");
                        }
                    }
                });
            }
        }
        crate::pqw::prof::begin();
        crate::pqw::prof::sw(crate::pqw::prof::S4);
        crate::complex::clicks_a(gr, tid);
        crate::pqw::prof::sw(0);
        ca_done.fetch_add(1, Ordering::AcqRel);
        let mut sc = crate::complex::PScratch::default();
        let mut kbuf = Vec::new();
        loop {
            if ca_done.load(Ordering::Acquire) == nt && sh_next.load(Ordering::Relaxed) < ns {
                let i = sh_next.fetch_add(1, Ordering::Relaxed);
                if i < ns {
                    #[cfg(feature = "tl")]
                    let t0 = crate::sys::tl::now();
                    crate::pqw::prof::sw(crate::pqw::prof::S4);
                    crate::complex::person_shard(gr, tid, order[i], &mut sc);
                    crate::pqw::prof::sw(0);
                    #[cfg(feature = "tl")]
                    unsafe {
                        if i < 8 {
                            crate::sys::tl::TASK[i] = (t0, crate::sys::tl::now(), tid as u64);
                        }
                        crate::sys::tl::TASK[63] = (crate::sys::tl::TASK[63].0.max(crate::sys::tl::now()), 0, 0);
                    }
                    sh_done.fetch_add(1, Ordering::AcqRel);
                    continue;
                }
            }
            let shards_done = ca_done.load(Ordering::Acquire) == nt && sh_done.load(Ordering::Acquire) == ns;
            if !shards_done || fq.load(Ordering::Relaxed) >= nf {
                if rg_todo.load(Ordering::Relaxed) && rg_todo.swap(false, Ordering::AcqRel) {
                    extern "C" {
                        fn munmap(p: *mut u8, n: usize) -> i32;
                    }
                    #[cfg(feature = "tl")]
                    let t0 = crate::sys::tl::now();
                    unsafe { munmap(gr.region as *mut u8, rg_len) };
                    #[cfg(feature = "tl")]
                    unsafe {
                        crate::sys::tl::TASK[62] = (t0, crate::sys::tl::now(), tid as u64);
                    }
                    continue;
                }
                if pq.load(Ordering::Relaxed) < np {
                    let i = pq.fetch_add(1, Ordering::Relaxed);
                    if i < np {
                        let (t, a, b) = pieces3[i];
                        #[cfg(feature = "tl")]
                        let t0 = crate::sys::tl::now();
                        crate::pqw::prof::sw(crate::pqw::prof::MISC);
                        write_rows(&v3[t], a, b, &ud.join(format!("part-{i}.parquet")), &cats_off, &cats_data, &mut kbuf);
                        crate::pqw::prof::sw(0);
                        #[cfg(feature = "tl")]
                        unsafe {
                            crate::sys::tl::TASK[(16 + i).min(47)] = (t0, crate::sys::tl::now(), tid as u64);
                        }
                        continue;
                    }
                }
                if !shards_done {
                    std::hint::spin_loop();
                    continue;
                }
            }
            let i = fq.fetch_add(1, Ordering::Relaxed);
            if i >= nf {
                if pq.load(Ordering::Relaxed) < np {
                    continue;
                }
                break;
            }
            #[cfg(feature = "tl")]
            let t0 = crate::sys::tl::now();
            if i < nt {
                let (lo, hi) = (ncp * i / nt, ncp * (i + 1) / nt);
                let _pv = crate::pqw::prof::sw(crate::pqw::prof::CAMP);
                let s = write_campaign_part(gr, &cd.join(format!("part-{i}.parquet")), lo, hi);
                *csum[i].lock().unwrap() = s;
                crate::pqw::prof::sw(_pv);
                #[cfg(feature = "tl")]
                unsafe {
                    crate::sys::tl::TASK[8 + i.min(7)] = (t0, crate::sys::tl::now(), tid as u64);
                }
            } else {
                let t = i - nt;
                let v = view(&gr.t(t).core);
                if v.rows > v3[t].rows || (np == 0 && t == 0 && !s3_any) {
                    crate::pqw::prof::sw(crate::pqw::prof::MISC);
                    write_rows(&v, v3[t].rows, v.rows, &ud.join(format!("part-{}.parquet", np + t)), &cats_off, &cats_data, &mut kbuf);
                    crate::pqw::prof::sw(0);
                }
            }
        }
        end_user_file();
        crate::pqw::prof::sw(0);
    });
    #[cfg(feature = "tl")]
    crate::pqw::prof::report(pw0.elapsed().as_secs_f64() * 1e3, crate::sys::tl::now() - pt0);
    if gr.check {
        let (mut us, mut ur) = (0i64, 0i64);
        for tid in 0..nt {
            let c = &gr.t(tid).core;
            us += c.ses.as_slice().iter().map(|&x| x as i64).sum::<i64>();
            ur += c.rev.as_slice().iter().map(|&x| x as i64).sum::<i64>();
        }
        let mut s = [0i64; 4];
        for m in &csum {
            let x = *m.lock().unwrap();
            for k in 0..4 {
                s[k] += x[k];
            }
        }
        let [cs, cr, ca, organic] = s;
        eprintln!("[v2b] check: sessions user={us} campaign={cs} revenue user={ur} campaign={cr} assisted={ca} credited_non_organic={}", cs - organic);
        if us != cs || ur != cr || ca < cs - organic {
            eprintln!("[v2b] CHECK FAILED");
            unsafe { sys::_exit(5) }
        }
    }
}
