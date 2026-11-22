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

#[cfg(feature = "pmu")]
pub mod pm2 {
    use std::sync::atomic::{AtomicU64, Ordering};
    extern "C" {
        fn syscall(n: i64, ...) -> i64;
        fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    }
    pub const NP: usize = 27;
    pub const NE: usize = 3;
    pub static ACC: [[AtomicU64; NE]; NP] = [const { [const { AtomicU64::new(0) }; NE] }; NP];
    pub static HIST: [AtomicU64; 48] = [const { AtomicU64::new(0) }; 48];
    pub static HCL: [AtomicU64; 48] = [const { AtomicU64::new(0) }; 48];
    pub static NCK: AtomicU64 = AtomicU64::new(0);
    pub static PEND: AtomicU64 = AtomicU64::new(0);
    pub static EVC: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    pub static PSZ: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
    pub static PEL: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
    pub static PASS: AtomicU64 = AtomicU64::new(0);
    pub static TU: [AtomicU64; 48] = [const { AtomicU64::new(0) }; 48];
    pub static TP: [AtomicU64; 48] = [const { AtomicU64::new(0) }; 48];
    pub static EQ: [AtomicU64; 48] = [const { AtomicU64::new(0) }; 48];
    pub static NREC: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static FD: [i32; NE] = {
            let mut fd = [-1; NE];
            for (k, cfg) in [1u64, 5, 0].iter().enumerate() {
                let mut a = [0u64; 16];
                a[0] = 0 | (128u64 << 32);
                a[1] = *cfg;
                a[5] = (1 << 5) | (1 << 6);
                fd[k] = unsafe { syscall(298, a.as_ptr(), 0i32, -1i32, -1i32, 0u64) } as i32;
            }
            fd
        };
    }
    #[inline(never)]
    pub fn rd() -> [u64; NE] {
        let mut v = [0u64; NE];
        FD.with(|fd| for k in 0..NE {
            unsafe { read(fd[k], &mut v[k] as *mut u64 as *mut u8, 8) };
        });
        v
    }
    pub fn add(p: usize, a: [u64; NE], b: [u64; NE]) {
        for k in 0..NE {
            ACC[p][k].fetch_add(b[k].wrapping_sub(a[k]), Ordering::Relaxed);
        }
    }
    pub fn hist(n: usize) {
        let c = if n <= 32 { n } else { 32 + (usize::BITS - (n - 1).leading_zeros()) as usize - 5 };
        let c = c.min(47);
        HIST[c].fetch_add(1, Ordering::Relaxed);
        HCL[c].fetch_add(n as u64, Ordering::Relaxed);
    }
    pub fn print() {
        let nck = NCK.load(Ordering::Relaxed).max(1) as f64;
        let names = ["count", "links", "prefix", "scatter", "lists", "c2", "c4", "c8", "c16", "c32", "c64", "mid", "rows", "all", "linkrec", "ldone", "cxrows", "whale", "wh_split", "wh_small", "wh_mid", "wh_n", "tie", "mid_split", "part_split", "part_ks", "inplace"];
        let mut s = String::new();
        for p in 0..NP {
            let v: Vec<f64> = (0..NE).map(|k| ACC[p][k].load(Ordering::Relaxed) as f64 / nck).collect();
            s += &format!(" {}={:.1}i/{:.2}bm/{:.1}c", names[p], v[0], v[1], v[2]);
        }
        eprintln!("[pm2] per click (clicks {} records {}):{}", nck, NREC.load(Ordering::Relaxed), s);
        let mut h = String::new();
        for c in 0..48 {
            let n = HIST[c].load(Ordering::Relaxed);
            if n > 0 {
                let lab = if c <= 32 { format!("{c}") } else { format!("<={}", 1u64 << (c - 32 + 5)) };
                h += &format!(" {}:{}u/{:.1}%/tieu{}/tiep{}/eq{}", lab, n, 100.0 * HCL[c].load(Ordering::Relaxed) as f64 / nck, TU[c].load(Ordering::Relaxed), TP[c].load(Ordering::Relaxed), EQ[c].load(Ordering::Relaxed));
            }
        }
        eprintln!("[pm2] users by clicks:{}", h);
        eprintln!("[pm2] pend {} pass-entries {}", PEND.load(Ordering::Relaxed), PASS.load(Ordering::Relaxed));
        eprintln!("[pm2] parts by size (count/elems): {}", (0..10).map(|c| format!("{}:{}/{}", c, PSZ[c].load(Ordering::Relaxed), PEL[c].load(Ordering::Relaxed))).collect::<Vec<_>>().join(" "));
        eprintln!("[pm2] calls tie {} tie_rows {} rows_ix {} rows {} part {} mid {} tie_rows_n {} rows_n {}", EVC[0].load(Ordering::Relaxed), EVC[1].load(Ordering::Relaxed), EVC[2].load(Ordering::Relaxed), EVC[3].load(Ordering::Relaxed), EVC[4].load(Ordering::Relaxed), EVC[5].load(Ordering::Relaxed), EVC[6].load(Ordering::Relaxed), EVC[7].load(Ordering::Relaxed));
    }
}

#[derive(Default)]
pub struct Scratch {
    soff: Vec<u32>,
    seg: (usize, usize),
    whale: bool,
    tmp: Vec<[u64; 2]>,
    lk: Vec<u64>,
    l2: Vec<u32>,
    l8: Vec<u32>,
    l16: Vec<u32>,
    lbig: Vec<u32>,
    st: Vec<u32>,
    cls: Vec<u8>,
    ll: Vec<u32>,
    rows: Vec<[u64; 2]>,
    ka: Vec<u64>,
    wk: Vec<u64>,
    wc: Vec<u64>,
    wh: Vec<u32>,
    eqv: Vec<u64>,
    tiev: Vec<u64>,
    rs: Vec<u32>,
    rowbuf: Vec<u64>,
    d8: Vec<(usize, bool, Vec<[u64; 2]>)>,
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
        i = next_tie(s, i);
        if i >= n {
            break;
        }
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
fn next_tie(s: &[[u64; 2]], mut i: usize) -> usize {
    let n = s.len();
    const FIRST: [u8; 16] = [4, 0, 2, 0, 1, 0, 1, 0, 3, 0, 2, 0, 1, 0, 1, 0];
    unsafe {
        let p = s.as_ptr() as *const __m256i;
        let hi = _mm256_set1_epi64x(0xFFFF_FFFF_0000_0000u64 as i64);
        let lo = _mm256_set1_epi64x(0xFFFF_FFF0);
        let m4 = |j: usize| -> u8 {
            let pj = (p as *const [u64; 2]).add(j) as *const __m256i;
            let pm = (p as *const [u64; 2]).add(j - 1) as *const __m256i;
            let k = _mm256_unpacklo_epi64(_mm256_loadu_si256(pj), _mm256_loadu_si256(pj.add(1)));
            let q = _mm256_unpacklo_epi64(_mm256_loadu_si256(pm), _mm256_loadu_si256(pm.add(1)));
            let x = _mm256_xor_si256(k, q);
            _mm256_mask_test_epi64_mask(_mm256_testn_epi64_mask(x, hi), x, lo)
        };
        while i + 8 <= n {
            let a = m4(i);
            let b = m4(i + 4);
            if a | b != 0 {
                return if a != 0 { i + FIRST[a as usize] as usize } else { i + 4 + FIRST[b as usize] as usize };
            }
            i += 8;
        }
        while i < n {
            let x = s[i][0] ^ s[i - 1][0];
            if x >> 32 == 0 && x >> 4 != 0 {
                return i;
            }
            i += 1;
        }
    }
    n
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
    let mut rows = [[z::setzero(); 2]; R];
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
        rows[b] = [lo, hi];
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
    let idxb = z::set1_epi64(IDX_BITS as i64);
    let mut tie = 0u8;
    let mut pk = z::setzero();
    for b in 0..nb {
        let kc = z::andnot(idxb, r[b]);
        let left = n - 8 * b;
        let km: u8 = if left >= 8 { 0xFF } else { ((1u32 << left) - 1) as u8 };
        let first: u8 = if b == 0 { 0xFE } else { 0xFF };
        tie |= tie_mask(kc, z::prev_epi64(kc, pk)) & km & first;
        pk = kc;
    }
    let keys = r.as_ptr() as *const u64;
    let rp = rows.as_ptr() as *const __m128i;
    let dst = q as *mut __m128i;
    for i in 0..n {
        let j = ((*keys.add(i) >> IDX_SH) as usize) & (8 * R - 1);
        _mm_storeu_si128(dst.add(i), std::ptr::read_volatile(rp.add(j)));
    }
    if tie != 0 {
        fix_ties(g, s);
    }
}

unsafe fn sort_users(g: &G, off: *const u32, base: *mut [u64; 2], ua: usize, ub: usize, sc: &mut Scratch) {
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
        (base.add(a), *off.add(u as usize + 1) as usize - a)
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
    #[cfg(feature = "pmu")]
    for u in ua..ub {
        let (p, n) = row(u as u32);
        let mut t = 0u64;
        let mut eq = 0u64;
        for i in 1..n {
            let x = (*p.add(i))[0] ^ (*p.add(i - 1))[0];
            if x >> 32 == 0 { eq += 1; if x & 0x3FFF0 != 0 { t += 1; } }
        }
        let c = if n <= 32 { n } else { 32 + (usize::BITS - (n - 1).leading_zeros()) as usize - 5 }.min(47);
        if t > 0 { pm2::TU[c].fetch_add(1, Ordering::Relaxed); }
        pm2::TP[c].fetch_add(t, Ordering::Relaxed);
        pm2::EQ[c].fetch_add(eq, Ordering::Relaxed);
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
            if MODE8.load(Ordering::Relaxed) {
                TL_DEF.with(|d| d.borrow_mut().push((grp.as_mut_ptr() as usize, grp.len())));
            } else {
                DEFERRED.lock().unwrap().push((grp.as_mut_ptr() as usize, grp.len()));
            }
            return b;
        }
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
    }
    b
}
static DEFER_RAW: AtomicBool = AtomicBool::new(false);
static DEFERRED: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

static C8_NEXT: AtomicUsize = AtomicUsize::new(0);
static C8_BASE: AtomicUsize = AtomicUsize::new(0);
static C8_CAP: AtomicUsize = AtomicUsize::new(0);
static C8_SEG: AtomicUsize = AtomicUsize::new(0);
const C8_PAGE: usize = (2 << 20) / 8;
static MODE8: AtomicBool = AtomicBool::new(false);
thread_local! {
    static TL_DEF: std::cell::RefCell<Vec<(usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}
static DEFERRED8: std::sync::Mutex<Vec<(usize, bool, Vec<[u64; 2]>)>> = std::sync::Mutex::new(Vec::new());

#[inline]
fn c8_take(n: usize) -> usize {
    let n = (n + C8_PAGE - 1) & !(C8_PAGE - 1);
    let at = C8_NEXT.fetch_add(n, Ordering::Relaxed);
    if at + n + 64 > C8_CAP.load(Ordering::Relaxed) {
        fallback("CSR capacity");
    }
    at
}
#[inline]
fn c8_seg(sc: &mut Scratch, need: usize) -> usize {
    if sc.seg.1 < sc.seg.0 + need {
        let sz = need.max(C8_SEG.load(Ordering::Relaxed));
        let at = c8_take(sz);
        sc.seg = (at, at + sz);
    }
    sc.seg.0
}

#[inline(always)]
unsafe fn keys_out(src: *const [u64; 2], dst: *mut u64, n: usize) {
    let mut i = 0usize;
    while i + 4 <= n {
        let a = _mm256_loadu_si256(src.add(i) as *const __m256i);
        let b = _mm256_loadu_si256(src.add(i + 2) as *const __m256i);
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, _mm256_permute4x64_epi64::<0xD8>(_mm256_unpacklo_epi64(a, b)));
        i += 4;
    }
    while i < n {
        *dst.add(i) = (*src.add(i))[0];
        i += 1;
    }
}

unsafe fn rows_out(cxl: &[u32], soff: *const u32, off8: *const u32, src: *const [u64; 2], ua: usize, ub: usize) {
    let csr8 = C8_BASE.load(Ordering::Relaxed) as *mut u64;
    let mut s = *soff.add(ua) as usize;
    let mut d = *off8.add(ua) as usize;
    for &ul in cxl {
        let ul = ul as usize;
        let (a, b) = (*soff.add(ul) as usize, *soff.add(ul + 1) as usize);
        if *off8.add(ul) as usize != d + (a - s) || *off8.add(ul + 1) as usize != *off8.add(ul) as usize + 2 * (b - a) {
            fallback("CSR words");
        }
        keys_out(src.add(s), csr8.add(d), a - s);
        std::ptr::copy(src.add(a) as *const u64, csr8.add(*off8.add(ul) as usize), 2 * (b - a));
        s = b;
        d = *off8.add(ul + 1) as usize;
    }
    let e = *soff.add(ub) as usize;
    if *off8.add(ub) as usize != d + (e - s) {
        fallback("CSR words");
    }
    keys_out(src.add(s), csr8.add(d), e - s);
}

unsafe fn defer8(utab: &[u16], soff: *const u32, off8: *const u32, src: *const [u64; 2]) {
    TL_DEF.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_empty() {
            return;
        }
        let so = std::slice::from_raw_parts(soff, UBSIZE + 1);
        let mut out = DEFERRED8.lock().unwrap();
        for &(p, n) in d.iter() {
            let s = (p - src as usize) / 16;
            let ul = so.partition_point(|&x| x as usize <= s) - 1;
            if s + n > so[ul + 1] as usize {
                fallback("deferred tie group");
            }
            let two = utab[ul] & COMPLEX != 0;
            let at = *off8.add(ul) as usize + (s - so[ul] as usize) * (1 + two as usize);
            out.push((at, two, std::slice::from_raw_parts(p as *const [u64; 2], n).to_vec()));
        }
        d.clear();
    });
}

