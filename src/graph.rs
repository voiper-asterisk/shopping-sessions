use crate::keyrec::scramble;
use crate::sys::*;
use crate::*;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

pub struct Uf {
    map: U64Map,
    parent: Vec<u32>,
    toks: Vec<u64>,
    root: Vec<u32>,
    name_at: Vec<(u32, u32)>,
    name_buf: Vec<u8>,
}
const RAWKEY: u64 = 1 << 62;

fn hbytes(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &c in b {
        h = (h ^ c as u64).wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^ (h >> 32)
}

impl Uf {
    pub fn new() -> Uf {
        Uf::with_capacity(0)
    }
    pub fn with_capacity(n: usize) -> Uf {
        Uf { map: U64Map::with_capacity(n), parent: Vec::with_capacity(n), toks: Vec::with_capacity(n), root: Vec::new(), name_at: Vec::new(), name_buf: Vec::new() }
    }
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    fn node(&mut self, g: &G, tok: u64) -> u32 {
        let n = self.parent.len() as u32;
        let got = if tok & RAW == 0 {
            self.map.get_or_insert(tok, n)
        } else {
            let b = g.raw_bytes(tok);
            let got = self.map.get_or_insert(RAWKEY | (hbytes(b) & (RAWKEY - 1)), n);
            if got != n && g.raw_bytes(self.toks[got as usize]) != b {
                fallback("raw ID hash collision");
            }
            got
        };
        if got == n {
            self.parent.push(n);
            self.toks.push(tok);
        }
        got
    }
    #[inline]
    fn find(&mut self, mut x: u32) -> u32 {
        let mut r = x;
        while self.parent[r as usize] != r {
            r = self.parent[r as usize];
        }
        while self.parent[x as usize] != r {
            let n = self.parent[x as usize];
            self.parent[x as usize] = r;
            x = n;
        }
        r
    }
    #[inline]
    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            if ra < rb {
                self.parent[rb as usize] = ra;
            } else {
                self.parent[ra as usize] = rb;
            }
        }
    }
    fn finish(&mut self, g: &G) {
        let n = self.parent.len();
        let mut root = Vec::with_capacity(n);
        for i in 0..n as u32 {
            root.push(self.find(i));
        }
        let mut best = vec![u32::MAX; n];
        let mut enc = Vec::with_capacity(n);
        for &t in &self.toks {
            enc.push(if t & RAW == 0 { enc36(t).0 } else { 0 });
        }
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        for i in 0..n {
            let r = root[i] as usize;
            let b = best[r];
            let smaller = if b == u32::MAX {
                true
            } else {
                let (ta, tb) = (self.toks[i], self.toks[b as usize]);
                if (ta | tb) & RAW == 0 {
                    enc[i] < enc[b as usize]
                } else {
                    g.id_str(ta, b'u', &mut sa);
                    g.id_str(tb, b'u', &mut sb);
                    sa < sb
                }
            };
            if smaller {
                best[r] = i as u32;
            }
        }
        let mut at = vec![(0u32, 0u32); n];
        let mut buf = Vec::new();
        for r in 0..n {
            if best[r] != u32::MAX {
                g.id_str(self.toks[best[r] as usize], b'u', &mut sa);
                if buf.len() + sa.len() > u32::MAX as usize {
                    fallback("names too long");
                }
                at[r] = (buf.len() as u32, sa.len() as u32);
                buf.extend_from_slice(&sa);
            }
        }
        self.root = root;
        self.name_at = at;
        self.name_buf = buf;
    }
    #[inline]
    pub fn canon_root(&self, y: u64) -> Option<u32> {
        self.map.get(y).map(|n| self.root[n as usize])
    }
    pub fn raw_root(&self, g: &G, bytes: &[u8]) -> Option<u32> {
        let n = self.map.get(RAWKEY | (hbytes(bytes) & (RAWKEY - 1)))?;
        if g.raw_bytes(self.toks[n as usize]) != bytes {
            return None;
        }
        Some(self.root[n as usize])
    }
    pub fn name(&self, root: u32) -> &[u8] {
        let (a, l) = self.name_at[root as usize];
        &self.name_buf[a as usize..(a + l) as usize]
    }
    fn part1(&mut self, g: &G) {
        for tid in 0..g.nt {
            let t = g.t(tid);
            for &(a, b) in &t.gen_edges {
                let na = self.node(g, a);
                let nb = self.node(g, b);
                self.union(na, nb);
            }
            for &tok in &t.odd_users {
                if tok & RAW != 0 {
                    self.node(g, tok);
                }
            }
            for c in &t.odd_clicks {
                if c[0] & RAW != 0 || (g.ccap != 0 && g.normal(c[0]).is_some()) {
                    self.node(g, c[0]);
                }
            }
        }
    }
    fn cx(&mut self, g: &G, edges: &[(u32, u8, u8)]) {
        for &(u, a, b) in edges {
            let na = self.node(g, scramble((u as u64) << 9 | a as u64, g.ukey));
            let nb = self.node(g, scramble((u as u64) << 9 | b as u64, g.ukey));
            self.union(na, nb);
        }
    }
    fn part2a(&mut self, g: &G) {
        for tid in 0..g.nt {
            self.cx(g, &g.t(tid).cx_edges);
        }
    }
}

#[inline]
pub fn resolve_normal(g: &G, u: u32, a: u32) -> u64 {
    let tv = unsafe { *g.utab.add(u as usize) };
    if tv & COMPLEX != 0 {
        let y = scramble((u as u64) << 9 | a as u64, g.ukey);
        match g.uf.canon_root(y) {
            Some(r) => TAG_UF | r as u64,
            None => (u as u64) << 4 | a as u64,
        }
    } else if ((tv & MASK) >> a) & 1 != 0 {
        (u as u64) << 4 | 15
    } else {
        (u as u64) << 4 | a as u64
    }
}

pub fn resolve_token(g: &G, tok: u64) -> u64 {
    if tok & RAW == 0 {
        match g.uf.canon_root(tok) {
            Some(r) => TAG_UF | r as u64,
            None => TAG_ODDC | tok,
        }
    } else {
        match g.uf.raw_root(g, g.raw_bytes(tok)) {
            Some(r) => TAG_UF | r as u64,
            None => fallback("raw user ID was not interned"),
        }
    }
}

pub fn name_of(g: &G, pkey: u64, out: &mut Vec<u8>) {
    out.clear();
    match pkey >> 62 {
        0 => {
            let u = (pkey >> 4) as u32;
            let code = (pkey & 15) as u32;
            let class = if code == 15 { (unsafe { *g.utab.add(u as usize) } & MASK) as u32 } else { 1 << code };
            let (v, len) = crate::core::simple_name(g, u, class);
            out.push(b'u');
            out.extend_from_slice(&v.to_be_bytes()[..len]);
        }
        1 => out.extend_from_slice(g.uf.name((pkey & 0xFFFF_FFFF) as u32)),
        _ => g.id_str(pkey & (RAW - 1), b'u', out),
    }
}

#[derive(Default)]
pub struct Scratch {
    tmp: Vec<[u64; 2]>,
    lk: Vec<u64>,
    l2: Vec<u32>,
    l8: Vec<u32>,
    l16: Vec<u32>,
    lbig: Vec<u32>,
}

use std::arch::x86_64::*;
use crate::sys::z::{self, Z};

const IDX_SH: u32 = 18;
const IDX_BITS: u64 = 0xFFFF_FFFF & !((1 << IDX_SH) - 1);
const NET_MAX: usize = 512;
const SORT_STRIDE: usize = 8;

#[inline(always)]
fn sort_clicks(g: &G, s: &mut [[u64; 2]], sc: &mut Scratch) {
    unsafe { sort_from(g, s.as_ptr(), s, sc) }
}

#[inline(always)]
unsafe fn sort_from(g: &G, src: *const [u64; 2], s: &mut [[u64; 2]], sc: &mut Scratch) {
    let n = s.len();
    if n < 2 {
        if n == 1 {
            *s.as_mut_ptr() = *src;
        }
    } else if n == 2 {
        sort2(g, src, s.as_mut_ptr());
    } else if n <= 8 {
        sort_r1(g, src, s.as_mut_ptr(), n);
    } else if n <= 16 {
        sort_r2(g, src, s.as_mut_ptr(), n);
    } else {
        sort_big(g, src, s, sc);
    }
}

#[inline(never)]
unsafe fn sort_big(g: &G, src: *const [u64; 2], s: &mut [[u64; 2]], sc: &mut Scratch) {
    let n = s.len();
    if n <= 32 {
        sort_net::<4>(g, src, s);
    } else if n <= 64 {
        sort_net::<8>(g, src, s);
    } else if n <= SPLIT_MIN.min(128) {
        sort_net::<16>(g, src, s);
    } else if n <= SPLIT_MIN.min(256) {
        sort_net::<32>(g, src, s);
    } else if n <= SPLIT_MIN.min(NET_MAX) {
        sort_net::<64>(g, src, s);
    } else if n >= LSD_MIN {
        if src != s.as_ptr() {
            std::ptr::copy_nonoverlapping(src, s.as_mut_ptr(), n);
        }
        lsd_sort(g, s, sc);
    } else {
        if src != s.as_ptr() {
            std::ptr::copy_nonoverlapping(src, s.as_mut_ptr(), n);
        }
        let bits = (usize::BITS - (n / SPLIT_PER).leading_zeros()).clamp(1, 11);
        let top = (64 - g.clen.max(1).leading_zeros()).clamp(11, 32);
        let mut tmp = std::mem::take(&mut sc.tmp);
        tmp.clear();
        tmp.reserve(n + 16);
        radix_split(g, s, tmp.as_mut_ptr(), top - bits, bits, true, sc);
        sc.tmp = tmp;
    }
}
const LSD_MIN: usize = env_u32(option_env!("XL_LSD_MIN"), 2048) as usize;

