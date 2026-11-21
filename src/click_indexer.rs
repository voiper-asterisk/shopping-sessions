use crate::iface::*;
use crate::parse::mul8;
use crate::*;
use std::arch::x86_64::*;

pub const VALID: u64 = 1 << 32;

#[repr(C, align(64))]
pub struct Pend {
    rec: [[u64; 2]; NB + 8],
    at: [u64; NB + 8],
    n: usize,
    pub end: u64,
}

pub struct CkxP {
    pub cbase: i64,
    pub clen: u64,
    pub ukey: u64,
    pub ucap: u64,
    pub ckey: u64,
    pub ccap: u64,
    pub ctab: *const u8,
    pub crange: Option<(u64, u64)>,
    pub csr: *mut [u64; 2],
}

#[inline(always)]
pub unsafe fn flush(pd: &mut Pend, csr: *mut [u64; 2]) {
    let n = pd.n;
    for i in 0..n {
        let r = _mm_load_si128(pd.rec.as_ptr().add(i) as *const __m128i);
        _mm_store_si128(csr.add(*pd.at.get_unchecked(i) as usize) as *mut __m128i, r);
    }
    pd.n = 0;
}

#[inline(always)]
pub unsafe fn store_one(pd: &mut Pend, csr: *mut [u64; 2], c: u64, w0: u64, u: u64) {
    *csr.add(c as usize) = [w0, u | VALID];
    pd.end = pd.end.max(c + 1);
}

#[inline(always)]
pub unsafe fn ck_vec_ix(cb: &CkBatch, pd: &mut Pend, n: usize, p: &CkxP) -> u64 {
    flush(pd, p.csr);
    let mut spb = 0u64;
    let s = |v: u64| _mm512_set1_epi64(v as i64);
    let m40 = s(crate::keyrec::MASK);
    let binv = s(crate::keyrec::BINV);
    let ainv = s(crate::keyrec::AINV);
    let ukey = s(p.ukey & crate::keyrec::MASK);
    let ckey = s(p.ckey & crate::keyrec::MASK);
    let lim40 = s(1 << 40);
    let cbase = s(p.cbase as u64);
    let clen = s(p.clen);
    let empty = s(ID_EMPTY);
    let cbound = s(CBOUND as u64);
    let mff = s(255);
    let ucap = s(p.ucap);
    let ccap = s(p.ccap);
    let nal = s(NALIAS);
    let m511 = s(511);
    let valid = s(VALID);
    let one = s(1);
    let ranged = p.crange.is_some();
    let (clo, cspan) = (s(p.crange.unwrap_or((0, 0)).0), s(p.crange.unwrap_or((0, 0)).1));
    let ix_lo = _mm512_setr_epi64(0, 8, 1, 9, 2, 10, 3, 11);
    let ix_hi = _mm512_setr_epi64(4, 12, 5, 13, 6, 14, 7, 15);
    let nm = if n >= 64 { !0 } else { (1u64 << n) - 1 };
    let at = pd.at.as_mut_ptr();
    let rec = pd.rec.as_mut_ptr() as *mut u64;
    let mut k = 0usize;
    let mut endv = _mm512_setzero_si512();
    let mut j = 0;
    while j < n {
        let ts = _mm512_load_si512(cb.ts.as_ptr().add(j) as *const __m512i);
        let rel = _mm512_sub_epi64(ts, cbase);
        let cid = _mm512_load_si512(cb.cid.as_ptr().add(j) as *const __m512i);
        let uid = _mm512_load_si512(cb.uid.as_ptr().add(j) as *const __m512i);
        let camp = _mm512_load_si512(cb.camp.as_ptr().add(j) as *const __m512i);
        let incsv = if ranged {
            _mm512_cmple_epu64_mask(_mm512_sub_epi64(camp, clo), cspan)
        } else {
            let inb = _mm512_cmplt_epu64_mask(camp, cbound);
            let ch = _mm512_and_si512(_mm512_mask_i64gather_epi64::<1>(mff, inb, camp, p.ctab as *const i64), mff);
            inb & _mm512_cmpneq_epi64_mask(ch, mff)
        };
        let keep = incsv & _mm512_cmpneq_epi64_mask(uid, empty) & _mm512_cmplt_epu64_mask(rel, clen);
        let x = _mm512_and_si512(mul8(uid, binv), m40);
        let x = _mm512_xor_si512(x, _mm512_xor_si512(_mm512_srli_epi64::<17>(x), _mm512_srli_epi64::<34>(x)));
        let w = _mm512_and_si512(mul8(_mm512_xor_si512(x, ukey), ainv), m40);
        let u = _mm512_srli_epi64::<9>(w);
        let alias = _mm512_and_si512(w, m511);
        let normal = _mm512_cmplt_epu64_mask(uid, lim40) & _mm512_cmplt_epu64_mask(u, ucap) & _mm512_cmplt_epu64_mask(alias, nal);
        let y = _mm512_and_si512(mul8(cid, binv), m40);
        let y = _mm512_xor_si512(y, _mm512_xor_si512(_mm512_srli_epi64::<17>(y), _mm512_srli_epi64::<34>(y)));
        let c = _mm512_and_si512(mul8(_mm512_xor_si512(y, ckey), ainv), m40);
        let cidok = _mm512_cmplt_epu64_mask(cid, lim40) & _mm512_cmplt_epu64_mask(c, ccap);
        let gm = (nm >> j) as u8;
        let ok = keep & normal & cidok & gm;
        let key = _mm512_or_si512(_mm512_slli_epi64::<32>(rel), _mm512_slli_epi64::<4>(camp));
        let o0 = _mm512_or_si512(key, alias);
        let o1 = _mm512_or_si512(u, valid);
        let a0 = _mm512_maskz_compress_epi64(ok, o0);
        let a1 = _mm512_maskz_compress_epi64(ok, o1);
        _mm512_storeu_si512(at.add(k) as *mut __m512i, _mm512_maskz_compress_epi64(ok, c));
        _mm512_storeu_si512(rec.add(2 * k) as *mut __m512i, _mm512_permutex2var_epi64(a0, ix_lo, a1));
        _mm512_storeu_si512(rec.add(2 * k + 8) as *mut __m512i, _mm512_permutex2var_epi64(a0, ix_hi, a1));
        endv = _mm512_mask_max_epu64(endv, ok, endv, _mm512_add_epi64(c, one));
        k += ok.count_ones() as usize;
        spb |= ((keep & !ok & gm) as u64) << j;
        j += 8;
    }
    pd.n = k;
    if PF_AHEAD {
        for i in 0..k {
            _mm_prefetch::<{ _MM_HINT_ET0 }>(p.csr.add(*at.add(i) as usize) as *const i8);
        }
    } else {
        flush(pd, p.csr);
    }
    pd.end = pd.end.max(_mm512_reduce_max_epu64(endv));
    spb
}
const PF_AHEAD: bool = env_u32(option_env!("XL_CKPF"), 1) == 1;