fn convert16(g: &mut G) {
    let words: usize = 2 * g.ncsr + 64;
    let dst = crate::sys::anon(words * 8) as *mut u64;
    let mut at = 0usize;
    for ub in 0..g.nub {
        let utab = utab_of(g, ub);
        let off = off_of(g, ub);
        let mut prev = off[0] as usize;
        for ul in 0..UBSIZE {
            let (a, b) = (prev, off[ul + 1] as usize);
            prev = b;
            off[ul] = at as u32;
            let two = utab[ul] & COMPLEX != 0;
            for i in a..b {
                let r = unsafe { *g.csr.add(i) };
                unsafe { *dst.add(at) = r[0] };
                at += 1;
                if two {
                    unsafe { *dst.add(at) = r[1] };
                    at += 1;
                }
            }
        }
        off[UBSIZE] = at as u32;
    }
    if at + 16 > words || at >= u32::MAX as usize {
        fallback("CSR capacity");
    }
    g.csr = dst as *mut [u64; 2];
    g.csr_cap = words / 2;
    g.ncsr = at + 16;
}

struct Heavy {
    ub: usize,
    cnt: Vec<UnsafeCell<Vec<u32>>>,
    links: Vec<UnsafeCell<Vec<u64>>>,
    cx: UnsafeCell<Vec<(u32, u8, u8)>>,
    scr: UnsafeCell<Vec<[u64; 2]>>,
    soff: UnsafeCell<Vec<u32>>,
    cxl: UnsafeCell<Vec<u32>>,
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
    whale: usize,
    nres: usize,
    rnext: AtomicUsize,
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
    #[cfg(feature = "pmu")]
    pm2::PEND.fetch_add(t.pend.len() as u64, Ordering::Relaxed);
    if !t.pend.is_empty() {
        let v = &mut t.pend;
        loop {
            #[cfg(feature = "pmu")]
            pm2::PASS.fetch_add(v.len() as u64, Ordering::Relaxed);
            let mut changed = false;
            let mut j = 0;
            for i in 0..v.len() {
                let (ul, bits) = v[i];
                let m = utab[ul as usize] & MASK;
                if m & bits != 0 {
                    if m | bits != m {
                        utab[ul as usize] |= bits;
                        changed = true;
                    }
                } else {
                    v[j] = (ul, bits);
                    j += 1;
                }
            }
            v.truncate(j);
            if !changed {
                break;
            }
        }
        for &(ul, bits) in v.iter() {
            if utab[ul as usize] & bits == 0 {
                utab[ul as usize] |= COMPLEX;
            }
        }
    }
    const _: () = assert!(UBSIZE % 32 == 0);
    let mut anyc = false;
    if utab.len() != UBSIZE {
        fallback("user table");
    }
    unsafe {
        let p = utab.as_ptr();
        let cm = _mm256_set1_epi16(COMPLEX as i16);
        let mut ul = 0usize;
        while ul < UBSIZE {
            let a = _mm256_test_epi16_mask(_mm256_loadu_si256(p.add(ul) as *const __m256i), cm) as u32;
            let b = _mm256_test_epi16_mask(_mm256_loadu_si256(p.add(ul + 16) as *const __m256i), cm) as u32;
            let mut m = a | b << 16;
            if m != 0 {
                anyc = true;
                while m != 0 {
                    t.cx_users.push((u0 + ul + m.trailing_zeros() as usize) as u32);
                    m &= m - 1;
                }
            }
            ul += 32;
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

static K8ON: AtomicBool = AtomicBool::new(true);
const TIEM: u64 = 0xFFFF_FFFF_0000_0000 | 0x3_FFF0;
const CIDM: u64 = RAW | (RAW - 1);
const MID_MAX: usize = { let m = env_u32(option_env!("XL_MID_MAX"), (1 << 14) - 1) as usize; assert!(m >= 64 && m < 1 << 14); m };
const MID_PER: usize = env_u32(option_env!("XL_MID_PER"), 16) as usize;
const _: () = assert!(CBOUND <= 1 << 14 && NALIAS <= 16 && IDX_SH == 18 && IDX_BITS == 0xFFFC_0000);

#[repr(align(64))]
struct K8Pos([u64; 64]);
static K8POS2: K8Pos = {
    let mut a = [0u64; 64];
    let ord = [0u64, 2, 1, 3];
    let mut i = 0;
    while i < 64 {
        a[i] = ((i as u64 & !3) + ord[i & 3]) << IDX_SH;
        i += 1;
    }
    K8Pos(a)
};
static K8LM: [u8; 16] = {
    let mut t = [0u8; 16];
    let mut m = 0;
    while m < 16 {
        t[m] = ((m & 9) | (m & 2) << 1 | (m & 4) >> 1) as u8;
        m += 1;
    }
    t
};
#[inline(always)]
unsafe fn k8_cid(cids: *const u64, k: u64) -> u64 {
    *cids.add(2 * ((k & IDX_BITS) >> IDX_SH) as usize) & CIDM
}

#[inline(always)]
unsafe fn cex(v: &mut [__m256i], a: usize, b: usize) {
    let lo = _mm256_min_epu64(v[a], v[b]);
    let hi = _mm256_max_epu64(v[a], v[b]);
    v[a] = lo;
    v[b] = hi;
}
#[inline(always)]
unsafe fn in1(v: __m256i) -> __m256i {
    let s = _mm256_shuffle_epi32::<0x4E>(v);
    _mm256_blend_epi32::<0xCC>(_mm256_min_epu64(v, s), _mm256_max_epu64(v, s))
}
#[inline(always)]
unsafe fn rev1(v: __m256i) -> __m256i {
    let s = _mm256_permute4x64_epi64::<0x1B>(v);
    _mm256_blend_epi32::<0xF0>(_mm256_min_epu64(v, s), _mm256_max_epu64(v, s))
}
#[inline(always)]
unsafe fn mir2(v: &mut [__m256i], a: usize, b: usize) {
    let s = _mm256_shuffle_epi32::<0x4E>(v[b]);
    let mn = _mm256_min_epu64(v[a], s);
    let mx = _mm256_max_epu64(v[a], s);
    v[a] = _mm256_blend_epi32::<0xCC>(mn, mx);
    v[b] = _mm256_shuffle_epi32::<0x4E>(_mm256_blend_epi32::<0xCC>(mx, mn));
}
#[inline(always)]
unsafe fn mir4(v: &mut [__m256i], a: usize, b: usize) {
    let s = _mm256_permute4x64_epi64::<0x1B>(v[b]);
    let mn = _mm256_min_epu64(v[a], s);
    let mx = _mm256_max_epu64(v[a], s);
    v[a] = _mm256_blend_epi32::<0xF0>(mn, mx);
    v[b] = _mm256_permute4x64_epi64::<0x1B>(_mm256_blend_epi32::<0xF0>(mx, mn));
}

#[inline(always)]
unsafe fn ks0<const E: bool>(src: *const u64, _n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let (a, b) = if E { (*src.add(1), *src.add(3)) } else { (*src, *src.add(1)) };
    if E && (a | b) & IDX_BITS != 0 {
        return 2;
    }
    let b = if E { b | 1 << IDX_SH } else { b };
    let lo = a.min(b);
    let hi = a.max(b);
    if ((lo ^ hi) & TIEM).wrapping_sub(1) < 0xFFFF_FFFF {
        *buf = lo;
        *buf.add(1) = hi;
        return 1;
    }
    *dst = lo & !IDX_BITS;
    *dst.add(1) = hi & !IDX_BITS;
    0
}

#[inline(always)]
unsafe fn ks1<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let mx = _mm256_set1_epi64x(-1);
    let mb: u64 = _bzhi_u64(!0, n as u32);
    let mut chk = _mm256_setzero_si256();
    let pv = K8POS2.0.as_ptr() as *const __m256i;
    let mut v = [mx; 1];
    { let k = ((mb >> 0) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(0) as *const __m256i), _mm256_loadu_si256(src.add(4) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[0] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(0))); } else { v[0] = _mm256_mask_loadu_epi64(mx, k, src.add(0) as *const i64); } }
    if E && _mm256_test_epi64_mask(chk, _mm256_set1_epi64x(IDX_BITS as i64)) != 0 {
        return 2;
    }
    v[0] = in1(v[0]);
    v[0] = rev1(v[0]);
    v[0] = in1(v[0]);
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let acc = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[0], _mm256_alignr_epi64::<3>(v[0], mx), tm), m1);
    let tie = _mm256_cmplt_epu64_mask(acc, _mm256_set1_epi64x(0xFFFF_FFFF)) != 0;
    let t = [v[0]];
    if tie {
        let bq = buf as *mut __m256i;
        _mm256_storeu_si256(bq.add(0), t[0]);
        return 1;
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    _mm256_mask_storeu_epi64(dst.add(0) as *mut i64, ((mb >> 0) & 15) as u8, _mm256_andnot_si256(pm, t[0]));
    0
}

#[inline(always)]
unsafe fn ks2<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let mx = _mm256_set1_epi64x(-1);
    let mb: u64 = _bzhi_u64(!0, n as u32);
    let mut chk = _mm256_setzero_si256();
    let pv = K8POS2.0.as_ptr() as *const __m256i;
    let mut v = [mx; 2];
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(0) as *const __m256i), _mm256_loadu_si256(src.add(4) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[0] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(0))); } else { v[0] = _mm256_loadu_si256(src.add(0) as *const __m256i); }
    { let k = ((mb >> 4) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(8) as *const __m256i), _mm256_loadu_si256(src.add(12) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[1] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(1))); } else { v[1] = _mm256_mask_loadu_epi64(mx, k, src.add(4) as *const i64); } }
    if E && _mm256_test_epi64_mask(chk, _mm256_set1_epi64x(IDX_BITS as i64)) != 0 {
        return 2;
    }
    cex(&mut v, 0, 1);
    mir2(&mut v, 0, 1);
    cex(&mut v, 0, 1);
    mir4(&mut v, 0, 1);
    v[0] = in1(v[0]);
    v[1] = in1(v[1]);
    cex(&mut v, 0, 1);
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let mut acc = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[0], _mm256_alignr_epi64::<3>(v[1], mx), tm), m1);
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[1], v[0], tm), m1));
    let tie = _mm256_cmplt_epu64_mask(acc, _mm256_set1_epi64x(0xFFFF_FFFF)) != 0;
    let a = _mm256_unpacklo_epi64(v[0], v[1]);
    let b = _mm256_unpackhi_epi64(v[0], v[1]);
    let t = [_mm256_permute2x128_si256::<0x20>(a, b), _mm256_permute2x128_si256::<0x31>(a, b)];
    if tie {
        let bq = buf as *mut __m256i;
        _mm256_storeu_si256(bq.add(0), t[0]);
        _mm256_storeu_si256(bq.add(1), t[1]);
        return 1;
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    _mm256_storeu_si256(dst.add(0) as *mut __m256i, _mm256_andnot_si256(pm, t[0]));
    _mm256_mask_storeu_epi64(dst.add(4) as *mut i64, ((mb >> 4) & 15) as u8, _mm256_andnot_si256(pm, t[1]));
    0
}