#[inline(never)]
unsafe fn lsd_sort(g: &G, s: &mut [[u64; 2]], sc: &mut Scratch) {
    let n = s.len();
    if n > u32::MAX as usize {
        return slow_sort(g, s);
    }
    let mut tmp = std::mem::take(&mut sc.tmp);
    tmp.clear();
    tmp.reserve(n + 16);
    let p = s.as_mut_ptr();
    let mut h = [[0u32; 256]; 4];
    for i in 0..n {
        let k = ((*p.add(i))[0] >> 32) as usize;
        h[0][k & 255] += 1;
        h[1][(k >> 8) & 255] += 1;
        h[2][(k >> 16) & 255] += 1;
        h[3][k >> 24] += 1;
    }
    let k0 = ((*p)[0] >> 32) as usize;
    let (mut src, mut dst) = (p, tmp.as_mut_ptr());
    for d in 0..4 {
        let sh = 32 + 8 * d as u32;
        if h[d][(k0 >> (8 * d)) & 255] as usize == n {
            continue;
        }
        let mut pos = [0u32; 256];
        let mut at = 0u32;
        for b in 0..256 {
            pos[b] = at;
            at += h[d][b];
        }
        for i in 0..n {
            let r = src.add(i);
            let b = (((*r)[0] >> sh) & 255) as usize;
            let a = *pos.get_unchecked(b);
            *pos.get_unchecked_mut(b) = a + 1;
            _mm_storeu_si128(dst.add(a as usize) as *mut __m128i, _mm_loadu_si128(r as *const __m128i));
        }
        std::mem::swap(&mut src, &mut dst);
    }
    if src != p {
        std::ptr::copy_nonoverlapping(src, p, n);
    }
    sc.tmp = tmp;
    fix_ties(g, s);
}

const SPLIT_MIN: usize = env_u32(option_env!("XL_SPLIT_MIN"), 128) as usize;
const SPLIT_PER: usize = env_u32(option_env!("XL_SPLIT_PER"), 6) as usize;

#[inline(never)]
unsafe fn radix_split(g: &G, s: &mut [[u64; 2]], tmp: *mut [u64; 2], sh: u32, bits: u32, first: bool, sc: &mut Scratch) {
    let n = s.len();
    if n > u32::MAX as usize || bits == 0 || bits > 11 {
        return slow_sort(g, s);
    }
    let np = 1usize << bits;
    let digit = |w0: u64| (((w0 >> 32) >> sh) as usize) & (np - 1);
    let mut start = [0u32; 2049];
    let mut any = 0u64;
    for c in s.iter() {
        start[digit(c[0]) + 1] += 1;
        any |= c[0];
    }
    if first && ((any >> 32) >> sh) as usize > np - 1 {
        return slow_sort(g, s);
    }
    for d in 0..np {
        start[d + 1] += start[d];
    }
    let mut next = [0u32; 2048];
    next[..np].copy_from_slice(&start[..np]);
    let p = s.as_mut_ptr();
    for i in 0..n {
        let d = digit((*p.add(i))[0]);
        let at = *next.get_unchecked(d) as usize;
        *next.get_unchecked_mut(d) = at as u32 + 1;
        _mm_storeu_si128(tmp.add(at) as *mut __m128i, _mm_loadu_si128(p.add(i) as *const __m128i));
    }
    for d in 0..np {
        let (a, b) = (start[d] as usize, start[d + 1] as usize);
        let part = std::slice::from_raw_parts_mut(p.add(a), b - a);
        if b - a <= SPLIT_MIN.min(NET_MAX) {
            sort_from(g, tmp.add(a), part, sc);
        } else if sh == 0 {
            slow_from(g, tmp.add(a), p.add(a), b - a);
        } else {
            std::ptr::copy_nonoverlapping(tmp.add(a), p.add(a), b - a);
            let nb = sh.min(11);
            radix_split(g, part, tmp.add(a), sh - nb, nb, false, sc);
        }
    }
}

#[cold]
#[inline(never)]
fn slow_sort(g: &G, s: &mut [[u64; 2]]) {
    s.sort_by_key(|c| c[0] >> 32);
    fix_ties(g, s);
}

#[cold]
#[inline(never)]
unsafe fn slow_from(g: &G, src: *const [u64; 2], dst: *mut [u64; 2], n: usize) {
    if src != dst as *const [u64; 2] {
        std::ptr::copy_nonoverlapping(src, dst, n);
    }
    slow_sort(g, std::slice::from_raw_parts_mut(dst, n));
}

#[inline(never)]
fn fix_ties(g: &G, s: &mut [[u64; 2]]) {
    let n = s.len();
    let mut i = 1;
    while i < n {
        let x = s[i][0] ^ s[i - 1][0];
        if x >> 32 == 0 && x >> 4 != 0 {
            let ts = s[i][0] >> 32;
            if (i < 2 || s[i - 2][0] >> 32 != ts) && (i + 1 >= n || s[i + 1][0] >> 32 != ts) && (s[i - 1][1] | s[i][1]) & RAW == 0 {
                let (e0, e1) = (enc36(s[i - 1][1]).0, enc36(s[i][1]).0);
                if e0 < e1 || (e0 == e1 && s[i - 1][0] > s[i][0]) {
                    s.swap(i - 1, i);
                }
                i += 1;
            } else {
                i = tie_group(g, s, i);
            }
        } else {
            i += 1;
        }
    }
}

#[inline(always)]
unsafe fn sort2(g: &G, src: *const [u64; 2], p: *mut [u64; 2]) {
    let a = _mm_loadu_si128(src as *const __m128i);
    let b = _mm_loadu_si128(src.add(1) as *const __m128i);
    let (ka, kb) = ((*src)[0], (*src.add(1))[0]);
    let sw = (((kb >> 32) < (ka >> 32)) as u8).wrapping_neg() & 3;
    _mm_storeu_si128(p as *mut __m128i, _mm_mask_blend_epi64(sw, a, b));
    _mm_storeu_si128(p.add(1) as *mut __m128i, _mm_mask_blend_epi64(sw, b, a));
    let x = ka ^ kb;
    if x >> 32 == 0 && x >> 4 != 0 {
        fix_ties(g, std::slice::from_raw_parts_mut(p, 2));
    }
}

#[inline(always)]
unsafe fn cx(v: Z, perm: Z, hi: u8) -> Z {
    let p = z::permutexvar_epi64(perm, v);
    z::mask_mov_epi64(z::min_epu64(v, p), hi, z::max_epu64(v, p))
}
#[inline(always)]
unsafe fn cx_halves(v: Z) -> Z {
    Z(_mm256_min_epu64(v.0, v.1), _mm256_max_epu64(v.0, v.1))
}
#[inline(always)]
unsafe fn sort8(v: Z) -> Z {
    let p1 = z::setr_epi64(1, 0, 3, 2, 5, 4, 7, 6);
    let v = cx(v, p1, 0xAA);
    let v = cx(v, z::setr_epi64(3, 2, 1, 0, 7, 6, 5, 4), 0xCC);
    let v = cx(v, p1, 0xAA);
    let v = cx(v, z::setr_epi64(7, 6, 5, 4, 3, 2, 1, 0), 0xF0);
    let v = cx(v, z::setr_epi64(2, 3, 0, 1, 6, 7, 4, 5), 0xCC);
    cx(v, p1, 0xAA)
}
#[inline(always)]
unsafe fn merge8(v: Z) -> Z {
    let v = cx_halves(v);
    let v = cx(v, z::setr_epi64(2, 3, 0, 1, 6, 7, 4, 5), 0xCC);
    cx(v, z::setr_epi64(1, 0, 3, 2, 5, 4, 7, 6), 0xAA)
}
#[inline(always)]
unsafe fn rev8(v: Z) -> Z {
    Z(_mm256_permute4x64_epi64::<0b00_01_10_11>(v.1), _mm256_permute4x64_epi64::<0b00_01_10_11>(v.0))
}
#[inline(always)]
unsafe fn tie_mask(kc: Z, prev: Z) -> u8 {
    let x = z::xor(kc, prev);
    z::testn_epi64_mask(x, z::set1_epi64(0xFFFF_FFFF_0000_0000u64 as i64)) & z::test_epi64_mask(x, z::set1_epi64(0xFFFF_FFF0))
}
#[inline(always)]
unsafe fn rows_lo(k: Z, p: Z) -> Z {
    let (a, b) = (_mm256_unpacklo_epi64(k.0, p.0), _mm256_unpackhi_epi64(k.0, p.0));
    Z(_mm256_permute2x128_si256::<0x20>(a, b), _mm256_permute2x128_si256::<0x31>(a, b))
}
#[inline(always)]
unsafe fn rows_hi(k: Z, p: Z) -> Z {
    let (a, b) = (_mm256_unpacklo_epi64(k.1, p.1), _mm256_unpackhi_epi64(k.1, p.1));
    Z(_mm256_permute2x128_si256::<0x20>(a, b), _mm256_permute2x128_si256::<0x31>(a, b))
}
#[inline(always)]
unsafe fn pos8(base: i64) -> Z {
    let s = IDX_SH as i64;
    z::setr_epi64(base << s, (base + 1) << s, (base + 2) << s, (base + 3) << s, (base + 4) << s, (base + 5) << s, (base + 6) << s, (base + 7) << s)
}

#[inline(always)]
unsafe fn sort_r1(g: &G, src: *const [u64; 2], p: *mut [u64; 2], n: usize) {
    let q = p as *mut u64;
    let sq = src as *const u64;
    let m = (1u32 << (2 * n)) - 1;
    let km = ((1u32 << n) - 1) as u8;
    let idxb = z::set1_epi64(IDX_BITS as i64);
    let lo = z::maskz_loadu_epi64(m as u8, sq);
    let hi = z::maskz_loadu_epi64((m >> 8) as u8, sq.add(8));
    let kv = z::even_epi64(lo, hi);
    let vv = z::odd_epi64(lo, hi);
    if z::test_epi64_mask(kv, idxb) != 0 {
        return slow_from(g, src, p, n);
    }
    let k = sort8(z::mask_or_epi64(z::set1_epi64(-1), km, kv, pos8(0)));
    let pv = z::permutexvar_epi64(z::srli_epi64::<{ IDX_SH as i32 }>(k), vv);
    let kc = z::andnot(idxb, k);
    let tie = tie_mask(kc, z::prev_epi64(kc, kc)) & km & 0xFE;
    z::mask_storeu_epi64(q, m as u8, rows_lo(kc, pv));
    z::mask_storeu_epi64(q.add(8), (m >> 8) as u8, rows_hi(kc, pv));
    if tie != 0 {
        fix_ties(g, std::slice::from_raw_parts_mut(p, n));
    }
}