#[inline(always)]
unsafe fn ks4<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let mx = _mm256_set1_epi64x(-1);
    let mb: u64 = _bzhi_u64(!0, n as u32);
    let mut chk = _mm256_setzero_si256();
    let pv = K8POS2.0.as_ptr() as *const __m256i;
    let mut v = [mx; 4];
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(0) as *const __m256i), _mm256_loadu_si256(src.add(4) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[0] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(0))); } else { v[0] = _mm256_loadu_si256(src.add(0) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(8) as *const __m256i), _mm256_loadu_si256(src.add(12) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[1] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(1))); } else { v[1] = _mm256_loadu_si256(src.add(4) as *const __m256i); }
    { let k = ((mb >> 8) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(16) as *const __m256i), _mm256_loadu_si256(src.add(20) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[2] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(2))); } else { v[2] = _mm256_mask_loadu_epi64(mx, k, src.add(8) as *const i64); } }
    { let k = ((mb >> 12) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(24) as *const __m256i), _mm256_loadu_si256(src.add(28) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[3] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(3))); } else { v[3] = _mm256_mask_loadu_epi64(mx, k, src.add(12) as *const i64); } }
    if E && _mm256_test_epi64_mask(chk, _mm256_set1_epi64x(IDX_BITS as i64)) != 0 {
        return 2;
    }
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 0, 3);
    cex(&mut v, 1, 2);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    mir2(&mut v, 0, 3);
    mir2(&mut v, 1, 2);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    mir4(&mut v, 0, 3);
    mir4(&mut v, 1, 2);
    v[0] = in1(v[0]);
    v[1] = in1(v[1]);
    v[2] = in1(v[2]);
    v[3] = in1(v[3]);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let mut acc = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[0], _mm256_alignr_epi64::<3>(v[3], mx), tm), m1);
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[1], v[0], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[2], v[1], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[3], v[2], tm), m1));
    let tie = _mm256_cmplt_epu64_mask(acc, _mm256_set1_epi64x(0xFFFF_FFFF)) != 0;
    let mut t = [mx; 4];
    { let a0 = _mm256_unpacklo_epi64(v[0], v[1]); let a1 = _mm256_unpackhi_epi64(v[0], v[1]); let a2 = _mm256_unpacklo_epi64(v[2], v[3]); let a3 = _mm256_unpackhi_epi64(v[2], v[3]);
      t[0] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[1] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[2] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[3] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    if tie {
        let bq = buf as *mut __m256i;
        _mm256_storeu_si256(bq.add(0), t[0]);
        _mm256_storeu_si256(bq.add(1), t[1]);
        _mm256_storeu_si256(bq.add(2), t[2]);
        _mm256_storeu_si256(bq.add(3), t[3]);
        return 1;
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    _mm256_storeu_si256(dst.add(0) as *mut __m256i, _mm256_andnot_si256(pm, t[0]));
    _mm256_storeu_si256(dst.add(4) as *mut __m256i, _mm256_andnot_si256(pm, t[1]));
    _mm256_mask_storeu_epi64(dst.add(8) as *mut i64, ((mb >> 8) & 15) as u8, _mm256_andnot_si256(pm, t[2]));
    _mm256_mask_storeu_epi64(dst.add(12) as *mut i64, ((mb >> 12) & 15) as u8, _mm256_andnot_si256(pm, t[3]));
    0
}

#[inline(always)]
unsafe fn ks8<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let mx = _mm256_set1_epi64x(-1);
    let mb: u64 = _bzhi_u64(!0, n as u32);
    let mut chk = _mm256_setzero_si256();
    let pv = K8POS2.0.as_ptr() as *const __m256i;
    let mut v = [mx; 8];
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(0) as *const __m256i), _mm256_loadu_si256(src.add(4) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[0] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(0))); } else { v[0] = _mm256_loadu_si256(src.add(0) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(8) as *const __m256i), _mm256_loadu_si256(src.add(12) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[1] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(1))); } else { v[1] = _mm256_loadu_si256(src.add(4) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(16) as *const __m256i), _mm256_loadu_si256(src.add(20) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[2] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(2))); } else { v[2] = _mm256_loadu_si256(src.add(8) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(24) as *const __m256i), _mm256_loadu_si256(src.add(28) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[3] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(3))); } else { v[3] = _mm256_loadu_si256(src.add(12) as *const __m256i); }
    { let k = ((mb >> 16) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(32) as *const __m256i), _mm256_loadu_si256(src.add(36) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[4] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(4))); } else { v[4] = _mm256_mask_loadu_epi64(mx, k, src.add(16) as *const i64); } }
    { let k = ((mb >> 20) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(40) as *const __m256i), _mm256_loadu_si256(src.add(44) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[5] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(5))); } else { v[5] = _mm256_mask_loadu_epi64(mx, k, src.add(20) as *const i64); } }
    { let k = ((mb >> 24) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(48) as *const __m256i), _mm256_loadu_si256(src.add(52) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[6] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(6))); } else { v[6] = _mm256_mask_loadu_epi64(mx, k, src.add(24) as *const i64); } }
    { let k = ((mb >> 28) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(56) as *const __m256i), _mm256_loadu_si256(src.add(60) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[7] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(7))); } else { v[7] = _mm256_mask_loadu_epi64(mx, k, src.add(28) as *const i64); } }
    if E && _mm256_test_epi64_mask(chk, _mm256_set1_epi64x(IDX_BITS as i64)) != 0 {
        return 2;
    }
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 0, 3);
    cex(&mut v, 1, 2);
    cex(&mut v, 4, 7);
    cex(&mut v, 5, 6);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 0, 7);
    cex(&mut v, 1, 6);
    cex(&mut v, 2, 5);
    cex(&mut v, 3, 4);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    mir2(&mut v, 0, 7);
    mir2(&mut v, 1, 6);
    mir2(&mut v, 2, 5);
    mir2(&mut v, 3, 4);
    cex(&mut v, 0, 4);
    cex(&mut v, 1, 5);
    cex(&mut v, 2, 6);
    cex(&mut v, 3, 7);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    mir4(&mut v, 0, 7);
    mir4(&mut v, 1, 6);
    mir4(&mut v, 2, 5);
    mir4(&mut v, 3, 4);
    v[0] = in1(v[0]);
    v[1] = in1(v[1]);
    v[2] = in1(v[2]);
    v[3] = in1(v[3]);
    v[4] = in1(v[4]);
    v[5] = in1(v[5]);
    v[6] = in1(v[6]);
    v[7] = in1(v[7]);
    cex(&mut v, 0, 4);
    cex(&mut v, 1, 5);
    cex(&mut v, 2, 6);
    cex(&mut v, 3, 7);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let mut acc = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[0], _mm256_alignr_epi64::<3>(v[7], mx), tm), m1);
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[1], v[0], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[2], v[1], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[3], v[2], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[4], v[3], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[5], v[4], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[6], v[5], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[7], v[6], tm), m1));
    let tie = _mm256_cmplt_epu64_mask(acc, _mm256_set1_epi64x(0xFFFF_FFFF)) != 0;
    let mut t = [mx; 8];
    { let a0 = _mm256_unpacklo_epi64(v[0], v[1]); let a1 = _mm256_unpackhi_epi64(v[0], v[1]); let a2 = _mm256_unpacklo_epi64(v[2], v[3]); let a3 = _mm256_unpackhi_epi64(v[2], v[3]);
      t[0] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[2] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[4] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[6] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    { let a0 = _mm256_unpacklo_epi64(v[4], v[5]); let a1 = _mm256_unpackhi_epi64(v[4], v[5]); let a2 = _mm256_unpacklo_epi64(v[6], v[7]); let a3 = _mm256_unpackhi_epi64(v[6], v[7]);
      t[1] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[3] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[5] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[7] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    if tie {
        let bq = buf as *mut __m256i;
        _mm256_storeu_si256(bq.add(0), t[0]);
        _mm256_storeu_si256(bq.add(1), t[1]);
        _mm256_storeu_si256(bq.add(2), t[2]);
        _mm256_storeu_si256(bq.add(3), t[3]);
        _mm256_storeu_si256(bq.add(4), t[4]);
        _mm256_storeu_si256(bq.add(5), t[5]);
        _mm256_storeu_si256(bq.add(6), t[6]);
        _mm256_storeu_si256(bq.add(7), t[7]);
        return 1;
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    _mm256_storeu_si256(dst.add(0) as *mut __m256i, _mm256_andnot_si256(pm, t[0]));
    _mm256_storeu_si256(dst.add(4) as *mut __m256i, _mm256_andnot_si256(pm, t[1]));
    _mm256_storeu_si256(dst.add(8) as *mut __m256i, _mm256_andnot_si256(pm, t[2]));
    _mm256_storeu_si256(dst.add(12) as *mut __m256i, _mm256_andnot_si256(pm, t[3]));
    _mm256_mask_storeu_epi64(dst.add(16) as *mut i64, ((mb >> 16) & 15) as u8, _mm256_andnot_si256(pm, t[4]));
    _mm256_mask_storeu_epi64(dst.add(20) as *mut i64, ((mb >> 20) & 15) as u8, _mm256_andnot_si256(pm, t[5]));
    _mm256_mask_storeu_epi64(dst.add(24) as *mut i64, ((mb >> 24) & 15) as u8, _mm256_andnot_si256(pm, t[6]));
    _mm256_mask_storeu_epi64(dst.add(28) as *mut i64, ((mb >> 28) & 15) as u8, _mm256_andnot_si256(pm, t[7]));
    0
}

#[inline(always)]
unsafe fn ks16<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    let mx = _mm256_set1_epi64x(-1);
    let mb: u64 = _bzhi_u64(!0, n as u32);
    let mut chk = _mm256_setzero_si256();
    let pv = K8POS2.0.as_ptr() as *const __m256i;
    let mut v = [mx; 16];
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(0) as *const __m256i), _mm256_loadu_si256(src.add(4) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[0] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(0))); } else { v[0] = _mm256_loadu_si256(src.add(0) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(8) as *const __m256i), _mm256_loadu_si256(src.add(12) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[1] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(1))); } else { v[1] = _mm256_loadu_si256(src.add(4) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(16) as *const __m256i), _mm256_loadu_si256(src.add(20) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[2] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(2))); } else { v[2] = _mm256_loadu_si256(src.add(8) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(24) as *const __m256i), _mm256_loadu_si256(src.add(28) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[3] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(3))); } else { v[3] = _mm256_loadu_si256(src.add(12) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(32) as *const __m256i), _mm256_loadu_si256(src.add(36) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[4] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(4))); } else { v[4] = _mm256_loadu_si256(src.add(16) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(40) as *const __m256i), _mm256_loadu_si256(src.add(44) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[5] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(5))); } else { v[5] = _mm256_loadu_si256(src.add(20) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(48) as *const __m256i), _mm256_loadu_si256(src.add(52) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[6] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(6))); } else { v[6] = _mm256_loadu_si256(src.add(24) as *const __m256i); }
    if E { let raw = _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(56) as *const __m256i), _mm256_loadu_si256(src.add(60) as *const __m256i)); chk = _mm256_or_si256(chk, raw); v[7] = _mm256_or_si256(raw, _mm256_load_si256(pv.add(7))); } else { v[7] = _mm256_loadu_si256(src.add(28) as *const __m256i); }
    { let k = ((mb >> 32) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(64) as *const __m256i), _mm256_loadu_si256(src.add(68) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[8] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(8))); } else { v[8] = _mm256_mask_loadu_epi64(mx, k, src.add(32) as *const i64); } }
    { let k = ((mb >> 36) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(72) as *const __m256i), _mm256_loadu_si256(src.add(76) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[9] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(9))); } else { v[9] = _mm256_mask_loadu_epi64(mx, k, src.add(36) as *const i64); } }
    { let k = ((mb >> 40) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(80) as *const __m256i), _mm256_loadu_si256(src.add(84) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[10] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(10))); } else { v[10] = _mm256_mask_loadu_epi64(mx, k, src.add(40) as *const i64); } }
    { let k = ((mb >> 44) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(88) as *const __m256i), _mm256_loadu_si256(src.add(92) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[11] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(11))); } else { v[11] = _mm256_mask_loadu_epi64(mx, k, src.add(44) as *const i64); } }
    { let k = ((mb >> 48) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(96) as *const __m256i), _mm256_loadu_si256(src.add(100) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[12] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(12))); } else { v[12] = _mm256_mask_loadu_epi64(mx, k, src.add(48) as *const i64); } }
    { let k = ((mb >> 52) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(104) as *const __m256i), _mm256_loadu_si256(src.add(108) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[13] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(13))); } else { v[13] = _mm256_mask_loadu_epi64(mx, k, src.add(52) as *const i64); } }
    { let k = ((mb >> 56) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(112) as *const __m256i), _mm256_loadu_si256(src.add(116) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[14] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(14))); } else { v[14] = _mm256_mask_loadu_epi64(mx, k, src.add(56) as *const i64); } }
    { let k = ((mb >> 60) & 15) as u8; if E { let kl = K8LM[k as usize]; let raw = _mm256_maskz_mov_epi64(kl, _mm256_unpackhi_epi64(_mm256_loadu_si256(src.add(120) as *const __m256i), _mm256_loadu_si256(src.add(124) as *const __m256i))); chk = _mm256_or_si256(chk, raw); v[15] = _mm256_mask_or_epi64(mx, kl, raw, _mm256_load_si256(pv.add(15))); } else { v[15] = _mm256_mask_loadu_epi64(mx, k, src.add(60) as *const i64); } }
    if E && _mm256_test_epi64_mask(chk, _mm256_set1_epi64x(IDX_BITS as i64)) != 0 {
        return 2;
    }
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    cex(&mut v, 0, 3);
    cex(&mut v, 1, 2);
    cex(&mut v, 4, 7);
    cex(&mut v, 5, 6);
    cex(&mut v, 8, 11);
    cex(&mut v, 9, 10);
    cex(&mut v, 12, 15);
    cex(&mut v, 13, 14);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    cex(&mut v, 0, 7);
    cex(&mut v, 1, 6);
    cex(&mut v, 2, 5);
    cex(&mut v, 3, 4);
    cex(&mut v, 8, 15);
    cex(&mut v, 9, 14);
    cex(&mut v, 10, 13);
    cex(&mut v, 11, 12);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 8, 10);
    cex(&mut v, 9, 11);
    cex(&mut v, 12, 14);
    cex(&mut v, 13, 15);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    cex(&mut v, 0, 15);
    cex(&mut v, 1, 14);
    cex(&mut v, 2, 13);
    cex(&mut v, 3, 12);
    cex(&mut v, 4, 11);
    cex(&mut v, 5, 10);
    cex(&mut v, 6, 9);
    cex(&mut v, 7, 8);
    cex(&mut v, 0, 4);
    cex(&mut v, 1, 5);
    cex(&mut v, 2, 6);
    cex(&mut v, 3, 7);
    cex(&mut v, 8, 12);
    cex(&mut v, 9, 13);
    cex(&mut v, 10, 14);
    cex(&mut v, 11, 15);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 8, 10);
    cex(&mut v, 9, 11);
    cex(&mut v, 12, 14);
    cex(&mut v, 13, 15);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    mir2(&mut v, 0, 15);
    mir2(&mut v, 1, 14);
    mir2(&mut v, 2, 13);
    mir2(&mut v, 3, 12);
    mir2(&mut v, 4, 11);
    mir2(&mut v, 5, 10);
    mir2(&mut v, 6, 9);
    mir2(&mut v, 7, 8);
    cex(&mut v, 0, 8);
    cex(&mut v, 1, 9);
    cex(&mut v, 2, 10);
    cex(&mut v, 3, 11);
    cex(&mut v, 4, 12);
    cex(&mut v, 5, 13);
    cex(&mut v, 6, 14);
    cex(&mut v, 7, 15);
    cex(&mut v, 0, 4);
    cex(&mut v, 1, 5);
    cex(&mut v, 2, 6);
    cex(&mut v, 3, 7);
    cex(&mut v, 8, 12);
    cex(&mut v, 9, 13);
    cex(&mut v, 10, 14);
    cex(&mut v, 11, 15);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 8, 10);
    cex(&mut v, 9, 11);
    cex(&mut v, 12, 14);
    cex(&mut v, 13, 15);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    mir4(&mut v, 0, 15);
    mir4(&mut v, 1, 14);
    mir4(&mut v, 2, 13);
    mir4(&mut v, 3, 12);
    mir4(&mut v, 4, 11);
    mir4(&mut v, 5, 10);
    mir4(&mut v, 6, 9);
    mir4(&mut v, 7, 8);
    v[0] = in1(v[0]);
    v[1] = in1(v[1]);
    v[2] = in1(v[2]);
    v[3] = in1(v[3]);
    v[4] = in1(v[4]);
    v[5] = in1(v[5]);
    v[6] = in1(v[6]);
    v[7] = in1(v[7]);
    v[8] = in1(v[8]);
    v[9] = in1(v[9]);
    v[10] = in1(v[10]);
    v[11] = in1(v[11]);
    v[12] = in1(v[12]);
    v[13] = in1(v[13]);
    v[14] = in1(v[14]);
    v[15] = in1(v[15]);
    cex(&mut v, 0, 8);
    cex(&mut v, 1, 9);
    cex(&mut v, 2, 10);
    cex(&mut v, 3, 11);
    cex(&mut v, 4, 12);
    cex(&mut v, 5, 13);
    cex(&mut v, 6, 14);
    cex(&mut v, 7, 15);
    cex(&mut v, 0, 4);
    cex(&mut v, 1, 5);
    cex(&mut v, 2, 6);
    cex(&mut v, 3, 7);
    cex(&mut v, 8, 12);
    cex(&mut v, 9, 13);
    cex(&mut v, 10, 14);
    cex(&mut v, 11, 15);
    cex(&mut v, 0, 2);
    cex(&mut v, 1, 3);
    cex(&mut v, 4, 6);
    cex(&mut v, 5, 7);
    cex(&mut v, 8, 10);
    cex(&mut v, 9, 11);
    cex(&mut v, 12, 14);
    cex(&mut v, 13, 15);
    cex(&mut v, 0, 1);
    cex(&mut v, 2, 3);
    cex(&mut v, 4, 5);
    cex(&mut v, 6, 7);
    cex(&mut v, 8, 9);
    cex(&mut v, 10, 11);
    cex(&mut v, 12, 13);
    cex(&mut v, 14, 15);
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let mut acc = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[0], _mm256_alignr_epi64::<3>(v[15], mx), tm), m1);
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[1], v[0], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[2], v[1], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[3], v[2], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[4], v[3], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[5], v[4], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[6], v[5], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[7], v[6], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[8], v[7], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[9], v[8], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[10], v[9], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[11], v[10], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[12], v[11], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[13], v[12], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[14], v[13], tm), m1));
    acc = _mm256_min_epu64(acc, _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(v[15], v[14], tm), m1));
    let tie = _mm256_cmplt_epu64_mask(acc, _mm256_set1_epi64x(0xFFFF_FFFF)) != 0;
    let mut t = [mx; 16];
    { let a0 = _mm256_unpacklo_epi64(v[0], v[1]); let a1 = _mm256_unpackhi_epi64(v[0], v[1]); let a2 = _mm256_unpacklo_epi64(v[2], v[3]); let a3 = _mm256_unpackhi_epi64(v[2], v[3]);
      t[0] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[4] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[8] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[12] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    { let a0 = _mm256_unpacklo_epi64(v[4], v[5]); let a1 = _mm256_unpackhi_epi64(v[4], v[5]); let a2 = _mm256_unpacklo_epi64(v[6], v[7]); let a3 = _mm256_unpackhi_epi64(v[6], v[7]);
      t[1] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[5] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[9] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[13] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    { let a0 = _mm256_unpacklo_epi64(v[8], v[9]); let a1 = _mm256_unpackhi_epi64(v[8], v[9]); let a2 = _mm256_unpacklo_epi64(v[10], v[11]); let a3 = _mm256_unpackhi_epi64(v[10], v[11]);
      t[2] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[6] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[10] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[14] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    { let a0 = _mm256_unpacklo_epi64(v[12], v[13]); let a1 = _mm256_unpackhi_epi64(v[12], v[13]); let a2 = _mm256_unpacklo_epi64(v[14], v[15]); let a3 = _mm256_unpackhi_epi64(v[14], v[15]);
      t[3] = _mm256_permute2x128_si256::<0x20>(a0, a2); t[7] = _mm256_permute2x128_si256::<0x20>(a1, a3); t[11] = _mm256_permute2x128_si256::<0x31>(a0, a2); t[15] = _mm256_permute2x128_si256::<0x31>(a1, a3); }
    if tie {
        let bq = buf as *mut __m256i;
        _mm256_storeu_si256(bq.add(0), t[0]);
        _mm256_storeu_si256(bq.add(1), t[1]);
        _mm256_storeu_si256(bq.add(2), t[2]);
        _mm256_storeu_si256(bq.add(3), t[3]);
        _mm256_storeu_si256(bq.add(4), t[4]);
        _mm256_storeu_si256(bq.add(5), t[5]);
        _mm256_storeu_si256(bq.add(6), t[6]);
        _mm256_storeu_si256(bq.add(7), t[7]);
        _mm256_storeu_si256(bq.add(8), t[8]);
        _mm256_storeu_si256(bq.add(9), t[9]);
        _mm256_storeu_si256(bq.add(10), t[10]);
        _mm256_storeu_si256(bq.add(11), t[11]);
        _mm256_storeu_si256(bq.add(12), t[12]);
        _mm256_storeu_si256(bq.add(13), t[13]);
        _mm256_storeu_si256(bq.add(14), t[14]);
        _mm256_storeu_si256(bq.add(15), t[15]);
        return 1;
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    _mm256_storeu_si256(dst.add(0) as *mut __m256i, _mm256_andnot_si256(pm, t[0]));
    _mm256_storeu_si256(dst.add(4) as *mut __m256i, _mm256_andnot_si256(pm, t[1]));
    _mm256_storeu_si256(dst.add(8) as *mut __m256i, _mm256_andnot_si256(pm, t[2]));
    _mm256_storeu_si256(dst.add(12) as *mut __m256i, _mm256_andnot_si256(pm, t[3]));
    _mm256_storeu_si256(dst.add(16) as *mut __m256i, _mm256_andnot_si256(pm, t[4]));
    _mm256_storeu_si256(dst.add(20) as *mut __m256i, _mm256_andnot_si256(pm, t[5]));
    _mm256_storeu_si256(dst.add(24) as *mut __m256i, _mm256_andnot_si256(pm, t[6]));
    _mm256_storeu_si256(dst.add(28) as *mut __m256i, _mm256_andnot_si256(pm, t[7]));
    _mm256_mask_storeu_epi64(dst.add(32) as *mut i64, ((mb >> 32) & 15) as u8, _mm256_andnot_si256(pm, t[8]));
    _mm256_mask_storeu_epi64(dst.add(36) as *mut i64, ((mb >> 36) & 15) as u8, _mm256_andnot_si256(pm, t[9]));
    _mm256_mask_storeu_epi64(dst.add(40) as *mut i64, ((mb >> 40) & 15) as u8, _mm256_andnot_si256(pm, t[10]));
    _mm256_mask_storeu_epi64(dst.add(44) as *mut i64, ((mb >> 44) & 15) as u8, _mm256_andnot_si256(pm, t[11]));
    _mm256_mask_storeu_epi64(dst.add(48) as *mut i64, ((mb >> 48) & 15) as u8, _mm256_andnot_si256(pm, t[12]));
    _mm256_mask_storeu_epi64(dst.add(52) as *mut i64, ((mb >> 52) & 15) as u8, _mm256_andnot_si256(pm, t[13]));
    _mm256_mask_storeu_epi64(dst.add(56) as *mut i64, ((mb >> 56) & 15) as u8, _mm256_andnot_si256(pm, t[14]));
    _mm256_mask_storeu_epi64(dst.add(60) as *mut i64, ((mb >> 60) & 15) as u8, _mm256_andnot_si256(pm, t[15]));
    0
}

#[cfg(feature = "pmu")]
#[inline(never)]
pub unsafe fn probe_ks1(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 { ks1::<true>(src, n, dst, buf) }
#[cfg(feature = "pmu")]
#[inline(never)]
pub unsafe fn probe_ks2(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 { ks2::<true>(src, n, dst, buf) }
#[cfg(feature = "pmu")]
#[inline(never)]
pub unsafe fn probe_ks4(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 { ks4::<true>(src, n, dst, buf) }
#[cfg(feature = "pmu")]
#[inline(never)]
pub unsafe fn probe_ks16(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 { ks16::<true>(src, n, dst, buf) }
#[inline(always)]
unsafe fn ks_any<const E: bool>(src: *const u64, n: usize, dst: *mut u64, buf: *mut u64) -> u32 {
    match n {
        0 => 0,
        1 => {
            let a = if E { *src.add(1) } else { *src };
            if E && a & IDX_BITS != 0 {
                return 2;
            }
            *dst = a & !IDX_BITS;
            0
        }
        2 => ks0::<E>(src, n, dst, buf),
        3..=4 => ks1::<E>(src, n, dst, buf),
        5..=8 => ks2::<E>(src, n, dst, buf),
        9..=16 => ks4::<E>(src, n, dst, buf),
        17..=32 => ks8::<E>(src, n, dst, buf),
        _ => ks16::<E>(src, n, dst, buf),
    }
}

#[inline(never)]
unsafe fn k8_put(rows: &[[u64; 2]], dst: *mut u64, two: bool, sc: &mut Scratch) {
    k8_put2(rows, dst, dst, two, sc)
}
#[inline(never)]
unsafe fn k8_put2(rows: &[[u64; 2]], dst: *mut u64, csr: *mut u64, two: bool, sc: &mut Scratch) {
    TL_DEF.with(|d| {
        let mut d = d.borrow_mut();
        if !d.is_empty() {
            let dw = (csr as usize - C8_BASE.load(Ordering::Relaxed)) / 8;
            for &(p, len) in d.iter() {
                let s = p.wrapping_sub(rows.as_ptr() as usize) / 16;
                if s > rows.len() || s + len > rows.len() || p & 15 != (rows.as_ptr() as usize) & 15 {
                    fallback("deferred tie group");
                }
                sc.d8.push((dw + s * (1 + two as usize), two, rows[s..s + len].to_vec()));
            }
            d.clear();
        }
    });
    if two {
        for (i, r) in rows.iter().enumerate() {
            *dst.add(2 * i) = r[0];
            *dst.add(2 * i + 1) = r[1];
        }
    } else {
        for (i, r) in rows.iter().enumerate() {
            *dst.add(i) = r[0];
        }
    }
}

#[inline(never)]
unsafe fn k8_tie(g: &G, buf: *const u64, n: usize, cids: *const u64, dst: *mut u64, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    let y0 = pm2::rd();
    k8_tie_in(g, buf, n, cids, dst, sc);
    #[cfg(all(feature = "pmu", pm2fine))]
    { let y1 = pm2::rd(); pm2::add(22, y0, y1); }
}
const P36: [u64; 9] = [1, 36, 1296, 46656, 1679616, 60466176, 2176782336, 78364164096, 2821109907456];
const L36: [u8; 65] = {
    let mut t = [1u8; 65];
    let mut b = 1;
    while b < 65 {
        let v = 1u128 << (b - 1);
        let mut l = 1u8;
        let mut p = 36u128;
        while p <= v {
            p *= 36;
            l += 1;
        }
        t[b] = l;
        b += 1;
    }
    t
};
#[inline(always)]
fn e36(y: u64) -> u64 {
    let b = 64 - y.leading_zeros() as usize;
    let mut l = L36[b] as usize;
    l += (y >= P36[l.min(8)]) as usize;
    let l = l.min(8);
    (y * P36[8 - l]) << 4 | l as u64
}

#[inline(always)]
unsafe fn k8_tie_in(g: &G, buf: *const u64, n: usize, cids: *const u64, dst: *mut u64, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    pm2::EVC[0].fetch_add(1, Ordering::Relaxed);
    let b = buf as *mut u64;
    let tm = _mm256_set1_epi64x(TIEM as i64);
    let m1 = _mm256_set1_epi64x(-1);
    let lim = _mm256_set1_epi64x(0xFFFF_FFFF);
    let nq = (n + 3) / 4;
    let mut last = m1;
    let mut ties = 0u64;
    for q in 0..nq {
        let cur = _mm256_loadu_si256(b.add(4 * q) as *const __m256i);
        let y = _mm256_add_epi64(_mm256_ternarylogic_epi64::<0x28>(cur, _mm256_alignr_epi64::<3>(cur, last), tm), m1);
        ties |= (_mm256_cmplt_epu64_mask(y, lim) as u64) << (4 * q);
        last = cur;
    }
    ties &= _bzhi_u64(!1, n as u32);
    let mut em = std::mem::MaybeUninit::<[u64; 64]>::uninit();
    let e = &mut *em.as_mut_ptr();
    while ties != 0 {
        let i = ties.trailing_zeros() as usize;
        let ts = *b.add(i) >> 32;
        if (i < 2 || *b.add(i - 2) >> 32 != ts) && (i + 1 >= n || *b.add(i + 1) >> 32 != ts) {
            let (k0, k1) = (*b.add(i - 1), *b.add(i));
            let c0 = k8_cid(cids, k0);
            let c1 = k8_cid(cids, k1);
            if (c0 | c1) & RAW != 0 {
                return k8_tie_rows(g, buf, n, cids, dst, sc);
            }
            let (e0, e1) = (e36(c0), e36(c1));
            if e0 < e1 || (e0 == e1 && k0 & !IDX_BITS > k1 & !IDX_BITS) {
                *b.add(i - 1) = k1;
                *b.add(i) = k0;
            }
            ties &= if i + 2 >= 64 { 0 } else { !0u64 << (i + 2) };
            continue;
        }
        let mut a = i - 1;
        while a > 0 && *b.add(a - 1) >> 32 == ts {
            a -= 1;
        }
        let mut z = i + 1;
        while z < n && *b.add(z) >> 32 == ts {
            z += 1;
        }
        for j in a..z {
            let c = k8_cid(cids, *b.add(j));
            if c & RAW != 0 {
                return k8_tie_rows(g, buf, n, cids, dst, sc);
            }
            e[j] = e36(c);
        }
        for x in a + 1..z {
            let (ex, kx) = (e[x], *b.add(x));
            let mut y = x;
            while y > a && (e[y - 1] < ex || (e[y - 1] == ex && *b.add(y - 1) & !IDX_BITS > kx & !IDX_BITS)) {
                e[y] = e[y - 1];
                *b.add(y) = *b.add(y - 1);
                y -= 1;
            }
            e[y] = ex;
            *b.add(y) = kx;
        }
        ties &= if z + 1 >= 64 { 0 } else { !0u64 << (z + 1) };
    }
    let pm = _mm256_set1_epi64x(IDX_BITS as i64);
    let mb = _bzhi_u64(!0, n as u32);
    for q in 0..nq {
        let v = _mm256_andnot_si256(pm, _mm256_loadu_si256(b.add(4 * q) as *const __m256i));
        _mm256_mask_storeu_epi64(dst.add(4 * q) as *mut i64, ((mb >> (4 * q)) & 15) as u8, v);
    }
}

#[cold]
#[inline(never)]
unsafe fn k8_tie_rows(g: &G, buf: *const u64, n: usize, cids: *const u64, dst: *mut u64, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    pm2::EVC[1].fetch_add(1, Ordering::Relaxed);

    let mut rows = std::mem::take(&mut sc.rows);
    rows.clear();
    for i in 0..n {
        let k = *buf.add(i);
        let p = ((k & IDX_BITS) >> IDX_SH) as usize;
        rows.push([k & !IDX_BITS, *cids.add(2 * p) & CIDM]);
    }
    fix_ties(g, &mut rows);
    k8_put(&rows, dst, false, sc);
    sc.rows = rows;
}

#[cold]
#[inline(never)]
unsafe fn k8_rows(g: &G, keys: *const u64, cids: *const u64, n: usize, dst: *mut u64, two: bool, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    pm2::EVC[3].fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "pmu")]
    pm2::EVC[7].fetch_add(n as u64, Ordering::Relaxed);
    let mut rows = std::mem::take(&mut sc.rows);
    rows.clear();
    rows.reserve(n + 1040);
    for i in 0..n {
        rows.push([*keys.add(2 * i), *cids.add(2 * i) & CIDM]);
    }
    sort_clicks(g, &mut rows[..], sc);
    k8_put(&rows, dst, two, sc);
    sc.rows = rows;
}

#[cold]
#[inline(never)]
unsafe fn k8_rows_ix(g: &G, src: *const u64, n: usize, cids: *const u64, dst: *mut u64, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    pm2::EVC[2].fetch_add(1, Ordering::Relaxed);
    let mut rows = std::mem::take(&mut sc.rows);
    rows.clear();
    rows.reserve(n + 1040);
    for i in 0..n {
        let k = *src.add(i);
        let p = ((k & IDX_BITS) >> IDX_SH) as usize;
        rows.push([k & !IDX_BITS, *cids.add(2 * p) & CIDM]);
    }
    sort_clicks(g, &mut rows[..], sc);
    k8_put(&rows, dst, false, sc);
    sc.rows = rows;
}

#[inline(always)]
unsafe fn k8_hist(h: &mut std::mem::MaybeUninit<[u32; 2049]>, np: usize) -> *mut u32 {
    let p = h.as_mut_ptr() as *mut u32;
    std::ptr::write_bytes(p, 0, np + 1);
    p
}

#[inline(always)]
fn k8_dig(x: u64, base: u64, sh: u32, np: usize) -> usize {
    (((x >> 32).wrapping_sub(base)) >> sh) as usize & (np - 1)
}

unsafe fn k8_part(g: &G, src: *mut u64, m: usize, dst: *mut u64, tmp: *mut u64, base: u64, sh: u32, cids: *const u64, sc: &mut Scratch) {
    #[cfg(all(feature = "pmu", pm2fine))]
    { let c = if m <= 1 { 0 } else { (usize::BITS - (m - 1).leading_zeros()) as usize }.min(9); pm2::PSZ[c].fetch_add(1, Ordering::Relaxed); pm2::PEL[c].fetch_add(m as u64, Ordering::Relaxed); }
    #[cfg(feature = "pmu")]
    pm2::EVC[4].fetch_add(1, Ordering::Relaxed);
    if m <= 64 {
        let mut buf = std::mem::MaybeUninit::<[u64; 64]>::uninit();
        let bp = buf.as_mut_ptr() as *mut u64;
        #[cfg(all(feature = "pmu", pm2fine))]
        let y0 = pm2::rd();
        let r = ks_any::<false>(src, m, dst, bp);
        #[cfg(all(feature = "pmu", pm2fine))]
        { let y1 = pm2::rd(); pm2::add(25, y0, y1); }
        if r != 0 {
            k8_tie(g, bp, m, cids, dst, sc);
        }
        return;
    }
    #[cfg(feature = "pmu")]
    let y0 = pm2::rd();
    if sh == 0 {
        return k8_rows_ix(g, src, m, cids, dst, sc);
    }
    let nb = (usize::BITS - (m / MID_PER).leading_zeros()).clamp(1, 11).min(sh);
    let sh2 = sh - nb;
    let np = 1usize << nb;
    let mut hm = std::mem::MaybeUninit::<[u32; 2049]>::uninit();
    let h = k8_hist(&mut hm, np);
    for i in 0..m {
        *h.add(k8_dig(*src.add(i), base, sh2, np) + 1) += 1;
    }
    for d in 0..np {
        *h.add(d + 1) += *h.add(d);
    }
    let mut nm = std::mem::MaybeUninit::<[u32; 2049]>::uninit();
    let nx = nm.as_mut_ptr() as *mut u32;
    std::ptr::copy_nonoverlapping(h, nx, np);
    for i in 0..m {
        let x = *src.add(i);
        let d = k8_dig(x, base, sh2, np);
        let at = *nx.add(d);
        *nx.add(d) = at + 1;
        *tmp.add(at as usize) = x;
    }
    #[cfg(all(feature = "pmu", pm2fine))]
    { let y1 = pm2::rd(); pm2::add(24, y0, y1); }
    for d in 0..np {
        let (a, b) = (*h.add(d) as usize, *h.add(d + 1) as usize);
        if b > a {
            k8_part(g, tmp.add(a), b - a, dst.add(a), src.add(a), base, sh2, cids, sc);
        }
    }
}

#[inline(always)]
unsafe fn k8_range(rw: *const u64, n: usize) -> (u64, u64, u64) {
    let mut mn = _mm256_set1_epi64x(-1);
    let mut mx = _mm256_setzero_si256();
    let mut or = _mm256_setzero_si256();
    let mut i = 0;
    while i + 4 <= n {
        let x = _mm256_unpackhi_epi64(_mm256_loadu_si256(rw.add(2 * i) as *const __m256i), _mm256_loadu_si256(rw.add(2 * i + 4) as *const __m256i));
        mn = _mm256_min_epu64(mn, x);
        mx = _mm256_max_epu64(mx, x);
        or = _mm256_or_si256(or, x);
        i += 4;
    }
    let mut a = [0u64; 4];
    let mut b = [0u64; 4];
    let mut c = [0u64; 4];
    _mm256_storeu_si256(a.as_mut_ptr() as *mut __m256i, mn);
    _mm256_storeu_si256(b.as_mut_ptr() as *mut __m256i, mx);
    _mm256_storeu_si256(c.as_mut_ptr() as *mut __m256i, or);
    let mut lo = a[0].min(a[1]).min(a[2]).min(a[3]);
    let mut hi = b[0].max(b[1]).max(b[2]).max(b[3]);
    let mut o = c[0] | c[1] | c[2] | c[3];
    while i < n {
        let x = *rw.add(2 * i + 1);
        lo = lo.min(x);
        hi = hi.max(x);
        o |= x;
        i += 1;
    }
    (lo >> 32, hi >> 32, o)
}

#[inline(never)]
unsafe fn k8_mid(g: &G, rw: *const u64, n: usize, dst: *mut u64, sc: &mut Scratch) {
    let cids = rw;
    #[cfg(feature = "pmu")]
    pm2::EVC[5].fetch_add(1, Ordering::Relaxed);
    #[cfg(all(feature = "pmu", pm2fine))]
    let y0 = pm2::rd();
    let (lo, hi, any) = k8_range(rw, n);
    if any & IDX_BITS != 0 || hi == lo {
        return k8_rows(g, rw.add(1), rw, n, dst, false, sc);
    }
    let span = 64 - (hi - lo).leading_zeros();
    let bits = (usize::BITS - (n / MID_PER).leading_zeros()).clamp(1, 11).min(span);
    let sh = span - bits;
    let np = 1usize << bits;
    let mut hm = std::mem::MaybeUninit::<[u32; 2049]>::uninit();
    let h = k8_hist(&mut hm, np);
    let b64 = lo << 32;
    let s1 = 32 + sh;
    for i in 0..n {
        *h.add(((*rw.add(2 * i + 1) - b64) >> s1) as usize + 1) += 1;
    }
    for d in 0..np {
        *h.add(d + 1) += *h.add(d);
    }
    let mut nm = std::mem::MaybeUninit::<[u32; 2049]>::uninit();
    let nx = nm.as_mut_ptr() as *mut u32;
    std::ptr::copy_nonoverlapping(h, nx, np);
    let mut ka = std::mem::take(&mut sc.ka);
    ka.clear();
    ka.reserve(n + 64);
    let a = ka.as_mut_ptr();
    let mut pos = 0u64;
    for i in 0..n {
        let x = *rw.add(2 * i + 1) | pos;
        pos += 1 << IDX_SH;
        let d = ((x - b64) >> s1) as usize;
        let at = *nx.add(d);
        *nx.add(d) = at + 1;
        *a.add(at as usize) = x;
    }
    #[cfg(all(feature = "pmu", pm2fine))]
    { let y1 = pm2::rd(); pm2::add(23, y0, y1); }
    let mut buf = std::mem::MaybeUninit::<[u64; 64]>::uninit();
    let bp = buf.as_mut_ptr() as *mut u64;
    for d in 0..np {
        let (p, q) = (*h.add(d) as usize, *h.add(d + 1) as usize);
        let m = q - p;
        if m <= 64 {
            if m != 0 && ks_any::<false>(a.add(p), m, dst.add(p), bp) != 0 {
                k8_tie(g, bp, m, cids, dst.add(p), sc);
            }
        } else {
            k8_part(g, a.add(p), m, dst.add(p), dst.add(p), lo, sh, cids, sc);
        }
    }
    sc.ka = ka;
}

#[inline(never)]
unsafe fn k8_whale(g: &G, rw: *const u64, n: usize, k: *mut u64, sc: &mut Scratch) {
    #[cfg(feature = "pmu")]
    let y00 = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::EVC[6].fetch_add(n as u64, Ordering::Relaxed);
    let (lo, hi, any) = k8_range(rw, n);
    if any & IDX_BITS != 0 || n >= 1 << 24 {
        return k8_rows(g, rw.add(1), rw, n, k, false, sc);
    }
    let span = 64 - (hi - lo).leading_zeros();
    let passes = ((span + 10) / 11) as usize;
    let mut wv = std::mem::take(&mut sc.wk);
    let mut tv = std::mem::take(&mut sc.wc);
    let mut ov = std::mem::take(&mut sc.ka);
    for v in [&mut wv, &mut tv, &mut ov] {
        v.clear();
        v.reserve(n + 64);
    }
    let (mut w, mut t, o) = (wv.as_mut_ptr(), tv.as_mut_ptr(), ov.as_mut_ptr());
    let mut hv = std::mem::take(&mut sc.wh);
    hv.clear();
    hv.resize(3 * 2048, 0);
    let h = hv.as_mut_ptr();
    for i in 0..n {
        let x = ((*rw.add(2 * i + 1) >> 32) - lo) << 24 | i as u64;
        *w.add(i) = x;
        *h.add(((x >> 24) & 2047) as usize) += 1;
        *h.add(2048 + ((x >> 35) & 2047) as usize) += 1;
        *h.add(4096 + ((x >> 46) & 2047) as usize) += 1;
    }
    #[cfg(feature = "pmu")]
    let mut yw = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(18, y00, yw);
    for p in 0..passes {
        let hp = h.add(2048 * p);
        let mut at = 0u32;
        for d in 0..2048 {
            let c = *hp.add(d);
            *hp.add(d) = at;
            at += c;
        }
        let s = 24 + 11 * p as u32;
        for i in 0..n {
            let x = *w.add(i);
            let d = ((x >> s) & 2047) as usize;
            let a = *hp.add(d);
            *hp.add(d) = a + 1;
            *t.add(a as usize) = x;
        }
        std::mem::swap(&mut w, &mut t);
    }
    #[cfg(feature = "pmu")]
    { let x = pm2::rd(); pm2::add(19, yw, x); yw = x; }
    for i in 0..n {
        *o.add(i) = *rw.add(2 * (*w.add(i) & 0xFF_FFFF) as usize + 1);
    }
    #[cfg(feature = "pmu")]
    { let x = pm2::rd(); pm2::add(20, yw, x); yw = x; }
    let nb = n / 64 + 2;
    let mut eqv = std::mem::take(&mut sc.eqv);
    let mut tiev = std::mem::take(&mut sc.tiev);
    eqv.clear();
    eqv.resize(nb, 0);
    tiev.clear();
    tiev.resize(nb, 0);
    {
        let hi32 = _mm256_set1_epi64x(0xFFFF_FFFF_0000_0000u64 as i64);
        let cm = _mm256_set1_epi64x(0x3_FFF0);
        let mut last = _mm256_set1_epi64x(-1);
        let ep = eqv.as_mut_ptr();
        let tp = tiev.as_mut_ptr();
        let mut q = 0;
        while 4 * q + 4 <= n {
            let cur = _mm256_loadu_si256(o.add(4 * q) as *const __m256i);
            let x = _mm256_xor_si256(cur, _mm256_alignr_epi64::<3>(cur, last));
            let e = _mm256_testn_epi64_mask(x, hi32);
            let t = _mm256_mask_test_epi64_mask(e, x, cm);
            let (wd, sh) = (q / 16, 4 * (q % 16));
            *ep.add(wd) |= (e as u64) << sh;
            *tp.add(wd) |= (t as u64) << sh;
            last = cur;
            q += 1;
        }
        for j in (4 * q).max(1)..n {
            let x = *o.add(j) ^ *o.add(j - 1);
            if x >> 32 == 0 {
                *ep.add(j / 64) |= 1 << (j % 64);
                if x & 0x3_FFF0 != 0 {
                    *tp.add(j / 64) |= 1 << (j % 64);
                }
            }
        }
        *ep &= !1;
        *tp &= !1;
    }
    #[cfg(feature = "pmu")]
    { let x = pm2::rd(); pm2::add(23, yw, x); yw = x; }
    let eqb = |j: usize| -> bool { j < n && (*eqv.as_ptr().add(j / 64) >> (j % 64)) & 1 != 0 };
    let mut next = 1usize;
    for wd in 0..nb {
        let mut tb = *tiev.as_ptr().add(wd);
        while tb != 0 {
            let i = wd * 64 + tb.trailing_zeros() as usize;
            tb &= tb - 1;
            if i < next {
                continue;
            }
            if !eqb(i - 1) && !eqb(i + 1) {
                let a = i - 1;
                let (x0, x1) = (*o.add(a), *o.add(i));
                let c0 = *rw.add(2 * (*w.add(a) & 0xFF_FFFF) as usize) & CIDM;
                let c1 = *rw.add(2 * (*w.add(i) & 0xFF_FFFF) as usize) & CIDM;
                if (c0 | c1) & RAW == 0 {
                    let (e0, e1) = (e36(c0), e36(c1));
                    if e0 < e1 || (e0 == e1 && x0 > x1) {
                        *o.add(a) = x1;
                        *o.add(i) = x0;
                    }
                    next = i + 2;
                    #[cfg(feature = "pmu")]
                    pm2::PSZ[0].fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            let mut a = i - 1;
            while a > 0 && eqb(a) {
                a -= 1;
            }
            let mut z = i + 1;
            while eqb(z) {
                z += 1;
            }
            #[cfg(feature = "pmu")]
            { pm2::PSZ[1].fetch_add(1, Ordering::Relaxed); pm2::PEL[1].fetch_add((z - a) as u64, Ordering::Relaxed); }
            let mut rows = std::mem::take(&mut sc.rows);
            rows.clear();
            for j in a..z {
                rows.push([*o.add(j), *rw.add(2 * (*w.add(j) & 0xFF_FFFF) as usize) & CIDM]);
            }
            let canon = rows.iter().all(|r| r[1] & RAW == 0);
            if canon && rows.len() <= 64 {
                let mut em = std::mem::MaybeUninit::<[u64; 64]>::uninit();
                let e = &mut *em.as_mut_ptr();
                for (j, r) in rows.iter().enumerate() {
                    e[j] = e36(r[1]);
                }
                for x in 1..rows.len() {
                    let (ex, rx) = (e[x], rows[x]);
                    let mut y = x;
                    while y > 0 && (e[y - 1] < ex || (e[y - 1] == ex && rows[y - 1][0] > rx[0])) {
                        e[y] = e[y - 1];
                        rows[y] = rows[y - 1];
                        y -= 1;
                    }
                    e[y] = ex;
                    rows[y] = rx;
                }
                for (j, r) in rows.iter().enumerate() {
                    *o.add(a + j) = r[0];
                }
                sc.rows = rows;
            } else {
                #[cfg(feature = "pmu")]
                { pm2::PSZ[9].fetch_add(1, Ordering::Relaxed); pm2::PEL[9].fetch_add(rows.len() as u64, Ordering::Relaxed); }
                fix_ties(g, &mut rows);
                k8_put2(&rows, o.add(a), k.add(a), false, sc);
                sc.rows = rows;
            }
            next = z + 1;
        }
    }
    #[cfg(feature = "pmu")]
    { let tb: u64 = tiev.iter().map(|x| x.count_ones() as u64).sum(); let eb: u64 = eqv.iter().map(|x| x.count_ones() as u64).sum(); pm2::PEL[2].fetch_add(tb, Ordering::Relaxed); pm2::PEL[3].fetch_add(eb, Ordering::Relaxed); }
    sc.eqv = eqv;
    sc.tiev = tiev;
    #[cfg(feature = "pmu")]
    { let x = pm2::rd(); pm2::add(22, yw, x); }
    std::ptr::copy_nonoverlapping(o, k, n);
    sc.wk = wv;
    sc.wc = tv;
    sc.ka = ov;
    sc.wh = hv;
}

#[cold]
#[inline(never)]
unsafe fn k8_fix(g: &G, r: u32, buf: *const u64, n: usize, rw: *const u64, dst: *mut u64, sc: &mut Scratch) {
    if r == 1 {
        k8_tie(g, buf, n, rw, dst, sc);
    } else {
        k8_rows(g, rw.add(1), rw, n, dst, false, sc);
    }
}

#[inline(never)]
unsafe fn link_recs(up: *mut u16, pp: *mut (u32, u16), lkp: *const u64, nl: usize) -> usize {
    static AB: [u16; 256] = {
        let mut t = [0u16; 256];
        let mut i = 0;
        while i < 256 {
            t[i] = (1u16 << (i >> 4)) | (1u16 << (i & 15));
            i += 1;
        }
        t
    };
    let mut np = 0usize;
    for j in 0..nl {
        let w0 = *lkp.add(2 * j);
        let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
        let bits = *AB.get_unchecked((w0 & 255) as usize) as u32;
        let u = *up.add(ul) as u32;
        let m = u & MASK as u32;
        let ok = (m & bits != 0) as u32 | (m == 0) as u32;
        *up.add(ul) = (u | (bits & 0u32.wrapping_sub(ok))) as u16;
        *pp.add(np) = (ul as u32, bits as u16);
        np += (ok ^ 1) as usize;
    }
    np
}

#[inline(always)]
unsafe fn scatter16(recs: &[Rec], pos: *mut u32, rp: *mut u8) {
    let mut p = recs.as_ptr();
    let e = p.add(recs.len());
    while p < e {
        let w0 = (*p).w0;
        let pp = pos.add(_pext_u64(w0, CNT_BITS) as usize);
        let at = *pp;
        *pp = at + 16;
        _mm_storeu_si128(rp.add(at as usize) as *mut __m128i, _mm_loadu_si128(p as *const __m128i));
        p = p.add(1);
    }
}

#[inline(never)]
unsafe fn k8_inplace(g: &G, sc: &mut Scratch, utab: &[u16], act: &[u64], clp: *const u8, rsp: *const u32, stp: *const u32, rp: *mut u64, kb: *mut u64) {
    let mut buf = [0u64; 64];
    let bp = buf.as_mut_ptr();
    for (w, &m) in act.iter().enumerate() {
        let mut m = m;
        while m != 0 {
            let ul = w * 64 + m.trailing_zeros() as usize;
            m &= m - 1;
            let r0 = *rsp.add(ul) as usize;
            let n = *rsp.add(ul + 1) as usize - r0;
            let d = kb.add(*stp.add(ul) as usize);
            let src = rp.add(2 * r0);
            let c = *clp.add(ul);
            let r = match c {
                0 => {
                    *d = *src.add(1);
                    0
                }
                1 => ks0::<true>(src, n, d, bp),
                2 => ks1::<true>(src, n, d, bp),
                3 => ks2::<true>(src, n, d, bp),
                4 => ks4::<true>(src, n, d, bp),
                5 => ks8::<true>(src, n, d, bp),
                6 => ks16::<true>(src, n, d, bp),
                _ => {
                    let mut rb = std::mem::take(&mut sc.rowbuf);
                    rb.clear();
                    rb.reserve(2 * n + 192);
                    std::ptr::copy_nonoverlapping(src, rb.as_mut_ptr(), 2 * n);
                    let s = rb.as_ptr();
                    if c == 7 {
                        k8_mid(g, s, n, d, sc);
                    } else if utab[ul] & COMPLEX != 0 {
                        k8_rows(g, s.add(1), s, n, d, true, sc);
                    } else {
                        k8_whale(g, s, n, d, sc);
                    }
                    sc.rowbuf = rb;
                    0
                }
            };
            if r != 0 {
                k8_fix(g, r, bp, n, src, d, sc);
            }
        }
    }
}

const K8_ROWS_SEG: usize = env_u32(option_env!("XL_K8_ROWS_SEG"), 1 << 15) as usize;

#[inline(always)]
unsafe fn count8(recs: &[Rec], cnt: *mut u32) {
    let n = recs.len();
    let p = recs.as_ptr();
    let nextc = if n == crate::parse::LINK_OFF / 16 { *((p as usize + crate::parse::LINK_OFF) as *const usize) as *const i8 } else { std::ptr::null() };
    _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(crate::parse::LINK_OFF));
    let mut i = 0usize;
    while i + 4 <= n {
        _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(i * 16));
        *cnt.add(_pext_u64((*p.add(i)).w0, CNT_BITS) as usize) += 1;
        *cnt.add(_pext_u64((*p.add(i + 1)).w0, CNT_BITS) as usize) += 1;
        *cnt.add(_pext_u64((*p.add(i + 2)).w0, CNT_BITS) as usize) += 1;
        *cnt.add(_pext_u64((*p.add(i + 3)).w0, CNT_BITS) as usize) += 1;
        i += 4;
    }
    while i < n {
        *cnt.add(_pext_u64((*p.add(i)).w0, CNT_BITS) as usize) += 1;
        i += 1;
    }
}

fn bucket8(g: &G, t: &mut TState, sc: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    if ub == unsafe { PLAN.as_ref().unwrap_unchecked() }.whale {
        sc.whale = true;
    }
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    let total = pbase(ub + 1) - pbase(ub);
    let mut cnt = std::mem::take(&mut t.cnt);
    cnt.clear();
    cnt.resize(2 * UBSIZE, 0);
    t.pend.clear();
    let cp = cnt.as_mut_ptr();
    let mut seen = 0usize;
    #[cfg(feature = "pmu")]
    let pa = pm2::rd();
    for th in 0..g.nt {
        g.t(th).us.for_each(ub, |recs| {
            seen += recs.len();
            unsafe { count8(recs, cp) };
        });
    }
    if seen != total {
        fallback("click partition count");
    }
    #[cfg(feature = "pmu")]
    let pb = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(0, pa, pb);
    let mut rs = std::mem::take(&mut sc.rs);
    rs.clear();
    rs.reserve(UBSIZE + 16);
    let (mut nck, mut base8, mut kb, mut rp) = (0usize, 0usize, std::ptr::null_mut::<u64>(), std::ptr::null_mut::<u64>());
    let mut co = 0usize;
    if total != 0 {
        if (total + 1) * 16 >= u32::MAX as usize {
            fallback("CSR capacity");
        }
        unsafe {
            let rsp = rs.as_mut_ptr();
            let z0 = _mm256_setzero_si256();
            let mut carry = z0;
            let mut j = 0usize;
            while j < 2 * UBSIZE {
                let c = _mm256_loadu_si256(cp.add(j) as *const __m256i);
                let mut x = _mm256_add_epi32(c, _mm256_alignr_epi32::<7>(c, z0));
                x = _mm256_add_epi32(x, _mm256_alignr_epi32::<6>(x, z0));
                x = _mm256_add_epi32(x, _mm256_alignr_epi32::<4>(x, z0));
                let incl = _mm256_add_epi32(x, carry);
                let excl = _mm256_sub_epi32(incl, c);
                _mm256_storeu_si256(cp.add(j) as *mut __m256i, _mm256_slli_epi32::<4>(excl));
                if j < UBSIZE {
                    _mm256_storeu_si256(rsp.add(j) as *mut __m256i, excl);
                }
                carry = _mm256_permutexvar_epi32(_mm256_set1_epi32(7), incl);
                j += 8;
                if j == UBSIZE {
                    nck = _mm256_extract_epi32::<7>(carry) as u32 as usize;
                }
            }
            if _mm256_extract_epi32::<7>(carry) as u32 as usize != total {
                fallback("click partition count");
            }
            *rsp.add(UBSIZE) = nck as u32;
            rs.set_len(UBSIZE + 1);
        }
        if total > K8_ROWS_SEG {
            co = 0;
            base8 = c8_seg(sc, 2 * (total + 1) + 192);
            kb = (C8_BASE.load(Ordering::Relaxed) as *mut u64).wrapping_add(base8);
            rp = kb;
        } else {
            co = 2 * nck + 64;
            base8 = c8_seg(sc, co);
            kb = (C8_BASE.load(Ordering::Relaxed) as *mut u64).wrapping_add(base8);
            sc.rowbuf.clear();
            sc.rowbuf.reserve(2 * (total + 1) + 192);
            rp = sc.rowbuf.as_mut_ptr();
        }
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| unsafe { scatter16(recs, cp, rp as *mut u8) });
        }
    }
    let nl = total - nck;
    #[cfg(feature = "pmu")]
    let pb1 = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(3, pb, pb1);
    let lkp = rp.wrapping_add(2 * nck) as *const u64;
    unsafe {
        t.pend.reserve(nl);
        let np = link_recs(utab.as_mut_ptr(), t.pend.as_mut_ptr(), lkp, nl);
        t.pend.set_len(np);
    }
    #[cfg(feature = "pmu")]
    let pl = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(14, pb1, pl);
    let dbg1 = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += dbg1;
    let cx0 = t.cx_users.len();
    if links_done(t, utab, u0) {
        for j in 0..nl {
            let w0 = unsafe { *lkp.add(2 * j) };
            let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
            if utab[ul] & COMPLEX != 0 {
                t.cx_edges.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
            }
        }
    }
    #[cfg(feature = "pmu")]
    let pb2 = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(15, pl, pb2);
    #[cfg(feature = "pmu")]
    pm2::add(1, pb1, pb2);
    if total != 0 {
        let ncx: usize = t.cx_users[cx0..].iter().map(|&u| (rs[u as usize - u0 + 1] - rs[u as usize - u0]) as usize).sum();
        let nw = nck + ncx;
        let inplace = rp == kb;
        if nw > 2 * nck || (!inplace && nw + 64 > co) {
            fallback("CSR words");
        }
        let off = off_of(g, ub);
        let mut st = std::mem::take(&mut sc.st);
        st.clear();
        st.reserve(UBSIZE + 16);
        let mut cls = std::mem::take(&mut sc.cls);
        cls.clear();
        cls.reserve(UBSIZE + 64);
        let mut act = [0u64; UBSIZE / 64];
        unsafe {
            let stp = st.as_mut_ptr();
            let clp = cls.as_mut_ptr();
            let up = utab.as_ptr();
            let offp = off.as_mut_ptr();
            let rsp = rs.as_ptr();
            let z0 = _mm256_setzero_si256();
            let b8 = _mm256_set1_epi32(base8 as i32);
            let one = _mm256_set1_epi32(1);
            let mut carry = z0;
            let mut ul = 0usize;
            while ul < UBSIZE {
                let c = _mm256_sub_epi32(_mm256_loadu_si256(rsp.add(ul + 1) as *const __m256i), _mm256_loadu_si256(rsp.add(ul) as *const __m256i));
                let two = _mm256_srli_epi32::<15>(_mm256_cvtepu16_epi32(_mm_loadu_si128(up.add(ul) as *const __m128i)));
                let w = _mm256_sllv_epi32(c, two);
                let mut x = _mm256_add_epi32(w, _mm256_alignr_epi32::<7>(w, z0));
                x = _mm256_add_epi32(x, _mm256_alignr_epi32::<6>(x, z0));
                x = _mm256_add_epi32(x, _mm256_alignr_epi32::<4>(x, z0));
                let incl = _mm256_add_epi32(x, carry);
                let excl = _mm256_sub_epi32(incl, w);
                _mm256_storeu_si256(stp.add(ul) as *mut __m256i, excl);
                _mm256_storeu_si256(offp.add(ul) as *mut __m256i, _mm256_add_epi32(excl, b8));
                carry = _mm256_permutexvar_epi32(_mm256_set1_epi32(7), incl);
                let lz = _mm256_lzcnt_epi32(_mm256_sub_epi32(c, one));
                let k = _mm256_min_epu32(_mm256_sub_epi32(_mm256_set1_epi32(32), lz), _mm256_set1_epi32(7));
                let k = _mm256_mask_mov_epi32(k, _mm256_cmpgt_epu32_mask(c, _mm256_set1_epi32(MID_MAX as i32)), _mm256_set1_epi32(8));
                let isx = _mm256_test_epi32_mask(two, two);
                let k = _mm256_mask_mov_epi32(k, isx, _mm256_set1_epi32(8));
                let a = _mm256_cmpneq_epi32_mask(c, z0);
                _mm_storel_epi64(clp.add(ul) as *mut __m128i, _mm256_cvtepi32_epi8(k));
                *act.get_unchecked_mut(ul / 64) |= (a as u64) << (ul % 64);
                ul += 8;
            }
            let tot = _mm256_extract_epi32::<7>(carry) as u32 as usize;
            if tot != nw {
                fallback("click partition count");
            }
            *stp.add(UBSIZE) = nw as u32;
            *offp.add(UBSIZE) = (base8 + nw) as u32;
            st.set_len(UBSIZE + 1);
            cls.set_len(UBSIZE);
        }
        #[cfg(feature = "pmu")]
        let pc = pm2::rd();
        #[cfg(feature = "pmu")]
        {
            pm2::add(2, pb2, pc);
            pm2::NCK.fetch_add(nck as u64, Ordering::Relaxed);
            pm2::NREC.fetch_add(total as u64, Ordering::Relaxed);
        }
        let dbg2 = tstart.elapsed().as_nanos() as u64;
        t.prof[6] += dbg2;
        #[cfg(feature = "pmu")]
        let pd = pm2::rd();
        if inplace {
            unsafe { k8_inplace(g, sc, utab, &act, cls.as_ptr(), rs.as_ptr(), st.as_ptr(), rp, kb) };
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(26, pd, x); }
        } else { unsafe {
            let mut ll = std::mem::take(&mut sc.ll);
            ll.clear();
            ll.reserve(9 * UBSIZE);
            let lp = ll.as_mut_ptr();
            let mut lc = [0usize; 9];
            let clp = cls.as_ptr();
            for (w, &m) in act.iter().enumerate() {
                let mut m = m;
                while m != 0 {
                    let ul = w * 64 + m.trailing_zeros() as usize;
                    m &= m - 1;
                    let c = *clp.add(ul) as usize;
                    let i = lc.get_unchecked_mut(c);
                    *lp.add(c * UBSIZE + *i) = ul as u32;
                    *i += 1;
                }
            }
            let stp = st.as_ptr();
            let rsp = rs.as_ptr();
            let mut buf = [0u64; 64];
            let _ = &mut buf;
            #[cfg(feature = "pmu")]
            let mut pq = pm2::rd();
            #[cfg(feature = "pmu")]
            pm2::add(4, pd, pq);
            macro_rules! k8_class {
                ($f:ident, $c:expr) => {{
                    let l = lp.add($c * UBSIZE);
                    let m = lc[$c];
                    for ph in 0..SORT_STRIDE {
                        let mut i = ph;
                        while i < m {
                            let ul = *l.add(i) as usize;
                            let r0 = *rsp.add(ul) as usize;
                            let n = *rsp.add(ul + 1) as usize - r0;
                            let d = kb.add(*stp.add(ul) as usize);
                            let r = $f::<true>(rp.add(2 * r0), n, d, buf.as_mut_ptr());
                            if r != 0 {
                                k8_fix(g, r, buf.as_ptr(), n, rp.add(2 * r0), d, sc);
                            }
                            i += SORT_STRIDE;
                        }
                    }
                }};
            }
            for i in 0..lc[0] {
                let ul = *lp.add(i) as usize;
                *kb.add(*stp.add(ul) as usize) = *rp.add(2 * *rsp.add(ul) as usize + 1);
            }
            k8_class!(ks0, 1);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(5, pq, x); pq = x; }
            k8_class!(ks1, 2);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(6, pq, x); pq = x; }
            k8_class!(ks2, 3);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(7, pq, x); pq = x; }
            k8_class!(ks4, 4);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(8, pq, x); pq = x; }
            k8_class!(ks8, 5);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(9, pq, x); pq = x; }
            k8_class!(ks16, 6);
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(10, pq, x); pq = x; }
            for i in 0..lc[7] {
                let ul = *lp.add(7 * UBSIZE + i) as usize;
                let r0 = *rsp.add(ul) as usize;
                let n = *rsp.add(ul + 1) as usize - r0;
                k8_mid(g, rp.add(2 * r0), n, kb.add(*stp.add(ul) as usize), sc);
            }
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(11, pq, x); pq = x; }
            for i in 0..lc[8] {
                let ul = *lp.add(8 * UBSIZE + i) as usize;
                let r0 = *rsp.add(ul) as usize;
                let n = *rsp.add(ul + 1) as usize - r0;
                let d = kb.add(*stp.add(ul) as usize);
                let two = utab[ul] & COMPLEX != 0;
                #[cfg(feature = "pmu")]
                let y0 = pm2::rd();
                if two {
                    k8_rows(g, rp.add(2 * r0 + 1), rp.add(2 * r0), n, d, true, sc);
                } else {
                    k8_whale(g, rp.add(2 * r0), n, d, sc);
                }
                #[cfg(feature = "pmu")]
                { let y1 = pm2::rd(); pm2::add(if two { 16 } else { 17 }, y0, y1); }
            }
            #[cfg(feature = "pmu")]
            { let x = pm2::rd(); pm2::add(12, pq, x); }
            sc.ll = ll;
        } }
        #[cfg(feature = "pmu")]
        { let x = pm2::rd(); pm2::add(13, pa, x); }
        if !sc.d8.is_empty() {
            DEFERRED8.lock().unwrap().extend(sc.d8.drain(..));
        }
        sc.seg.0 = base8 + ((nw + 1) & !1);
        sc.st = st;
        sc.cls = cls;
    } else {
        t.prof[6] += dbg1;
    }
    sc.rs = rs;
    t.cnt = cnt;
    let el = tstart.elapsed().as_nanos() as u64;
    t.prof[3] += el;
    t.prof[4] = t.prof[4].max(el);
}