#[inline(always)]
unsafe fn sort_r2(g: &G, src: *const [u64; 2], p: *mut [u64; 2], n: usize) {
    let q = p as *mut u64;
    let sq = src as *const u64;
    let m1 = (1u32 << (2 * (n - 8))) - 1;
    let km1 = ((1u32 << (n - 8)) - 1) as u8;
    let idxb = z::set1_epi64(IDX_BITS as i64);
    let lo0 = z::loadu(sq);
    let hi0 = z::loadu(sq.add(8));
    let lo1 = z::maskz_loadu_epi64(m1 as u8, sq.add(16));
    let hi1 = z::maskz_loadu_epi64((m1 >> 8) as u8, sq.add(24));
    let kv0 = z::even_epi64(lo0, hi0);
    let v0 = z::odd_epi64(lo0, hi0);
    let kv1 = z::even_epi64(lo1, hi1);
    let v1 = z::odd_epi64(lo1, hi1);
    if z::test_epi64_mask(z::or(kv0, kv1), idxb) != 0 {
        return slow_from(g, src, p, n);
    }
    let a = sort8(z::or(kv0, pos8(0)));
    let b = rev8(sort8(z::mask_or_epi64(z::set1_epi64(-1), km1, kv1, pos8(8))));
    let k0 = merge8(z::min_epu64(a, b));
    let k1 = merge8(z::max_epu64(a, b));
    let p0 = z::permutex2var_epi64(v0, z::srli_epi64::<{ IDX_SH as i32 }>(k0), v1);
    let p1 = z::permutex2var_epi64(v0, z::srli_epi64::<{ IDX_SH as i32 }>(k1), v1);
    let kc0 = z::andnot(idxb, k0);
    let kc1 = z::andnot(idxb, k1);
    let tie = (tie_mask(kc0, z::prev_epi64(kc0, kc0)) & 0xFE) | (tie_mask(kc1, z::prev_epi64(kc1, kc0)) & km1);
    z::storeu(q, rows_lo(kc0, p0));
    z::storeu(q.add(8), rows_hi(kc0, p0));
    z::mask_storeu_epi64(q.add(16), m1 as u8, rows_lo(kc1, p1));
    z::mask_storeu_epi64(q.add(24), (m1 >> 8) as u8, rows_hi(kc1, p1));
    if tie != 0 {
        fix_ties(g, std::slice::from_raw_parts_mut(p, n));
    }
}

#[inline(never)]
unsafe fn sort_net<const R: usize>(g: &G, src: *const [u64; 2], s: &mut [[u64; 2]]) {
    let n = s.len();
    let q = s.as_mut_ptr() as *mut u64;
    let sq = src as *const u64;
    let maxv = z::set1_epi64(-1);
    let mut r = [maxv; R];
    let mut pay = [maxv; R];
    let mut orall = z::setzero();
    let nb = (n + 7) / 8;
    if nb > R || R * 8 > 1 << (32 - IDX_SH) {
        return slow_from(g, src, s.as_mut_ptr(), n);
    }
    for b in 0..nb {
        let left = n - 8 * b;
        let m: u32 = if left >= 8 { 0xFFFF } else { (1u32 << (2 * left)) - 1 };
        let km: u8 = if left >= 8 { 0xFF } else { ((1u32 << left) - 1) as u8 };
        let lo = z::maskz_loadu_epi64(m as u8, sq.add(16 * b));
        let hi = z::maskz_loadu_epi64((m >> 8) as u8, sq.add(16 * b + 8));
        let kv = z::even_epi64(lo, hi);
        orall = z::or(orall, kv);
        r[b] = sort8(z::mask_or_epi64(maxv, km, kv, pos8(8 * b as i64)));
        pay[b] = z::odd_epi64(lo, hi);
    }
    if z::test_epi64_mask(orall, z::set1_epi64(IDX_BITS as i64)) != 0 {
        return slow_from(g, src, s.as_mut_ptr(), n);
    }
    let mut k = 1usize;
    while k < R {
        let mut base = 0usize;
        while base < R {
            let (mut i, mut j) = (base + k, base + 2 * k - 1);
            while i < j {
                let (x, y) = (rev8(r[i]), rev8(r[j]));
                r[i] = y;
                r[j] = x;
                i += 1;
                j -= 1;
            }
            if i == j {
                r[i] = rev8(r[i]);
            }
            let mut d = k;
            while d >= 1 {
                let mut blk = base;
                while blk < base + 2 * k {
                    for t in blk..blk + d {
                        let (x, y) = (r[t], r[t + d]);
                        r[t] = z::min_epu64(x, y);
                        r[t + d] = z::max_epu64(x, y);
                    }
                    blk += 2 * d;
                }
                d /= 2;
            }
            for t in base..base + 2 * k {
                r[t] = merge8(r[t]);
            }
            base += 2 * k;
        }
        k *= 2;
    }
    let keys = r.as_ptr() as *const u64;
    let pv = pay.as_ptr() as *const u64;
    let mut prev = *keys & !IDX_BITS;
    let mut tie = false;
    for i in 0..n {
        let k = *keys.add(i);
        let kc = k & !IDX_BITS;
        let x = kc ^ prev;
        tie |= (x >> 32 == 0) & (x >> 4 != 0);
        prev = kc;
        *q.add(2 * i) = kc;
        *q.add(2 * i + 1) = std::ptr::read_volatile(pv.add(((k >> IDX_SH) as usize) & (8 * R - 1)));
    }
    if tie {
        fix_ties(g, s);
    }
}

unsafe fn sort_users(g: &G, off: *const u32, ua: usize, ub: usize, sc: &mut Scratch) {
    let cap = ub - ua + 16;
    for l in [&mut sc.l2, &mut sc.l8, &mut sc.l16, &mut sc.lbig] {
        l.clear();
        l.reserve(cap);
    }
    let (l2, l8, l16, lbig) = (sc.l2.as_mut_ptr(), sc.l8.as_mut_ptr(), sc.l16.as_mut_ptr(), sc.lbig.as_mut_ptr());
    let (mut n2, mut n8, mut n16, mut nbig) = (0usize, 0usize, 0usize, 0usize);
    let iota = z::iota_epi32();
    let mut u = ua;
    while u < ub {
        let tail: u16 = if ub - u >= 16 { 0xFFFF } else { ((1u32 << (ub - u)) - 1) as u16 };
        let n = z::sub_epi32(z::maskz_loadu_epi32(tail, off.add(u + 1)), z::maskz_loadu_epi32(tail, off.add(u)));
        let uv = z::add_epi32(z::set1_epi32(u as i32), iota);
        let m2 = z::cmpeq_epi32_mask(n, z::set1_epi32(2));
        let m8 = z::cmple_epu32_mask(z::sub_epi32(n, z::set1_epi32(3)), z::set1_epi32(5));
        let m16 = z::cmple_epu32_mask(z::sub_epi32(n, z::set1_epi32(9)), z::set1_epi32(7));
        let mb = z::cmpge_epu32_mask(n, z::set1_epi32(17));
        n2 += z::compress_store_epi32(l2.add(n2), m2, uv);
        n8 += z::compress_store_epi32(l8.add(n8), m8, uv);
        n16 += z::compress_store_epi32(l16.add(n16), m16, uv);
        nbig += z::compress_store_epi32(lbig.add(nbig), mb, uv);
        u += 16;
    }
    let row = |u: u32| -> (*mut [u64; 2], usize) {
        let a = *off.add(u as usize) as usize;
        (g.csr.add(a), *off.add(u as usize + 1) as usize - a)
    };
    for i in 0..n2 {
        let p = row(*l2.add(i)).0;
        sort2(g, p, p);
    }
    for ph in 0..SORT_STRIDE {
        let mut i = ph;
        while i < n8 {
            let (p, n) = row(*l8.add(i));
            sort_r1(g, p, p, n);
            i += SORT_STRIDE;
        }
    }
    for ph in 0..SORT_STRIDE {
        let mut i = ph;
        while i < n16 {
            let (p, n) = row(*l16.add(i));
            sort_r2(g, p, p, n);
            i += SORT_STRIDE;
        }
    }
    for i in 0..nbig {
        let (p, n) = row(*lbig.add(i));
        sort_big(g, p, std::slice::from_raw_parts_mut(p, n), sc);
    }
}

#[cold]
#[inline(never)]
fn tie_group(g: &G, s: &mut [[u64; 2]], i: usize) -> usize {
    let ts = s[i][0] >> 32;
    let mut a = i - 1;
    while a > 0 && s[a - 1][0] >> 32 == ts {
        a -= 1;
    }
    let mut b = i + 1;
    while b < s.len() && s[b][0] >> 32 == ts {
        b += 1;
    }
    let grp = &mut s[a..b];
    if grp.iter().all(|c| c[1] & RAW == 0) {
        if grp.len() <= 16 {
            let mut e = [0u64; 16];
            for (k, c) in grp.iter().enumerate() {
                e[k] = enc36(c[1]).0;
            }
            for x in 1..grp.len() {
                let (ex, cx) = (e[x], grp[x]);
                let mut y = x;
                while y > 0 && (e[y - 1] < ex || (e[y - 1] == ex && grp[y - 1][0] > cx[0])) {
                    e[y] = e[y - 1];
                    grp[y] = grp[y - 1];
                    y -= 1;
                }
                e[y] = ex;
                grp[y] = cx;
            }
        } else {
            let mut v: Vec<(u64, [u64; 2])> = grp.iter().map(|c| (enc36(c[1]).0, *c)).collect();
            v.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1[0].cmp(&y.1[0])));
            for (d, x) in grp.iter_mut().zip(&v) {
                *d = x.1;
            }
        }
    } else {
        if DEFER_RAW.load(Ordering::Relaxed) {
            DEFERRED.lock().unwrap().push((grp.as_mut_ptr() as usize, grp.len()));
            return b;
        }
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
    }
    b
}
static DEFER_RAW: AtomicBool = AtomicBool::new(false);
static DEFERRED: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

struct Heavy {
    ub: usize,
    cnt: Vec<UnsafeCell<Vec<u32>>>,
    links: Vec<UnsafeCell<Vec<u64>>>,
    cx: UnsafeCell<Vec<(u32, u8, u8)>>,
    counted: AtomicUsize,
    links_taken: AtomicBool,
    links_fin: AtomicBool,
    scat_next: AtomicUsize,
    scat_fin: AtomicUsize,
    sort_next: AtomicUsize,
    sort_fin: AtomicUsize,
}
use std::sync::atomic::{AtomicBool, AtomicUsize};
const HEAVY_DIV: usize = env_u32(option_env!("XL_HEAVY_DIV"), 4) as usize;
const SORT_UNITS: usize = 64 + UBSIZE / 64 - 1;
pub static DBG: AtomicBool = AtomicBool::new(false);

struct Plan {
    heavy: Vec<Heavy>,
    normal: Vec<u32>,
    base: Vec<usize>,
    next: AtomicUsize,
    fin: AtomicUsize,
}
unsafe impl Sync for Plan {}
static mut PLAN: Option<Plan> = None;
#[inline(always)]
fn pbase(ub: usize) -> usize {
    unsafe { *PLAN.as_ref().unwrap_unchecked().base.get_unchecked(ub) }
}

const CNT_BITS: u64 = 1 << 63 | (UBSIZE as u64 - 1) << 43;

#[inline(always)]
fn link_rec(t: &mut TState, utab: &mut [u16], w0: u64) {
    let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
    let bits = (1u16 << ((w0 >> 4) & 15)) | (1u16 << (w0 & 15));
    let m = utab[ul] & MASK;
    if m == 0 || m & bits != 0 {
        utab[ul] |= bits;
    } else {
        t.pend.push((ul as u32, bits));
    }
}

fn links_done(t: &mut TState, utab: &mut [u16], u0: usize) -> bool {
    if !t.pend.is_empty() {
        loop {
            let mut changed = false;
            for &(ul, bits) in &t.pend {
                let m = utab[ul as usize] & MASK;
                if m & bits != 0 && m | bits != m {
                    utab[ul as usize] |= bits;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for &(ul, bits) in &t.pend {
            if utab[ul as usize] & bits == 0 {
                utab[ul as usize] |= COMPLEX;
            }
        }
    }
    let mut anyc = false;
    for (ul, &v) in utab.iter().enumerate() {
        if v & COMPLEX != 0 {
            t.cx_users.push((u0 + ul) as u32);
            anyc = true;
        }
    }
    anyc
}

#[inline(always)]
fn utab_of(g: &G, ub: usize) -> &mut [u16] {
    unsafe { std::slice::from_raw_parts_mut(g.utab.add(ub << UB), UBSIZE) }
}
#[inline(always)]
fn off_of(g: &G, ub: usize) -> &mut [u32] {
    unsafe { std::slice::from_raw_parts_mut(g.csr_off.add((ub << UB) + ub), UBSIZE + 1) }
}

#[inline(always)]
unsafe fn count_chunk(recs: &[Rec], cnt: *mut u32, lk: *mut u64, mut nl: usize) -> usize {
    let n = recs.len();
    let p = recs.as_ptr();
    let nextc = if n == crate::parse::LINK_OFF / 16 { *((p as usize + crate::parse::LINK_OFF) as *const usize) as *const i8 } else { std::ptr::null() };
    let one = |r: *const Rec, nl: usize| -> usize {
        let w0 = (*r).w0;
        *cnt.add(_pext_u64(w0, CNT_BITS) as usize) += 1;
        *lk.add(nl) = w0;
        nl + (w0 >> 63) as usize
    };
    _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(crate::parse::LINK_OFF));
    let mut i = 0usize;
    while i + 4 <= n {
        _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(i * 16));
        nl = one(p.add(i), nl);
        nl = one(p.add(i + 1), nl);
        nl = one(p.add(i + 2), nl);
        nl = one(p.add(i + 3), nl);
        i += 4;
    }
    while i < n {
        nl = one(p.add(i), nl);
        i += 1;
    }
    nl
}
#[inline(always)]
unsafe fn scatter_chunk(recs: &[Rec], pos: *mut u32, csr: *mut [u64; 2]) {
    let keep = _mm_set_epi64x((RAW | (RAW - 1)) as i64, -1);
    for r in recs {
        let w0 = r.w0;
        let pp = pos.add(_pext_u64(w0, CNT_BITS) as usize);
        let at = *pp;
        *pp = at + 1 - (w0 >> 63) as u32;
        let v = _mm_shuffle_epi32::<0x4E>(_mm_loadu_si128(r as *const Rec as *const __m128i));
        _mm_storeu_si128(csr.add(at as usize) as *mut __m128i, _mm_and_si128(v, keep));
    }
}

fn bucket(g: &G, t: &mut TState, sc: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    let total = pbase(ub + 1) - pbase(ub);
    let mut cnt = std::mem::take(&mut t.cnt);
    cnt.clear();
    cnt.resize(2 * UBSIZE, 0);
    t.pend.clear();
    let lk = &mut sc.lk;
    lk.clear();
    let cp = cnt.as_mut_ptr();
    let mut nl = 0usize;
    let mut seen = 0usize;
    for th in 0..g.nt {
        g.t(th).us.for_each(ub, |recs| {
            seen += recs.len();
            lk.reserve(recs.len() + 1);
            nl = unsafe { count_chunk(recs, cp, lk.as_mut_ptr(), nl) };
            unsafe { lk.set_len(nl) };
        });
    }
    if seen != total {
        fallback("click partition count");
    }
    let lks = unsafe { std::slice::from_raw_parts(lk.as_ptr(), nl) };
    for &w0 in lks {
        link_rec(t, utab, w0);
    }
    let dbg1 = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += dbg1;
    if links_done(t, utab, u0) {
        for &w0 in lks {
            let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
            if utab[ul] & COMPLEX != 0 {
                t.cx_edges.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
            }
        }
    }
    if total != 0 {
        let off = off_of(g, ub);
        let mut at = pbase(ub) as u32;
        for ul in 0..UBSIZE {
            off[ul] = at;
            at += cnt[ul];
            cnt[ul] = off[ul];
        }
        off[UBSIZE] = at;
        if at as usize - pbase(ub) + nl != total {
            fallback("click partition count");
        }
        cnt[UBSIZE..].fill(at);
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| unsafe { scatter_chunk(recs, cp, g.csr) });
        }
        let dbg2 = tstart.elapsed().as_nanos() as u64;
        t.prof[6] += dbg2;
        unsafe { sort_users(g, off.as_ptr(), 0, UBSIZE, sc) };
    } else {
        t.prof[6] += dbg1;
    }
    t.cnt = cnt;
    let el = tstart.elapsed().as_nanos() as u64;
    t.prof[3] += el;
    t.prof[4] = t.prof[4].max(el);
}


fn heavy_count(g: &G, h: &Heavy, th: usize) {
    let cnt = unsafe { &mut *h.cnt[th].get() };
    let lk = unsafe { &mut *h.links[th].get() };
    cnt.clear();
    cnt.resize(2 * UBSIZE, 0);
    let total = g.t(th).us.count(h.ub);
    lk.clear();
    let cp = cnt.as_mut_ptr();
    let mut nl = 0usize;
    let mut seen = 0usize;
    g.t(th).us.for_each(h.ub, |recs| {
        seen += recs.len();
        lk.reserve(recs.len() + 1);
        nl = unsafe { count_chunk(recs, cp, lk.as_mut_ptr(), nl) };
        unsafe { lk.set_len(nl) };
    });
    if seen != total {
        fallback("click partition count");
    }
}

fn heavy_links(g: &G, t: &mut TState, h: &Heavy) {
    let ub = h.ub;
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    t.pend.clear();
    for th in 0..g.nt {
        for &w0 in unsafe { &*h.links[th].get() } {
            link_rec(t, utab, w0);
        }
    }
    if links_done(t, utab, u0) {
        let cx = unsafe { &mut *h.cx.get() };
        for th in 0..g.nt {
            for &w0 in unsafe { &*h.links[th].get() } {
                let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                if utab[ul] & COMPLEX != 0 {
                    cx.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
                }
            }
        }
    }
    let off = off_of(g, ub);
    let mut at = pbase(ub) as u32;
    for ul in 0..UBSIZE {
        off[ul] = at;
        for th in 0..g.nt {
            at += unsafe { &*h.cnt[th].get() }[ul];
        }
    }
    off[UBSIZE] = at;
}

fn heavy_scatter(g: &G, t: &mut TState, h: &Heavy, th: usize) {
    let mut pos = std::mem::take(&mut t.cnt);
    pos.clear();
    pos.resize(2 * UBSIZE, 0);
    let mut at = pbase(h.ub) as u32;
    for ul in 0..UBSIZE {
        for t2 in 0..g.nt {
            if t2 == th {
                pos[ul] = at;
            }
            at += unsafe { &*h.cnt[t2].get() }[ul];
        }
    }
    let mut spare = at as usize;
    let mut links = 0usize;
    for t2 in 0..g.nt {
        let n = unsafe { &*h.links[t2].get() }.len();
        if t2 < th {
            spare += n;
        }
        links += n;
    }
    let total = pbase(h.ub + 1) - pbase(h.ub);
    if at as usize - pbase(h.ub) + links != total {
        fallback("click partition count");
    }
    pos[UBSIZE..].fill(spare as u32);
    let pp = pos.as_mut_ptr();
    g.t(th).us.for_each(h.ub, |recs| unsafe { scatter_chunk(recs, pp, g.csr) });
    t.cnt = pos;
}

fn heavy_step(g: &G, p: &Plan, t: &mut TState, sc: &mut Scratch) -> bool {
    use Ordering::SeqCst;
    let nt = g.nt;
    for h in &p.heavy {
        if h.counted.load(SeqCst) < nt {
            continue;
        }
        if !h.links_taken.load(SeqCst) && !h.links_taken.swap(true, SeqCst) {
            heavy_links(g, t, h);
            h.links_fin.store(true, SeqCst);
            return true;
        }
        if h.scat_next.load(SeqCst) < nt {
            let j = h.scat_next.fetch_add(1, SeqCst);
            if j < nt {
                heavy_scatter(g, t, h, j);
                h.scat_fin.fetch_add(1, SeqCst);
                return true;
            }
        }
        if h.scat_fin.load(SeqCst) == nt && h.links_fin.load(SeqCst) && h.sort_next.load(SeqCst) < SORT_UNITS {
            let j = h.sort_next.fetch_add(1, SeqCst);
            if j < SORT_UNITS {
                let (a, b) = if j < 64 { (j, j + 1) } else { ((j - 63) * 64, (j - 62) * 64) };
                unsafe { sort_users(g, off_of(g, h.ub).as_ptr(), a, b, sc) };
                h.sort_fin.fetch_add(1, SeqCst);
                return true;
            }
        }
    }
    false
}