#[inline]
fn bucket(g: &G, t: &mut TState, sc: &mut Scratch, ub: usize) {
    if K8ON.load(Ordering::Relaxed) {
        bucket8(g, t, sc, ub)
    } else {
        bucket16(g, t, sc, ub)
    }
}

fn bucket16(g: &G, t: &mut TState, sc: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    if ub == unsafe { PLAN.as_ref().unwrap_unchecked() }.whale {
        sc.whale = true;
    }
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
    #[cfg(feature = "pmu")]
    let pa = pm2::rd();
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
    #[cfg(feature = "pmu")]
    let pb = pm2::rd();
    #[cfg(feature = "pmu")]
    pm2::add(0, pa, pb);
    let lks = unsafe { std::slice::from_raw_parts(lk.as_ptr(), nl) };
    for &w0 in lks {
        link_rec(t, utab, w0);
    }
    let dbg1 = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += dbg1;
    let cx0 = t.cx_users.len();
    if links_done(t, utab, u0) {
        for &w0 in lks {
            let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
            if utab[ul] & COMPLEX != 0 {
                t.cx_edges.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
            }
        }
    }
    if total != 0 {
        let cxl: Vec<u32> = t.cx_users[cx0..].iter().map(|&u| u - u0 as u32).collect();
        let mut soff = std::mem::take(&mut sc.soff);
        soff.clear();
        soff.resize(UBSIZE + 1, 0);
        let mut at = 0u32;
        for ul in 0..UBSIZE {
            soff[ul] = at;
            at += cnt[ul];
            cnt[ul] = soff[ul];
        }
        soff[UBSIZE] = at;
        let nck = at as usize;
        if nck + nl != total {
            fallback("click partition count");
        }
        let ncx: usize = cxl.iter().map(|&ul| (soff[ul as usize + 1] - soff[ul as usize]) as usize).sum();
        let off = off_of(g, ub);
        let base8 = c8_seg(sc, 2 * (nck + 1 + 32));
        {
            let mut d = base8 as u32;
            let mut k = 0usize;
            for ul in 0..UBSIZE {
                off[ul] = d;
                let n = soff[ul + 1] - soff[ul];
                let two = k < cxl.len() && cxl[k] as usize == ul;
                k += two as usize;
                d += n << two as u32;
            }
            off[UBSIZE] = d;
        }
        cnt[UBSIZE..].fill(at);
        #[cfg(feature = "pmu")]
        let pc = pm2::rd();
        #[cfg(feature = "pmu")]
        { pm2::add(1, pb, pc); pm2::NCK.fetch_add(nck as u64, Ordering::Relaxed); pm2::NREC.fetch_add(total as u64, Ordering::Relaxed); for ul in 0..UBSIZE { let n = (soff[ul + 1] - soff[ul]) as usize; if n > 0 { pm2::hist(n); } } }
        #[cfg(feature = "pmu")]
        let pc = pm2::rd();
        let rp = (C8_BASE.load(Ordering::Relaxed) as *mut u64).wrapping_add(base8) as *mut [u64; 2];
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| unsafe { scatter_chunk(recs, cp, rp) });
        }
        let dbg2 = tstart.elapsed().as_nanos() as u64;
        t.prof[6] += dbg2;
        #[cfg(feature = "pmu")]
        let pd = pm2::rd();
        #[cfg(feature = "pmu")]
        pm2::add(2, pc, pd);
        unsafe {
            sort_users(g, soff.as_ptr(), rp, 0, UBSIZE, sc);
            #[cfg(feature = "pmu")]
            let pe = pm2::rd();
            #[cfg(feature = "pmu")]
            pm2::add(3, pd, pe);
            defer8(utab, soff.as_ptr(), off.as_ptr(), rp);
            rows_out(&cxl, soff.as_ptr(), off.as_ptr(), rp, 0, UBSIZE);
            #[cfg(feature = "pmu")]
            { let pf = pm2::rd(); pm2::add(4, pe, pf); pm2::add(5, pa, pf); }
        }
        sc.seg.0 = base8 + ((nck + ncx + 1) & !1);
        sc.soff = soff;
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
    let cx0 = t.cx_users.len();
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
    let cxl = unsafe { &mut *h.cxl.get() };
    cxl.clear();
    cxl.extend(t.cx_users[cx0..].iter().map(|&u| u - u0 as u32));
    let soff = unsafe { &mut *h.soff.get() };
    soff.clear();
    soff.resize(UBSIZE + 1, 0);
    let mut at = 0u32;
    for ul in 0..UBSIZE {
        soff[ul] = at;
        for th in 0..g.nt {
            at += unsafe { &*h.cnt[th].get() }[ul];
        }
    }
    soff[UBSIZE] = at;
    let ncx: usize = cxl.iter().map(|&ul| (soff[ul as usize + 1] - soff[ul as usize]) as usize).sum();
    let off = off_of(g, ub);
    let mut d = c8_take(at as usize + ncx) as u32;
    let mut k = 0usize;
    for ul in 0..UBSIZE {
        off[ul] = d;
        let n = soff[ul + 1] - soff[ul];
        let two = k < cxl.len() && cxl[k] as usize == ul;
        k += two as usize;
        d += n << two as u32;
    }
    off[UBSIZE] = d;
}