struct HeavyX {
    ub: usize,
    pieces: Vec<(usize, i64)>,
    piece_next: AtomicUsize,
    piece_fin: AtomicUsize,
    fin_taken: AtomicBool,
    fin_done: AtomicBool,
    sort_next: AtomicUsize,
    sort_fin: AtomicUsize,
    cx: UnsafeCell<Vec<(u32, u8, u8)>>,
}
struct Fix {
    user: u32,
    w0: u64,
    slot: usize,
    tok: u64,
}
struct PlanX {
    fixes: Vec<Fix>,
    bounds: Vec<usize>,
    heavy: Vec<HeavyX>,
    normal: Vec<u32>,
    next: AtomicUsize,
    fin: AtomicUsize,
}
unsafe impl Sync for PlanX {}
static mut PLANX: Option<PlanX> = None;
static READY: AtomicBool = AtomicBool::new(false);
static US_FIN: AtomicUsize = AtomicUsize::new(0);
static CEND: AtomicU64 = AtomicU64::new(0);
use std::sync::atomic::AtomicU64;

pub fn us_finished(g: &G, end: u64, inq: bool) {
    CEND.fetch_max(end, Ordering::SeqCst);
    if US_FIN.fetch_add(1, Ordering::SeqCst) + 1 == g.nt && inq {
        prelude(g, true);
    }
}

fn prelude_ix(g: &G) {
    use crate::ckix::VALID;
    let end = CEND.load(Ordering::SeqCst) as usize;
    if end as u64 > g.ccap {
        fallback("click index range");
    }
    let csr = g.csr;
    let used = |i: usize| unsafe { (*csr.add(i))[1] & VALID != 0 };
    let user = |i: usize| unsafe { (*csr.add(i))[1] & 0xFFFF_FFFF };
    let mut first = 0usize;
    while first < end && !used(first) {
        first += 1;
    }
    let lower = |t: u64| -> usize {
        let (mut l, mut h) = (first, end);
        while l < h {
            let mid = l + (h - l) / 2;
            let mut m = mid;
            while m < h && !used(m) {
                m += 1;
            }
            if m < h && user(m) < t {
                l = m + 1;
            } else {
                h = mid;
            }
        }
        while l < end && !used(l) {
            l += 1;
        }
        l
    };
    let mut n_odd = 0u64;
    let mut fixes = Vec::new();
    for tid in 0..g.nt {
        let t = g.t(tid);
        for &(a, b) in &t.gen_edges {
            mark_complex(g, a);
            mark_complex(g, b);
        }
        let mut i = 0;
        while i < t.odd_clicks.len() {
            let c = t.odd_clicks[i];
            if c[0] & RAW == 0 {
                if let Some((u, a)) = g.normal(c[0]) {
                    let (s, e) = (lower(u as u64), lower(u as u64 + 1));
                    let mut h = s;
                    while h < e && used(h) {
                        h += 1;
                    }
                    if h < e {
                        let w0 = c[1] | a as u64;
                        unsafe { *csr.add(h) = [w0, u as u64 | VALID] };
                        fixes.push(Fix { user: u, w0, slot: h, tok: c[2] });
                        t.odd_clicks.swap_remove(i);
                        continue;
                    }
                    if DBG.load(Ordering::Relaxed) {
                        eprintln!("[dbg] odd click of normal user {u} alias {a} without a free slot: range {s}..{e}, tok {:x}", c[2]);
                    }
                    unsafe { *g.utab.add(u as usize) |= COMPLEX };
                }
            }
            i += 1;
        }
        n_odd += t.odd_clicks.len() as u64;
    }
    let nub = g.nub;
    let mut lo = vec![first; nub + 1];
    let mut hi = vec![end; nub + 1];
    loop {
        let mut active = false;
        for ub in 1..nub {
            let (l, h) = (lo[ub], hi[ub]);
            if l < h {
                active = true;
                let mid = l + (h - l) / 2;
                let mut m = mid;
                while m < h && !used(m) {
                    m += 1;
                }
                if m < h && user(m) < (ub as u64) << UB {
                    lo[ub] = m + 1;
                } else {
                    hi[ub] = mid;
                }
            }
        }
        if !active {
            break;
        }
    }
    let mut bounds = vec![first; nub + 1];
    for ub in 1..nub {
        let mut m = lo[ub];
        while m < end && !used(m) {
            m += 1;
        }
        bounds[ub] = m;
    }
    bounds[nub] = end;
    for ub in 0..nub {
        if bounds[ub] > bounds[ub + 1] {
            fallback("click index order");
        }
    }
    let total = end - first;
    let nt = g.nt;
    let mut heavy = Vec::new();
    let mut normal = Vec::new();
    for ub in 0..nub {
        let (a, b) = (bounds[ub], bounds[ub + 1]);
        if b - a > (total / HEAVY_DIV).max(1 << 17) {
            let u0 = (ub << UB) as i64;
            let mut pieces = vec![(a, u0 - 1)];
            for k in 1..nt {
                let mut s = a + (b - a) * k / nt;
                while s < b && !used(s) {
                    s += 1;
                }
                if s < b && s > pieces[pieces.len() - 1].0 {
                    let mut q = s - 1;
                    while !used(q) {
                        q -= 1;
                    }
                    pieces.push((s, user(q) as i64));
                }
            }
            pieces.push((b, 0));
            off_of(g, ub)[..UBSIZE].fill(u32::MAX);
            heavy.push(HeavyX {
                ub,
                pieces,
                piece_next: AtomicUsize::new(0),
                piece_fin: AtomicUsize::new(0),
                fin_taken: AtomicBool::new(false),
                fin_done: AtomicBool::new(false),
                sort_next: AtomicUsize::new(0),
                sort_fin: AtomicUsize::new(0),
                cx: UnsafeCell::new(Vec::new()),
            });
        } else {
            normal.push(ub as u32);
        }
    }
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] index mode: slots {first}..{end}, odd clicks {n_odd}, fixes {}, heavy {:?}", fixes.len(), heavy.iter().map(|h| (h.ub, h.pieces.len() - 1, bounds[h.ub + 1] - bounds[h.ub])).collect::<Vec<_>>());
    }
    unsafe { PLANX = Some(PlanX { fixes, bounds, heavy, normal, next: AtomicUsize::new(0), fin: AtomicUsize::new(0) }) };
    READY.store(true, Ordering::SeqCst);
}

#[inline(never)]
unsafe fn piece_ix(g: &G, a: usize, b: usize, prev: i64, u0: u32, start: *mut u32) -> bool {
    use crate::ckix::VALID;
    use crate::keyrec::{A, B, MASK as M40};
    let csr = g.csr;
    let key = g.ckey & M40;
    let mut dummy = 0u32;
    let dp = &mut dummy as *mut u32;
    let mut prev = prev;
    let mut bad = false;
    let mut xa = (a as u64).wrapping_mul(A) & M40;
    if b > a && (*csr.add(a))[1] & VALID == 0 {
        return false;
    }
    for i in a..b {
        let r = csr.add(i);
        let w1 = (*r)[1];
        if w1 & VALID == 0 {
            *r = *r.sub(1);
        } else {
            let u = (w1 & 0xFFFF_FFFF) as i64;
            let ul = (u as u32).wrapping_sub(u0);
            bad |= (u < prev) | (ul >= UBSIZE as u32);
            let sp = if u != prev { start.add(ul as usize & (UBSIZE - 1)) } else { dp };
            *sp = i as u32;
            prev = u;
            let x = xa ^ key;
            (*r)[1] = (x ^ (x >> 17)).wrapping_mul(B) & M40;
        }
        xa = xa.wrapping_add(A) & M40;
    }
    !bad
}

fn fill_off(off: &mut [u32], lo: usize, hi: usize) {
    off[UBSIZE] = hi as u32;
    for ul in (0..UBSIZE).rev() {
        off[ul] = off[ul].min(off[ul + 1]);
    }
    if off[0] as usize != lo {
        fallback("click index order");
    }
}

fn links_ix(g: &G, t: &mut TState, ub: usize, cx: &mut Vec<(u32, u8, u8)>) {
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    t.pend.clear();
    let mut seen = 0usize;
    let mut total = 0usize;
    for th in 0..g.nt {
        total += g.t(th).us.count(ub);
        g.t(th).us.for_each(ub, |recs| {
            seen += recs.len();
            for r in recs {
                link_rec(t, utab, r.w0);
            }
        });
    }
    if seen != total {
        fallback("link partition count");
    }
    if links_done(t, utab, u0) {
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| {
                for r in recs {
                    let w0 = r.w0;
                    let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                    if utab[ul] & COMPLEX != 0 {
                        cx.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
                    }
                }
            });
        }
    }
}

fn bucket_ix(g: &G, p: &PlanX, t: &mut TState, sc: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    let mut cx = std::mem::take(&mut t.cx_edges);
    links_ix(g, t, ub, &mut cx);
    t.cx_edges = cx;
    let (lo, hi) = (p.bounds[ub], p.bounds[ub + 1]);
    let d1 = tstart.elapsed().as_nanos() as u64;
    let mut d2 = d1;
    if hi > lo {
        let off = off_of(g, ub);
        off[..UBSIZE].fill(u32::MAX);
        if !unsafe { piece_ix(g, lo, hi, (ub << UB) as i64 - 1, (ub << UB) as u32, off.as_mut_ptr()) } {
            fallback("click index order");
        }
        fill_off(off, lo, hi);
        d2 = tstart.elapsed().as_nanos() as u64;
        unsafe { sort_users(g, off.as_ptr(), 0, UBSIZE, sc) };
    }
    let el = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += d1;
    t.prof[6] += d2;
    t.prof[3] += el;
    t.prof[4] = t.prof[4].max(el);
}

fn heavy_step_ix(g: &G, p: &PlanX, t: &mut TState, sc: &mut Scratch) -> bool {
    use Ordering::SeqCst;
    for h in &p.heavy {
        let np = h.pieces.len() - 1;
        let tstart = std::time::Instant::now();
        if h.piece_next.load(SeqCst) < np {
            let j = h.piece_next.fetch_add(1, SeqCst);
            if j < np {
                let (a, prev) = h.pieces[j];
                if !unsafe { piece_ix(g, a, h.pieces[j + 1].0, prev, (h.ub << UB) as u32, off_of(g, h.ub).as_mut_ptr()) } {
                    fallback("click index order");
                }
                h.piece_fin.fetch_add(1, SeqCst);
                let el = tstart.elapsed().as_nanos() as u64;
                t.prof[3] += el;
                t.prof[6] += el;
                return true;
            }
        }
        if h.piece_fin.load(SeqCst) == np && !h.fin_taken.load(SeqCst) && !h.fin_taken.swap(true, SeqCst) {
            links_ix(g, t, h.ub, unsafe { &mut *h.cx.get() });
            fill_off(off_of(g, h.ub), p.bounds[h.ub], p.bounds[h.ub + 1]);
            h.fin_done.store(true, SeqCst);
            let el = tstart.elapsed().as_nanos() as u64;
            t.prof[3] += el;
            t.prof[6] += el;
            return true;
        }
        if h.fin_done.load(SeqCst) && h.sort_next.load(SeqCst) < SORT_UNITS {
            let j = h.sort_next.fetch_add(1, SeqCst);
            if j < SORT_UNITS {
                let (a, b) = if j < 64 { (j, j + 1) } else { ((j - 63) * 64, (j - 62) * 64) };
                unsafe { sort_users(g, off_of(g, h.ub).as_ptr(), a, b, sc) };
                h.sort_fin.fetch_add(1, SeqCst);
                t.prof[3] += tstart.elapsed().as_nanos() as u64;
                return true;
            }
        }
    }
    false
}

fn job_ix(g: &G, t: &mut TState, sc: &mut Scratch) -> bool {
    let p = unsafe { PLANX.as_ref().unwrap_unchecked() };
    if heavy_step_ix(g, p, t, sc) {
        return true;
    }
    if p.next.load(Ordering::Relaxed) >= p.normal.len() {
        return false;
    }
    let i = p.next.fetch_add(1, Ordering::Relaxed);
    if i >= p.normal.len() {
        return false;
    }
    bucket_ix(g, p, t, sc, p.normal[i] as usize);
    p.fin.fetch_add(1, Ordering::SeqCst);
    true
}
fn done_ix() -> bool {
    let p = unsafe { PLANX.as_ref().unwrap_unchecked() };
    p.fin.load(Ordering::SeqCst) == p.normal.len() && p.heavy.iter().all(|h| h.sort_fin.load(Ordering::SeqCst) == SORT_UNITS)
}

fn fix_odd(g: &G, f: &Fix) {
    let i = f.user as usize + (f.user as usize >> UB);
    let (a, b) = unsafe { (*g.csr_off.add(i) as usize, *g.csr_off.add(i + 1) as usize) };
    let rows = unsafe { std::slice::from_raw_parts_mut(g.csr.add(a), b - a) };
    let y = scramble(f.slot as u64, g.ckey);
    let ts = f.w0 >> 32;
    let ga = rows.partition_point(|r| r[0] >> 32 < ts);
    let gb = rows.partition_point(|r| r[0] >> 32 <= ts);
    let mut found = false;
    let mut mixed = false;
    for r in &mut rows[ga..gb] {
        if r[0] == f.w0 && r[1] == y {
            r[1] = f.tok;
            found = true;
        }
        mixed |= (r[0] ^ f.w0) >> 4 != 0;
    }
    if !found {
        fallback("odd click row");
    }
    if mixed {
        tie_group(g, rows, ga + 1);
    }
}


type Cand = (u32, u32, u64, u64);
struct HeavyA {
    ub: usize,
    links: Vec<UnsafeCell<Vec<u64>>>,
    mm: Vec<UnsafeCell<(usize, usize)>>,
    pieces: UnsafeCell<Vec<(usize, i64)>>,
    scat_next: AtomicUsize,
    scat_fin: AtomicUsize,
    mid_taken: AtomicBool,
    mid_done: AtomicBool,
    piece_next: AtomicUsize,
    piece_fin: AtomicUsize,
    fin_taken: AtomicBool,
    fin_done: AtomicBool,
    sort_next: AtomicUsize,
    sort_fin: AtomicUsize,
    cx: UnsafeCell<Vec<(u32, u8, u8)>>,
}
struct PlanA {
    heavy: Vec<HeavyA>,
    normal: Vec<u32>,
    cand: Vec<Vec<Cand>>,
    range: Vec<UnsafeCell<(usize, usize)>>,
    fixes: std::sync::Mutex<Vec<Fix>>,
    next: AtomicUsize,
    fin: AtomicUsize,
}
unsafe impl Sync for PlanA {}
static mut PLANA: Option<PlanA> = None;

fn prelude_a(g: &G) {
    let nub = g.nub;
    let mut cand: Vec<Vec<Cand>> = (0..nub).map(|_| z::nv(Vec::new())).collect();
    for tid in 0..g.nt {
        let t = g.t(tid);
        for &(a, b) in &t.gen_edges {
            mark_complex(g, a);
            mark_complex(g, b);
        }
        let mut i = 0;
        while i < t.odd_clicks.len() {
            let c = t.odd_clicks[i];
            if c[0] & RAW == 0 {
                if let Some((u, a)) = g.normal(c[0]) {
                    cand[(u >> UB) as usize].push((u, a, c[1], c[2]));
                    t.odd_clicks.swap_remove(i);
                    continue;
                }
            }
            i += 1;
        }
    }
    let nt = g.nt;
    let count = |ub: usize| (0..nt).map(|th| g.t(th).us.count(ub)).sum::<usize>();
    let total: usize = (0..nub).map(count).sum();
    let mut heavy = Vec::new();
    let mut normal = Vec::new();
    for ub in 0..nub {
        if count(ub) > (total / HEAVY_DIV).max(1 << 17) {
            heavy.push(HeavyA {
                ub,
                links: (0..nt).map(|_| z::nv(UnsafeCell::new(Vec::new()))).collect(),
                mm: (0..nt).map(|_| z::nv(UnsafeCell::new((usize::MAX, 0)))).collect(),
                pieces: UnsafeCell::new(Vec::new()),
                scat_next: AtomicUsize::new(0),
                scat_fin: AtomicUsize::new(0),
                mid_taken: AtomicBool::new(false),
                mid_done: AtomicBool::new(false),
                piece_next: AtomicUsize::new(0),
                piece_fin: AtomicUsize::new(0),
                fin_taken: AtomicBool::new(false),
                fin_done: AtomicBool::new(false),
                sort_next: AtomicUsize::new(0),
                sort_fin: AtomicUsize::new(0),
                cx: UnsafeCell::new(Vec::new()),
            });
        } else {
            normal.push(ub as u32);
        }
    }
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] mode A: records {total}, odd clicks of normal users {}, heavy {:?}", cand.iter().map(|c| z::nvq(c.len() as u64) as usize).sum::<usize>(), heavy.iter().map(|h| z::nv((h.ub, count(h.ub)))).collect::<Vec<_>>());
    }
    let range = (0..nub).map(|_| z::nv(UnsafeCell::new((0, 0)))).collect();
    unsafe { PLANA = Some(PlanA { heavy, normal, cand, range, fixes: std::sync::Mutex::new(Vec::new()), next: AtomicUsize::new(0), fin: AtomicUsize::new(0) }) };
    READY.store(true, Ordering::SeqCst);
}

#[inline(always)]
unsafe fn scatter_a(recs: &[Rec], csr: *mut [u64; 2], u0: u64, lk: *mut u64, mut nl: usize, mm: &mut (usize, usize)) -> usize {
    use crate::ckix::VALID;
    let mut spare = [0u64; 2];
    let sp = &mut spare as *mut [u64; 2];
    let (mut lo, mut hi) = (mm.0 as u64, mm.1 as u64);
    let n = recs.len();
    let p = recs.as_ptr();
    let nextc = if n == crate::parse::LINK_OFF / 16 { *((p as usize + crate::parse::LINK_OFF) as *const usize) as *const i8 } else { std::ptr::null() };
    _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(crate::parse::LINK_OFF));
    for i in 0..n {
        if i & 3 == 0 {
            _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(i * 16));
        }
        let r = &*p.add(i);
        let w0 = r.w0;
        let link = w0 >> 63;
        let lm = link.wrapping_neg();
        let c = w0 & 0xFFFF_FFFF;
        *lk.add(nl) = w0;
        nl += link as usize;
        lo = lo.min(c | lm);
        hi = hi.max((c + 1) & !lm);
        let dst = if link != 0 { sp } else { csr.add(c as usize) };
        *dst = [r.w1, (u0 + ((w0 >> 43) & (UBSIZE as u64 - 1))) | VALID];
    }
    *mm = (lo as usize, hi as usize);
    nl
}

fn place_a(g: &G, p: &PlanA, t: &mut TState, ub: usize, lo: usize, hi: usize) {
    use crate::ckix::VALID;
    if p.cand[ub].is_empty() {
        return;
    }
    let csr = g.csr;
    let used = |i: usize| unsafe { (*csr.add(i))[1] & VALID != 0 };
    let user = |i: usize| unsafe { (*csr.add(i))[1] & 0xFFFF_FFFF };
    let lower = |x: u64| -> usize {
        let (mut l, mut h) = (lo, hi);
        while l < h {
            let mid = l + (h - l) / 2;
            let mut m = mid;
            while m < h && !used(m) {
                m += 1;
            }
            if m < h && user(m) < x {
                l = m + 1;
            } else {
                h = mid;
            }
        }
        while l < hi && !used(l) {
            l += 1;
        }
        l
    };
    for &(u, a, key, tok) in &p.cand[ub] {
        let mut placed = false;
        if hi > lo {
            let (s, e) = (lower(u as u64), lower(u as u64 + 1));
            let mut h = s;
            while h < e && used(h) {
                h += 1;
            }
            if h < e {
                let w0 = key | a as u64;
                unsafe { *csr.add(h) = [w0, u as u64 | VALID] };
                p.fixes.lock().unwrap().push(Fix { user: u, w0, slot: h, tok });
                placed = true;
            }
        }
        if !placed {
            if DBG.load(Ordering::Relaxed) {
                eprintln!("[dbg] odd click of normal user {u} alias {a} without a free slot");
            }
            unsafe { *g.utab.add(u as usize) |= COMPLEX };
            t.odd_clicks.push([scramble((u as u64) << 9 | a as u64, g.ukey), key, tok]);
        }
    }
}