fn heavy_scatter(g: &G, t: &mut TState, h: &Heavy, th: usize) {
    let mut pos = std::mem::take(&mut t.cnt);
    pos.clear();
    pos.resize(2 * UBSIZE, 0);
    let mut at = 0u32;
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
    if at as usize + links != total || unsafe { &*h.scr.get() }.capacity() < total {
        fallback("click partition count");
    }
    pos[UBSIZE..].fill(spare as u32);
    let pp = pos.as_mut_ptr();
    let rp = unsafe { (*h.scr.get()).as_mut_ptr() };
    g.t(th).us.for_each(h.ub, |recs| unsafe { scatter_chunk(recs, pp, rp) });
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
                unsafe {
                    let soff = (*h.soff.get()).as_ptr();
                    let off8 = off_of(g, h.ub).as_ptr();
                    let rp = (*h.scr.get()).as_mut_ptr();
                    sort_users(g, soff, rp, a, b, sc);
                    let cxl = &*h.cxl.get();
                    let lo = cxl.partition_point(|&x| (x as usize) < a);
                    let hi = cxl.partition_point(|&x| (x as usize) < b);
                    defer8(utab_of(g, h.ub), soff, off8, rp);
                    rows_out(&cxl[lo..hi], soff, off8, rp, a, b);
                }
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
        unsafe { sort_users(g, off.as_ptr(), g.csr, 0, UBSIZE, sc) };
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
                unsafe { sort_users(g, off_of(g, h.ub).as_ptr(), g.csr, a, b, sc) };
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
        unsafe { sort_users(g, off.as_ptr(), g.csr, 0, UBSIZE, sc) };
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
                unsafe { sort_users(g, off_of(g, h.ub).as_ptr(), g.csr, a, b, sc) };
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
    #[cfg(feature = "pmu")]
    pm2::print();
    odd_events(g);
    DEFER_RAW.store(false, Ordering::SeqCst);
    for (p, n) in std::mem::take(&mut *DEFERRED.lock().unwrap()) {
        let grp = unsafe { std::slice::from_raw_parts_mut(p as *mut [u64; 2], n) };
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
    }
    if MODE8.load(Ordering::SeqCst) {
        g.csr = C8_BASE.load(Ordering::SeqCst) as *mut [u64; 2];
    }
    let d8 = std::mem::take(&mut *DEFERRED8.lock().unwrap());
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] C8 deferred tie groups {}, two-word {}", d8.len(), d8.iter().filter(|x| x.1).count());
    }
    for (at, two, mut grp) in d8 {
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
        let c8 = g.csr as *mut u64;
        for (k, r) in grp.iter().enumerate() {
            unsafe {
                if two {
                    *c8.add(at + 2 * k) = r[0];
                    *c8.add(at + 2 * k + 1) = r[1];
                } else {
                    *c8.add(at + k) = r[0];
                }
            }
        }
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
        convert16(g);
    } else if g.ccap != 0 {
        let mut p = unsafe { PLANX.take().unwrap() };
        for f in &p.fixes {
            fix_odd(g, f);
        }
        g.ncsr = p.bounds[g.nub];
        g.csr_base = std::mem::take(&mut p.bounds);
        cx = p.heavy.iter_mut().map(|h| std::mem::take(h.cx.get_mut())).collect();
        convert16(g);
    } else {
        let mut p = unsafe { PLAN.take().unwrap() };
        g.ncsr = C8_NEXT.load(Ordering::SeqCst) + 64;
        if DBG.load(Ordering::Relaxed) {
            extern "C" {
                fn mincore(a: *mut u8, len: usize, vec: *mut u8) -> i32;
            }
            let len = (g.ncsr * 8 + 4095) & !4095;
            let mut v = vec![0u8; len / 4096];
            let r = unsafe { mincore(g.csr as *mut u8, len, v.as_mut_ptr()) };
            let res = v.iter().filter(|&&x| x & 1 != 0).count();
            eprintln!("[dbg] C8: {} words of segments, mincore {} resident {} kB", g.ncsr, r, res * 4);
        }
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
    MODE8.store(g.ccap == 0, Ordering::SeqCst);
    C8_NEXT.store(0, Ordering::SeqCst);
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
    if g.clen > 0xFFFF_FFFF {
        fallback("click window");
    }
    K8ON.store(!std::env::var("XL_K8").is_ok_and(|v| v == "0"), Ordering::SeqCst);
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
    let seg = (2 * at / g.nt).max(1 << 20);
    let cap = 8 * at + (4 * g.nt + 8) * (seg + C8_PAGE) + 4096;
    if cap >= u32::MAX as usize {
        fallback("CSR capacity");
    }
    C8_BASE.store(crate::sys::anon(cap * 8) as usize, Ordering::SeqCst);
    C8_CAP.store(cap, Ordering::SeqCst);
    C8_SEG.store(seg, Ordering::SeqCst);
    let total = at;
    let nt = g.nt;
    let mut plan = Plan { heavy: Vec::new(), normal: Vec::new(), whale: usize::MAX, nres: 0, rnext: AtomicUsize::new(0), base, next: AtomicUsize::new(0), fin: AtomicUsize::new(0) };
    for ub in 0..g.nub {
        let n = plan.base[ub + 1] - plan.base[ub];
        if n > (total / HEAVY_DIV).max(1 << 17) {
            plan.heavy.push(Heavy {
                ub,
                cnt: (0..nt).map(|_| z::nv(UnsafeCell::new(Vec::new()))).collect(),
                links: (0..nt).map(|_| z::nv(UnsafeCell::new(Vec::new()))).collect(),
                cx: UnsafeCell::new(Vec::new()),
                scr: UnsafeCell::new(Vec::with_capacity(n + 32)),
                soff: UnsafeCell::new(Vec::new()),
                cxl: UnsafeCell::new(Vec::new()),
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
    if let Some((k, &wb)) = plan.normal.iter().enumerate().max_by_key(|&(_, &ub)| plan.base[ub as usize + 1] - plan.base[ub as usize]) {
        let nw = plan.base[wb as usize + 1] - plan.base[wb as usize];
        let on = !std::env::var("XL_C8RES").is_ok_and(|v| v == "0");
        if on && plan.normal.len() > 8 && nw >= 1 << 16 && nw > 8 * total / plan.normal.len() {
            plan.normal.remove(k);
            plan.normal.insert(0, wb);
            plan.whale = wb as usize;
            let room = nw - nw / 8;
            let mut big = 0usize;
            let mut sum = 0usize;
            let mut r = 0usize;
            while r + 1 < plan.normal.len() {
                let ub = plan.normal[plan.normal.len() - 1 - r] as usize;
                let n = plan.base[ub + 1] - plan.base[ub];
                if sum + n + 2 * big.max(n) + 256 > room {
                    break;
                }
                sum += n;
                big = big.max(n);
                r += 1;
            }
            plan.nres = r;
        }
    }
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] heavy buckets {:?} total {total}, whale {} kept {}", plan.heavy.iter().map(|h| (h.ub, plan.base[h.ub + 1] - plan.base[h.ub])).collect::<Vec<_>>(), plan.whale as isize, plan.nres);
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
    let nq = nh * nt + p.normal.len() - p.nres;
    if sc.whale && p.rnext.load(Ordering::Relaxed) < p.nres {
        let r = p.rnext.fetch_add(1, Ordering::Relaxed);
        if r < p.nres {
            bucket(g, t, sc, p.normal[p.normal.len() - 1 - r] as usize);
            p.fin.fetch_add(1, Ordering::SeqCst);
            return true;
        }
    }
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

unsafe fn k8_user(g: &G, k: *mut u64, cids: *const u64, n: usize, sc: &mut Scratch) {
    let mut buf = [0u64; 64];
    let mut rows: Vec<u64> = Vec::with_capacity(2 * n + 256);
    for i in 0..n {
        rows.push(*cids.add(i));
        rows.push(*k.add(i));
    }
    rows.resize(2 * n + 256, 0);
    let rw = rows.as_ptr();
    if n < 2 {
        if n == 1 && *k & IDX_BITS != 0 {
            k8_rows(g, rw.add(1), rw, n, k, false, sc);
        }
    } else if n <= 64 {
        let r = ks_any::<true>(rw, n, k, buf.as_mut_ptr());
        if r != 0 {
            k8_fix(g, r, buf.as_ptr(), n, rw, k, sc);
        }
    } else if n <= MID_MAX {
        k8_mid(g, rw, n, k, sc);
    } else {
        k8_whale(g, rw, n, k, sc);
    }
}

pub fn k8_bench(g: &G) {
    #[cfg(feature = "pmu")]
    if std::env::var("XL_PROBE").is_ok() {
        let mut a = [1u64; 80];
        let mut b = [0u64; 80];
        unsafe {
            let p = std::hint::black_box(a.as_mut_ptr());
            let q = std::hint::black_box(b.as_mut_ptr());
            eprintln!("{} {} {} {}", probe_ks1(p, 3, p, q), probe_ks2(p, 6, p, q), probe_ks4(p, 12, p, q), probe_ks16(p, 50, p, q));
        }
    }
    let mut sc = Scratch::default();
    let mut x = 0x1234_5678_9abc_def0u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let sizes: Vec<usize> = (1..=70).chain([90, 100, 127, 128, 129, 200, 255, 256, 257, 300, 400, 511, 512, 513, 600, 1500, 2047, 2048, 2049, 5000, 16383, 16384, 20000, 70000]).collect();
    let mut nchk = 0usize;
    for &n in &sizes {
        let checks = if n <= 64 { 3000 } else if n <= 5000 { 200 } else { 20 };
        for rep in 0..checks {
            let tr = match rep % 5 {
                0 => 4,
                1 => 1000,
                2 => n as u64,
                3 => 3,
                _ => 1_800_000_000,
            };
            let cr = if rep % 3 == 0 { 2 } else { 2000 };
            let mut keys: Vec<u64> = (0..n).map(|_| (rnd() % tr) << 32 | (rnd() % cr) << 4 | (rnd() % 14)).collect();
            if rep % 97 == 96 {
                let i = (rnd() % n as u64) as usize;
                keys[i] |= 1 << (18 + rnd() % 14);
            }
            let cids: Vec<u64> = (0..n).map(|_| rnd() % (1 << 40) | (rnd() % 4096) << 43).collect();
            let mut b: Vec<[u64; 2]> = keys.iter().zip(&cids).map(|(&k, &c)| [k, c & CIDM]).collect();
            slow_sort(g, &mut b);
            let mut a = keys.clone();
            a.reserve(64);
            unsafe { k8_user(g, a.as_mut_ptr(), cids.as_ptr(), n, &mut sc) };
            let mut bk: Vec<u64> = b.iter().map(|r| r[0]).collect();
            let norm = |v: &mut Vec<u64>| {
                let mut i = 0;
                while i < v.len() {
                    let mut j = i + 1;
                    while j < v.len() && (v[j] ^ v[i]) >> 4 << 4 & !IDX_BITS == 0 {
                        j += 1;
                    }
                    v[i..j].sort_unstable();
                    i = j;
                }
            };
            norm(&mut a);
            norm(&mut bk);
            if a != bk {
                eprintln!("[k8bench] SORT WRONG n={n} rep={rep}");
                for i in 0..n.min(80) {
                    eprintln!("  {i}: {:016x} {:016x}", a[i], bk[i]);
                }
                unsafe { crate::sys::_exit(9) };
            }
            nchk += 1;
        }
    }
    eprintln!("[k8bench] {} checks passed for {} sizes", nchk, sizes.len());
    {
        let mut v: Vec<u64> = vec![0, 1, 35, 36, 37];
        for k in 1..9 {
            let p = P36[k];
            for d in [p - 1, p, p + 1, 2 * p - 1, 2 * p, 35 * p, 35 * p + 1] {
                if d < P36[8] {
                    v.push(d);
                }
            }
        }
        for _ in 0..200000 {
            let s = rnd() % 42;
            v.push(rnd() % (1u64 << s).max(1));
        }
        for b in 0..42 {
            v.push(1u64 << b);
            v.push((1u64 << b) - 1);
        }
        v.retain(|&y| y < P36[8]);
        for i in 0..v.len() {
            let j = (i * 7919 + 13) % v.len();
            let (x, y) = (v[i], v[j]);
            if (e36(x).cmp(&e36(y))) != (enc36(x).0.cmp(&enc36(y).0)) || (e36(x) == e36(y)) != (x == y) {
                eprintln!("[k8bench] E36 WRONG {x} {y}");
                unsafe { crate::sys::_exit(9) };
            }
        }
        eprintln!("[k8bench] e36 checks passed ({})", v.len());
    }
    for &n in &[2usize, 3, 4, 5, 6, 7, 8, 9, 12, 15, 16, 17, 24, 31, 32, 33, 40, 48, 63, 64, 65, 100, 128, 200, 300, 512, 600, 1500, 2048, 5000, 16000, 100000] {
        let reps = (400_000 / n).max(4);
        let data: Vec<u64> = (0..reps * n).map(|_| { let ts = rnd() % 1_000_000_000; let c = rnd() % 2000; ts << 32 | c << 4 | (rnd() % 3) }).collect();
        let cids: Vec<u64> = (0..reps * n + 64).map(|_| rnd() % (1 << 40)).collect();
        let mut d = data.clone();
        d.reserve(64);
        let t0 = unsafe { std::arch::x86_64::_rdtsc() };
        #[cfg(feature = "pmu")]
        let q0 = pm2::rd();
        let mut r = 0;
        for _ in 0..reps {
            unsafe { k8_user(g, d.as_mut_ptr().add(r * n), cids.as_ptr().add(r * n), n, &mut sc) };
            r = (r + 37) % reps;
        }
        let el = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        #[cfg(feature = "pmu")]
        {
            let q1 = pm2::rd();
            eprintln!("[k8bench] n={n}: {:.1} instr/click {:.3} bm/click", (q1[0] - q0[0]) as f64 / (reps * n) as f64, (q1[1] - q0[1]) as f64 / (reps * n) as f64);
        }
        eprintln!("[k8bench] n={n}: {:.0} cycles/call, {:.1} per click", el as f64 / reps as f64, el as f64 / (reps * n) as f64);
    }
}

pub fn sort_bench(g: &G) {
    if std::env::var("XL_SORTBENCH").is_ok_and(|v| v == "8") {
        return k8_bench(g);
    }
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
        #[cfg(feature = "pmu")]
        let q0 = pm2::rd();
        let mut r = 0;
        for _ in 0..reps {
            sort_clicks(g, &mut data[r * n..(r + 1) * n], &mut sc);
            r = (r + 37) % reps;
        }
        let el = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        #[cfg(feature = "pmu")]
        { let q1 = pm2::rd(); eprintln!("[sortbench] n={n}: {:.1} instr/click {:.3} bm/click", (q1[0] - q0[0]) as f64 / (reps * n) as f64, (q1[1] - q0[1]) as f64 / (reps * n) as f64); }
        eprintln!("[sortbench] n={n}: {:.0} cycles/call, {:.1} per click", el as f64 / reps as f64, el as f64 / (reps * n) as f64);
    }
}