fn links_a<'a>(t: &mut TState, utab: &mut [u16], u0: usize, lks: impl Iterator<Item = &'a u64> + Clone, cx: &mut Vec<(u32, u8, u8)>) {
    t.pend.clear();
    for &w0 in lks.clone() {
        link_rec(t, utab, w0);
    }
    if links_done(t, utab, u0) {
        for &w0 in lks {
            let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
            if utab[ul] & COMPLEX != 0 {
                cx.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
            }
        }
    }
}

fn bucket_a(g: &G, p: &PlanA, t: &mut TState, sc: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    let u0 = ub << UB;
    let mut lk = std::mem::take(&mut sc.lk);
    lk.clear();
    let mut nl = 0usize;
    let mut mm = (usize::MAX, 0usize);
    let mut seen = 0usize;
    let mut total = 0usize;
    for th in 0..g.nt {
        total += g.t(th).us.count(ub);
        g.t(th).us.for_each(ub, |recs| {
            seen += recs.len();
            lk.reserve(recs.len() + 1);
            nl = unsafe { scatter_a(recs, g.csr, u0 as u64, lk.as_mut_ptr(), nl, &mut mm) };
            unsafe { lk.set_len(nl) };
        });
    }
    if seen != total {
        fallback("click partition count");
    }
    let (lo, hi) = if mm.1 > mm.0 { mm } else { (0, 0) };
    let d1 = tstart.elapsed().as_nanos() as u64;
    place_a(g, p, t, ub, lo, hi);
    let mut cx = std::mem::take(&mut t.cx_edges);
    links_a(t, utab_of(g, ub), u0, lk.iter(), &mut cx);
    t.cx_edges = cx;
    sc.lk = lk;
    let mut d2 = tstart.elapsed().as_nanos() as u64;
    if hi > lo {
        let off = off_of(g, ub);
        off[..UBSIZE].fill(u32::MAX);
        if !unsafe { piece_ix(g, lo, hi, u0 as i64 - 1, u0 as u32, off.as_mut_ptr()) } {
            fallback("click index order");
        }
        fill_off(off, lo, hi);
        d2 = tstart.elapsed().as_nanos() as u64;
        unsafe { sort_users(g, off.as_ptr(), 0, UBSIZE, sc) };
    }
    unsafe { *p.range[ub].get() = (lo, hi) };
    let el = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += d1;
    t.prof[6] += d2;
    t.prof[3] += el;
    t.prof[4] = t.prof[4].max(el);
}

fn heavy_step_a(g: &G, p: &PlanA, t: &mut TState, sc: &mut Scratch) -> bool {
    use crate::ckix::VALID;
    use Ordering::SeqCst;
    let nt = g.nt;
    for h in &p.heavy {
        let u0 = h.ub << UB;
        if h.scat_next.load(SeqCst) < nt {
            let th = h.scat_next.fetch_add(1, SeqCst);
            if th < nt {
                let lk = unsafe { &mut *h.links[th].get() };
                let mm = unsafe { &mut *h.mm[th].get() };
                let mut nl = 0usize;
                let mut seen = 0usize;
                g.t(th).us.for_each(h.ub, |recs| {
                    seen += recs.len();
                    lk.reserve(recs.len() + 1);
                    nl = unsafe { scatter_a(recs, g.csr, u0 as u64, lk.as_mut_ptr(), nl, mm) };
                    unsafe { lk.set_len(nl) };
                });
                if seen != g.t(th).us.count(h.ub) {
                    fallback("click partition count");
                }
                h.scat_fin.fetch_add(1, SeqCst);
                return true;
            }
        }
        if h.scat_fin.load(SeqCst) < nt {
            continue;
        }
        if !h.mid_taken.load(SeqCst) && !h.mid_taken.swap(true, SeqCst) {
            let (mut lo, mut hi) = (usize::MAX, 0usize);
            for th in 0..nt {
                let m = z::nv(unsafe { *h.mm[th].get() });
                lo = lo.min(m.0);
                hi = hi.max(m.1);
            }
            if hi <= lo {
                (lo, hi) = (0, 0);
            }
            place_a(g, p, t, h.ub, lo, hi);
            let csr = g.csr;
            let used = |i: usize| unsafe { (*csr.add(i))[1] & VALID != 0 };
            let pieces = unsafe { &mut *h.pieces.get() };
            if hi > lo {
                pieces.push((lo, u0 as i64 - 1));
                for k in 1..nt {
                    let mut s = lo + (hi - lo) * k / nt;
                    while s < hi && !used(s) {
                        s += 1;
                    }
                    if s < hi && s > pieces[pieces.len() - 1].0 {
                        let mut q = s - 1;
                        while !used(q) {
                            q -= 1;
                        }
                        pieces.push((s, unsafe { (*csr.add(q))[1] & 0xFFFF_FFFF } as i64));
                    }
                }
            }
            pieces.push((hi, 0));
            off_of(g, h.ub)[..UBSIZE].fill(u32::MAX);
            unsafe { *p.range[h.ub].get() = (lo, hi) };
            h.mid_done.store(true, SeqCst);
            return true;
        }
        if !h.mid_done.load(SeqCst) {
            continue;
        }
        let pieces = unsafe { &*h.pieces.get() };
        let np = pieces.len() - 1;
        if h.piece_next.load(SeqCst) < np {
            let j = h.piece_next.fetch_add(1, SeqCst);
            if j < np {
                let (a, prev) = pieces[j];
                if !unsafe { piece_ix(g, a, pieces[j + 1].0, prev, u0 as u32, off_of(g, h.ub).as_mut_ptr()) } {
                    fallback("click index order");
                }
                h.piece_fin.fetch_add(1, SeqCst);
                return true;
            }
        }
        if h.piece_fin.load(SeqCst) == np && !h.fin_taken.load(SeqCst) && !h.fin_taken.swap(true, SeqCst) {
            let lks = (0..nt).flat_map(|th| unsafe { &*h.links[th].get() }.iter());
            links_a(t, utab_of(g, h.ub), u0, lks, unsafe { &mut *h.cx.get() });
            let (lo, hi) = unsafe { *p.range[h.ub].get() };
            if hi > lo {
                fill_off(off_of(g, h.ub), lo, hi);
            } else {
                off_of(g, h.ub).fill(0);
            }
            h.fin_done.store(true, SeqCst);
            return true;
        }
        if h.fin_done.load(SeqCst) && h.sort_next.load(SeqCst) < SORT_UNITS {
            let j = h.sort_next.fetch_add(1, SeqCst);
            if j < SORT_UNITS {
                let (a, b) = if j < 64 { (j, j + 1) } else { ((j - 63) * 64, (j - 62) * 64) };
                unsafe { sort_users(g, off_of(g, h.ub).as_ptr(), a, b, sc) };
                h.sort_fin.fetch_add(1, SeqCst);
                return true;
            }
        }
    }
    false
}

fn job_a(g: &G, t: &mut TState, sc: &mut Scratch) -> bool {
    let p = unsafe { PLANA.as_ref().unwrap_unchecked() };
    if heavy_step_a(g, p, t, sc) {
        return true;
    }
    if p.next.load(Ordering::Relaxed) >= p.normal.len() {
        return false;
    }
    let i = p.next.fetch_add(1, Ordering::Relaxed);
    if i >= p.normal.len() {
        return false;
    }
    bucket_a(g, p, t, sc, p.normal[i] as usize);
    p.fin.fetch_add(1, Ordering::SeqCst);
    true
}

struct Tail {
    uf: Uf,
    cx: Vec<Vec<(u32, u8, u8)>>,
    part1: bool,
}
static mut TAIL: Option<Tail> = None;
static TAIL_TAKEN: AtomicBool = AtomicBool::new(true);
const _: () = crate::events::UF_TAIL_IN_STAGE3;

pub fn uf_tail(g: &G) {
    if TAIL_TAKEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let c0 = thread_cpu_ms();
    unsafe {
        if let Some(t) = TAIL.as_mut() {
            if t.part1 {
                let mut n = 0;
                for th in 0..g.nt {
                    let ts = g.t(th);
                    n += 2 * ts.gen_edges.len() + ts.odd_users.len() / 8 + ts.odd_clicks.len() / 8;
                }
                t.uf = Uf::with_capacity(n + 1024);
                t.uf.part1(g);
                t.part1 = false;
            }
            t.uf.part2a(g);
            for cx in &t.cx {
                t.uf.cx(g, cx);
            }
            t.uf.finish(g);
            if DBG.load(Ordering::Relaxed) {
                eprintln!("[dbg] uf part2 cpu {:.2} ms, nodes {}", thread_cpu_ms() - c0, t.uf.len());
            }
        }
    }
}
pub fn uf_take(g: &G) -> Uf {
    uf_tail(g);
    unsafe { TAIL.take().map(|t| t.uf).unwrap_or_else(Uf::new) }
}

pub fn stage2(g: &mut G) {
    if !READY.load(Ordering::SeqCst) {
        prelude(g, false);
    }
    if !done(g) {
        let gr: &G = g;
        run_par(g.nt, |tid| {
            let t = gr.t(tid);
            let mut sc = Scratch::default();
            loop {
                if job(gr, t, &mut sc) {
                    continue;
                }
                if done(gr) {
                    break;
                }
                std::hint::spin_loop();
            }
        });
    }
    lap("st2b");
    odd_events(g);
    DEFER_RAW.store(false, Ordering::SeqCst);
    for (p, n) in std::mem::take(&mut *DEFERRED.lock().unwrap()) {
        let grp = unsafe { std::slice::from_raw_parts_mut(p as *mut [u64; 2], n) };
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
    }
    let cx;
    if g.ccap != 0 && g.cpart {
        let mut p = unsafe { PLANA.take().unwrap() };
        let mut end = 0usize;
        let mut base = Vec::with_capacity(g.nub + 1);
        for r in &mut p.range {
            let (lo, hi) = *r.get_mut();
            if hi > lo {
                if lo < end {
                    fallback("click index order");
                }
                end = hi;
            }
            base.push(lo.min(end));
        }
        base.push(end);
        for f in p.fixes.get_mut().unwrap().iter() {
            fix_odd(g, f);
        }
        g.ncsr = end;
        g.csr_base = base;
        cx = p.heavy.iter_mut().map(|h| std::mem::take(h.cx.get_mut())).collect();
    } else if g.ccap != 0 {
        let mut p = unsafe { PLANX.take().unwrap() };
        for f in &p.fixes {
            fix_odd(g, f);
        }
        g.ncsr = p.bounds[g.nub];
        g.csr_base = std::mem::take(&mut p.bounds);
        cx = p.heavy.iter_mut().map(|h| std::mem::take(h.cx.get_mut())).collect();
    } else {
        let mut p = unsafe { PLAN.take().unwrap() };
        g.ncsr = p.base[g.nub];
        g.csr_base = std::mem::take(&mut p.base);
        cx = p.heavy.iter_mut().map(|h| std::mem::take(h.cx.get_mut())).collect();
    }
    unsafe { TAIL = Some(Tail { uf: Uf::new(), cx, part1: true }) };
    TAIL_TAKEN.store(false, Ordering::SeqCst);
    lap("st2a");
}

fn prelude(g: &G, inq: bool) {
    DBG.store(std::env::var("XL_DBG").is_ok(), Ordering::Relaxed);
    DEFER_RAW.store(inq, Ordering::SeqCst);
    if g.ccap == 0 {
        prelude_part(g);
    } else if g.cpart {
        prelude_a(g);
    } else {
        prelude_ix(g);
    }
}
#[inline]
pub fn job(g: &G, t: &mut TState, sc: &mut Scratch) -> bool {
    if !READY.load(Ordering::Acquire) {
        return false;
    }
    if g.ccap == 0 {
        job_part(g, t, sc)
    } else if g.cpart {
        job_a(g, t, sc)
    } else {
        job_ix(g, t, sc)
    }
}
pub fn done(g: &G) -> bool {
    if !READY.load(Ordering::Acquire) {
        return false;
    }
    if g.ccap != 0 && g.cpart {
        let p = unsafe { PLANA.as_ref().unwrap_unchecked() };
        p.fin.load(Ordering::SeqCst) == p.normal.len() && p.heavy.iter().all(|h| h.sort_fin.load(Ordering::SeqCst) == SORT_UNITS)
    } else if g.ccap != 0 {
        done_ix()
    } else {
        let p = unsafe { PLAN.as_ref().unwrap_unchecked() };
        p.fin.load(Ordering::SeqCst) == p.normal.len() && p.heavy.iter().all(|h| h.sort_fin.load(Ordering::SeqCst) == SORT_UNITS)
    }
}

fn mark_complex(g: &G, tok: u64) {
    if tok & RAW == 0 {
        if let Some((u, _)) = g.normal(tok) {
            unsafe { *g.utab.add(u as usize) |= COMPLEX };
        }
    }
}

fn odd_events(g: &mut G) {
    let mut n_odd_ev = 0u64;
    let mut n_ev = 0u64;
    for tid in 0..g.nt {
        let t = g.t(tid);
        n_odd_ev += t.odd_events.len() as u64;
        n_ev += t.n_ev_rows;
    }
    if n_odd_ev * 50 > n_ev + 5000 {
        fallback("more than 2% odd events");
    }
    let mut m: HashMap<Vec<u8>, OddEv> = HashMap::new();
    let mut s = Vec::new();
    for tid in 0..g.nt {
        for oe in &g.t(tid).odd_events {
            g.id_str(oe.eid, b'e', &mut s);
            match m.get_mut(&s) {
                Some(old) => {
                    if oe.ing < old.ing {
                        *old = *oe;
                    }
                }
                None => {
                    m.insert(s.clone(), *oe);
                }
            }
        }
    }
    let mut extra: HashMap<u32, Vec<u64>> = HashMap::new();
    let mut odd_kept = Vec::new();
    for oe in m.values() {
        if oe.w0 >> 33 == INVALID_TS {
            continue;
        }
        if oe.user & ODDU != 0 {
            odd_kept.push(*oe);
        } else {
            unsafe { *g.utab.add(oe.user as usize) |= EXTRA };
            extra.entry(oe.user as u32).or_default().push(oe.w0);
        }
    }
    for v in extra.values_mut() {
        v.sort_unstable();
    }
    g.extra = extra;
    g.odd_kept = odd_kept;
}

fn prelude_part(g: &G) {
    for tid in 0..g.nt {
        let t = g.t(tid);
        for &(a, b) in &t.gen_edges {
            mark_complex(g, a);
            mark_complex(g, b);
        }
    }
    let mut base = Vec::with_capacity(g.nub + 1);
    let mut at = 0usize;
    for ub in 0..g.nub {
        base.push(at);
        for tid in 0..g.nt {
            at += g.t(tid).us.count(ub);
        }
    }
    base.push(at);
    if at >= u32::MAX as usize {
        fallback("too many clicks");
    }
    if at + 16 > g.csr_cap {
        fallback("CSR capacity");
    }
    let total = at;
    let nt = g.nt;
    let mut plan = Plan { heavy: Vec::new(), normal: Vec::new(), base, next: AtomicUsize::new(0), fin: AtomicUsize::new(0) };
    for ub in 0..g.nub {
        let n = plan.base[ub + 1] - plan.base[ub];
        if n > (total / HEAVY_DIV).max(1 << 17) {
            plan.heavy.push(Heavy {
                ub,
                cnt: (0..nt).map(|_| z::nv(UnsafeCell::new(Vec::new()))).collect(),
                links: (0..nt).map(|_| z::nv(UnsafeCell::new(Vec::new()))).collect(),
                cx: UnsafeCell::new(Vec::new()),
                counted: AtomicUsize::new(0),
                links_taken: AtomicBool::new(false),
                links_fin: AtomicBool::new(false),
                scat_next: AtomicUsize::new(0),
                scat_fin: AtomicUsize::new(0),
                sort_next: AtomicUsize::new(0),
                sort_fin: AtomicUsize::new(0),
            });
        } else {
            plan.normal.push(ub as u32);
        }
    }
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] heavy buckets {:?} total {total}", plan.heavy.iter().map(|h| (h.ub, plan.base[h.ub + 1] - plan.base[h.ub])).collect::<Vec<_>>());
    }
    unsafe { PLAN = Some(plan) };
    READY.store(true, Ordering::SeqCst);
}

fn job_part(g: &G, t: &mut TState, sc: &mut Scratch) -> bool {
    let p = unsafe { PLAN.as_ref().unwrap_unchecked() };
    if heavy_step(g, p, t, sc) {
        return true;
    }
    let nt = g.nt;
    let nh = p.heavy.len();
    let nq = nh * nt + p.normal.len();
    if p.next.load(Ordering::Relaxed) >= nq {
        return false;
    }
    let i = p.next.fetch_add(1, Ordering::Relaxed);
    if i < nh * nt {
        let h = &p.heavy[i / nt];
        heavy_count(g, h, i % nt);
        h.counted.fetch_add(1, Ordering::SeqCst);
        true
    } else if i < nq {
        bucket(g, t, sc, p.normal[i - nh * nt] as usize);
        p.fin.fetch_add(1, Ordering::SeqCst);
        true
    } else {
        false
    }
}

pub fn sort_bench(g: &G) {
    let mut sc = Scratch::default();
    let mut x = 0x1234_5678_9abc_def0u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let sizes: Vec<usize> = (2..=40).chain([47, 48, 49, 63, 64, 65, 100, 127, 128, 129, 200, 255, 256, 257, 300, 400, 511, 512, 513, 600, 1500, 2047, 2048, 2049, 5000, 20000]).collect();
    for &n in &sizes {
        let checks = if n <= 64 { 4000 } else if n <= 5000 { 300 } else { 40 };
        for rep in 0..checks {
            let tr = match rep % 4 {
                0 => 4,
                1 => 1000,
                2 => n as u64,
                _ => 1_800_000_000,
            };
            let cr = if rep % 3 == 0 { 2 } else { 2000 };
            let mut a: Vec<[u64; 2]> = (0..n).map(|_| [(rnd() % tr) << 32 | (rnd() % cr) << 4 | (rnd() % 14), rnd() % (1 << 40)]).collect();
            if rep % 97 == 96 {
                let i = (rnd() % n as u64) as usize;
                a[i][0] |= 1 << (18 + rnd() % 14);
            }
            let mut b = a.clone();
            slow_sort(g, &mut b);
            sort_clicks(g, &mut a, &mut sc);
            let norm = |v: &mut Vec<[u64; 2]>| {
                let mut i = 0;
                while i < v.len() {
                    let mut j = i + 1;
                    while j < v.len() && (v[j][0] ^ v[i][0]) >> 4 << 4 & !IDX_BITS == 0 {
                        j += 1;
                    }
                    v[i..j].sort_unstable();
                    i = j;
                }
            };
            norm(&mut a);
            norm(&mut b);
            if a != b {
                eprintln!("[sortbench] SORT WRONG n={n} rep={rep}");
                unsafe { crate::sys::_exit(9) };
            }
        }
    }
    eprintln!("[sortbench] checks passed for {} sizes", sizes.len());
    for &n in &[2usize, 3, 4, 5, 6, 7, 8, 9, 12, 15, 16, 17, 24, 31, 32, 33, 40, 48, 63, 64, 100, 128, 200, 300, 512, 600, 1500, 2048, 5000, 20000, 75000] {
        let reps = (400_000 / n).max(4);
        let mut data: Vec<[u64; 2]> = (0..reps * n).map(|_| { let ts = rnd() % 1_000_000_000; let c = rnd() % 2000; [ts << 32 | c << 4 | (rnd() % 3), rnd() % (1 << 40)] }).collect();
        let t0 = unsafe { std::arch::x86_64::_rdtsc() };
        let mut r = 0;
        for _ in 0..reps {
            sort_clicks(g, &mut data[r * n..(r + 1) * n], &mut sc);
            r = (r + 37) % reps;
        }
        let el = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        eprintln!("[sortbench] n={n}: {:.0} cycles/call, {:.1} per click", el as f64 / reps as f64, el as f64 / (reps * n) as f64);
    }
}
