#![allow(clippy::too_many_arguments)]
use crate::iface::*;
use crate::pqw;
use crate::sys::*;
use crate::{enc36, Kind, DAY_MS, G, K, PBOUND, CBOUND};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

pub static ON: AtomicBool = AtomicBool::new(false);
pub fn forced() -> bool {
    std::env::var("XL_V3").is_ok_and(|v| v == "1")
}

static mut SKIP: u32 = 0;
static mut KN: [usize; 8] = [65536, 0, 0, 100, 0, 0, 0, 0];
const ROWS_PM: usize = 200;
const GAP_MS: i64 = 1_800_000;
const WEEK_MS: i64 = 7 * DAY_MS;
const W0_INVALID: u64 = u64::MAX;
const SHORT_END: u64 = 1 << 42;
const ODDK: u64 = 1 << 63;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub hi: u64,
    pub lo: u64,
}

#[inline(always)]
fn hash(k: Key) -> u64 {
    let mut x = (k.lo ^ k.hi.wrapping_mul(0xA24B_AED4_963E_E407)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 32;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^ (x >> 32)
}
#[inline(always)]
fn part(h: u64, bits: u32) -> usize {
    if bits == 0 {
        0
    } else {
        (h >> (64 - bits)) as usize
    }
}

const PAD: u64 = u64::MAX;
const SW: usize = 24;
const CW: usize = 504;
const CHUNK: usize = 4096;
const BLOCK: usize = 2 << 20;

#[repr(C, align(64))]
struct Stage {
    fill: usize,
    wptr: *mut u64,
    head: *mut u64,
    nstages: usize,
    _pad: [u64; 4],
    data: [u64; SW],
}
const STAGE_SZ: usize = std::mem::size_of::<Stage>();
const _: () = assert!(STAGE_SZ == 64 + SW * 8 && CW % SW == 0);

pub struct WBins {
    st: *mut Stage,
    np: usize,
    blk: *mut u8,
    blk_end: *mut u8,
    free: Vec<*mut u8>,
}
impl WBins {
    fn new(np: usize) -> WBins {
        WBins { st: crate::sys::anon_pack(np * STAGE_SZ) as *mut Stage, np, blk: std::ptr::null_mut(), blk_end: std::ptr::null_mut(), free: Vec::new() }
    }
    #[inline(never)]
    #[cold]
    fn chunk(&mut self) -> *mut u64 {
        unsafe {
            if let Some(c) = self.free.pop() {
                return c as *mut u64;
            }
            if self.blk.is_null() || self.blk.add(CHUNK) > self.blk_end {
                self.blk = anon(BLOCK);
                self.blk_end = self.blk.add(BLOCK);
            }
            let c = self.blk;
            self.blk = self.blk.add(CHUNK);
            c as *mut u64
        }
    }
    #[inline(always)]
    unsafe fn flush(&mut self, s: *mut Stage) {
        use std::arch::x86_64::*;
        let mut w = (*s).wptr;
        if w.is_null() || (w as usize & (CHUNK - 1)) == CW * 8 {
            let c = self.chunk();
            if w.is_null() {
                (*s).head = c;
            } else {
                *(w as *mut *mut u64) = c;
            }
            w = c;
        }
        let src = (*s).data.as_ptr() as *const __m256i;
        let dst = w as *mut __m256i;
        for k in 0..SW / 4 {
            _mm256_stream_si256(dst.add(k), _mm256_load_si256(src.add(k)));
        }
        (*s).wptr = w.add(SW);
        (*s).nstages += 1;
    }
    #[inline(always)]
    fn push<const N: usize>(&mut self, p: usize, rec: [u64; N]) {
        unsafe {
            let s = self.st.add(p);
            let mut f = (*s).fill;
            if f + N > SW {
                while f < SW {
                    *(*s).data.get_unchecked_mut(f) = PAD;
                    f += 1;
                }
                self.flush(s);
                f = 0;
            }
            for k in 0..N {
                *(*s).data.get_unchecked_mut(f + k) = rec[k];
            }
            f += N;
            if f == SW {
                self.flush(s);
                f = 0;
            }
            (*s).fill = f;
        }
    }
    fn finish(&mut self) {
        unsafe {
            for p in 0..self.np {
                let s = self.st.add(p);
                let mut f = (*s).fill;
                if f > 0 {
                    while f < SW {
                        (*s).data[f] = PAD;
                        f += 1;
                    }
                    self.flush(s);
                    (*s).fill = 0;
                }
            }
            std::arch::x86_64::_mm_sfence();
        }
    }
    #[inline(always)]
    fn chunks(&self, p: usize, mut f: impl FnMut(*mut u64, usize)) {
        unsafe {
            let s = self.st.add(p);
            let mut c = (*s).head;
            let mut rem = (*s).nstages * SW;
            while rem > 0 {
                let k = rem.min(CW);
                f(c, k);
                rem -= k;
                if rem > 0 {
                    c = *(c.add(CW) as *const *mut u64);
                }
            }
        }
    }
}

const BMB: u32 = 19;
const BMM: u64 = (1 << BMB) - 1;
const WT_SE: u64 = 0;
const WT_LE: u64 = 1;
const WT_X: u64 = 2;
const WT_INV: u64 = 4;

#[inline(always)]
unsafe fn nt_line(dst: *mut u64, src: *const u64) {
    #[cfg(feature = "w512")]
    {
        use std::arch::x86_64::*;
        _mm512_stream_si512(dst as *mut _, _mm512_load_si512(src as *const _));
    }
    #[cfg(not(feature = "w512"))]
    core::arch::asm!(
        "vmovdqa {a}, ymmword ptr [{s}]",
        "vmovdqa {b}, ymmword ptr [{s} + 32]",
        "vmovntdq ymmword ptr [{d}], {a}",
        "vmovntdq ymmword ptr [{d} + 32], {b}",
        a = out(ymm_reg) _,
        b = out(ymm_reg) _,
        s = in(reg) src,
        d = in(reg) dst,
        options(nostack, preserves_flags),
    );
}
#[inline(always)]
pub(crate) fn opq(x: u32) -> u32 {
    #[cfg(feature = "w512")]
    {
        x
    }
    #[cfg(not(feature = "w512"))]
    {
        let r: u32;
        unsafe { core::arch::asm!("/* {0:e} */", inout(reg) x => r, options(pure, nomem, nostack, preserves_flags)) };
        r
    }
}
const EDRAIN: usize = 8;
const EQCAP: usize = EDRAIN + 3 * NB + 8;

struct ELane<const CS: u32 = 12> {
    pool: *mut u64,
    cnt: *mut u8,
    wp: *mut *mut u64,
    cur: *mut u32,
    q: *mut u32,
    nq: usize,
    next: Vec<u32>,
    adr: Vec<*mut u64>,
    nch: u32,
    arp: *mut EArena,
    own: EArena,
    sub: *mut u8,
    sub_n: usize,
    first: Vec<u32>,
}
struct EArena {
    cur: *mut u8,
    end: *mut u8,
    free: Vec<*mut u8>,
}
impl EArena {
    const fn new() -> EArena {
        EArena { cur: std::ptr::null_mut(), end: std::ptr::null_mut(), free: Vec::new() }
    }
    #[inline]
    unsafe fn take(&mut self, bytes: usize) -> *mut u8 {
        if bytes == 4096 {
            if let Some(c) = self.free.pop() {
                return c;
            }
        }
        if self.cur.is_null() || (self.end as usize - self.cur as usize) < bytes {
            let x = if bytes <= EXT_SZ { ext_new(self as *mut EArena as usize) } else { std::ptr::null_mut() };
            if !x.is_null() {
                self.cur = x;
                self.end = x.add(EXT_SZ);
            } else {
                let len = (bytes + BLOCK - 1) & !(BLOCK - 1);
                self.cur = anon(len);
                self.end = self.cur.add(len);
            }
        }
        let p = self.cur;
        self.cur = p.add(bytes);
        p
    }
}

const EXT_B: u32 = crate::env_u32(option_env!("XL_EXTB"), 25);
const EXT_SZ: usize = 1 << EXT_B;
const EXT_CAP: usize = 4096;
const EXT_MAXK: usize = 14;
const EXT_LIM: usize = 128 << 30;
static EXT_ON: AtomicBool = AtomicBool::new(false);
static EXT_N: AtomicUsize = AtomicUsize::new(0);
static mut EXT_L: [(usize, usize); EXT_CAP] = [(0, 0); EXT_CAP];
static mut EXT_KEEP: [u64; EXT_LIM >> EXT_B >> 6] = [0; EXT_LIM >> EXT_B >> 6];
static mut EXT_BASE: usize = 0;
static mut EXT_NK: usize = 0;
static ROWS_N: AtomicUsize = AtomicUsize::new(0);
#[cold]
#[inline(never)]
fn ext_new(owner: usize) -> *mut u8 {
    if !EXT_ON.load(Ordering::Relaxed) {
        return std::ptr::null_mut();
    }
    let p = crate::sys::anon_ext(EXT_SZ, EXT_SZ);
    if p.is_null() {
        return p;
    }
    let i = EXT_N.fetch_add(1, Ordering::Relaxed);
    if i < EXT_CAP {
        unsafe { EXT_L[i] = (p as usize, owner) };
    }
    p
}
#[inline(always)]
fn ext_kept(c: usize) -> bool {
    unsafe {
        let off = c.wrapping_sub(EXT_BASE);
        off < EXT_LIM && (EXT_KEEP[off >> EXT_B >> 6] >> ((off >> EXT_B) & 63)) & 1 != 0
    }
}
#[allow(static_mut_refs)]
fn ext_decide(pm: usize) {
    if !EXT_ON.load(Ordering::Relaxed) || pm == 0 {
        return;
    }
    unsafe {
        let n = EXT_N.load(Ordering::Relaxed).min(EXT_CAP);
        let want = (n * pm / 1000).min(EXT_MAXK);
        let mut k = 0;
        for i in 0..n {
            if k >= want {
                break;
            }
            let (b, o) = EXT_L[i];
            if (i + 1..n).any(|j| EXT_L[j].1 == o) {
                let x = (b - EXT_BASE) >> EXT_B;
                EXT_KEEP[x >> 6] |= 1 << (x & 63);
                k += 1;
            }
        }
        EXT_NK = k;
    }
}
#[cold]
#[inline(never)]
fn ext_check(cx: &Cx) {
    let (mut n, mut bad) = (0usize, 0usize);
    let mut one = |p: usize, w: usize| {
        if w > 0 {
            n += 1;
            let p = std::hint::black_box(p);
            if ext_kept(p) || ext_kept(p + w * 8 - 1) {
                bad += 1;
            }
        }
    };
    let mut sl: Vec<(*const u64, usize)> = Vec::new();
    for tid in 0..cx.t.len() {
        let t = cx.t(tid);
        sl.clear();
        for q in 0..t.u2.s.np {
            t.u2.s.slices(q, &mut sl);
            t.u2.w.slices(q, &mut sl);
        }
        for q in 0..t.ck.np {
            t.ck.s.slices(q, &mut sl);
            t.ck.w.slices(q, &mut sl);
        }
        for &(p, w) in sl.iter() {
            one(p as usize, w);
        }
        for &c in t.u2.s.free.iter().chain(t.u2.w.free.iter()) {
            one(c as usize, 1);
        }
    }
    eprintln!("[e2we] keep check: {n} slices, {bad} in kept extents");
    if bad != 0 {
        fallback("e2we keep check");
    }
}
#[cold]
#[inline(never)]
#[allow(static_mut_refs)]
fn ext_poison() -> usize {
    let mut n = 0;
    unsafe {
        for i in 0..EXT_N.load(Ordering::Relaxed).min(EXT_CAP) {
            let b = EXT_L[i].0;
            if ext_kept(b) {
                std::ptr::write_bytes(b as *mut u8, 0xA5, EXT_SZ);
                n += 1;
            }
        }
    }
    eprintln!("[e2p] poisoned {n} kept extents");
    n
}
#[allow(static_mut_refs)]
fn ext_donate() -> usize {
    unsafe {
        let mut v: Vec<usize> = (0..EXT_N.load(Ordering::Relaxed).min(EXT_CAP)).map(|i| EXT_L[i].0).filter(|&b| ext_kept(b)).collect();
        v.sort_unstable();
        let mut i = 0;
        while i < v.len() {
            let mut j = i + 1;
            while j < v.len() && v[j] == v[j - 1] + EXT_SZ {
                j += 1;
            }
            crate::sys::donate(v[i], (j - i) * EXT_SZ);
            i = j;
        }
        v.len() * EXT_SZ
    }
}
core::arch::global_asm!(
    ".text",
    ".p2align 4",
    ".globl xl_cold_thunk",
    ".type xl_cold_thunk,@function",
    "xl_cold_thunk:",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "sub rsp, 8",
    "call rax",
    "add rsp, 8",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "ret",
    ".size xl_cold_thunk, .-xl_cold_thunk",
);
extern "C" {
    fn xl_cold_thunk();
}
#[derive(Clone, Copy)]
struct ECur<const CS: u32 = 12> {
    pool: *mut u64,
    cnt: *mut u8,
    q: *mut u32,
    nq: usize,
    lane: *mut ELane<CS>,
}
impl<const CS: u32> ECur<CS> {
    #[inline(always)]
    unsafe fn relief(&mut self) {
        if self.nq >= EDRAIN {
            (*self.lane).nq = self.nq;
            (*self.lane).drain();
            self.nq = 0;
        }
    }
    #[inline(always)]
    unsafe fn full(&mut self, p: usize) -> usize {
        let r: usize;
        core::arch::asm!(
            "call {t}",
            t = sym xl_cold_thunk,
            inout("rax") ELane::<CS>::flush_now_c as usize => r,
            in("rdi") self.lane,
            in("rsi") p,
            out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, out("zmm6") _, out("zmm7") _,
            out("zmm8") _, out("zmm9") _, out("zmm10") _, out("zmm11") _, out("zmm12") _, out("zmm13") _, out("zmm14") _, out("zmm15") _,
            out("zmm16") _, out("zmm17") _, out("zmm18") _, out("zmm19") _, out("zmm20") _, out("zmm21") _, out("zmm22") _, out("zmm23") _,
            out("zmm24") _, out("zmm25") _, out("zmm26") _, out("zmm27") _, out("zmm28") _, out("zmm29") _, out("zmm30") _, out("zmm31") _,
            out("k0") _, out("k1") _, out("k2") _, out("k3") _, out("k4") _, out("k5") _, out("k6") _, out("k7") _,
        );
        r
    }
    #[inline(always)]
    unsafe fn push2(&mut self, p: usize, r: std::arch::x86_64::__m128i) {
        let mut f = *self.cnt.add(p) as usize;
        if f == 64 {
            f = self.full(p);
        }
        std::arch::x86_64::_mm_store_si128((self.pool as *mut u8).add(p * 64 + f) as *mut _, r);
        f += 16;
        *self.cnt.add(p) = f as u8;
        *self.q.add(self.nq) = p as u32;
        self.nq += f >> 6;
    }
    #[inline(always)]
    unsafe fn push4(&mut self, p: usize, r: [u64; 4]) {
        let mut f = *self.cnt.add(p) as usize;
        if f == 64 {
            f = self.full(p);
        }
        let x = (self.pool as *mut u8).add(p * 64 + f) as *mut u64;
        *x = r[0];
        *x.add(1) = r[1];
        *x.add(2) = r[2];
        *x.add(3) = r[3];
        f += 32;
        *self.cnt.add(p) = f as u8;
        *self.q.add(self.nq) = p as u32;
        self.nq += f >> 6;
    }
}
impl<const CS: u32> ELane<CS> {
    const CB: usize = 1 << CS;
    const CWD: usize = (1 << CS) / 8;
    const fn empty() -> ELane<CS> {
        ELane { pool: std::ptr::null_mut(), cnt: std::ptr::null_mut(), wp: std::ptr::null_mut(), cur: std::ptr::null_mut(), q: std::ptr::null_mut(), nq: 0, next: Vec::new(), adr: Vec::new(), nch: 0, arp: std::ptr::null_mut(), own: EArena::new(), sub: std::ptr::null_mut(), sub_n: 0, first: Vec::new() }
    }
    const fn mem(np: usize) -> usize {
        np * 64 + np * 8 + np * 4 + EQCAP * 4 + np
    }
    unsafe fn init(&mut self, np: usize, m: *mut u8, arp: *mut EArena) {
        self.arp = arp;
        self.pool = m as *mut u64;
        self.wp = m.add(np * 64) as *mut *mut u64;
        self.cur = self.wp.add(np) as *mut u32;
        self.q = self.cur.add(np);
        self.cnt = self.q.add(EQCAP) as *mut u8;
        self.next.reserve(np * 4);
        self.adr.reserve(np * 4);
        for p in 0..np {
            *self.wp.add(p) = self.alloc();
            *self.cur.add(p) = p as u32;
        }
    }
    unsafe fn init_lazy(&mut self, np: usize, m: *mut u8, arp: *mut EArena) {
        self.arp = arp;
        self.pool = m as *mut u64;
        self.wp = m.add(np * 64) as *mut *mut u64;
        self.cur = self.wp.add(np) as *mut u32;
        self.q = self.cur.add(np);
        self.cnt = self.q.add(EQCAP) as *mut u8;
        self.first = vec![u32::MAX; np];
    }
    #[cold]
    #[inline(never)]
    unsafe fn first_chunk(&mut self, p: usize) -> *mut u64 {
        let c = self.nch;
        let a = self.alloc();
        *self.first.get_unchecked_mut(p) = c;
        *self.cur.add(p) = c;
        a
    }
    #[inline(always)]
    fn cursor(&mut self) -> ECur<CS> {
        ECur { pool: self.pool, cnt: self.cnt, q: self.q, nq: self.nq, lane: self }
    }
    #[inline(always)]
    unsafe fn put(&mut self, c: ECur<CS>) {
        self.nq = c.nq;
        if c.nq >= EDRAIN {
            self.drain();
        }
    }
    #[inline(always)]
    unsafe fn flush_line(&mut self, p: usize) {
        let dst = *self.wp.add(p);
        nt_line(dst, self.pool.add(p * 8));
        let mut nw = dst.add(8);
        if nw as usize & (Self::CB - 1) == 0 {
            nw = self.new_chunk(p);
        }
        *self.wp.add(p) = nw;
        *self.cnt.add(p) = 0;
    }
    #[cold]
    #[inline(never)]
    unsafe fn flush_now(&mut self, p: usize) -> usize {
        self.flush_line(p);
        0
    }
    #[cold]
    #[inline(never)]
    unsafe extern "C" fn flush_now_c(lane: *mut ELane<CS>, p: usize) -> usize {
        (*lane).flush_line(p);
        0
    }
    #[inline(never)]
    unsafe fn drain(&mut self) {
        let (q, cnt, pool, wp, n) = (self.q, self.cnt, self.pool, self.wp, self.nq);
        for i in 0..n {
            let p = *q.add(i) as usize;
            if *cnt.add(p) == 64 {
                let dst = *wp.add(p);
                nt_line(dst, pool.add(p * 8));
                *cnt.add(p) = 0;
                let nw = dst.add(8);
                *wp.add(p) = nw;
                if nw as usize & (Self::CB - 1) == 0 {
                    *wp.add(p) = self.new_chunk(p);
                }
            }
        }
        self.nq = 0;
    }
    #[inline(always)]
    fn addr(&self, c: u32) -> *mut u64 {
        unsafe { *self.adr.get_unchecked(c as usize) }
    }
    fn alloc(&mut self) -> *mut u64 {
        let c = self.nch;
        if c == u32::MAX - 1 {
            fallback("event lane: too many chunks");
        }
        let a = unsafe {
            if CS >= 12 {
                (*self.arp).take(Self::CB)
            } else {
                if self.sub_n == 0 {
                    self.sub = (*self.arp).take(4096);
                    self.sub_n = 4096 >> CS;
                }
                let a = self.sub;
                self.sub = a.add(Self::CB);
                self.sub_n -= 1;
                a
            }
        } as *mut u64;
        self.nch = c + 1;
        self.next.push(u32::MAX);
        self.adr.push(a);
        a
    }
    #[cold]
    #[inline(never)]
    unsafe fn new_chunk(&mut self, p: usize) -> *mut u64 {
        let c = self.nch;
        let a = self.alloc();
        let cu = self.cur.add(p);
        *self.next.get_unchecked_mut(*cu as usize) = c;
        *cu = c;
        a
    }
    #[inline(never)]
    unsafe fn push8(&mut self, p: usize, r: [u64; 8]) {
        use std::arch::x86_64::*;
        let mut dst = *self.wp.add(p);
        if dst.is_null() {
            dst = self.first_chunk(p);
        }
        for k in 0..8 {
            _mm_stream_si64(dst.add(k) as *mut i64, r[k] as i64);
        }
        let mut nw = dst.add(8);
        if nw as usize & (Self::CB - 1) == 0 {
            nw = self.new_chunk(p);
        }
        *self.wp.add(p) = nw;
    }
    fn release(&self, p: usize, out: &mut Vec<*mut u8>) {
        unsafe {
            let last = *self.cur.add(p);
            let mut c = p as u32;
            if !self.first.is_empty() {
                c = self.first[p];
                if c == u32::MAX {
                    return;
                }
            }
            loop {
                out.push(self.addr(c) as *mut u8);
                if c == last {
                    break;
                }
                c = self.next[c as usize];
            }
        }
    }
    fn slices(&self, p: usize, out: &mut Vec<(*const u64, usize)>) -> usize {
        unsafe {
            let last = *self.cur.add(p);
            let mut c = p as u32;
            if !self.first.is_empty() {
                c = self.first[p];
                if c == u32::MAX {
                    return 0;
                }
            }
            let mut n = 0;
            while c != last {
                out.push((self.addr(c) as *const u64, Self::CWD));
                n += Self::CWD;
                c = self.next[c as usize];
            }
            let base = self.addr(last) as usize;
            let w = (*self.wp.add(p) as usize - base) >> 3;
            if w > 0 {
                out.push((base as *const u64, w));
            }
            let f = *self.cnt.add(p) as usize / 8;
            if f > 0 {
                out.push((self.pool.add(p * 8) as *const u64, f));
            }
            n + w + f
        }
    }
    fn copy_out(&self, p: usize, out: &mut Vec<u64>) {
        unsafe {
            if self.cur.is_null() {
                return;
            }
            let last = *self.cur.add(p);
            let (mut c, mut n) = (p as u32, 0usize);
            while c != last {
                n += Self::CWD;
                c = self.next[c as usize];
            }
            let base = self.addr(last) as usize;
            let w = (*self.wp.add(p) as usize - base) >> 3;
            let f = *self.cnt.add(p) as usize / 8;
            out.reserve(n + w + f);
            let mut dst = out.as_mut_ptr().add(out.len());
            c = p as u32;
            while c != last {
                std::ptr::copy_nonoverlapping(self.addr(c) as *const u64, dst, Self::CWD);
                dst = dst.add(Self::CWD);
                c = self.next[c as usize];
            }
            std::ptr::copy_nonoverlapping(base as *const u64, dst, w);
            std::ptr::copy_nonoverlapping(self.pool.add(p * 8) as *const u64, dst.add(w), f);
            out.set_len(out.len() + n + w + f);
        }
    }
}

#[derive(Clone, Copy)]
struct Cand {
    k: Key,
    ing: i64,
    w0: u64,
    uh: u64,
    ul: u64,
}

struct P3 {
    es: ELane,
    ew: ELane,
    ex: ELane,
    seen: Vec<u64>,
    ssl: Vec<(*const u64, usize)>,
    wsl: Vec<(*const u64, usize)>,
    xsl: Vec<(*const u64, usize)>,
    cptr: Vec<usize>,
    ca: Vec<u64>,
    cb: Vec<u64>,
    ms: Vec<u64>,
    mw: Vec<u64>,
    msf: Vec<u8>,
    mwf: Vec<u8>,
    fl: Vec<u16>,
    flw: Vec<u16>,
    ow: Vec<u64>,
    oq: Vec<u32>,
    so: Vec<u64>,
    sq: Vec<u32>,
    wo: Vec<u64>,
    wq: Vec<u32>,
    dc: [u64; 8],
    cands: Vec<Cand>,
    ctab: Vec<u32>,
    dead: Vec<u8>,
    out: Vec<[u64; 2]>,
    out3: Vec<[u64; 3]>,
    cyc: [u64; 8],
}
static mut P3S: Vec<P3> = Vec::new();
#[inline(always)]
fn p3(tid: usize) -> &'static mut P3 {
    unsafe { &mut *(*std::ptr::addr_of_mut!(P3S)).as_mut_ptr().add(tid) }
}
static mut DDK: u32 = 0;
static mut ESH: u32 = 30;
static mut EK: u32 = 0;
#[inline(always)]
fn e_part(v: u64, sh: u32, b1: u32) -> usize {
    (v >> sh) as usize & ((1usize << b1) - 1)
}
#[inline(always)]
fn e_rem(v: u64, sh: u32) -> u64 {
    (v & ((1u64 << sh) - 1)) | (v >> 40) << sh
}
#[inline(always)]
fn bm_key(k: Key) -> u64 {
    (hash(k) >> 20) & BMM
}

const EMPTY: u64 = u64::MAX;
#[derive(Default)]
struct Tab {
    slots: Vec<u64>,
    mask: usize,
}
impl Tab {
    fn reset(&mut self, n: usize) {
        let cap = (n * 2).next_power_of_two().max(16);
        self.slots.clear();
        self.slots.resize(cap, EMPTY);
        self.mask = cap - 1;
    }
    #[inline(always)]
    fn find(&self, h: u64, eq: impl Fn(u32) -> bool) -> Result<(usize, u32), usize> {
        let mut s = h as usize & self.mask;
        let tag = h >> 32;
        loop {
            let e = unsafe { *self.slots.get_unchecked(s) };
            if e == EMPTY {
                return Err(s);
            }
            if e >> 32 == tag && eq(e as u32) {
                return Ok((s, e as u32));
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn set(&mut self, slot: usize, h: u64, i: u32) {
        unsafe { *self.slots.get_unchecked_mut(slot) = h >> 32 << 32 | i as u64 };
    }
}

#[derive(Default)]
struct KTab {
    slots: Vec<u32>,
    keys: Vec<Key>,
    vals: Vec<u32>,
    mask: usize,
}
impl KTab {
    fn clear(&mut self) {
        if self.slots.len() != 1024 {
            self.slots.clear();
            self.slots.resize(1024, u32::MAX);
        } else {
            self.slots.fill(u32::MAX);
        }
        self.mask = 1023;
        self.keys.clear();
        self.vals.clear();
    }
    #[inline(always)]
    fn get(&self, k: Key, h: u64) -> Option<u32> {
        let mut s = h as usize & self.mask;
        loop {
            let i = unsafe { *self.slots.get_unchecked(s) };
            if i == u32::MAX {
                return None;
            }
            if unsafe { *self.keys.get_unchecked(i as usize) } == k {
                return Some(unsafe { *self.vals.get_unchecked(i as usize) });
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn insert(&mut self, k: Key, h: u64, v: u32) {
        if (self.keys.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut s = h as usize & self.mask;
        while unsafe { *self.slots.get_unchecked(s) } != u32::MAX {
            s = (s + 1) & self.mask;
        }
        self.slots[s] = self.keys.len() as u32;
        self.keys.push(k);
        self.vals.push(v);
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        self.slots.clear();
        self.slots.resize(cap, u32::MAX);
        self.mask = cap - 1;
        for (i, &k) in self.keys.iter().enumerate() {
            let mut s = hash(k) as usize & self.mask;
            while self.slots[s] != u32::MAX {
                s = (s + 1) & self.mask;
            }
            self.slots[s] = i as u32;
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EvRec {
    ek: Key,
    uk: Key,
    w0: u64,
    ing: i64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct URec {
    k: Key,
    w0: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CkRec {
    k: Key,
    ck: Key,
    ts: i64,
    camp: u32,
}

#[repr(C, align(64))]
struct VStage {
    fill: usize,
    wptr: *mut u64,
    head: *mut u64,
    nwords: usize,
    _pad: [u64; 4],
    data: [u64; SW],
}
const _: () = assert!(std::mem::size_of::<VStage>() == 64 + SW * 8);

pub struct VB {
    st: *mut VStage,
    np: usize,
    blk: *mut u8,
    blk_end: *mut u8,
    free: Vec<*mut u64>,
}
impl VB {
    fn new(np: usize) -> VB {
        VB { st: crate::sys::anon_pack(np * std::mem::size_of::<VStage>()) as *mut VStage, np, blk: std::ptr::null_mut(), blk_end: std::ptr::null_mut(), free: Vec::new() }
    }
    #[inline(never)]
    #[cold]
    fn chunk(&mut self) -> *mut u64 {
        if let Some(c) = self.free.pop() {
            return c;
        }
        unsafe {
            if self.blk.is_null() || self.blk.add(CHUNK) > self.blk_end {
                self.blk = anon(BLOCK);
                self.blk_end = self.blk.add(BLOCK);
            }
            let c = self.blk;
            self.blk = self.blk.add(CHUNK);
            c as *mut u64
        }
    }
    #[inline(always)]
    unsafe fn flush(&mut self, s: *mut VStage, valid: usize) {
        use std::arch::x86_64::*;
        let mut w = (*s).wptr;
        if w.is_null() || (w as usize & (CHUNK - 1)) == CW * 8 {
            let c = self.chunk();
            if w.is_null() {
                (*s).head = c;
            } else {
                *(w as *mut *mut u64) = c;
            }
            w = c;
        }
        let src = (*s).data.as_ptr() as *const __m256i;
        let dst = w as *mut __m256i;
        for k in 0..SW / 4 {
            _mm256_stream_si256(dst.add(k), _mm256_load_si256(src.add(k)));
        }
        (*s).wptr = w.add(SW);
        (*s).nwords += valid;
    }
    #[inline(always)]
    fn push1(&mut self, p: usize, w: u64) {
        unsafe {
            let s = self.st.add(p);
            let f = (*s).fill;
            *(*s).data.get_unchecked_mut(f) = w;
            if f + 1 == SW {
                self.flush(s, SW);
                (*s).fill = 0;
            } else {
                (*s).fill = f + 1;
            }
        }
    }
    #[inline(always)]
    fn push<const N: usize>(&mut self, p: usize, rec: [u64; N]) {
        unsafe {
            let s = self.st.add(p);
            let mut f = (*s).fill;
            for k in 0..N {
                *(*s).data.get_unchecked_mut(f) = rec[k];
                f += 1;
                if f == SW {
                    self.flush(s, SW);
                    f = 0;
                }
            }
            (*s).fill = f;
        }
    }
    #[inline]
    fn put(&mut self, p: usize, src: &[u64]) {
        unsafe {
            let s = self.st.add(p);
            let mut f = (*s).fill;
            let mut i = 0;
            while i < src.len() {
                let k = (SW - f).min(src.len() - i);
                std::ptr::copy_nonoverlapping(src.as_ptr().add(i), (*s).data.as_mut_ptr().add(f), k);
                f += k;
                i += k;
                if f == SW {
                    self.flush(s, SW);
                    f = 0;
                }
            }
            (*s).fill = f;
        }
    }
    #[inline(never)]
    unsafe fn flush_out(&mut self, s: *mut VStage) {
        self.flush(s, SW);
    }
    #[inline(always)]
    fn put_events<E: Rec, P: Rec>(&mut self, p: usize, hdr: &[u64], ev: &[E], pp: &[P]) {
        unsafe {
            let s = self.st.add(p);
            let mut f = (*s).fill;
            macro_rules! word {
                ($w:expr) => {{
                    *(*s).data.get_unchecked_mut(f) = $w;
                    f += 1;
                    if f == SW {
                        self.flush_out(s);
                        f = 0;
                    }
                }};
            }
            for &h in hdr {
                word!(h);
            }
            let n = ev.len();
            let e = ev.as_ptr();
            let mut i = 0;
            while i + 1 < n {
                word!((*e.add(i)).lo32() as u64 | ((*e.add(i + 1)).lo32() as u64) << 32);
                i += 2;
            }
            if i < n {
                word!((*e.add(i)).lo32() as u64);
            }
            for x in pp {
                word!(((x.lo32() >> 1) as u64) << 32 | x.last());
            }
            (*s).fill = f;
        }
    }
    #[inline(always)]
    fn put_clicks<C: Rec>(&mut self, p: usize, hdr: &[u64], ck: &[C], cside: &[Key]) {
        unsafe {
            let s = self.st.add(p);
            let mut f = (*s).fill;
            macro_rules! word {
                ($w:expr) => {{
                    *(*s).data.get_unchecked_mut(f) = $w;
                    f += 1;
                    if f == SW {
                        self.flush_out(s);
                        f = 0;
                    }
                }};
            }
            for &h in hdr {
                word!(h);
            }
            for c in ck {
                word!(c.lo32() as u64);
                word!(c.last());
            }
            for c in ck {
                if c.lo32() & 1 != 0 {
                    let k = cside[(c.last() & M42) as usize];
                    word!(k.lo);
                    word!(k.hi);
                }
            }
            (*s).fill = f;
        }
    }
    #[inline(always)]
    unsafe fn put_words(&mut self, s: *mut VStage, mut f: usize, src: *const u64, n: usize) -> usize {
        use std::arch::x86_64::*;
        let mut i = 0;
        while i < n {
            let k = (SW - f).min(n - i);
            let d = (*s).data.as_mut_ptr().add(f);
            let mut j = 0;
            while j < k {
                let m = _bzhi_u32(0xFF, (k - j) as u32) as __mmask8;
                _mm512_mask_storeu_epi64(d.add(j) as *mut i64, m, _mm512_maskz_loadu_epi64(m, src.add(i + j) as *const i64));
                j += 8;
            }
            f += k;
            i += k;
            if f == SW {
                self.flush_out(s);
                f = 0;
            }
        }
        f
    }
    #[inline(always)]
    unsafe fn put_ev32(&mut self, p: usize, pk: Key, ev: *const u32, n: usize, pp: *const u64, x: usize) {
        let s = self.st.add(p);
        let mut f = (*s).fill;
        macro_rules! word {
            ($w:expr) => {{
                *(*s).data.get_unchecked_mut(f) = $w;
                f += 1;
                if f == SW {
                    self.flush_out(s);
                    f = 0;
                }
            }};
        }
        let wide = pk.hi != 0;
        word!(n as u64 | (x as u64) << 32 | (wide as u64) << 62);
        word!(pk.lo);
        if wide {
            word!(pk.hi);
        }
        f = self.put_words(s, f, ev as *const u64, n / 2);
        if n & 1 != 0 {
            word!(*ev.add(n - 1) as u64);
        }
        if x != 0 {
            f = self.put_words(s, f, pp, x);
        }
        (*s).fill = f;
    }
    #[inline(always)]
    unsafe fn put_ck2(&mut self, p: usize, pk: Key, ck: *const [u64; 2], n: usize, cside: &[Key]) {
        let s = self.st.add(p);
        let mut f = (*s).fill;
        macro_rules! word {
            ($w:expr) => {{
                *(*s).data.get_unchecked_mut(f) = $w;
                f += 1;
                if f == SW {
                    self.flush_out(s);
                    f = 0;
                }
            }};
        }
        let mut x = 0u64;
        for i in 0..n {
            x += (*ck.add(i))[0] & 1;
        }
        let wide = pk.hi != 0;
        word!(n as u64 | x << 32 | (wide as u64) << 62 | 1 << 63);
        word!(pk.lo);
        if wide {
            word!(pk.hi);
        }
        f = self.put_words(s, f, ck as *const u64, 2 * n);
        if x != 0 {
            for i in 0..n {
                let c = *ck.add(i);
                if c[0] & 1 != 0 {
                    let k = cside[(c[1] & M42) as usize];
                    word!(k.lo);
                    word!(k.hi);
                }
            }
        }
        (*s).fill = f;
    }
    #[inline(always)]
    unsafe fn put_block(&mut self, p: usize, src: *const u64, n: usize) {
        let s = self.st.add(p);
        let f = self.put_words(s, (*s).fill, src, n);
        (*s).fill = f;
    }
    #[inline(never)]
    unsafe fn put_long(&mut self, p: usize, ck: *const u64, n: usize, cside: &[Key]) {
        let s = self.st.add(p);
        let mut f = (*s).fill;
        for i in 0..n {
            if *ck.add(2 * i) & 1 != 0 {
                let k = cside[(*ck.add(2 * i + 1) & M42) as usize];
                for w in [k.lo, k.hi] {
                    *(*s).data.get_unchecked_mut(f) = w;
                    f += 1;
                    if f == SW {
                        self.flush_out(s);
                        f = 0;
                    }
                }
            }
        }
        (*s).fill = f;
    }
    fn finish(&mut self) {
        unsafe {
            for p in 0..self.np {
                let s = self.st.add(p);
                let f = (*s).fill;
                if f > 0 {
                    self.flush(s, f);
                    (*s).fill = 0;
                }
            }
            std::arch::x86_64::_mm_sfence();
        }
    }
    #[inline(always)]
    fn prefetch(&self, p: usize) {
        use std::arch::x86_64::*;
        unsafe {
            let s = self.st.add(p) as *const i8;
            _mm_prefetch(s, _MM_HINT_T0);
            if KN[5] == 0 {
                _mm_prefetch(s.add(64), _MM_HINT_T0);
                _mm_prefetch(s.add(128), _MM_HINT_T0);
                _mm_prefetch(s.add(192), _MM_HINT_T0);
            }
        }
    }
    #[inline(always)]
    fn words(&self, p: usize) -> usize {
        unsafe { (*self.st.add(p)).nwords }
    }
    #[inline(always)]
    fn chunks(&self, p: usize, mut f: impl FnMut(*const u64, usize)) {
        unsafe {
            let s = self.st.add(p);
            let mut c = (*s).head;
            let mut rem = (*s).nwords;
            while rem > 0 {
                let k = rem.min(CW);
                f(c, k);
                rem -= k;
                if rem > 0 {
                    c = *(c.add(CW) as *const *mut u64);
                }
            }
        }
    }
    fn slices(&self, p: usize, out: &mut Vec<(*const u64, usize)>) {
        self.chunks(p, |ptr, n| out.push((ptr, n)));
    }
    fn copy_out(&self, p: usize, out: &mut Vec<u64>) {
        out.reserve(self.words(p));
        self.chunks(p, |ptr, n| unsafe {
            let l = out.len();
            std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr().add(l), n);
            out.set_len(l + n);
        });
    }
}

#[repr(C, align(64))]
struct LStage {
    fill: usize,
    wptr: *mut u64,
    head: *mut u64,
    nwords: usize,
    _pad: [u64; 4],
    data: [u64; 8],
}
pub struct LB {
    st: *mut LStage,
    np: usize,
    blk: *mut u8,
    blk_end: *mut u8,
    free: Vec<*mut u64>,
}
impl LB {
    fn new(np: usize) -> LB {
        LB { st: crate::sys::anon_pack(np * std::mem::size_of::<LStage>()) as *mut LStage, np, blk: std::ptr::null_mut(), blk_end: std::ptr::null_mut(), free: Vec::new() }
    }
    #[inline(never)]
    #[cold]
    fn chunk(&mut self) -> *mut u64 {
        if let Some(c) = self.free.pop() {
            return c;
        }
        unsafe {
            if self.blk.is_null() || self.blk.add(CHUNK) > self.blk_end {
                self.blk = anon(BLOCK);
                self.blk_end = self.blk.add(BLOCK);
            }
            let c = self.blk;
            self.blk = self.blk.add(CHUNK);
            c as *mut u64
        }
    }
    #[cfg(feature = "w512")]
    #[inline(always)]
    unsafe fn line(&mut self, s: *mut LStage, v: std::arch::x86_64::__m512i, valid: usize) {
        let mut w = (*s).wptr;
        if w.is_null() || (w as usize & (CHUNK - 1)) == CW * 8 {
            let c = self.chunk();
            if w.is_null() {
                (*s).head = c;
            } else {
                *(w as *mut *mut u64) = c;
            }
            w = c;
        }
        std::arch::x86_64::_mm512_stream_si512(w as *mut _, v);
        (*s).wptr = w.add(8);
        (*s).nwords += valid;
    }
    #[cfg(not(feature = "w512"))]
    #[inline(always)]
    unsafe fn line(&mut self, s: *mut LStage, v0: std::arch::x86_64::__m256i, v1: std::arch::x86_64::__m256i, valid: usize) {
        let mut w = (*s).wptr;
        if w.is_null() || (w as usize & (CHUNK - 1)) == CW * 8 {
            let c = self.chunk();
            if w.is_null() {
                (*s).head = c;
            } else {
                *(w as *mut *mut u64) = c;
            }
            w = c;
        }
        std::arch::x86_64::_mm256_stream_si256(w as *mut _, v0);
        std::arch::x86_64::_mm256_stream_si256(w.add(4) as *mut _, v1);
        (*s).wptr = w.add(8);
        (*s).nwords += valid;
    }
    #[cfg(feature = "w512")]
    #[inline(always)]
    unsafe fn put(&mut self, p: usize, mut src: *const u64, mut n: usize) {
        use std::arch::x86_64::*;
        let s = self.st.add(p);
        let f = (*s).fill;
        let dp = (*s).data.as_mut_ptr() as *mut __m512i;
        let tail = _mm512_load_si512(dp as *const _);
        if f + n < 8 {
            let m = (_bzhi_u32(0xFF, n as u32) << f) as __mmask8;
            _mm512_store_si512(dp as *mut _, _mm512_mask_expandloadu_epi64(tail, m, src as *const i64));
            (*s).fill = f + n;
            return;
        }
        let v = _mm512_mask_expandloadu_epi64(tail, (0xFFu32 << f) as __mmask8, src as *const i64);
        self.line(s, v, 8);
        src = src.add(8 - f);
        n -= 8 - f;
        while n >= 8 {
            self.line(s, _mm512_loadu_si512(src as *const _), 8);
            src = src.add(8);
            n -= 8;
        }
        _mm512_store_si512(dp as *mut _, _mm512_maskz_loadu_epi64(_bzhi_u32(0xFF, n as u32) as __mmask8, src as *const i64));
        (*s).fill = n;
    }
    #[cfg(not(feature = "w512"))]
    #[inline(always)]
    unsafe fn put(&mut self, p: usize, mut src: *const u64, mut n: usize) {
        use std::arch::x86_64::*;
        let s = self.st.add(p);
        let f = (*s).fill;
        let dp = (*s).data.as_mut_ptr() as *mut __m256i;
        let (t0, t1) = (_mm256_load_si256(dp as *const _), _mm256_load_si256(dp.add(1) as *const _));
        if f + n < 8 {
            let m = _bzhi_u32(0xFF, n as u32) << f;
            let (m0, m1) = ((m & 15) as __mmask8, (m >> 4) as __mmask8);
            _mm256_store_si256(dp, _mm256_mask_expandloadu_epi64(t0, m0, src as *const i64));
            _mm256_store_si256(dp.add(1), _mm256_mask_expandloadu_epi64(t1, m1, src.add((m & 15).count_ones() as usize) as *const i64));
            (*s).fill = f + n;
            return;
        }
        let m = (0xFFu32 << f) & 0xFF;
        let v0 = _mm256_mask_expandloadu_epi64(t0, (m & 15) as __mmask8, src as *const i64);
        let v1 = _mm256_mask_expandloadu_epi64(t1, (m >> 4) as __mmask8, src.add((m & 15).count_ones() as usize) as *const i64);
        self.line(s, v0, v1, 8);
        src = src.add(8 - f);
        n -= 8 - f;
        while n >= 8 {
            self.line(s, _mm256_loadu_si256(src as *const _), _mm256_loadu_si256(src.add(4) as *const _), 8);
            src = src.add(8);
            n -= 8;
        }
        let m = _bzhi_u32(0xFF, n as u32);
        _mm256_store_si256(dp, _mm256_maskz_loadu_epi64((m & 15) as __mmask8, src as *const i64));
        _mm256_store_si256(dp.add(1), _mm256_maskz_loadu_epi64((m >> 4) as __mmask8, src.add(4) as *const i64));
        (*s).fill = n;
    }
    #[cfg(feature = "w512")]
    fn finish(&mut self) {
        unsafe {
            for p in 0..self.np {
                let s = self.st.add(p);
                let f = (*s).fill;
                if f > 0 {
                    self.line(s, std::arch::x86_64::_mm512_load_si512((*s).data.as_ptr() as *const _), f);
                    (*s).fill = 0;
                }
            }
            std::arch::x86_64::_mm_sfence();
        }
    }
    #[cfg(not(feature = "w512"))]
    fn finish(&mut self) {
        unsafe {
            for p in 0..self.np {
                let s = self.st.add(p);
                let f = (*s).fill;
                if f > 0 {
                    let dp = (*s).data.as_ptr() as *const std::arch::x86_64::__m256i;
                    self.line(s, std::arch::x86_64::_mm256_load_si256(dp), std::arch::x86_64::_mm256_load_si256(dp.add(1)), f);
                    (*s).fill = 0;
                }
            }
            std::arch::x86_64::_mm_sfence();
        }
    }
    #[inline(always)]
    fn prefetch(&self, p: usize) {
        use std::arch::x86_64::*;
        unsafe {
            let s = self.st.add(p) as *const i8;
            _mm_prefetch(s, _MM_HINT_T0);
            _mm_prefetch(s.add(64), _MM_HINT_T0);
        }
    }
    fn slices(&self, p: usize, out: &mut Vec<(*const u64, usize)>) {
        unsafe {
            let s = self.st.add(p);
            let mut c = (*s).head;
            let mut rem = (*s).nwords;
            while rem > 0 {
                let k = rem.min(CW);
                out.push((c as *const u64, k));
                rem -= k;
                if rem > 0 {
                    c = *(c.add(CW) as *const *mut u64);
                }
            }
        }
    }
}

const MIXC: u64 = 0x278D_DE6E_5FD;
const M42: u64 = SHORT_END - 1;
const fn inv64(c: u64) -> u64 {
    let mut x = c;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(c.wrapping_mul(x)));
        i += 1;
    }
    x
}
const MIXINV: u64 = inv64(MIXC);
const _: () = assert!(MIXC.wrapping_mul(MIXINV) == 1 && MIXC & 1 == 1 && MIXC < SHORT_END);
#[inline(always)]
fn mix42(v: u64) -> u64 {
    v.wrapping_mul(MIXC) & M42
}
#[inline(always)]
fn unmix42(m: u64) -> u64 {
    m.wrapping_mul(MIXINV) & M42
}
#[inline(always)]
fn is_short(k: Key) -> bool {
    k.hi == 0 && k.lo < SHORT_END
}
#[inline(always)]
fn upart(k: Key, b2: u32) -> usize {
    if is_short(k) {
        (mix42(k.lo) >> (42 - b2)) as usize
    } else {
        part(hash(k), b2)
    }
}
#[inline(always)]
fn h32(h: u64, b2: u32) -> u64 {
    (h << b2) >> 32
}

pub struct UBins {
    s: VB,
    w: VB,
    np: Vec<u32>,
}
impl UBins {
    fn new(np: usize) -> UBins {
        UBins { s: VB::new(np), w: VB::new(np), np: vec![0; np] }
    }
    fn finish(&mut self) {
        self.s.finish();
        self.w.finish();
    }
}
#[inline(always)]
fn urec_put(u: &mut UBins, b2: u32, w0: u64, uh: u64, ul: u64) {
    let pay = (w0 >> 4) & 0x1FFF_FFFF;
    let pur = (pay >> 5 != 0) as u64;
    let lo = (w0 >> 33) << 1 | pur;
    if uh == 0 && ul < SHORT_END {
        let m = mix42(ul);
        let rb = 42 - b2;
        let q = (m >> rb) as usize;
        u.s.push1(q, (m & ((1u64 << rb) - 1)) << 32 | lo);
        if pur != 0 {
            u.s.push1(q, pay);
            unsafe { *u.np.get_unchecked_mut(q) += 1 };
        }
    } else {
        let h = hash(Key { hi: uh, lo: ul });
        let q = part(h, b2);
        let a = h32(h, b2) << 32 | lo;
        if pur != 0 {
            u.w.push(q, [a, ul, uh, pay]);
        } else {
            u.w.push(q, [a, ul, uh]);
        }
    }
}
pub struct CBins {
    s: ELane<CKS>,
    w: ELane<CKS>,
    np: usize,
    ar: EArena,
}
const CKS: u32 = 10;
impl CBins {
    fn new(np: usize) -> CBins {
        CBins { s: ELane::empty(), w: ELane::empty(), np, ar: EArena::new() }
    }
    const fn mem(np: usize) -> usize {
        2 * ((ELane::<CKS>::mem(np) + 63) & !63)
    }
    unsafe fn init(&mut self, m: *mut u8, arp: *mut EArena) {
        let sz = (ELane::<CKS>::mem(self.np) + 63) & !63;
        self.s.init(self.np, m, arp);
        self.w.init(self.np, m.add(sz), arp);
    }
    fn finish(&mut self) {
        unsafe {
            if !self.s.cur.is_null() {
                self.s.drain();
                self.w.drain();
            }
            std::arch::x86_64::_mm_sfence();
        }
    }
}
#[inline(always)]
unsafe fn ckrec_put(sc: &mut ECur<CKS>, wc: &mut ECur<CKS>, b2: u32, rel: u64, camp: u32, uk: Key, ck: Key) {
    use std::arch::x86_64::_mm_set_epi64x;
    let cshort = is_short(ck);
    let bw = (camp as u64) << 42 | if cshort { ck.lo } else { 0 };
    let lo = rel << 1 | (!cshort) as u64;
    if is_short(uk) {
        let m = mix42(uk.lo);
        let rb = 42 - b2;
        let q = (m >> rb) as usize;
        let a = (m & ((1u64 << rb) - 1)) << 32 | lo;
        sc.push2(q, _mm_set_epi64x(bw as i64, a as i64));
        if !cshort {
            sc.push2(q, _mm_set_epi64x(ck.hi as i64, ck.lo as i64));
        }
        sc.relief();
    } else {
        let h = hash(uk);
        let q = part(h, b2);
        let a = h32(h, b2) << 32 | lo;
        wc.push2(q, _mm_set_epi64x(uk.lo as i64, a as i64));
        wc.push2(q, _mm_set_epi64x(bw as i64, uk.hi as i64));
        if !cshort {
            wc.push2(q, _mm_set_epi64x(ck.hi as i64, ck.lo as i64));
        }
        wc.relief();
    }
}

#[inline(always)]
fn opq64(x: u64) -> u64 {
    #[cfg(feature = "w512")]
    {
        x
    }
    #[cfg(not(feature = "w512"))]
    {
        let r: u64;
        unsafe { core::arch::asm!("/* {0} */", inout(reg) x => r, options(pure, nomem, nostack, preserves_flags)) };
        r
    }
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn opqv(v: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    let r: std::arch::x86_64::__m256i;
    core::arch::asm!("/* {0} */", inout(ymm_reg) v => r, options(pure, nomem, nostack, preserves_flags));
    r
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn opqx(v: std::arch::x86_64::__m128i) -> std::arch::x86_64::__m128i {
    let r: std::arch::x86_64::__m128i;
    core::arch::asm!("/* {0} */", inout(xmm_reg) v => r, options(pure, nomem, nostack, preserves_flags));
    r
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn max4_epu64(v: std::arch::x86_64::__m256i) -> u64 {
    use std::arch::x86_64::*;
    let x = _mm_max_epu64(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
    _mm_cvtsi128_si64(_mm_max_epu64(x, _mm_unpackhi_epi64(x, x))) as u64
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn or8_epi32(v: std::arch::x86_64::__m256i) -> u32 {
    use std::arch::x86_64::*;
    let x = _mm_or_si128(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
    let y = _mm_cvtsi128_si64(_mm_or_si128(x, _mm_unpackhi_epi64(x, x))) as u64;
    (y | y >> 32) as u32
}

trait Rec: Copy {
    fn w0(&self) -> u64;
    fn ext(&self) -> (u64, u64);
    fn last(&self) -> u64;
    #[inline(always)]
    fn lo32(&self) -> u32 {
        self.w0() as u32
    }
    #[inline(always)]
    fn ukey(&self) -> (u32, u64, u64) {
        let (a, b) = self.ext();
        ((self.w0() >> 32) as u32, a, b)
    }
    fn usort(v: &mut [Self]) {
        v.sort_unstable_by_key(|r| r.ukey());
    }
    #[inline(always)]
    fn uless(&self, o: &Self) -> bool {
        self.ukey() < o.ukey()
    }
    #[inline(always)]
    fn usame(&self, o: &Self) -> bool {
        self.ukey() == o.ukey()
    }
    #[inline(always)]
    fn unordered(v: &[Self], from: usize) -> usize {
        let mut i = from;
        while i < v.len() && !unsafe { v.get_unchecked(i).uless(v.get_unchecked(i - 1)) } {
            i += 1;
        }
        i
    }
}
impl Rec for u64 {
    #[inline(always)]
    fn usort(v: &mut [Self]) {
        v.sort_unstable();
    }
    #[cfg(feature = "w512")]
    #[inline(always)]
    fn unordered(v: &[Self], from: usize) -> usize {
        use std::arch::x86_64::*;
        let n = v.len();
        let mut i = from;
        unsafe {
            let p = v.as_ptr();
            while i + 8 <= n {
                let a = _mm512_srli_epi64::<32>(_mm512_loadu_si512(p.add(i) as *const _));
                let b = _mm512_srli_epi64::<32>(_mm512_loadu_si512(p.add(i - 1) as *const _));
                let m = _mm512_cmplt_epu64_mask(a, b);
                if m != 0 {
                    return i + m.trailing_zeros() as usize;
                }
                i += 8;
            }
            while i < n && *p.add(i) >> 32 >= *p.add(i - 1) >> 32 {
                i += 1;
            }
        }
        i
    }
    #[cfg(not(feature = "w512"))]
    #[inline(always)]
    fn unordered(v: &[Self], from: usize) -> usize {
        use std::arch::x86_64::*;
        let n = v.len();
        let mut i = from;
        unsafe {
            let p = v.as_ptr();
            while i + 4 <= n {
                let a = _mm256_srli_epi64::<32>(_mm256_loadu_si256(p.add(i) as *const _));
                let b = _mm256_srli_epi64::<32>(_mm256_loadu_si256(p.add(i - 1) as *const _));
                let m = _mm256_cmplt_epu64_mask(a, b);
                if m != 0 {
                    return i + m.trailing_zeros() as usize;
                }
                i += 4;
            }
            while i < n && *p.add(i) >> 32 >= *p.add(i - 1) >> 32 {
                i += 1;
            }
        }
        i
    }
    #[inline(always)]
    fn uless(&self, o: &Self) -> bool {
        *self >> 32 < *o >> 32
    }
    #[inline(always)]
    fn usame(&self, o: &Self) -> bool {
        (*self ^ *o) >> 32 == 0
    }
    #[inline(always)]
    fn w0(&self) -> u64 {
        *self
    }
    #[inline(always)]
    fn ext(&self) -> (u64, u64) {
        (0, 0)
    }
    #[inline(always)]
    fn last(&self) -> u64 {
        *self
    }
}
impl Rec for [u64; 2] {
    #[inline(always)]
    fn usort(v: &mut [Self]) {
        v.sort_unstable_by_key(|r| r[0]);
    }
    #[inline(always)]
    fn uless(&self, o: &Self) -> bool {
        self[0] >> 32 < o[0] >> 32
    }
    #[inline(always)]
    fn usame(&self, o: &Self) -> bool {
        (self[0] ^ o[0]) >> 32 == 0
    }
    #[inline(always)]
    fn w0(&self) -> u64 {
        self[0]
    }
    #[inline(always)]
    fn ext(&self) -> (u64, u64) {
        (0, 0)
    }
    #[inline(always)]
    fn last(&self) -> u64 {
        self[1]
    }
}
impl Rec for [u64; 3] {
    #[inline(always)]
    fn w0(&self) -> u64 {
        self[0]
    }
    #[inline(always)]
    fn ext(&self) -> (u64, u64) {
        (self[1], self[2])
    }
    #[inline(always)]
    fn last(&self) -> u64 {
        self[2]
    }
}
impl Rec for [u64; 4] {
    #[inline(always)]
    fn w0(&self) -> u64 {
        self[0]
    }
    #[inline(always)]
    fn ext(&self) -> (u64, u64) {
        (self[1], self[2])
    }
    #[inline(always)]
    fn last(&self) -> u64 {
        self[3]
    }
}

const GBITS: u32 = 15;
struct GTab {
    cnt: Vec<u32>,
    used: Vec<u8>,
}
impl GTab {
    fn new() -> GTab {
        GTab { cnt: vec![0; 1 << GBITS], used: vec![0; 1 << GBITS] }
    }
    #[inline(always)]
    fn shift(n: usize, kbits: u32, maxb: u32) -> u32 {
        let bb = (usize::BITS - n.leading_zeros() + 2).clamp(8, maxb.min(GBITS)).min(kbits);
        32 + kbits - bb
    }
    #[inline(always)]
    fn count(&mut self, w0: u64, sh: u32) {
        unsafe {
            let b = (w0 >> sh) as usize;
            *self.cnt.get_unchecked_mut(b) += 1;
            *self.used.get_unchecked_mut(b) = 1;
        }
    }
    #[cfg(feature = "w512")]
    #[inline(always)]
    fn walk(&mut self, nb: usize, clear: bool, mut f: impl FnMut(&mut [u32], usize)) {
        use std::arch::x86_64::*;
        for wi in 0..nb / 64 {
            let mut x = unsafe {
                let p = self.used.as_mut_ptr().add(wi * 64);
                let v = _mm512_loadu_si512(p as *const _);
                let x = _mm512_test_epi8_mask(v, v);
                if clear && x != 0 {
                    _mm512_storeu_si512(p as *mut _, _mm512_setzero_si512());
                }
                x
            };
            while x != 0 {
                let b = wi * 64 + x.trailing_zeros() as usize;
                x &= x - 1;
                f(&mut self.cnt, b);
            }
        }
    }
    #[cfg(not(feature = "w512"))]
    #[inline(always)]
    fn walk(&mut self, nb: usize, clear: bool, mut f: impl FnMut(&mut [u32], usize)) {
        use std::arch::x86_64::*;
        for wi in 0..nb / 64 {
            let mut x = unsafe {
                let p = self.used.as_mut_ptr().add(wi * 64);
                let (v0, v1) = (_mm256_loadu_si256(p as *const _), _mm256_loadu_si256(p.add(32) as *const _));
                let x = _mm256_test_epi8_mask(v0, v0) as u64 | opq64(_mm256_test_epi8_mask(v1, v1) as u64) << 32;
                if clear && x != 0 {
                    _mm256_storeu_si256(p as *mut _, _mm256_setzero_si256());
                    _mm256_storeu_si256(p.add(32) as *mut _, _mm256_setzero_si256());
                }
                x
            };
            while x != 0 {
                let b = wi * 64 + x.trailing_zeros() as usize;
                x &= x - 1;
                f(&mut self.cnt, b);
            }
        }
    }
}
#[inline(never)]
fn group<T: Rec>(src: &[T], dst: &mut Vec<T>, g: &mut GTab, kbits: u32, maxb: u32, counted: Option<u32>) {
    let n = src.len();
    dst.clear();
    if n == 0 {
        return;
    }
    if n >= u32::MAX as usize {
        fallback("a user partition with 2^32 records");
    }
    let sh = counted.unwrap_or(GTab::shift(n, kbits, maxb));
    let nb = 1usize << (32 + kbits - sh);
    if counted.is_none() {
        for r in src {
            g.count(r.w0(), sh);
        }
    }
    let mut s = 0u32;
    g.walk(nb, false, |cnt, b| unsafe {
        let v = *cnt.get_unchecked(b);
        *cnt.get_unchecked_mut(b) = s;
        s += v;
    });
    dst.reserve(n);
    let d = dst.as_mut_ptr();
    let c = g.cnt.as_mut_ptr();
    for r in src {
        unsafe {
            let p = c.add((r.w0() >> sh) as usize);
            *d.add(*p as usize) = *r;
            *p += 1;
        }
    }
    unsafe { dst.set_len(n) };
    let mut i = 1;
    loop {
        i = T::unordered(dst, i);
        if i >= n {
            break;
        }
        let b = (dst[i].w0() >> sh) as usize;
        let e = g.cnt[b] as usize;
        let mut j = i - 1;
        while j > 0 && (dst[j - 1].w0() >> sh) as usize == b {
            j -= 1;
        }
        T::usort(&mut dst[j..e]);
        i = e + 1;
    }
    g.walk(nb, true, |cnt, b| unsafe { *cnt.get_unchecked_mut(b) = 0 });
}

#[repr(C)]
struct Sess {
    start: i64,
    day: i32,
    row: u32,
    rev: u64,
}
const PURCH: u64 = 1 << 63;
impl Sess {
    #[inline(always)]
    fn purch(&self) -> bool {
        self.rev >> 63 != 0
    }
    #[inline(always)]
    fn revenue(&self) -> u64 {
        self.rev & !PURCH
    }
}
#[derive(Clone, Copy)]
#[repr(C)]
struct Bin {
    mn: u32,
    mx: u32,
    cnt: u32,
    si: u32,
}
const BIN0: Bin = Bin { mn: u32::MAX, mx: 0, cnt: 0, si: 0 };
struct SBins {
    occ: Vec<u8>,
    b: Vec<Bin>,
    cs: Vec<u64>,
    rw: Vec<[u64; 2]>,
    sm: u64,
    rv: Vec<u64>,
    tp: Vec<u8>,
}
impl SBins {
    fn new(wlen: u64) -> SBins {
        let nb = (wlen / GAP_MS as u64) as usize + 2;
        let nw = nb.div_ceil(64);
        SBins { occ: vec![0; nw * 64], b: vec![BIN0; nw * 64], cs: Vec::new(), rw: Vec::with_capacity(nw * 64 + 2), sm: 0, rv: Vec::new(), tp: Vec::new() }
    }
    #[inline(always)]
    fn add(&mut self, it: impl Iterator<Item = u32>) {
        use std::arch::x86_64::*;
        let (occ, bp) = (self.occ.as_mut_ptr(), self.b.as_mut_ptr());
        let pm = pkpm::on();
        let mut pl = [0u64; 3];
        if pm {
            pl = pkpm::rd();
        }
        unsafe {
            let one = _mm_setr_epi32(0, 0, 1, 0);
            let mut sm = self.sm;
            for rel in it {
                let bi = (rel / GAP_MS as u32) as usize;
                let w_idx = bi >> 6;
                if w_idx < 64 {
                    sm |= 1u64 << w_idx;
                }
                *occ.add(bi) = 0xFF;
                let p = bp.add(bi) as *mut __m128i;
                let v = _mm_loadu_si128(p);
                let r = _mm_set1_epi32(rel as i32);
                let x = _mm_blend_epi32::<0b0010>(_mm_min_epu32(v, r), _mm_max_epu32(v, r));
                _mm_storeu_si128(p, _mm_blend_epi32::<0b1100>(x, _mm_add_epi32(v, one)));
            }
            self.sm = sm;
        }
        if pm {
            pkpm::lap(&mut pl, 6);
        }
    }
}

struct Acc {
    ses: Vec<u16>,
    asst: Vec<u32>,
    ses_hi: Vec<u64>,
    pur: Vec<[u64; 2]>,
}
impl Acc {
    fn new(n: usize) -> Acc {
        Acc { ses: vec![0; n], asst: vec![0; n + 16], ses_hi: vec![0; n], pur: vec![[0; 2]; n] }
    }
    #[inline(always)]
    fn add16(lo: &mut [u16], hi: &mut [u64], i: usize, v: u32) {
        let x = lo[i] as u64 + v as u64;
        lo[i] = x as u16;
        if x > 0xFFFF {
            hi[i] += x & !0xFFFF;
        }
    }
}

#[derive(Default)]
struct StrBuf {
    off: Vec<u32>,
    data: Vec<u8>,
}
impl StrBuf {
    fn new() -> StrBuf {
        StrBuf { off: vec![0], data: Vec::new() }
    }
    fn push(&mut self, s: &[u8]) {
        self.data.extend_from_slice(s);
        self.off.push(self.data.len() as u32);
    }
    fn col(&self) -> pqw::StrCol<'_> {
        pqw::StrCol { offsets: &self.off, data: &self.data }
    }
}

struct Rows {
    names: StrBuf,
    ukeys: Vec<u32>,
    day: Vec<i32>,
    ses: Vec<i64>,
    evs: Vec<i64>,
    rev: Vec<i64>,
    top: Vec<u8>,
}

struct T3 {
    eb: Box<EvBatch>,
    cb: Box<CkBatch>,
    lb: Box<LkBatch>,
    ev: WBins,
    ck: CBins,
    lk: Vec<(Key, Key)>,
    u2: UBins,
    rows: Rows,
    acc: Acc,
    tab: Tab,
    sl: Vec<(*mut u64, usize)>,
    tab4: Vec<u32>,
    kt: KTab,
    w0s: Vec<u64>,
    ck64: Vec<u64>,
    stamp: Vec<u32>,
    serial: u32,
    lkv: Vec<[u64; 2]>,
    prv: Vec<URec>,
    names: Vec<Key>,
    pid: Vec<u32>,
    off: Vec<u32>,
    pos: Vec<u32>,
    pe: Vec<u64>,
    cpid: Vec<u32>,
    coff: Vec<u32>,
    sess: Vec<Sess>,
    starts: Vec<i64>,
    days: Vec<i32>,
    day_end: Vec<u32>,
    catsum: [u64; 32],
    ct: Vec<(u32, i64)>,
    rs: Vec<u32>,
    n_rows: [u64; 4],
    prof: [u64; 16],
    st: [u64; 16],
    sb: SBins,
}
#[inline(always)]
fn tick(t: &mut std::time::Instant) -> u64 {
    let n = std::time::Instant::now();
    let d = (n - *t).as_nanos() as u64;
    *t = n;
    d
}

struct Odd {
    map: std::collections::HashMap<Vec<u8>, u32>,
    strs: Vec<Vec<u8>>,
}

struct Cx<'a> {
    g: &'a G,
    b1: u32,
    b2: u32,
    queue: Vec<crate::Chunk>,
    qnext: AtomicUsize,
    next: AtomicUsize,
    lk_left: AtomicUsize,
    lkh: Mutex<Option<std::thread::JoinHandle<[u64; 3]>>>,
    t: Vec<UnsafeCell<T3>>,
    odd: Mutex<Odd>,
    odds: UnsafeCell<Vec<Vec<u8>>>,
    bk: UnsafeCell<Back>,
}
unsafe impl<'a> Sync for Cx<'a> {}
impl<'a> Cx<'a> {
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn t(&self, tid: usize) -> &mut T3 {
        unsafe { &mut *self.t[tid].get() }
    }
    fn odds(&self) -> &Vec<Vec<u8>> {
        unsafe { &*self.odds.get() }
    }
}

#[inline]
fn id_text(k: Key, prefix: u8, odds: &[Vec<u8>], out: &mut Vec<u8>) {
    out.clear();
    if k.hi != 0 {
        out.push(prefix);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for w in [k.hi, k.lo] {
            for i in (0..16).rev() {
                out.push(HEX[(w >> (4 * i)) as usize & 15]);
            }
        }
    } else if k.lo & ODDK != 0 {
        out.extend_from_slice(&odds[(k.lo & !ODDK) as usize]);
    } else {
        let (v, len) = enc36(k.lo);
        out.push(prefix);
        out.extend_from_slice(&v.to_be_bytes()[..len]);
    }
}
#[inline]
fn id_cmp(a: Key, b: Key, prefix: u8, odds: &[Vec<u8>]) -> std::cmp::Ordering {
    if a == b {
        return std::cmp::Ordering::Equal;
    }
    if a.hi == 0 && b.hi == 0 && (a.lo | b.lo) & ODDK == 0 {
        return enc36(a.lo).0.cmp(&enc36(b.lo).0);
    }
    if a.hi != 0 && b.hi != 0 {
        return (a.hi, a.lo).cmp(&(b.hi, b.lo));
    }
    let (mut x, mut y) = (Vec::with_capacity(40), Vec::with_capacity(40));
    id_text(a, prefix, odds, &mut x);
    id_text(b, prefix, odds, &mut y);
    x.cmp(&y)
}

#[inline(always)]
unsafe fn mulu_ymm(a: std::arch::x86_64::__m256i, b: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    let o: std::arch::x86_64::__m256i;
    std::arch::asm!("vpmuludq {o}, {a}, {b}", o = lateout(ymm_reg) o, a = in(ymm_reg) a, b = in(ymm_reg) b, options(pure, nomem, nostack, preserves_flags));
    o
}

const EVR: usize = NB + 8;
#[inline(never)]
unsafe fn ev_se(w: &mut ELane, eb: &EvBatch, r: *const u64, sei: *const u64, nse: usize) -> u64 {
    let mut bad = 0u64;
    let mut wc = w.cursor();
    for j in 0..nse {
        let i = *sei.add(j) as usize & (NB - 1);
        let uid = *eb.uid.get_unchecked(i);
        let long = uid >= SHORT_END;
        let uh = std::hint::select_unpredictable(long, *eb.uhi.get_unchecked(i), 0);
        let ul = std::hint::select_unpredictable(long, *eb.ulo.get_unchecked(i), uid);
        bad |= (long & (uh == 0)) as u64;
        wc.push4(*r.add(i) as usize, [*r.add(EVR + i), uh, ul, *r.add(2 * EVR + i) | WT_SE]);
    }
    w.put(wc);
    bad
}
#[inline(never)]
unsafe fn ev_le(w: &mut ELane, eb: &EvBatch, r: *const u64, lei: *const u64, nle: usize, b1: u32) -> u64 {
    let mut mn = u64::MAX;
    let ws = 64 - b1;
    let mut wc = w.cursor();
    for j in 0..nle {
        let i = *lei.add(j) as usize & (NB - 1);
        let ek = Key { hi: *eb.ehi.get_unchecked(i), lo: *eb.elo.get_unchecked(i) };
        mn = mn.min(ek.hi);
        let w0 = *r.add(2 * EVR + i);
        let m = ((w0 != WT_INV) as u64).wrapping_neg();
        let is = (*r.add(EVR + i) as u32 >> 11) as u64;
        wc.push4((hash(ek) >> ws) as usize, [ek.hi, ek.lo, is << 42 | (*eb.uid.get_unchecked(i) & m), w0 | WT_LE]);
    }
    w.put(wc);
    (mn == 0) as u64
}
#[inline(never)]
unsafe fn ev_xl(x: &mut ELane, eb: &EvBatch, r: *const u64, xi: *const u64, nx: usize, b1: u32, ing_base: i64) -> u64 {
    let mut mn = u64::MAX;
    let ws = 64 - b1;
    for j in 0..nx {
        let i = *xi.add(j) as usize & (NB - 1);
        let ek = Key { hi: *eb.ehi.get_unchecked(i), lo: *eb.elo.get_unchecked(i) };
        let (uh, ul) = (*eb.uhi.get_unchecked(i), *eb.ulo.get_unchecked(i));
        mn = mn.min(ek.hi).min(uh);
        let ir = (*eb.ing.get_unchecked(i)).wrapping_sub(ing_base) as u64;
        x.push8((hash(ek) >> ws) as usize, [ek.hi, ek.lo, uh, *r.add(2 * EVR + i) | WT_X, ul, ir, 0, 0]);
    }
    (mn == 0) as u64
}
#[inline(never)]
unsafe fn ck_f(s: &mut ELane<CKS>, sp: *const u64, sr: *const u64, ns: usize, ctab: *const u8) {
    use std::arch::x86_64::*;
    let mut sc = s.cursor();
    for j in 0..ns {
        if *ctab.add((*sr.add(2 * j + 1) >> 42) as usize) == 255 {
            continue;
        }
        sc.push2(*sp.add(j) as usize, _mm_loadu_si128(sr.add(2 * j) as *const _));
    }
    s.put(sc);
}
#[inline(never)]
unsafe fn ck_c(s: &mut ELane<CKS>, cb: &CkBatch, ci: *const u64, nc: usize, pv: *const u64, av: *const u64, bv: *const u64, ctab: *const u8) -> u64 {
    use std::arch::x86_64::*;
    let mut bad = 0u64;
    let mut sc = s.cursor();
    for j in 0..nc {
        let i = *ci.add(j) as usize & (NB - 1);
        if *ctab.add((*bv.add(i) >> 42) as usize) == 255 {
            continue;
        }
        let p = *pv.add(i) as usize;
        let (ch, cl) = (*cb.chi.get_unchecked(i), *cb.clo.get_unchecked(i));
        bad |= (ch == 0) as u64;
        sc.push2(p, _mm_set_epi64x(*bv.add(i) as i64, *av.add(i) as i64));
        sc.push2(p, _mm_set_epi64x(ch as i64, cl as i64));
    }
    s.put(sc);
    bad
}
#[inline(never)]
unsafe fn ck_wide(w: &mut ELane<CKS>, cb: &CkBatch, ui: *const u64, nu: usize, cbase: i64, b2: u32, ctab: *const u8) -> u64 {
    use std::arch::x86_64::*;
    let mut bad = 0u64;
    let mut wc = w.cursor();
    let ws = 64 - b2;
    for j in 0..nu {
        let i = *ui.add(j) as usize & (NB - 1);
        let camp = *cb.camp.get_unchecked(i) as u32 as u64;
        if *ctab.add(camp as usize) == 255 {
            continue;
        }
        let rel = (*cb.ts.get_unchecked(i)).wrapping_sub(cbase) as u64;
        let (uh, ul, cid) = (*cb.uhi.get_unchecked(i), *cb.ulo.get_unchecked(i), *cb.cid.get_unchecked(i));
        bad |= (uh == 0) as u64;
        let h = hash(Key { hi: uh, lo: ul });
        let q = (h >> ws) as usize;
        let a = h32(h, b2) << 32 | rel << 1;
        if cid < SHORT_END {
            wc.push2(q, _mm_set_epi64x(ul as i64, a as i64));
            wc.push2(q, _mm_set_epi64x((camp << 42 | cid) as i64, uh as i64));
        } else {
            let (ch, cl) = (*cb.chi.get_unchecked(i), *cb.clo.get_unchecked(i));
            bad |= (ch == 0) as u64;
            wc.push2(q, _mm_set_epi64x(ul as i64, (a | 1) as i64));
            wc.push2(q, _mm_set_epi64x((camp << 42) as i64, uh as i64));
            wc.push2(q, _mm_set_epi64x(ch as i64, cl as i64));
        }
    }
    w.put(wc);
    bad
}

struct Sink<'a, 'b> {
    cx: &'a Cx<'b>,
    t: &'a mut T3,
    e: &'a mut P3,
    base: *const u8,
    fend: *const u8,
}
impl<'a, 'b> Sink<'a, 'b> {
    #[cold]
    #[inline(never)]
    fn odd_key(&mut self, id: u64) -> Key {
        unsafe {
            let p = self.base.add(odd_offset(id));
            let mut len = odd_len(id);
            if len >= 255 {
                while p.add(len) < self.fend {
                    let c = *p.add(len);
                    if c == b',' || c == b'"' || c == b'\n' {
                        break;
                    }
                    len += 1;
                }
            }
            let s = std::slice::from_raw_parts(p, len);
            let mut o = self.cx.odd.lock().unwrap_or_else(|e| e.into_inner());
            let idx = match o.map.get(s) {
                Some(&i) => i,
                None => {
                    let i = o.map.len() as u32;
                    o.strs.push(s.to_vec());
                    o.map.insert(s.to_vec(), i);
                    i
                }
            };
            Key { hi: 0, lo: ODDK | idx as u64 }
        }
    }
    #[inline(always)]
    fn key(&mut self, w: u64, hi: u64, lo: u64) -> Key {
        if w & ID_ODD != 0 {
            return self.odd_key(w);
        }
        if w & ID_LONG != 0 {
            if hi == 0 {
                fallback("long id with a zero high half");
            }
            return Key { hi, lo };
        }
        Key { hi: 0, lo: w }
    }
    #[inline(never)]
    fn ev_batch(&mut self) {
        use std::arch::x86_64::*;
        let n = self.t.eb.n;
        self.t.eb.n = 0;
        if unsafe { SKIP } & 1 != 0 {
            return;
        }
        let prof = unsafe { KN[7] } != 0;
        let c0 = if prof { unsafe { core::arch::x86_64::_rdtsc() } } else { 0 };
        let g = self.cx.g;
        let b1 = self.cx.b1;
        let sh = unsafe { ESH };
        if (TS_BAD.wrapping_sub(g.ing_base) as u64) < 1000 << 21 {
            fallback("ingest base");
        }
        let mut sp = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut sr = std::mem::MaybeUninit::<[u64; 2 * NB + 16]>::uninit();
        let mut wi = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut sei = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut lei = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut xli = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut pfi = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut rw = std::mem::MaybeUninit::<[u64; 3 * EVR]>::uninit();
        let (sp, sr, wi, sei, lei, pfi) = (sp.as_mut_ptr() as *mut u64, sr.as_mut_ptr() as *mut u64, wi.as_mut_ptr() as *mut u64, sei.as_mut_ptr() as *mut u64, lei.as_mut_ptr() as *mut u64, pfi.as_mut_ptr() as *mut u64);
        let (xli, rw) = (xli.as_mut_ptr() as *mut u64, rw.as_mut_ptr() as *mut u64);
        let (pv, a0v, w0v) = unsafe { (rw, rw.add(EVR), rw.add(2 * EVR)) };
        let (mut ns, mut nwd, mut nse, mut nle, mut nxl, mut npf) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        unsafe {
            let eb: &EvBatch = &self.t.eb;
            #[cfg(feature = "w512")]
            {
                let c = |x: u64| _mm512_set1_epi64(x as i64);
                let (v_ws, v_wlen, v_base) = (c(g.ws as u64), c(g.wlen), c(g.ing_base as u64));
                let v_short = c(SHORT_END);
                let v_sh = _mm_cvtsi64_si128(sh as i64);
                let v_low = c((1u64 << sh) - 1);
                let v_pm = c((1u64 << b1) - 1);
                let v_div = _mm256_set1_epi64x(274_877_907);
                let mut idx = _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0);
                let (il0, il1) = (_mm512_set_epi64(11, 3, 10, 2, 9, 1, 8, 0), _mm512_set_epi64(15, 7, 14, 6, 13, 5, 12, 4));
                let mut i = 0;
                while i < n {
                    let rows: __mmask8 = if n - i >= 8 { 0xFF } else { (1u8 << (n - i)) - 1 };
                    let eid = _mm512_loadu_si512(eb.eid.as_ptr().add(i) as *const _);
                    let uid = _mm512_loadu_si512(eb.uid.as_ptr().add(i) as *const _);
                    let ts = _mm512_loadu_si512(eb.ts.as_ptr().add(i) as *const _);
                    let ing = _mm512_loadu_si512(eb.ing.as_ptr().add(i) as *const _);
                    let pp = _mm512_loadu_si512(eb.pp.as_ptr().add(i) as *const _);
                    let rel = _mm512_sub_epi64(ts, v_ws);
                    let k_valid = _mm512_cmpneq_epi64_mask(uid, c(ID_EMPTY))
                        & _mm512_cmplt_epu64_mask(rel, v_wlen)
                        & _mm512_testn_epi64_mask(pp, c(PP_INVALID))
                        & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ing, ts), c(DAY_MS as u64));
                    let ir = _mm512_sub_epi64(ing, v_base);
                    let prod = _mm512_inserti64x4(_mm512_castsi256_si512(mulu_ymm(_mm512_castsi512_si256(ir), v_div)), mulu_ymm(_mm512_extracti64x4_epi64(ir, 1), v_div), 1);
                    let is = _mm512_srli_epi64(prod, 38);
                    let k_ing = _mm512_cmplt_epu64_mask(ir, c(1000 << 21)) & _mm512_testn_epi64_mask(prod, c(0x3FF << 28));
                    let k_purch = _mm512_cmpeq_epi64_mask(_mm512_and_si512(pp, c(3 << 56)), c((KIND_PURCHASE as u64) << 56));
                    let k_es = _mm512_cmplt_epu64_mask(eid, v_short);
                    let k_us = _mm512_cmplt_epu64_mask(uid, v_short);
                    let k_s = k_es & k_ing & (!k_valid | (k_us & !k_purch)) & rows;
                    let p = _mm512_and_si512(_mm512_srl_epi64(eid, v_sh), v_pm);
                    let rem = _mm512_or_si512(_mm512_and_si512(eid, v_low), _mm512_sll_epi64(_mm512_srli_epi64(eid, 40), v_sh));
                    let a0 = _mm512_or_si512(_mm512_slli_epi64(rem, 32), _mm512_slli_epi64(is, 11));
                    let a = _mm512_mask_or_epi64(a0, k_valid, a0, _mm512_srli_epi64(uid, 31));
                    let r33 = _mm512_slli_epi64(rel, 33);
                    let bv = _mm512_ternarylogic_epi64(r33, _mm512_slli_epi64(uid, 2), c(0x7FFF_FFFF << 2), 0xF8);
                    let b = _mm512_mask_blend_epi64(k_valid, c(2), bv);
                    _mm512_storeu_si512(pv.add(i) as *mut _, p);
                    _mm512_storeu_si512(a0v.add(i) as *mut _, a0);
                    _mm512_storeu_si512(w0v.add(i) as *mut _, _mm512_mask_blend_epi64(k_valid, c(WT_INV), r33));
                    _mm512_storeu_si512(sp.add(ns) as *mut _, _mm512_maskz_compress_epi64(k_s, p));
                    let (ca, cb) = (_mm512_maskz_compress_epi64(k_s, a), _mm512_maskz_compress_epi64(k_s, b));
                    _mm512_storeu_si512(sr.add(2 * ns) as *mut _, _mm512_permutex2var_epi64(ca, il0, cb));
                    _mm512_storeu_si512(sr.add(2 * ns + 8) as *mut _, _mm512_permutex2var_epi64(ca, il1, cb));
                    ns += k_s.count_ones() as usize;
                    let v_cls = c(ID_ODD | ID_LONG);
                    let k_el = _mm512_cmpeq_epi64_mask(_mm512_and_si512(eid, v_cls), c(ID_LONG));
                    let k_ul = _mm512_cmpeq_epi64_mask(_mm512_and_si512(uid, v_cls), c(ID_LONG));
                    let k_se = rows & !k_s & k_es & k_ing & k_valid & (k_us | k_ul);
                    let k_le = rows & k_el & k_ing & (!k_valid | k_us);
                    let k_xl = rows & k_el & k_ing & k_valid & k_ul;
                    _mm512_storeu_si512(sei.add(nse) as *mut _, _mm512_maskz_compress_epi64(k_se, idx));
                    nse += k_se.count_ones() as usize;
                    _mm512_storeu_si512(lei.add(nle) as *mut _, _mm512_maskz_compress_epi64(k_le, idx));
                    nle += k_le.count_ones() as usize;
                    _mm512_storeu_si512(xli.add(nxl) as *mut _, _mm512_maskz_compress_epi64(k_xl, idx));
                    nxl += k_xl.count_ones() as usize;
                    let k_pf = (k_se | k_le | k_xl) & k_purch & k_valid;
                    _mm512_storeu_si512(pfi.add(npf) as *mut _, _mm512_maskz_compress_epi64(k_pf, idx));
                    npf += k_pf.count_ones() as usize;
                    let k_w = !k_s & !k_se & !k_le & !k_xl & rows;
                    _mm512_storeu_si512(wi.add(nwd) as *mut _, _mm512_maskz_compress_epi64(k_w, idx));
                    nwd += k_w.count_ones() as usize;
                    idx = _mm512_add_epi64(idx, c(8));
                    i += 8;
                }
            }
            #[cfg(not(feature = "w512"))]
            {
                let c = |x: u64| _mm256_set1_epi64x(x as i64);
                let (v_ws, v_wlen, v_base) = (c(g.ws as u64), c(g.wlen), c(g.ing_base as u64));
                let v_short = c(SHORT_END);
                let v_sh = _mm_cvtsi64_si128(sh as i64);
                let v_low = c((1u64 << sh) - 1);
                let v_pm = c((1u64 << b1) - 1);
                let v_div = _mm256_set1_epi64x(274_877_907);
                let (v_empty, v_pinv, v_day, v_irmax, v_rnd) = (c(ID_EMPTY), c(PP_INVALID), c(DAY_MS as u64), c(1000 << 21), c(0x3FF << 28));
                let (v_k3, v_kp, v_cls, v_long) = (c(3 << 56), c((KIND_PURCHASE as u64) << 56), c(ID_ODD | ID_LONG), c(ID_LONG));
                let (v_two, v_inv, v_um, v_four) = (c(2), c(WT_INV), c(0x7FFF_FFFF << 2), c(4));
                let mut idx = _mm256_set_epi64x(3, 2, 1, 0);
                let (il0, il1) = (_mm256_set_epi64x(5, 1, 4, 0), _mm256_set_epi64x(7, 3, 6, 2));
                let mut i = 0;
                while i < n {
                    let rows: __mmask8 = if n - i >= 4 { 0xF } else { (1u8 << (n - i)) - 1 };
                    let eid = _mm256_loadu_si256(eb.eid.as_ptr().add(i) as *const _);
                    let uid = _mm256_loadu_si256(eb.uid.as_ptr().add(i) as *const _);
                    let ts = _mm256_loadu_si256(eb.ts.as_ptr().add(i) as *const _);
                    let ing = _mm256_loadu_si256(eb.ing.as_ptr().add(i) as *const _);
                    let pp = _mm256_loadu_si256(eb.pp.as_ptr().add(i) as *const _);
                    let rel = _mm256_sub_epi64(ts, v_ws);
                    let k_valid = _mm256_cmpneq_epi64_mask(uid, v_empty)
                        & _mm256_cmplt_epu64_mask(rel, v_wlen)
                        & _mm256_testn_epi64_mask(pp, v_pinv)
                        & _mm256_cmple_epi64_mask(_mm256_sub_epi64(ing, ts), v_day);
                    let ir = _mm256_sub_epi64(ing, v_base);
                    let prod = mulu_ymm(ir, v_div);
                    let is = _mm256_srli_epi64(prod, 38);
                    let k_ing = _mm256_cmplt_epu64_mask(ir, v_irmax) & _mm256_testn_epi64_mask(prod, v_rnd);
                    let k_purch = _mm256_cmpeq_epi64_mask(_mm256_and_si256(pp, v_k3), v_kp);
                    let k_es = _mm256_cmplt_epu64_mask(eid, v_short);
                    let k_us = _mm256_cmplt_epu64_mask(uid, v_short);
                    let k_s = k_es & k_ing & (!k_valid | (k_us & !k_purch)) & rows;
                    let p = _mm256_and_si256(_mm256_srl_epi64(eid, v_sh), v_pm);
                    let rem = _mm256_or_si256(_mm256_and_si256(eid, v_low), _mm256_sll_epi64(_mm256_srli_epi64(eid, 40), v_sh));
                    let a0 = _mm256_or_si256(_mm256_slli_epi64(rem, 32), _mm256_slli_epi64(is, 11));
                    let a = _mm256_mask_or_epi64(a0, k_valid, a0, _mm256_srli_epi64(uid, 31));
                    let r33 = _mm256_slli_epi64(rel, 33);
                    let bv = _mm256_ternarylogic_epi64(r33, _mm256_slli_epi64(uid, 2), v_um, 0xF8);
                    let b = _mm256_mask_blend_epi64(k_valid, v_two, bv);
                    _mm256_storeu_si256(pv.add(i) as *mut _, p);
                    _mm256_storeu_si256(a0v.add(i) as *mut _, a0);
                    _mm256_storeu_si256(w0v.add(i) as *mut _, _mm256_mask_blend_epi64(k_valid, v_inv, r33));
                    _mm256_storeu_si256(sp.add(ns) as *mut _, _mm256_maskz_compress_epi64(k_s, p));
                    let (ca, cb) = (_mm256_maskz_compress_epi64(k_s, a), _mm256_maskz_compress_epi64(k_s, b));
                    _mm256_storeu_si256(sr.add(2 * ns) as *mut _, _mm256_permutex2var_epi64(ca, il0, cb));
                    _mm256_storeu_si256(sr.add(2 * ns + 4) as *mut _, _mm256_permutex2var_epi64(ca, il1, cb));
                    ns += k_s.count_ones() as usize;
                    let k_el = _mm256_cmpeq_epi64_mask(_mm256_and_si256(eid, v_cls), v_long);
                    let k_ul = _mm256_cmpeq_epi64_mask(_mm256_and_si256(uid, v_cls), v_long);
                    let k_se = rows & !k_s & k_es & k_ing & k_valid & (k_us | k_ul);
                    let k_le = rows & k_el & k_ing & (!k_valid | k_us);
                    let k_xl = rows & k_el & k_ing & k_valid & k_ul;
                    _mm256_storeu_si256(sei.add(nse) as *mut _, _mm256_maskz_compress_epi64(k_se, idx));
                    nse += k_se.count_ones() as usize;
                    _mm256_storeu_si256(lei.add(nle) as *mut _, _mm256_maskz_compress_epi64(k_le, idx));
                    nle += k_le.count_ones() as usize;
                    _mm256_storeu_si256(xli.add(nxl) as *mut _, _mm256_maskz_compress_epi64(k_xl, idx));
                    nxl += k_xl.count_ones() as usize;
                    let k_pf = (k_se | k_le | k_xl) & k_purch & k_valid;
                    _mm256_storeu_si256(pfi.add(npf) as *mut _, _mm256_maskz_compress_epi64(k_pf, idx));
                    npf += k_pf.count_ones() as usize;
                    let k_w = !k_s & !k_se & !k_le & !k_xl & rows;
                    _mm256_storeu_si256(wi.add(nwd) as *mut _, _mm256_maskz_compress_epi64(k_w, idx));
                    nwd += k_w.count_ones() as usize;
                    idx = _mm256_add_epi64(idx, v_four);
                    i += 4;
                }
            }
            let c1 = if prof { core::arch::x86_64::_rdtsc() } else { 0 };
            let mut sc = self.e.es.cursor();
            for j in 0..ns {
                sc.push2(*sp.add(j) as usize, _mm_loadu_si128(sr.add(2 * j) as *const _));
            }
            self.e.es.put(sc);
            if prof {
                let c2 = core::arch::x86_64::_rdtsc();
                self.e.cyc[6] += c1 - c0;
                self.e.cyc[7] += c2 - c1;
            }
        }
        if nse + nle + nxl > 0 {
            let eb: &EvBatch = &self.t.eb;
            unsafe {
                for j in 0..npf {
                    let i = *pfi.add(j) as usize & (NB - 1);
                    let pp = eb.pp[i];
                    let cat = *g.prod.get_unchecked((pp as u32 as usize).min(PBOUND)) as u64;
                    *w0v.add(i) |= ((pp >> 32) & 0xFF_FFFF) << 9 | cat << 4;
                }
                let mut bad = 0u64;
                if nse > 0 {
                    bad |= ev_se(&mut self.e.ew, eb, rw, sei, nse);
                }
                if nle > 0 {
                    bad |= ev_le(&mut self.e.ew, eb, rw, lei, nle, b1);
                }
                if nxl > 0 {
                    bad |= ev_xl(&mut self.e.ex, eb, rw, xli, nxl, b1, g.ing_base);
                }
                if bad != 0 {
                    fallback("long id with a zero high half");
                }
            }
        }
        if nwd > 0 {
            let mut wc = self.e.ew.cursor();
            for j in 0..nwd {
                let i = unsafe { *wi.add(j) } as usize & (NB - 1);
                let (eid, uid, ts, ing, pp) = (self.t.eb.eid[i], self.t.eb.uid[i], self.t.eb.ts[i], self.t.eb.ing[i], self.t.eb.pp[i]);
                if ing == TS_BAD {
                    fallback("ingest_ts bad");
                }
                let rel = ts.wrapping_sub(g.ws) as u64;
                let valid = uid != ID_EMPTY && rel < g.wlen && pp & PP_INVALID == 0 && ing.wrapping_sub(ts) <= DAY_MS;
                let ir = ing.wrapping_sub(g.ing_base) as u64;
                let is = ir / 1000;
                let ing_ok = is < 1 << 21 && is * 1000 == ir;
                self.ev_wide(&mut wc, i, eid, uid, rel, valid, ir, is, ing_ok, pp);
            }
            unsafe { self.e.ew.put(wc) };
        }
        if prof {
            self.e.cyc[0] += unsafe { core::arch::x86_64::_rdtsc() } - c0;
        }
        self.t.n_rows[0] += n as u64;
    }
    #[inline(always)]
    fn ev_wide(&mut self, wc: &mut ECur, i: usize, eid: u64, uid: u64, rel: u64, valid: bool, ir: u64, is: u64, ing_ok: bool, pp: u64) {
        let g = self.cx.g;
        let b1 = self.cx.b1;
        let sh = unsafe { ESH };
        let e_short = eid < SHORT_END;
        let ek = if e_short { Key { hi: 0, lo: eid } } else { self.key(eid, self.t.eb.ehi[i], self.t.eb.elo[i]) };
        let mut w0 = WT_INV;
        let mut uk = Key { hi: 0, lo: 0 };
        if valid {
            uk = if uid < SHORT_END { Key { hi: 0, lo: uid } } else { self.key(uid, self.t.eb.uhi[i], self.t.eb.ulo[i]) };
            let mut pc = 0u64;
            if (pp >> 56) & 3 == KIND_PURCHASE as u64 {
                let price = (pp >> 32) & 0xFF_FFFF;
                let prod = (pp as u32 as usize).min(PBOUND);
                let cat = unsafe { *g.prod.get_unchecked(prod) } as u64;
                pc = price << 9 | cat << 4;
            }
            w0 = rel << 33 | pc;
        }
        unsafe {
            if e_short && ing_ok {
                wc.push4(e_part(eid, sh, b1), [e_rem(eid, sh) << 32 | is << 11, uk.hi, uk.lo, w0 | WT_SE]);
            } else if ek.hi != 0 && ing_ok && uk.hi == 0 && uk.lo < SHORT_END {
                wc.push4(part(hash(ek), b1), [ek.hi, ek.lo, is << 42 | uk.lo, w0 | WT_LE]);
            } else {
                if ir.wrapping_add(1 << 61) >= 1 << 62 {
                    fallback("ingest_ts out of range");
                }
                let p = if e_short { e_part(eid, sh, b1) } else { part(hash(ek), b1) };
                self.e.ex.push8(p, [ek.hi, ek.lo, uk.hi, w0 | WT_X, uk.lo, ir, 0, 0]);
            }
        }
    }
    #[cfg(feature = "w512")]
    #[inline(never)]
    fn ck_batch(&mut self) {
        use std::arch::x86_64::*;
        let n = self.t.cb.n;
        self.t.cb.n = 0;
        if unsafe { SKIP } & 2 != 0 {
            return;
        }
        let prof = unsafe { KN[7] } != 0;
        let c0 = if prof { unsafe { core::arch::x86_64::_rdtsc() } } else { 0 };
        let g = self.cx.g;
        let b2 = self.cx.b2;
        let mut sp = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut sr = std::mem::MaybeUninit::<[u64; 2 * NB + 16]>::uninit();
        let mut ci = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut ui = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut oi = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut pv = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut av = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut bv = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let (sp, sr, ci, ui, oi) = (sp.as_mut_ptr() as *mut u64, sr.as_mut_ptr() as *mut u64, ci.as_mut_ptr() as *mut u64, ui.as_mut_ptr() as *mut u64, oi.as_mut_ptr() as *mut u64);
        let (pv, av, bv) = (pv.as_mut_ptr() as *mut u64, av.as_mut_ptr() as *mut u64, bv.as_mut_ptr() as *mut u64);
        let (mut ns, mut nc, mut nu, mut no) = (0usize, 0usize, 0usize, 0usize);
        unsafe {
            let cb: &CkBatch = &self.t.cb;
            let c = |x: u64| _mm512_set1_epi64(x as i64);
            let (v_base, v_len, v_short, v_cb, v_m32, v_empty) = (c(g.cbase as u64), c(g.clen), c(SHORT_END), c(CBOUND as u64), c(0xFFFF_FFFF), c(ID_EMPTY));
            let (v_cls, v_long) = (c(ID_ODD | ID_LONG), c(ID_LONG));
            let ctab = g.camp.as_ptr();
            if g.camp.len() < CBOUND {
                fallback("campaign table");
            }
            let rb = 42 - b2;
            let (v_m42, v_rmask, v_rb) = (c(M42), c((1u64 << rb) - 1), _mm_cvtsi64_si128(rb as i64));
            let (v_c0, v_c1) = (_mm256_set1_epi64x(MIXC as i64), _mm256_set1_epi64x((MIXC >> 32) as i64));
            let mix = |u: __m256i| _mm256_add_epi64(mulu_ymm(u, v_c0), _mm256_slli_epi64(_mm256_add_epi64(mulu_ymm(u, v_c1), mulu_ymm(_mm256_srli_epi64(u, 32), v_c0)), 32));
            let (il0, il1) = (_mm512_set_epi64(11, 3, 10, 2, 9, 1, 8, 0), _mm512_set_epi64(15, 7, 14, 6, 13, 5, 12, 4));
            let mut idx = _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0);
            let mut i = 0;
            while i < n {
                let rows: __mmask8 = if n - i >= 8 { 0xFF } else { (1u8 << (n - i)) - 1 };
                let cid = _mm512_loadu_si512(cb.cid.as_ptr().add(i) as *const _);
                let uid = _mm512_loadu_si512(cb.uid.as_ptr().add(i) as *const _);
                let ts = _mm512_loadu_si512(cb.ts.as_ptr().add(i) as *const _);
                let camp = _mm512_and_si512(_mm512_loadu_si512(cb.camp.as_ptr().add(i) as *const _), v_m32);
                let rel = _mm512_sub_epi64(ts, v_base);
                let k_v = _mm512_mask_cmpneq_epi64_mask(rows, uid, v_empty) & _mm512_cmplt_epu64_mask(rel, v_len) & _mm512_cmplt_epu64_mask(camp, v_cb);
                let (k_us, k_cs) = (_mm512_cmplt_epu64_mask(uid, v_short), _mm512_cmplt_epu64_mask(cid, v_short));
                let k_cl = _mm512_cmpeq_epi64_mask(_mm512_and_si512(cid, v_cls), v_long);
                let k_f = k_v & k_us & k_cs;
                let k_c = k_v & k_us & k_cl;
                let k_u = k_v & _mm512_cmpeq_epi64_mask(_mm512_and_si512(uid, v_cls), v_long) & (k_cs | k_cl);
                let m = _mm512_and_si512(_mm512_inserti64x4(_mm512_castsi256_si512(mix(_mm512_castsi512_si256(uid))), mix(_mm512_extracti64x4_epi64(uid, 1)), 1), v_m42);
                let p = _mm512_srl_epi64(m, v_rb);
                let a0 = _mm512_or_si512(_mm512_slli_epi64(_mm512_and_si512(m, v_rmask), 32), _mm512_slli_epi64(rel, 1));
                let a = _mm512_mask_or_epi64(a0, k_cl, a0, c(1));
                let bw = _mm512_or_si512(_mm512_slli_epi64(camp, 42), _mm512_maskz_mov_epi64(k_cs, cid));
                _mm512_storeu_si512(pv.add(i) as *mut _, p);
                _mm512_storeu_si512(av.add(i) as *mut _, a);
                _mm512_storeu_si512(bv.add(i) as *mut _, bw);
                _mm512_storeu_si512(sp.add(ns) as *mut _, _mm512_maskz_compress_epi64(k_f, p));
                let (ca, cw) = (_mm512_maskz_compress_epi64(k_f, a), _mm512_maskz_compress_epi64(k_f, bw));
                _mm512_storeu_si512(sr.add(2 * ns) as *mut _, _mm512_permutex2var_epi64(ca, il0, cw));
                _mm512_storeu_si512(sr.add(2 * ns + 8) as *mut _, _mm512_permutex2var_epi64(ca, il1, cw));
                ns += k_f.count_ones() as usize;
                _mm512_storeu_si512(ci.add(nc) as *mut _, _mm512_maskz_compress_epi64(k_c, idx));
                nc += k_c.count_ones() as usize;
                _mm512_storeu_si512(ui.add(nu) as *mut _, _mm512_maskz_compress_epi64(k_u, idx));
                nu += k_u.count_ones() as usize;
                let k_o = k_v & !(k_f | k_c | k_u);
                _mm512_storeu_si512(oi.add(no) as *mut _, _mm512_maskz_compress_epi64(k_o, idx));
                no += k_o.count_ones() as usize;
                idx = _mm512_add_epi64(idx, c(8));
                i += 8;
            }
            ck_f(&mut self.t.ck.s, sp, sr, ns, ctab);
            let mut bad = 0u64;
            if nc > 0 {
                bad |= ck_c(&mut self.t.ck.s, cb, ci, nc, pv, av, bv, ctab);
            }
            if nu > 0 {
                bad |= ck_wide(&mut self.t.ck.w, cb, ui, nu, g.cbase, b2, ctab);
            }
            if bad != 0 {
                fallback("long id with a zero high half");
            }
        }
        if no > 0 {
            self.ck_rest(oi, no);
        }
        if prof {
            self.e.cyc[1] += unsafe { core::arch::x86_64::_rdtsc() } - c0;
        }
        self.t.n_rows[1] += n as u64;
    }
    #[cfg(not(feature = "w512"))]
    #[inline(never)]
    fn ck_batch(&mut self) {
        use std::arch::x86_64::*;
        let n = self.t.cb.n;
        self.t.cb.n = 0;
        if unsafe { SKIP } & 2 != 0 {
            return;
        }
        let prof = unsafe { KN[7] } != 0;
        let c0 = if prof { unsafe { core::arch::x86_64::_rdtsc() } } else { 0 };
        let g = self.cx.g;
        let b2 = self.cx.b2;
        let mut sp = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut sr = std::mem::MaybeUninit::<[u64; 2 * NB + 16]>::uninit();
        let mut ci = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut ui = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut oi = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut pv = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut av = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let mut bv = std::mem::MaybeUninit::<[u64; NB + 8]>::uninit();
        let (sp, sr, ci, ui, oi) = (sp.as_mut_ptr() as *mut u64, sr.as_mut_ptr() as *mut u64, ci.as_mut_ptr() as *mut u64, ui.as_mut_ptr() as *mut u64, oi.as_mut_ptr() as *mut u64);
        let (pv, av, bv) = (pv.as_mut_ptr() as *mut u64, av.as_mut_ptr() as *mut u64, bv.as_mut_ptr() as *mut u64);
        let (mut ns, mut nc, mut nu, mut no) = (0usize, 0usize, 0usize, 0usize);
        unsafe {
            let cb: &CkBatch = &self.t.cb;
            let c = |x: u64| _mm256_set1_epi64x(x as i64);
            let (v_base, v_len, v_short, v_cb, v_m32, v_empty) = (c(g.cbase as u64), c(g.clen), c(SHORT_END), c(CBOUND as u64), c(0xFFFF_FFFF), c(ID_EMPTY));
            let (v_cls, v_long) = (c(ID_ODD | ID_LONG), c(ID_LONG));
            let ctab = g.camp.as_ptr();
            if g.camp.len() < CBOUND {
                fallback("campaign table");
            }
            let rb = 42 - b2;
            let (v_m42, v_rmask, v_rb) = (c(M42), c((1u64 << rb) - 1), _mm_cvtsi64_si128(rb as i64));
            let (v_c0, v_c1) = (_mm256_set1_epi64x(MIXC as i64), _mm256_set1_epi64x((MIXC >> 32) as i64));
            let mix = |u: __m256i| _mm256_add_epi64(mulu_ymm(u, v_c0), _mm256_slli_epi64(_mm256_add_epi64(mulu_ymm(u, v_c1), mulu_ymm(_mm256_srli_epi64(u, 32), v_c0)), 32));
            let (il0, il1) = (_mm256_set_epi64x(5, 1, 4, 0), _mm256_set_epi64x(7, 3, 6, 2));
            let (v_one, v_four) = (c(1), c(4));
            let mut idx = _mm256_set_epi64x(3, 2, 1, 0);
            let mut i = 0;
            while i < n {
                let rows: __mmask8 = if n - i >= 4 { 0xF } else { (1u8 << (n - i)) - 1 };
                let cid = _mm256_loadu_si256(cb.cid.as_ptr().add(i) as *const _);
                let uid = _mm256_loadu_si256(cb.uid.as_ptr().add(i) as *const _);
                let ts = _mm256_loadu_si256(cb.ts.as_ptr().add(i) as *const _);
                let camp = _mm256_and_si256(_mm256_loadu_si256(cb.camp.as_ptr().add(i) as *const _), v_m32);
                let rel = _mm256_sub_epi64(ts, v_base);
                let k_v = _mm256_mask_cmpneq_epi64_mask(rows, uid, v_empty) & _mm256_cmplt_epu64_mask(rel, v_len) & _mm256_cmplt_epu64_mask(camp, v_cb);
                let (k_us, k_cs) = (_mm256_cmplt_epu64_mask(uid, v_short), _mm256_cmplt_epu64_mask(cid, v_short));
                let k_cl = _mm256_cmpeq_epi64_mask(_mm256_and_si256(cid, v_cls), v_long);
                let k_f = k_v & k_us & k_cs;
                let k_c = k_v & k_us & k_cl;
                let k_u = k_v & _mm256_cmpeq_epi64_mask(_mm256_and_si256(uid, v_cls), v_long) & (k_cs | k_cl);
                let m = _mm256_and_si256(mix(uid), v_m42);
                let p = _mm256_srl_epi64(m, v_rb);
                let a0 = _mm256_or_si256(_mm256_slli_epi64(_mm256_and_si256(m, v_rmask), 32), _mm256_slli_epi64(rel, 1));
                let a = _mm256_mask_or_epi64(a0, k_cl, a0, v_one);
                let bw = _mm256_or_si256(_mm256_slli_epi64(camp, 42), _mm256_maskz_mov_epi64(k_cs, cid));
                _mm256_storeu_si256(pv.add(i) as *mut _, p);
                _mm256_storeu_si256(av.add(i) as *mut _, a);
                _mm256_storeu_si256(bv.add(i) as *mut _, bw);
                _mm256_storeu_si256(sp.add(ns) as *mut _, _mm256_maskz_compress_epi64(k_f, p));
                let (ca, cw) = (_mm256_maskz_compress_epi64(k_f, a), _mm256_maskz_compress_epi64(k_f, bw));
                _mm256_storeu_si256(sr.add(2 * ns) as *mut _, _mm256_permutex2var_epi64(ca, il0, cw));
                _mm256_storeu_si256(sr.add(2 * ns + 4) as *mut _, _mm256_permutex2var_epi64(ca, il1, cw));
                ns += k_f.count_ones() as usize;
                _mm256_storeu_si256(ci.add(nc) as *mut _, _mm256_maskz_compress_epi64(k_c, idx));
                nc += k_c.count_ones() as usize;
                _mm256_storeu_si256(ui.add(nu) as *mut _, _mm256_maskz_compress_epi64(k_u, idx));
                nu += k_u.count_ones() as usize;
                let k_o = k_v & !(k_f | k_c | k_u);
                _mm256_storeu_si256(oi.add(no) as *mut _, _mm256_maskz_compress_epi64(k_o, idx));
                no += k_o.count_ones() as usize;
                idx = _mm256_add_epi64(idx, v_four);
                i += 4;
            }
            ck_f(&mut self.t.ck.s, sp, sr, ns, ctab);
            let mut bad = 0u64;
            if nc > 0 {
                bad |= ck_c(&mut self.t.ck.s, cb, ci, nc, pv, av, bv, ctab);
            }
            if nu > 0 {
                bad |= ck_wide(&mut self.t.ck.w, cb, ui, nu, g.cbase, b2, ctab);
            }
            if bad != 0 {
                fallback("long id with a zero high half");
            }
        }
        if no > 0 {
            self.ck_rest(oi, no);
        }
        if prof {
            self.e.cyc[1] += unsafe { core::arch::x86_64::_rdtsc() } - c0;
        }
        self.t.n_rows[1] += n as u64;
    }
    #[inline(never)]
    fn ck_rest(&mut self, oi: *const u64, no: usize) {
        let g = self.cx.g;
        let b2 = self.cx.b2;
        let (mut sc, mut wc) = (self.t.ck.s.cursor(), self.t.ck.w.cursor());
        for j in 0..no {
            let i = unsafe { *oi.add(j) } as usize & (NB - 1);
            let (cid, uid, ts, camp) = (self.t.cb.cid[i], self.t.cb.uid[i], self.t.cb.ts[i], self.t.cb.camp[i] as u32);
            let rel = ts.wrapping_sub(g.cbase) as u64;
            if uid == ID_EMPTY || rel >= g.clen || camp as usize >= CBOUND || unsafe { *g.camp.get_unchecked(camp as usize) } == 255 {
                continue;
            }
            let (ch, cl, uh, ul) = (self.t.cb.chi[i], self.t.cb.clo[i], self.t.cb.uhi[i], self.t.cb.ulo[i]);
            let ck = self.key(cid, ch, cl);
            let uk = self.key(uid, uh, ul);
            unsafe { ckrec_put(&mut sc, &mut wc, b2, rel, camp, uk, ck) };
        }
        unsafe {
            self.t.ck.s.put(sc);
            self.t.ck.w.put(wc);
        }
    }
    fn lk_batch(&mut self) {
        let n = self.t.lb.n;
        self.t.lb.n = 0;
        if n != 0 && self.t.lkv.capacity() == 0 {
            let bytes: usize = self.cx.g.files.iter().filter(|f| f.kind == Kind::Lk).map(|f| f.len).sum();
            self.t.lkv.reserve(bytes / 16 + NB);
            self.t.lk.reserve(bytes / 64 + NB);
        }
        for i in 0..n {
            let (a, b) = (self.t.lb.a[i], self.t.lb.b[i]);
            if (a | b) < SHORT_END {
                if a != b {
                    self.t.lkv.push([mix42(a), mix42(b)]);
                }
                continue;
            }
            let (ah, al, bh, bl) = (self.t.lb.ahi[i], self.t.lb.alo[i], self.t.lb.bhi[i], self.t.lb.blo[i]);
            let ka = self.key(a, ah, al);
            let kb = self.key(b, bh, bl);
            if ka != kb {
                self.t.lk.push((ka, kb));
            } else {
            }
        }
        self.t.n_rows[2] += n as u64;
    }
}
impl<'a, 'b> EventSink for Sink<'a, 'b> {
    #[inline(always)]
    fn ev(&mut self) -> *mut EvBatch {
        &mut *self.t.eb
    }
    #[inline(always)]
    fn ev_flush(&mut self) {
        self.ev_batch();
    }
}
impl<'a, 'b> ClickSink for Sink<'a, 'b> {
    #[inline(always)]
    fn ck(&mut self) -> *mut CkBatch {
        &mut *self.t.cb
    }
    #[inline(always)]
    fn ck_flush(&mut self) {
        self.ck_batch();
    }
}
impl<'a, 'b> LinkSink for Sink<'a, 'b> {
    #[inline(always)]
    fn lk(&mut self) -> *mut LkBatch {
        &mut *self.t.lb
    }
    #[inline(always)]
    fn lk_flush(&mut self) {
        self.lk_batch();
    }
}

fn next_line_start(data: &[u8], from: usize) -> usize {
    match data[from..].iter().position(|&b| b == b'\n') {
        Some(i) => from + i + 1,
        None => data.len(),
    }
}

static XPM: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
extern "C" {
    fn syscall(n: i64, ...) -> i64;
}
fn xpm_open() -> [i32; 6] {
    const EV: [(u32, u64); 4] = [(0, 1), (0, 0), (0, 5), (3, 0x10000)];
    let mut fd = [-1; 6];
    for (k, &(ty, cfg)) in EV.iter().enumerate() {
        let mut a = [0u64; 16];
        a[0] = ty as u64 | (128u64 << 32);
        a[1] = cfg;
        a[5] = (1 << 5) | (1 << 6);
        fd[k] = unsafe { syscall(298, a.as_ptr(), 0i32, -1i32, -1i32, 0u64) } as i32;
    }
    fd
}
fn xpm_close(fd: [i32; 6], nt: usize) {
    xpm_end(fd, nt, &XPM, "X");
}
static XPC: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
fn xpm_end(fd: [i32; 6], nt: usize, acc: &[AtomicU64; 7], name: &str) {
    for k in 0..6 {
        let mut v = 0u64;
        if fd[k] >= 0 {
            unsafe {
                syscall(0, fd[k], &mut v as *mut u64, 8usize);
                syscall(3, fd[k]);
            }
        }
        acc[k].fetch_add(v, Ordering::Relaxed);
    }
    if acc[6].fetch_add(1, Ordering::AcqRel) + 1 == nt as u64 {
        let x: Vec<u64> = acc.iter().map(|a| a.load(Ordering::Relaxed) / 1_000_000).collect();
        eprintln!("[v3b] {name} pmu(M): instr={} cycles={} brmiss={} l1dmiss={}", x[0], x[1], x[2], x[3]);
    }
}
fn rel_mode(n: usize) -> u32 {
    let m = std::env::var("XL_V3REL").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    if m == 1 && n / REL_U >= REL_N {
        return 2;
    }
    m
}
const REL_U: usize = 8;
const REL_N: usize = 4096;
static REL_CNT: [[std::sync::atomic::AtomicU32; REL_N]; 2] = [const { [const { std::sync::atomic::AtomicU32::new(0) }; REL_N] }; 2];
static REL_RANK: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
#[inline]
fn rel_unit(cx: &Cx, st: usize, qi: usize, sub: usize, late: bool) {
    let u = qi / REL_U;
    let n = cx.queue.len();
    let size = (n.min((u + 1) * REL_U) - u * REL_U) * sub;
    if REL_CNT[st][u].fetch_add(1, Ordering::AcqRel) as usize + 1 == size {
        rel_range(cx, u * REL_U, n.min((u + 1) * REL_U), st == 1, st == 0 && !late);
    }
}
fn rel_slice(cx: &Cx, k: usize, clicks: bool, all: bool) {
    let (n, nt) = (cx.queue.len(), cx.t.len());
    if k < nt {
        rel_range(cx, n * k / nt, n * (k + 1) / nt, clicks, all);
    }
}
fn rel_range(cx: &Cx, lo: usize, hi: usize, clicks: bool, all: bool) {
    let g = cx.g;
    let mut i = lo;
    while i < hi {
        let c = cx.queue[i];
        let f = &g.files[c.file as usize];
        if !all && (f.kind == Kind::Ck) != clicks {
            i += 1;
            continue;
        }
        let mut b = c.b;
        i += 1;
        while i < hi && cx.queue[i].file == c.file && cx.queue[i].a == b {
            b = cx.queue[i].b;
            i += 1;
        }
        let ra = (f.off + if c.a <= 1 { 0 } else { c.a }) & !4095;
        let rb = if b >= f.len { (f.off + f.len + 4095) & !4095 } else { (f.off + b) & !4095 };
        if rb > ra {
            unsafe { madvise(g.region.add(ra) as *mut u8, rb - ra, MADV_DONTNEED) };
        }
    }
}
fn parse_thread(cx: &Cx, tid: usize) {
    let g = cx.g;
    let rel = rel_mode(cx.queue.len());
    let xfd = if unsafe { KN[6] } != 0 { xpm_open() } else { [-1; 6] };
    let late = ck_late();
    let t = cx.t(tid);
    {
        let np = 1usize << cx.b1;
        let e = p3(tid);
        unsafe {
            let sz = (<ELane>::mem(np) + 63) & !63;
            let arp: *mut EArena = std::ptr::addr_of_mut!(e.es.own);
            let m = (*arp).take((3 * sz + if late { 0 } else { CBins::mem(t.ck.np) } + 4095) & !4095);
            e.es.init(np, m, arp);
            e.ew.init(np, m.add(sz), arp);
            e.ex.init_lazy(np, m.add(2 * sz), arp);
            if !late {
                t.ck.init(m.add(3 * sz), arp);
            }
        }
    }
    loop {
        let i = cx.qnext.fetch_add(1, Ordering::Relaxed);
        if i >= cx.queue.len() {
            break;
        }
        let c = cx.queue[i];
        let f = &g.files[c.file as usize];
        if late && f.kind == Kind::Ck {
            if rel == 1 {
                rel_unit(cx, 0, i, 1, late);
            }
            continue;
        }
        let data = unsafe { std::slice::from_raw_parts(g.region.add(f.off), f.len) };
        let s = next_line_start(data, c.a - 1);
        let e = if c.b >= f.len { f.len } else { next_line_start(data, c.b - 1) };
        let buf = if s >= e { &data[..0] } else { &data[s..e] };
        let mut sink = Sink { cx, t: &mut *t, e: p3(tid), base: buf.as_ptr(), fend: unsafe { data.as_ptr().add(f.len) } };
        match f.kind {
            Kind::Ev => {
                <K as Kernel>::events(buf, &mut sink);
                sink.ev_batch();
            }
            Kind::Ck => {
                <K as Kernel>::clicks(buf, &mut sink);
                sink.ck_batch();
            }
            Kind::Lk => {
                crate::k_free::links(buf, &mut sink);
                sink.lk_batch();
                if cx.lk_left.fetch_sub(1, Ordering::SeqCst) == 1 {
                    links_start(cx);
                }
            }
        }
        if rel == 2 {
            let ra = (f.off + s + 4095) & !4095;
            let rb = (f.off + e) & !4095;
            if rb > ra {
                unsafe { madvise(g.region.add(ra) as *mut u8, rb - ra, MADV_DONTNEED) };
            }
        }
        if rel == 1 {
            rel_unit(cx, 0, i, 1, late);
        }
    }
    if rel == 3 {
        rel_slice(cx, REL_RANK[0].fetch_add(1, Ordering::Relaxed), false, !late);
    }
    let t = cx.t(tid);
    t.ck.finish();
    unsafe {
        let e = p3(tid);
        e.es.drain();
        e.ew.drain();
        std::arch::x86_64::_mm_sfence();
    }
    if unsafe { KN[6] } != 0 {
        xpm_close(xfd, cx.t.len());
    }
}

fn ck_late() -> bool {
    std::env::var_os("XL_CKLATE").map_or(true, |v| v != "0")
}
fn click_thread(cx: &Cx, tid: usize) {
    if !ck_late() {
        return;
    }
    let g = cx.g;
    let rel = rel_mode(cx.queue.len());
    let xfd = if unsafe { KN[6] } != 0 { xpm_open() } else { [-1; 6] };
    let t = cx.t(tid);
    unsafe {
        let e = p3(tid);
        let m = e.es.own.take((CBins::mem(t.ck.np) + 4095) & !4095);
        t.ck.ar.free.reserve(t.u2.s.free.len() + t.u2.w.free.len());
        t.ck.ar.free.extend(t.u2.w.free.drain(..).map(|c| c as *mut u8));
        t.ck.ar.free.extend(t.u2.s.free.drain(..).map(|c| c as *mut u8));
        let arp: *mut EArena = std::ptr::addr_of_mut!(t.ck.ar);
        t.ck.init(m, arp);
    }
    const CKSUB: usize = 4;
    loop {
        let i = cx.next.fetch_add(1, Ordering::Relaxed);
        if i / CKSUB >= cx.queue.len() {
            break;
        }
        let c = cx.queue[i / CKSUB];
        let f = &g.files[c.file as usize];
        if f.kind != Kind::Ck {
            if rel == 1 {
                rel_unit(cx, 1, i / CKSUB, CKSUB, true);
            }
            continue;
        }
        let data = unsafe { std::slice::from_raw_parts(g.region.add(f.off), f.len) };
        let (xa, xb) = (c.a + (c.b - c.a) * (i % CKSUB) / CKSUB, c.a + (c.b - c.a) * (i % CKSUB + 1) / CKSUB);
        let s = next_line_start(data, xa - 1);
        let e = if xb >= f.len { f.len } else { next_line_start(data, xb - 1) };
        let buf = if s >= e { &data[..0] } else { &data[s..e] };
        let mut sink = Sink { cx, t: &mut *t, e: p3(tid), base: buf.as_ptr(), fend: unsafe { data.as_ptr().add(f.len) } };
        <K as Kernel>::clicks(buf, &mut sink);
        sink.ck_batch();
        if rel == 2 {
            let ra = (f.off + s + 4095) & !4095;
            let rb = (f.off + e) & !4095;
            if rb > ra {
                unsafe { madvise(g.region.add(ra) as *mut u8, rb - ra, MADV_DONTNEED) };
            }
        }
        if rel == 1 {
            rel_unit(cx, 1, i / CKSUB, CKSUB, true);
        }
    }
    if rel == 3 {
        rel_slice(cx, REL_RANK[1].fetch_add(1, Ordering::Relaxed), true, false);
    }
    let t = cx.t(tid);
    t.ck.finish();
    t.u2.s.free.extend(t.ck.ar.free.drain(..).map(|c| c as *mut u64));
    if unsafe { KN[6] } != 0 {
        xpm_end(xfd, cx.t.len(), &XPC, "C");
    }
}

fn uf_find(parent: &mut [u32], mut x: u32) -> u32 {
    let mut r = x;
    while parent[r as usize] != r {
        r = parent[r as usize];
    }
    while parent[x as usize] != r {
        let n = parent[x as usize];
        parent[x as usize] = r;
        x = n;
    }
    r
}

#[inline(always)]
fn lk_pf<T>(p: *const T) {
    unsafe { std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0) }
}

const P36: [u64; 9] = [2_821_109_907_456, 78_364_164_096, 2_176_782_336, 60_466_176, 1_679_616, 46_656, 1_296, 36, 1];
#[inline(always)]
fn ord36(v: u64) -> u64 {
    let len = 1 + (v >= 36) as usize + (v >= 1_296) as usize + (v >= 46_656) as usize + (v >= 1_679_616) as usize + (v >= 60_466_176) as usize + (v >= 2_176_782_336) as usize + (v >= 78_364_164_096) as usize;
    (v * P36[len]) << 4 | len as u64
}
#[inline(always)]
fn hex8(hi: u64) -> u64 {
    let n = unsafe { std::arch::x86_64::_pdep_u64(hi >> 32, 0x0F0F_0F0F_0F0F_0F0F) };
    let m = ((n + 0x0606_0606_0606_0606) >> 4) & 0x0101_0101_0101_0101;
    n + 0x3030_3030_3030_3030 + m * 0x27
}
#[inline(always)]
fn key_less(a: Key, b: Key, odds: &[Vec<u8>]) -> bool {
    let (sa, sb) = (is_short(a), is_short(b));
    if sa && sb {
        return ord36(a.lo) < ord36(b.lo);
    }
    let (la, lb) = (a.hi != 0, b.hi != 0);
    if la && lb {
        return (a.hi, a.lo) < (b.hi, b.lo);
    }
    if sa && lb {
        return enc36(a.lo).0 <= hex8(b.hi);
    }
    if la && sb {
        return hex8(a.hi) < enc36(b.lo).0;
    }
    id_cmp(a, b, b'u', odds) == std::cmp::Ordering::Less
}

fn links_start(cx: &Cx) {
    let odds_v: Vec<Vec<u8>> = cx.odd.lock().unwrap_or_else(|e| e.into_inner()).strs.clone();
    let p = cx as *const Cx as usize;
    let h = unsafe {
        std::thread::Builder::new().spawn_unchecked(move || {
            let cx: &Cx = &*(p as *const Cx);
            build_links(cx, &odds_v)
        })
    };
    if let Ok(h) = h {
        *cx.lkh.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }
}
fn links_join(cx: &Cx) {
    let h = cx.lkh.lock().unwrap_or_else(|e| e.into_inner()).take();
    let ns = match h {
        Some(h) => match h.join() {
            Ok(ns) => ns,
            Err(_) => die("links thread"),
        },
        None => build_links(cx, cx.odds()),
    };
    cx.t(0).prof[10..13].copy_from_slice(&ns);
}

#[allow(clippy::mut_from_ref)]
#[inline(always)]
fn lk_ss<'c>(cx: &'c Cx, tid: usize) -> &'c mut Vec<[u64; 2]> {
    unsafe { &mut *std::ptr::addr_of_mut!((*cx.t[tid].get()).lkv) }
}
#[allow(clippy::mut_from_ref)]
#[inline(always)]
fn lk_w<'c>(cx: &'c Cx, tid: usize) -> &'c mut Vec<(Key, Key)> {
    unsafe { &mut *std::ptr::addr_of_mut!((*cx.t[tid].get()).lk) }
}

fn lk_release<T>(v: Vec<T>) {
    const HP: usize = 2 << 20;
    let (p, bytes) = (v.as_ptr() as usize, v.capacity() * std::mem::size_of::<T>());
    if p & (HP - 1) == 0 && bytes >= HP {
        unsafe { release(p as *const u8, bytes & !(HP - 1)) };
        std::mem::forget(v);
    }
}

struct LkT {
    nd: *mut u64,
    s: u64,
    rb: u32,
    rmask: u64,
    magic: u64,
}
impl LkT {
    const PROBES: u64 = 2048;
    #[inline(always)]
    fn home(&self, m: u64) -> usize {
        (((m >> self.rb) + 1) * self.s + ((((m & self.rmask) * self.s) >> self.rb) & !7)) as usize
    }
    #[inline(always)]
    unsafe fn slot(&self, m: u64) -> u32 {
        use std::arch::x86_64::*;
        let rem = m & self.rmask;
        let base = ((m >> self.rb) + 1) * self.s;
        let i = ((rem * self.s) >> self.rb) & !7;
        let p = self.nd.add((base + i) as usize);
        let (v0, v1) = (_mm256_loadu_si256(p as *const __m256i), _mm256_loadu_si256(p.add(4) as *const __m256i));
        let r = _mm256_set1_epi64x(rem as i64);
        let mz = _mm256_testn_epi64_mask(v0, v0) as u32 | opq(_mm256_testn_epi64_mask(v1, v1) as u32) << 4;
        let me = _mm256_cmpeq_epi64_mask(_mm256_srli_epi64::<32>(v0), r) as u32 | opq(_mm256_cmpeq_epi64_mask(_mm256_srli_epi64::<32>(v1), r) as u32) << 4;
        let hit = mz | me;
        if hit != 0 {
            let k = hit.trailing_zeros() as u64;
            let x = base + i + k;
            let q = p.add(k as usize);
            *q = if (mz >> k) & 1 != 0 { rem << 32 | x } else { *q };
            return x as u32;
        }
        self.slot_far(rem, base, i)
    }
    #[inline(never)]
    unsafe fn slot_far(&self, rem: u64, base: u64, mut i: u64) -> u32 {
        let mut left = Self::PROBES.min(self.s);
        loop {
            let p = self.nd.add((base + i) as usize);
            let e = *p;
            if e == 0 {
                *p = rem << 32 | (base + i);
                return (base + i) as u32;
            }
            if e >> 32 == rem {
                return (base + i) as u32;
            }
            i += 1;
            if i == self.s {
                i = 0;
            }
            left -= 1;
            if left == 0 {
                return 0;
            }
        }
    }
    #[inline(always)]
    unsafe fn find(&self, mut x: u32) -> u32 {
        loop {
            let e = *self.nd.add(x as usize);
            let p = e as u32;
            let g = *self.nd.add(p as usize) as u32;
            if g == p {
                return p;
            }
            *self.nd.add(x as usize) = (e & !0xFFFF_FFFF) | g as u64;
            x = g;
        }
    }
    #[inline(always)]
    unsafe fn link(&self, child: u32, root: u32) {
        let p = self.nd.add(child as usize);
        *p = (*p & !0xFFFF_FFFF) | root as u64;
    }
    #[inline(always)]
    unsafe fn skey(&self, x: u32) -> u64 {
        let q = ((x as u128 * self.magic as u128) >> 64) as u64 - 1;
        unmix42(q << self.rb | *self.nd.add(x as usize) >> 32)
    }
}

fn build_links(cx: &Cx, odds: &[Vec<u8>]) -> [u64; 3] {
    let mut tk = std::time::Instant::now();
    let nt = cx.t.len();
    let np = 1usize << cx.b2;
    let bk = unsafe { &mut *cx.bk.get() };
    bk.rep = unsafe {
        let mut v = std::mem::ManuallyDrop::new(vec![0u64; np << (RBB - 6)]);
        Vec::from_raw_parts(v.as_mut_ptr() as *mut AtomicU64, v.len(), v.capacity())
    };
    let n_ss: usize = (0..nt).map(|t| lk_ss(cx, t).len()).sum();
    let n_w: usize = (0..nt).map(|t| lk_w(cx, t).len()).sum();
    if n_ss + n_w == 0 || unsafe { SKIP } & 4 != 0 {
        return [0; 3];
    }
    let check = std::env::var("XL_LKCHK").is_ok();
    let mut s = (2 * (n_ss + n_w) / np).max(16) as u64;
    if let Some(v) = std::env::var("XL_LKS").ok().and_then(|v| v.parse::<u64>().ok()) {
        s = v.max(2);
    }
    s = (s + 7) & !7;
    let mut tm = [0u64; 3];
    while !links_fast(cx, odds, s, check, &mut tk, &mut tm) {
        s *= 2;
    }

    if check {
        links_check(cx, odds);
    }
    for tid in 0..nt {
        lk_release(std::mem::take(lk_ss(cx, tid)));
        lk_release(std::mem::take(lk_w(cx, tid)));
    }
    tm
}

fn links_fast(cx: &Cx, odds: &[Vec<u8>], s: u64, check: bool, tk: &mut std::time::Instant, tm: &mut [u64; 3]) -> bool {
    use std::cmp::Ordering::Less;
    let nt = cx.t.len();
    let b2 = cx.b2;
    let np = 1usize << b2;
    let rb = 42 - b2;
    let n_ss: usize = (0..nt).map(|t| lk_ss(cx, t).len()).sum();
    let n_w: usize = (0..nt).map(|t| lk_w(cx, t).len()).sum();
    const D: usize = 16;
    let total = (np as u64 + 1) * s + 2 * n_w as u64 + 2 * D as u64;
    if total >= u32::MAX as u64 {
        fallback("too many link ids");
    }
    let ns = (np + 1) * s as usize;
    let mut nodes = vec![0u64; total as usize];
    let t = LkT { nd: nodes.as_mut_ptr(), s, rb, rmask: (1u64 << rb) - 1, magic: u64::MAX / s + 1 };
    const BL: usize = 32;
    let mut sl = [0u32; 2 * BL];
    unsafe {
        for tid in 0..nt {
            for blk in lk_ss(cx, tid).chunks(BL) {
                for p in blk {
                    lk_pf(t.nd.add(t.home(p[0])));
                    lk_pf(t.nd.add(t.home(p[1])));
                }
                for (j, p) in blk.iter().enumerate() {
                    let (a, b) = (t.slot(p[0]), t.slot(p[1]));
                    if a == 0 || b == 0 {
                        return false;
                    }
                    sl[2 * j] = a;
                    sl[2 * j + 1] = b;
                    lk_pf(t.nd.add(*t.nd.add(a as usize) as u32 as usize));
                    lk_pf(t.nd.add(*t.nd.add(b as usize) as u32 as usize));
                }
                for j in 0..blk.len() {
                    let rx = t.find(sl[2 * j]);
                    let ry = t.find(sl[2 * j + 1]);
                    if rx != ry {
                        let (kx, ky) = (t.skey(rx), t.skey(ry));
                        let less = if kx.min(ky) >= P36[1] { kx < ky } else { ord36(kx) < ord36(ky) };
                        if check && less != (id_cmp(Key { hi: 0, lo: kx }, Key { hi: 0, lo: ky }, b'u', odds) == Less) {
                            die("links: order check (short)");
                        }
                        let d = (rx ^ ry) & (less as u32).wrapping_neg();
                        t.link(rx ^ d, ry ^ d);
                    }
                }
            }
        }
    }
    let t_ss = tick(tk);
    let wmask = (2 * n_w + 16).next_power_of_two() - 1;
    let mut wtab = vec![0u32; wmask + 1];
    let mut wkeys: Vec<Key> = Vec::with_capacity(2 * n_w);
    let mut hs = [0u64; 2 * BL];
    unsafe {
        for tid in 0..nt {
            for blk in lk_w(cx, tid).chunks(BL) {
                for (j, &(a, b)) in blk.iter().enumerate() {
                    for (jj, k) in [a, b].into_iter().enumerate() {
                        if is_short(k) {
                            let m = mix42(k.lo);
                            hs[2 * j + jj] = m;
                            lk_pf(t.nd.add(t.home(m)));
                        } else {
                            let h = hash(k);
                            hs[2 * j + jj] = h;
                            lk_pf(wtab.as_ptr().add(h as usize & wmask));
                        }
                    }
                }

                for (j, &(a, b)) in blk.iter().enumerate() {
                    for (jj, k) in [a, b].into_iter().enumerate() {
                        let h = hs[2 * j + jj];
                        let x = if is_short(k) {
                            let x = t.slot(h);
                            if x == 0 {
                                return false;
                            }
                            x
                        } else {
                            let mut w = h as usize & wmask;
                            loop {
                                let e = *wtab.get_unchecked(w);
                                if e == 0 {
                                    let i = wkeys.len() as u32;
                                    wkeys.push(k);
                                    *wtab.get_unchecked_mut(w) = i + 1;
                                    let x = ns as u32 + i;
                                    *t.nd.add(x as usize) = x as u64;
                                    break x;
                                }
                                if *wkeys.get_unchecked(e as usize - 1) == k {
                                    break ns as u32 + e - 1;
                                }
                                w = (w + 1) & wmask;
                            }
                        };
                        sl[2 * j + jj] = x;
                        lk_pf(t.nd.add(*t.nd.add(x as usize) as u32 as usize));
                    }
                }
                for j in 0..blk.len() {
                    let rx = t.find(sl[2 * j]);
                    let ry = t.find(sl[2 * j + 1]);
                    if rx != ry {
                        let key = |x: u32| if (x as usize) < ns { Key { hi: 0, lo: t.skey(x) } } else { wkeys[x as usize - ns] };
                        let (kx, ky) = (key(rx), key(ry));
                        let less = key_less(kx, ky, odds);
                        if check && less != (id_cmp(kx, ky, b'u', odds) == Less) {
                            die("links: order check");
                        }
                        if less {
                            t.link(ry, rx)
                        } else {
                            t.link(rx, ry)
                        }
                    }
                }
            }
        }
    }
    let t_w = tick(tk);
    tm[0] += t_ss + t_w;
    tm[1] += t_w;
    let mw = wkeys.len();
    let mut wst = vec![0u32; np + 2];
    for k in &wkeys {
        wst[upart(*k, b2) + 2] += 1;
    }
    for q in 0..np {
        wst[q + 2] += wst[q + 1];
    }
    let mut word = vec![0u32; mw];
    for (i, k) in wkeys.iter().enumerate() {
        let q = upart(*k, b2);
        word[wst[q + 1] as usize] = i as u32;
        wst[q + 1] += 1;
    }
    let bk = unsafe { &mut *cx.bk.get() };
    let mut lkf: Vec<(Key, Key)> = Vec::with_capacity(2 * (n_ss + n_w));
    let out = lkf.as_mut_ptr();
    let mut pos = 0usize;
    let mut ids = mw;
    unsafe {
        use std::arch::x86_64::*;
        let key = |x: u32| if (x as usize) < ns { Key { hi: 0, lo: t.skey(x) } } else { wkeys[x as usize - ns] };
        let rep = bk.rep.as_mut_ptr() as *mut u64;
        let s = s as usize;
        let z = _mm256_setzero_si256();
        let used = |x: usize| {
            let p = t.nd.add(x) as *const __m256i;
            _mm256_cmpneq_epi64_mask(_mm256_loadu_si256(p), z) as u32 | opq(_mm256_cmpneq_epi64_mask(_mm256_loadu_si256(p.add(1)), z) as u32) << 4
        };
        for q in 0..np {
            bk.lko[q] = pos;
            let base = (q + 1) * s;
            let mut g = 0usize;
            while g < s {
                for k in 0..8 {
                    lk_pf(t.nd.add(*t.nd.add(base + g + D + k) as u32 as usize));
                }
                let mut m = used(base + g);
                if g + 8 > s {
                    m &= (1u32 << (s - g)) - 1;
                }
                ids += m.count_ones() as usize;
                while m != 0 {
                    let x = base + g + m.trailing_zeros() as usize;
                    m &= m - 1;
                    let e = *t.nd.add(x);
                    let r = t.find(x as u32);
                    if r as usize != x {
                        out.add(pos).write((Key { hi: 0, lo: unmix42((q as u64) << rb | e >> 32) }, key(r)));
                        pos += 1;
                    } else {
                        let ix = rep_ix_short((e >> 32) as u32);
                        *rep.add((q << (RBB - 6)) + (ix >> 6)) |= 1u64 << (ix & 63);
                    }
                }
                g += 8;
            }
            let wl = &word[wst[q] as usize..wst[q + 1] as usize];
            for (j, &i) in wl.iter().enumerate() {
                if j + 8 < wl.len() {
                    lk_pf(t.nd.add(*t.nd.add(ns + wl[j + 8] as usize) as u32 as usize));
                }
                let x = ns as u32 + i;
                let r = t.find(x);
                if r != x {
                    out.add(pos).write((wkeys[i as usize], key(r)));
                    pos += 1;
                } else {
                    let (w, m) = rep_slot(wkeys[i as usize], b2);
                    *rep.add(w) |= m;
                }
            }
        }
        bk.lko[np] = pos;
        lkf.set_len(pos);
    }
    bk.lkf = lkf;
    lk_release(nodes);
    lk_release(wtab);
    lk_release(wkeys);
    tm[2] += tick(tk);
    if std::env::var("XL_TIMING").is_ok() {
        eprintln!("[v3b] links: pairs={} ids={ids} moved={pos} slots={s} other={mw} us: uf={} wide={} out={}", n_ss + n_w, tm[0] / 1000, tm[1] / 1000, tm[2] / 1000);
    }
    true
}

fn links_check(cx: &Cx, odds: &[Vec<u8>]) {
    let nt = cx.t.len();
    let b2 = cx.b2;
    let np = 1usize << b2;
    let mut pairs: Vec<(Key, Key)> = Vec::new();
    for t in 0..nt {
        for p in lk_ss(cx, t).iter() {
            pairs.push((Key { hi: 0, lo: unmix42(p[0]) }, Key { hi: 0, lo: unmix42(p[1]) }));
        }
        pairs.extend_from_slice(lk_w(cx, t));
    }
    let n = pairs.len();
    let mut tab = Tab::default();
    tab.reset(2 * n);
    let mut ids: Vec<Key> = Vec::with_capacity(2 * n);
    let mut parent: Vec<u32> = Vec::with_capacity(2 * n);
    for &(a, b) in &pairs {
        let mut ix = [0u32; 2];
        for (jj, k) in [a, b].into_iter().enumerate() {
            let h = hash(k);
            ix[jj] = match tab.find(h, |i| ids[i as usize] == k) {
                Ok((_, i)) => i,
                Err(slot) => {
                    let i = ids.len() as u32;
                    ids.push(k);
                    parent.push(i);
                    tab.set(slot, h, i);
                    i
                }
            };
        }
        let rx = uf_find(&mut parent, ix[0]);
        let ry = uf_find(&mut parent, ix[1]);
        if rx != ry {
            if rx < ry {
                parent[ry as usize] = rx
            } else {
                parent[rx as usize] = ry
            }
        }
    }
    let m = ids.len();
    let mut min_of: Vec<u32> = (0..m as u32).collect();
    for i in 0..m {
        let r = uf_find(&mut parent, i as u32) as usize;
        if min_of[r] != i as u32 && id_cmp(ids[i], ids[min_of[r] as usize], b'u', odds) == std::cmp::Ordering::Less {
            min_of[r] = i as u32;
        }
    }
    let mut want: Vec<Vec<(Key, Key)>> = (0..np).map(|_| Vec::new()).collect();
    let mut rep = vec![0u64; np << (RBB - 6)];
    for i in 0..m {
        let r = uf_find(&mut parent, i as u32) as usize;
        let pk = ids[min_of[r] as usize];
        if pk != ids[i] {
            want[upart(ids[i], b2)].push((ids[i], pk));
            let (w, mk) = rep_slot(pk, b2);
            rep[w] |= mk;
        }
    }
    let bk = unsafe { &*cx.bk.get() };
    let ord = |e: &(Key, Key)| (e.0.hi, e.0.lo);
    for q in 0..np {
        let mut have = bk.lkf[bk.lko[q]..bk.lko[q + 1]].to_vec();
        have.sort_by_key(ord);
        want[q].sort_by_key(ord);
        if have != want[q] {
            eprintln!("[v3b] links check: partition {q}: {} entries, the plain way has {}", have.len(), want[q].len());
            die("links check");
        }
    }
    if bk.lko[np] != bk.lkf.len() || (0..rep.len()).any(|w| bk.rep[w].load(Ordering::Relaxed) != rep[w]) {
        die("links check (bitmap)");
    }
    eprintln!("[v3b] links check ok: pairs={n} ids={m} moved={}", bk.lkf.len());
}

#[inline(always)]
fn emit_s(t: &mut T3, b2: u32, w0: u64, ul: u64) {
    urec_put(&mut t.u2, b2, w0, 0, ul);
}
#[inline(always)]
unsafe fn dd_put(vb: &mut VB, q: usize, src: *const u64, n: usize) {
    use std::arch::x86_64::*;
    #[inline(always)]
    unsafe fn copy(d: *mut u64, s: *const u64, k: usize) {
        let m = (1u32 << k) - 1;
        _mm256_mask_storeu_epi64(d as *mut i64, m as u8, _mm256_loadu_si256(s as *const __m256i));
        _mm256_mask_storeu_epi64(d.add(4) as *mut i64, (m >> 4) as u8, _mm256_loadu_si256(s.add(4) as *const __m256i));
        _mm256_mask_storeu_epi64(d.add(8) as *mut i64, (m >> 8) as u8, _mm256_loadu_si256(s.add(8) as *const __m256i));
        _mm256_mask_storeu_epi64(d.add(12) as *mut i64, (m >> 12) as u8, _mm256_loadu_si256(s.add(12) as *const __m256i));
    }
    debug_assert!(n <= 16);
    let s = vb.st.add(q);
    let f = (*s).fill;
    let room = SW - f;
    copy((*s).data.as_mut_ptr().add(f), src, n.min(room));
    if n >= room {
        vb.flush(s, SW);
        copy((*s).data.as_mut_ptr(), src.add(room), n - room);
        (*s).fill = n - room;
    } else {
        (*s).fill = f + n;
    }
}
const RUN_PUT: bool = true;
#[inline(always)]
fn emit_run(t: &mut T3, b2: u32, q: usize, words: &[u64]) {
    if RUN_PUT {
        unsafe { dd_put(&mut t.u2.s, q, words.as_ptr(), words.len()) };
    } else {
        let rb = 42 - b2;
        let mut i = 0;
        while i < words.len() {
            let w = words[i];
            let pur = (w & 1) as usize;
            let pay = if pur != 0 { words[i + 1] } else { 0 };
            urec_put(&mut t.u2, b2, (w & 0xFFFF_FFFE) << 32 | pay << 4, 0, unmix42((q as u64) << rb | w >> 32));
            i += 1 + pur;
        }
    }
}
#[inline(always)]
fn emit_run_w(t: &mut T3, b2: u32, q: usize, words: &[u64]) {
    if RUN_PUT {
        unsafe {
            let n = words.len();
            dd_put(&mut t.u2.w, q, words.as_ptr(), n.min(16));
            if n > 16 {
                dd_put(&mut t.u2.w, q, words.as_ptr().add(16), n - 16);
            }
        }
    } else {
        let mut i = 0;
        while i < words.len() {
            let w = words[i];
            let pur = (w & 1) as usize;
            let pay = if pur != 0 { words[i + 3] } else { 0 };
            urec_put(&mut t.u2, b2, (w & 0xFFFF_FFFE) << 32 | pay << 4, words[i + 2], words[i + 1]);
            i += 3 + pur;
        }
    }
}
#[inline(always)]
fn emit_w(t: &mut T3, b2: u32, w0: u64, uh: u64, ul: u64) {
    urec_put(&mut t.u2, b2, w0, uh, ul);
}

#[inline(always)]
unsafe fn x_index(r: *const u64, sh: u32) -> u64 {
    let k = Key { hi: *r, lo: *r.add(1) };
    if k.hi == 0 && k.lo < SHORT_END {
        (e_rem(k.lo, sh) >> 3) & BMM
    } else {
        bm_key(k)
    }
}

const DBLK: usize = 128;
const MSW: usize = 16;
const MST: usize = 12;
const MWW: usize = 32;
const MWT: usize = 24;
const _: () = assert!(MSW <= 16 && MWW <= 32);
const DFL: usize = 24;
const DLA: usize = 4;
const DPA: usize = 8;
const DREC: usize = 256;

struct Runs {
    ms: *mut u64,
    mw: *mut u64,
    msf: *mut u8,
    mwf: *mut u8,
    fl: *mut u16,
    flw: *mut u16,
    nf: usize,
    nfw: usize,
}

const BMW: usize = 1 << (BMB - 6);
#[inline(always)]
unsafe fn bm_mark(bm: *mut u64, cl: *mut usize, nc: usize, ix: u64) -> usize {
    let w = (ix >> 6) as usize;
    let s = *bm.add(w);
    *bm.add(w) = s | 1u64 << (ix & 63);
    *cl.add(nc) = ix as usize;
    nc + ((s >> (ix & 63)) & 1) as usize
}
#[inline(never)]
unsafe fn bm_twice(bm: *mut u64, cl: *const usize, nc: usize) {
    let tw = bm.add(BMW);
    for i in 0..nc {
        let ix = *cl.add(i);
        *tw.add(ix >> 6) |= 1u64 << (ix & 63);
    }
}
#[inline(never)]
unsafe fn dd_mark_s(ptr: *const u64, n: usize, nptr: *const u64, nn: usize, bm: *mut u64, cl: *mut usize, mut nc: usize) -> usize {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
    let mut k = 0;
    while k < n {
        if k < nn {
            _mm_prefetch(nptr.add(k) as *const i8, _MM_HINT_T0);
        }
        if k + 8 <= n {
            for j in 0..4 {
                let ix = (*ptr.add(k + 2 * j) >> 35) & BMM;
                nc = bm_mark(bm, cl, nc, ix);
            }
            k += 8;
            continue;
        }
        while k < n {
            let ix = (*ptr.add(k) >> 35) & BMM;
            nc = bm_mark(bm, cl, nc, ix);
            k += 2;
        }
    }
    nc
}
#[inline(always)]
unsafe fn dd_ld(p: *const u64) -> std::arch::x86_64::__m256i {
    let r: std::arch::x86_64::__m256i;
    std::arch::asm!("vmovdqu {r}, ymmword ptr [{p}]", r = lateout(ymm_reg) r, p = in(reg) p, options(pure, readonly, nostack, preserves_flags));
    r
}
#[inline(always)]
unsafe fn dd_ld4(base: *const u64, i0: usize, i1: usize, i2: usize, i3: usize) -> std::arch::x86_64::__m256i {
    let r: std::arch::x86_64::__m256i;
    std::arch::asm!(
        "vmovq {r:x}, qword ptr [{b} + 8*{i0}]",
        "vpinsrq {r:x}, {r:x}, qword ptr [{b} + 8*{i1}], 1",
        "vmovq {h}, qword ptr [{b} + 8*{i2}]",
        "vpinsrq {h}, {h}, qword ptr [{b} + 8*{i3}], 1",
        "vinserti128 {r}, {r}, {h}, 1",
        r = out(ymm_reg) r,
        h = out(xmm_reg) _,
        b = in(reg) base,
        i0 = in(reg) i0,
        i1 = in(reg) i1,
        i2 = in(reg) i2,
        i3 = in(reg) i3,
        options(pure, readonly, nostack, preserves_flags)
    );
    r
}
#[inline(never)]
unsafe fn dd_mark_w(ptr: *const u64, n: usize, nptr: *const u64, nn: usize, bm: *mut u64, cl: *mut usize, mut nc: usize) -> usize {
    use std::arch::x86_64::*;
    let v_bmm = _mm256_set1_epi64x(BMM as i64);
    let v3 = _mm256_set1_epi64x(3);
    let mut ixs = [0u64; 4];
    let mut k = 0;
    while k + 16 <= n {
        if k + 8 < nn {
            _mm_prefetch(nptr.add(k) as *const i8, _MM_HINT_T0);
            _mm_prefetch(nptr.add(k + 8) as *const i8, _MM_HINT_T0);
        }
        let r = ptr.add(k);
        let (r0, r1, r2, r3) = (dd_ld(r), dd_ld(r.add(4)), dd_ld(r.add(8)), dd_ld(r.add(12)));
        let (t0, t1, t2, t3) = (_mm256_unpacklo_epi64(r0, r1), _mm256_unpackhi_epi64(r0, r1), _mm256_unpacklo_epi64(r2, r3), _mm256_unpackhi_epi64(r2, r3));
        let x0 = _mm256_permute2x128_si256::<0x20>(t0, t2);
        let x1 = _mm256_permute2x128_si256::<0x20>(t1, t3);
        let x3 = _mm256_permute2x128_si256::<0x31>(t1, t3);
        let kse = _mm256_testn_epi64_mask(x3, v3);
        let hx = _mm256_srli_epi64::<20>(dd_vhash(x0, x1));
        let ix = _mm256_and_si256(_mm256_mask_blend_epi64(kse, hx, _mm256_srli_epi64::<35>(x0)), v_bmm);
        _mm256_storeu_si256(ixs.as_mut_ptr() as *mut __m256i, ix);
        for j in 0..4 {
            nc = bm_mark(bm, cl, nc, ixs[j]);
        }
        k += 16;
    }
    while k < n {
        if k < nn {
            _mm_prefetch(nptr.add(k) as *const i8, _MM_HINT_T0);
        }
        let e = (k + 8).min(n);
        while k < e {
            let r = ptr.add(k);
            let hx = bm_key(Key { hi: *r, lo: *r.add(1) });
            let ix = if *r.add(3) & 3 == WT_SE { (*r >> 35) & BMM } else { hx };
            nc = bm_mark(bm, cl, nc, ix);
            k += 4;
        }
    }
    nc
}
#[inline(never)]
unsafe fn dd_scan_s(ptr: *const u64, n: usize, bm: *const u64, ca: *mut u64, cb: *mut u64, ow: *mut u64, oq: *mut u32, rb: u32, nptr: *const u64, nn: usize) -> (usize, usize) {
    use std::arch::x86_64::*;
    let rmask = (1u64 << rb) - 1;
    let (mut c, mut m) = (0usize, 0usize);
    let mut k = 0;
    const WIM: u64 = BMM >> 6;
    let bm1 = bm.add(BMW);
    let v63 = _mm256_set1_epi64x(63);
    let v1 = _mm256_set1_epi64x(1);
    let v2 = _mm256_set1_epi64x(2);
    let v7ff = _mm256_set1_epi64x(0x7FF);
    let v31 = _mm256_set1_epi64x(0x7FFF_FFFF);
    let vmixc = _mm256_set1_epi64x(MIXC as i64);
    let vm42 = _mm256_set1_epi64x(M42 as i64);
    let vrmask = _mm256_set1_epi64x(rmask as i64);
    let vrb = _mm_cvtsi32_si128(rb as i32);
    while k + 8 <= n {
        if k < nn {
            _mm_prefetch(nptr.add(k) as *const i8, _MM_HINT_T0);
        }
        let r0 = _mm256_loadu_si256(ptr.add(k) as *const __m256i);
        let r1: __m256i;
        std::arch::asm!("vmovdqu {r}, ymmword ptr [{p} + 32]", r = lateout(ymm_reg) r1, p = in(reg) ptr.add(k), options(pure, readonly, nostack, preserves_flags));
        let va = _mm256_permute4x64_epi64::<0b11011000>(_mm256_unpacklo_epi64(r0, r1));
        let vb = _mm256_permute4x64_epi64::<0b11011000>(_mm256_unpackhi_epi64(r0, r1));
        let p = ptr.add(k);
        let g = dd_ld4(bm1, ((*p >> 41) & WIM) as usize, ((*p.add(2) >> 41) & WIM) as usize, ((*p.add(4) >> 41) & WIM) as usize, ((*p.add(6) >> 41) & WIM) as usize);
        let ktw = _mm256_test_epi64_mask(_mm256_srlv_epi64(g, _mm256_and_si256(_mm256_srli_epi64::<35>(va), v63)), v1);
        let kinv = _mm256_test_epi64_mask(vb, v2);
        let keep = !(ktw | kinv) & 0xF;
        let ul = _mm256_or_si256(_mm256_slli_epi64::<31>(_mm256_and_si256(va, v7ff)), _mm256_and_si256(_mm256_srli_epi64::<2>(vb), v31));
        let mx = _mm256_and_si256(_mm256_mullo_epi64(ul, vmixc), vm42);
        let w = _mm256_or_si256(_mm256_slli_epi64::<32>(_mm256_and_si256(mx, vrmask)), _mm256_slli_epi64::<1>(_mm256_srli_epi64::<33>(vb)));
        let q = _mm256_cvtepi64_epi32(_mm256_srl_epi64(mx, vrb));
        _mm256_storeu_si256(ow.add(m) as *mut __m256i, _mm256_maskz_compress_epi64(keep, w));
        _mm_storeu_si128(oq.add(m) as *mut __m128i, _mm_maskz_compress_epi32(keep, q));
        m += keep.count_ones() as usize;
        _mm256_storeu_si256(ca.add(c) as *mut __m256i, _mm256_maskz_compress_epi64(ktw, va));
        _mm256_storeu_si256(cb.add(c) as *mut __m256i, _mm256_maskz_compress_epi64(ktw, vb));
        c += ktw.count_ones() as usize;
        k += 8;
    }
    while k < n {
        let (a, b) = (*ptr.add(k), *ptr.add(k + 1));
        let ix = (a >> 35) & BMM;
        let tw = (*bm.add(BMW + (ix >> 6) as usize) >> (ix & 63)) & 1;
        *ca.add(c) = a;
        *cb.add(c) = b;
        c += tw as usize;
        let mx = mix42((a & 0x7FF) << 31 | ((b >> 2) & 0x7FFF_FFFF));
        *ow.add(m) = (mx & rmask) << 32 | (b >> 33) << 1;
        *oq.add(m) = (mx >> rb) as u32;
        m += ((tw ^ 1) & !(b >> 1) & 1) as usize;
        k += 2;
    }
    (c, m)
}
#[inline(always)]
fn dd_enc_s(w0: u64, ul: u64, b2: u32) -> ([u64; 2], u32) {
    let rb = 42 - b2;
    let pay = (w0 >> 4) & 0x1FFF_FFFF;
    let pur = (pay >> 5 != 0) as u64;
    let mx = mix42(ul);
    ([(mx & ((1u64 << rb) - 1)) << 32 | (w0 >> 33) << 1 | pur, pay], (mx >> rb) as u32 | (pur as u32) << 16)
}
#[inline(always)]
fn dd_enc_w(w0: u64, uh: u64, ul: u64, b2: u32) -> ([u64; 4], u32) {
    let pay = (w0 >> 4) & 0x1FFF_FFFF;
    let pur = (pay >> 5 != 0) as u64;
    let h = hash(Key { hi: uh, lo: ul });
    ([h32(h, b2) << 32 | (w0 >> 33) << 1 | pur, ul, uh, pay], part(h, b2) as u32 | (pur as u32) << 16)
}
#[derive(Clone, Copy)]
struct WOut {
    sw: *mut u64,
    sp: *mut u64,
    sq: *mut u32,
    wa: *mut u64,
    wl: *mut u64,
    wh: *mut u64,
    wp: *mut u64,
    wq: *mut u32,
}
impl WOut {
    #[inline(always)]
    unsafe fn put_s(&self, i: usize, w0: u64, ul: u64, b2: u32) {
        let (e, q) = dd_enc_s(w0, ul, b2);
        *self.sw.add(i) = e[0];
        *self.sp.add(i) = e[1];
        *self.sq.add(i) = q;
    }
    #[inline(always)]
    unsafe fn put_w(&self, i: usize, w0: u64, uh: u64, ul: u64, b2: u32) {
        let (e, q) = dd_enc_w(w0, uh, ul, b2);
        *self.wa.add(i) = e[0];
        *self.wl.add(i) = e[1];
        *self.wh.add(i) = e[2];
        *self.wp.add(i) = e[3];
        *self.wq.add(i) = q;
    }
}
#[inline(always)]
unsafe fn dd_vhash(hi: std::arch::x86_64::__m256i, lo: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;
    let x = _mm256_mullo_epi64(_mm256_xor_si256(lo, _mm256_mullo_epi64(hi, _mm256_set1_epi64x(0xA24B_AED4_963E_E407u64 as i64))), _mm256_set1_epi64x(0x9E37_79B9_7F4A_7C15u64 as i64));
    let x = _mm256_mullo_epi64(_mm256_xor_si256(x, _mm256_srli_epi64::<32>(x)), _mm256_set1_epi64x(0xD6E8_FEB8_6659_FD93u64 as i64));
    _mm256_xor_si256(x, _mm256_srli_epi64::<32>(x))
}
#[inline(never)]
unsafe fn dd_scan_w(ptr: *const u64, n: usize, bm: *const u64, cptr: *mut usize, o: &WOut, b2: u32, nptr: *const u64, nn: usize) -> (usize, usize, usize) {
    use std::arch::x86_64::*;
    let rb = 42 - b2;
    let (mut c, mut m1, mut m2) = (0usize, 0usize, 0usize);
    let mut k = 0;
    let v_bmm = _mm256_set1_epi64x(BMM as i64);
    let v63 = _mm256_set1_epi64x(63);
    let v1 = _mm256_set1_epi64x(1);
    let v3 = _mm256_set1_epi64x(3);
    let v4 = _mm256_set1_epi64x(WT_INV as i64);
    let vpaym = _mm256_set1_epi64x(0x1FFF_FFFF);
    let vpurm = _mm256_set1_epi64x(0x1FFF_FFE0);
    let vmixc = _mm256_set1_epi64x(MIXC as i64);
    let vm42 = _mm256_set1_epi64x(M42 as i64);
    let vse = _mm256_set1_epi64x(SHORT_END as i64);
    let vrmask = _mm256_set1_epi64x(((1u64 << rb) - 1) as i64);
    let vrb = _mm_cvtsi32_si128(rb as i32);
    let vb2 = _mm_cvtsi32_si128(b2 as i32);
    let vnb2 = _mm_cvtsi32_si128(64 - b2 as i32);
    let voff = _mm256_set_epi64x(97, 65, 33, 1);
    let zero = _mm256_setzero_si256();
    let bm1 = bm.add(BMW);
    let mut wis = [0u64; 4];
    while k + 16 <= n {
        if k + 8 < nn {
            _mm_prefetch(nptr.add(k) as *const i8, _MM_HINT_T0);
            _mm_prefetch(nptr.add(k + 8) as *const i8, _MM_HINT_T0);
        }
        let r = ptr.add(k);
        let (r0, r1, r2, r3) = (dd_ld(r), dd_ld(r.add(4)), dd_ld(r.add(8)), dd_ld(r.add(12)));
        let (t0, t1, t2, t3) = (_mm256_unpacklo_epi64(r0, r1), _mm256_unpackhi_epi64(r0, r1), _mm256_unpacklo_epi64(r2, r3), _mm256_unpackhi_epi64(r2, r3));
        let x0 = _mm256_permute2x128_si256::<0x20>(t0, t2);
        let x1 = _mm256_permute2x128_si256::<0x20>(t1, t3);
        let x2 = _mm256_permute2x128_si256::<0x31>(t0, t2);
        let x3 = _mm256_permute2x128_si256::<0x31>(t1, t3);
        let kse = _mm256_testn_epi64_mask(x3, v3);
        let hx = _mm256_srli_epi64::<20>(dd_vhash(x0, x1));
        let ix = _mm256_and_si256(_mm256_mask_blend_epi64(kse, hx, _mm256_srli_epi64::<35>(x0)), v_bmm);
        _mm256_storeu_si256(wis.as_mut_ptr() as *mut __m256i, _mm256_srli_epi64::<6>(ix));
        let g = dd_ld4(bm1, wis[0] as usize, wis[1] as usize, wis[2] as usize, wis[3] as usize);
        let ktw = _mm256_test_epi64_mask(_mm256_srlv_epi64(g, _mm256_and_si256(ix, v63)), v1);
        let keep = !(ktw | _mm256_test_epi64_mask(x3, v4)) & 0xF;
        let uh = _mm256_maskz_mov_epi64(kse, x1);
        let ul = _mm256_mask_blend_epi64(kse, _mm256_and_si256(x2, vm42), x2);
        let kshort = _mm256_cmpeq_epi64_mask(uh, zero) & _mm256_cmplt_epu64_mask(ul, vse);
        let pay = _mm256_and_si256(_mm256_srli_epi64::<4>(x3), vpaym);
        let pur = _mm256_maskz_mov_epi64(_mm256_test_epi64_mask(pay, vpurm), v1);
        let lo = _mm256_or_si256(_mm256_slli_epi64::<1>(_mm256_srli_epi64::<33>(x3)), pur);
        let pur16 = _mm_slli_epi32::<16>(_mm256_cvtepi64_epi32(pur));
        let ks = keep & kshort;
        let mx = _mm256_and_si256(_mm256_mullo_epi64(ul, vmixc), vm42);
        let sw = _mm256_or_si256(_mm256_slli_epi64::<32>(_mm256_and_si256(mx, vrmask)), lo);
        let sq = _mm_or_si128(_mm256_cvtepi64_epi32(_mm256_srl_epi64(mx, vrb)), pur16);
        _mm256_storeu_si256(o.sw.add(m1) as *mut __m256i, _mm256_maskz_compress_epi64(ks, sw));
        _mm256_storeu_si256(o.sp.add(m1) as *mut __m256i, _mm256_maskz_compress_epi64(ks, pay));
        _mm_storeu_si128(o.sq.add(m1) as *mut __m128i, _mm_maskz_compress_epi32(ks, sq));
        m1 += ks.count_ones() as usize;
        let kw = keep & !kshort;
        let h = dd_vhash(uh, ul);
        let wa = _mm256_or_si256(_mm256_slli_epi64::<32>(_mm256_srli_epi64::<32>(_mm256_sll_epi64(h, vb2))), lo);
        let wq = _mm_or_si128(_mm256_cvtepi64_epi32(_mm256_srl_epi64(h, vnb2)), pur16);
        _mm256_storeu_si256(o.wa.add(m2) as *mut __m256i, _mm256_maskz_compress_epi64(kw, wa));
        _mm256_storeu_si256(o.wl.add(m2) as *mut __m256i, _mm256_maskz_compress_epi64(kw, ul));
        _mm256_storeu_si256(o.wh.add(m2) as *mut __m256i, _mm256_maskz_compress_epi64(kw, uh));
        _mm256_storeu_si256(o.wp.add(m2) as *mut __m256i, _mm256_maskz_compress_epi64(kw, pay));
        _mm_storeu_si128(o.wq.add(m2) as *mut __m128i, _mm_maskz_compress_epi32(kw, wq));
        m2 += kw.count_ones() as usize;
        let va = _mm256_add_epi64(_mm256_set1_epi64x(r as i64), voff);
        _mm256_storeu_si256(cptr.add(c) as *mut __m256i, _mm256_maskz_compress_epi64(ktw, va));
        c += ktw.count_ones() as usize;
        k += 16;
    }
    while k < n {
        let r = ptr.add(k);
        let (x0, x1, x2, x3) = (*r, *r.add(1), *r.add(2), *r.add(3));
        let se = x3 & 3 == WT_SE;
        let hx = bm_key(Key { hi: x0, lo: x1 });
        let ix = if se { (x0 >> 35) & BMM } else { hx };
        let tw = (*bm.add(BMW + (ix >> 6) as usize) >> (ix & 63)) & 1;
        *cptr.add(c) = r as usize | 1;
        c += tw as usize;
        let keep = (tw ^ 1) & !(x3 >> 2) & 1;
        let uh = if se { x1 } else { 0 };
        let ul = if se { x2 } else { x2 & (SHORT_END - 1) };
        let short = (uh == 0 && ul < SHORT_END) as u64;
        o.put_s(m1, x3, ul, b2);
        m1 += (keep & short) as usize;
        o.put_w(m2, x3, uh, ul, b2);
        m2 += (keep & (short ^ 1)) as usize;
        k += 4;
    }
    (c, m1, m2)
}
#[cold]
#[inline(never)]
unsafe fn dd_run_s(t: &mut T3, b2: u32, q: usize, p: *const u64, n: usize) {
    emit_run(t, b2, q, std::slice::from_raw_parts(p, n));
}
#[cold]
#[inline(never)]
unsafe fn dd_run_w(t: &mut T3, b2: u32, q: usize, p: *const u64, n: usize) {
    emit_run_w(t, b2, q, std::slice::from_raw_parts(p, n));
}
#[inline(never)]
unsafe fn dd_push_s(t: &mut T3, b2: u32, r: &mut Runs, ow: *const u64, oq: *const u32, m: usize) {
    let (ms, msf, fl) = (r.ms, r.msf, r.fl);
    let mut nf = r.nf;
    for j in 0..m {
        let q = *oq.add(j) as usize;
        let mut f = *msf.add(q) as usize;
        if f >= MSW {
            dd_run_s(t, b2, q, ms.add(q * MSW), f);
            f = 0;
        }
        *ms.add(q * MSW + f) = *ow.add(j);
        *msf.add(q) = (f + 1) as u8;
        *fl.add(nf) = q as u16;
        nf += (f + 1 == MST) as usize;
    }
    r.nf = nf;
}
#[inline(never)]
unsafe fn dd_push_s2(t: &mut T3, b2: u32, r: &mut Runs, o: &WOut, m: usize) {
    let (ms, msf, fl) = (r.ms, r.msf, r.fl);
    let np = t.u2.np.as_mut_ptr();
    let mut nf = r.nf;
    for j in 0..m {
        let e = *o.sq.add(j);
        let q = (e & 0xFFFF) as usize;
        let mut f = *msf.add(q) as usize;
        if f + 2 > MSW {
            dd_run_s(t, b2, q, ms.add(q * MSW), f);
            f = 0;
        }
        *ms.add(q * MSW + f) = *o.sw.add(j);
        *ms.add(q * MSW + f + 1) = *o.sp.add(j);
        let f2 = f + 1 + (e >> 16) as usize;
        if RUN_PUT {
            *np.add(q) += e >> 16;
        }
        *msf.add(q) = f2 as u8;
        *fl.add(nf) = q as u16;
        nf += (f < MST && f2 >= MST) as usize;
    }
    r.nf = nf;
}
#[inline(never)]
unsafe fn dd_push_w(t: &mut T3, b2: u32, r: &mut Runs, o: &WOut, m: usize) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_ET0};
    let (mw, mwf, fl) = (r.mw, r.mwf, r.flw);
    let mut nf = r.nfw;
    for j in 0..m {
        let qa = (*o.wq.add((j + DPA).min(m - 1)) & 0xFFFF) as usize;
        _mm_prefetch(mw.add(qa * MWW + (*mwf.add(qa) as usize & !7)) as *const i8, _MM_HINT_ET0);
        let e = *o.wq.add(j);
        let q = (e & 0xFFFF) as usize;
        let mut f = *mwf.add(q) as usize;
        if f + 4 > MWW {
            dd_run_w(t, b2, q, mw.add(q * MWW), f);
            f = 0;
        }
        *(mw.add(q * MWW + f) as *mut [u64; 4]) = [*o.wa.add(j), *o.wl.add(j), *o.wh.add(j), *o.wp.add(j)];
        let f2 = f + 3 + (e >> 16) as usize;
        *mwf.add(q) = f2 as u8;
        *fl.add(nf) = q as u16;
        nf += (f < MWT && f2 >= MWT) as usize;
    }
    r.nfw = nf;
}
#[inline(always)]
unsafe fn dd_pf4(p: *const i8) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_ET0};
    _mm_prefetch(p, _MM_HINT_ET0);
    _mm_prefetch(p.add(64), _MM_HINT_ET0);
    _mm_prefetch(p.add(128), _MM_HINT_ET0);
    _mm_prefetch(p.add(192), _MM_HINT_ET0);
}
#[inline(never)]
unsafe fn dd_drain_s(t: &mut T3, b2: u32, r: &mut Runs) {
    let st = t.u2.s.st as *const i8;
    let vs = std::mem::size_of::<VStage>();
    let nf = r.nf;
    for i in 0..nf.min(DLA) {
        dd_pf4(st.add(*r.fl.add(i) as usize * vs));
    }
    for i in 0..nf {
        if i + DLA < nf {
            dd_pf4(st.add(*r.fl.add(i + DLA) as usize * vs));
        }
        let q = *r.fl.add(i) as usize;
        let f = *r.msf.add(q) as usize;
        if f >= MST {
            emit_run(t, b2, q, std::slice::from_raw_parts(r.ms.add(q * MSW), f));
            *r.msf.add(q) = 0;
        }
    }
    r.nf = 0;
}
#[inline(never)]
unsafe fn dd_drain_w(t: &mut T3, b2: u32, r: &mut Runs) {
    let st = t.u2.w.st as *const i8;
    let vs = std::mem::size_of::<VStage>();
    let nf = r.nfw;
    for i in 0..nf.min(DLA) {
        dd_pf4(st.add(*r.flw.add(i) as usize * vs));
    }
    for i in 0..nf {
        if i + DLA < nf {
            dd_pf4(st.add(*r.flw.add(i + DLA) as usize * vs));
        }
        let q = *r.flw.add(i) as usize;
        let f = *r.mwf.add(q) as usize;
        if f >= MWT {
            emit_run_w(t, b2, q, std::slice::from_raw_parts(r.mw.add(q * MWW), f));
            *r.mwf.add(q) = 0;
        }
    }
    r.nfw = 0;
}

const RGW: usize = 16;
const RGP: usize = 8;
#[derive(Clone, Copy)]
struct Rg {
    b: *mut u64,
    f: *mut u8,
    wp: *mut *mut u64,
    nw: *mut usize,
}
const RG_START: usize = CW * 8;
unsafe fn rg_init(v: &mut Vec<u64>, f: &mut Vec<u8>, np2: usize) -> Rg {
    v.clear();
    v.resize(np2 * RGW + 8 + 2 * np2, 0);
    f.clear();
    f.resize(np2, 0);
    let a = v.as_mut_ptr();
    let b = a.add(((64 - (a as usize & 63)) & 63) >> 3);
    let wp = b.add(np2 * RGW) as *mut *mut u64;
    for q in 0..np2 {
        *wp.add(q) = RG_START as *mut u64;
    }
    Rg { b, f: f.as_mut_ptr(), wp, nw: b.add(np2 * RGW + np2) as *mut usize }
}
#[cold]
#[inline(never)]
unsafe fn rg_chunk(vb: &mut VB, q: usize, w: *mut u64) -> *mut u64 {
    let c = vb.chunk();
    if w as usize == RG_START {
        (*vb.st.add(q)).head = c;
    } else {
        *(w as *mut *mut u64) = c;
    }
    c
}
#[inline(always)]
unsafe fn rg_line(vb: &mut VB, r: Rg, q: usize, src: *const u64) {
    let mut w = *r.wp.add(q);
    if (w as usize) & (CHUNK - 1) == CW * 8 {
        w = rg_chunk(vb, q, w);
    }
    nt_line(w, src);
    *r.wp.add(q) = w.add(8);
    *r.nw.add(q) += 8;
}
#[inline(never)]
unsafe fn rg_push_s(vb: &mut VB, r: Rg, ow: *const u64, oq: *const u32, m: usize) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_ET0};
    for j in 0..m {
        let qa = *oq.add((j + RGP).min(m - 1)) as usize;
        _mm_prefetch(r.b.add(qa * RGW) as *const i8, _MM_HINT_ET0);
        let q = *oq.add(j) as usize;
        let k = *r.f.add(q) as usize;
        let d = r.b.add(q * RGW);
        *d.add(k) = *ow.add(j);
        if k == 7 {
            rg_line(vb, r, q, d);
            *r.f.add(q) = 0;
        } else {
            *r.f.add(q) = (k + 1) as u8;
        }
    }
}
#[inline(never)]
unsafe fn rg_push_s2(vb: &mut VB, np: *mut u32, r: Rg, o: &WOut, m: usize) {
    use std::arch::x86_64::{_mm_prefetch, _MM_HINT_ET0};
    for j in 0..m {
        let qa = (*o.sq.add((j + RGP).min(m - 1)) & 0xFFFF) as usize;
        _mm_prefetch(r.b.add(qa * RGW) as *const i8, _MM_HINT_ET0);
        let e = *o.sq.add(j);
        let q = (e & 0xFFFF) as usize;
        let pur = (e >> 16) as usize;
        let k = *r.f.add(q) as usize;
        let d = r.b.add(q * RGW);
        *d.add(k) = *o.sw.add(j);
        *d.add(k + 1) = *o.sp.add(j);
        *np.add(q) += pur as u32;
        let mut k2 = k + 1 + pur;
        if k2 >= 8 {
            rg_line(vb, r, q, d);
            *d = *d.add(8);
            k2 -= 8;
        }
        *r.f.add(q) = k2 as u8;
    }
}
#[inline(never)]
unsafe fn rg_push_w(vb: &mut VB, r: Rg, o: &WOut, m: usize) {
    use std::arch::x86_64::*;
    for j in 0..m {
        let ja = (j + RGP).min(m - 1);
        let qa = (*o.wq.add(ja) & 0xFFFF) as usize;
        let pa = r.b.add(qa * RGW);
        _mm_prefetch(pa as *const i8, _MM_HINT_ET0);
        _mm_prefetch(pa.add(*r.f.add(qa) as usize + 3) as *const i8, _MM_HINT_ET0);
        let e = *o.wq.add(j);
        let q = (e & 0xFFFF) as usize;
        let pur = (e >> 16) as usize;
        let k = *r.f.add(q) as usize;
        let d = r.b.add(q * RGW);
        *d.add(k) = *o.wa.add(j);
        *d.add(k + 1) = *o.wl.add(j);
        *d.add(k + 2) = *o.wh.add(j);
        *d.add(k + 3) = *o.wp.add(j);
        let mut k2 = k + 3 + pur;
        if k2 >= 8 {
            rg_line(vb, r, q, d);
            _mm256_store_si256(d as *mut __m256i, _mm256_load_si256(d.add(8) as *const __m256i));
            k2 -= 8;
        }
        *r.f.add(q) = k2 as u8;
    }
}
unsafe fn rg_fin(vb: &mut VB, r: Rg, np2: usize) {
    for q in 0..np2 {
        let k = *r.f.add(q) as usize;
        if k > 0 {
            rg_line(vb, r, q, r.b.add(q * RGW));
            *r.nw.add(q) -= 8 - k;
            *r.f.add(q) = 0;
        }
        let w = *r.wp.add(q);
        let s = vb.st.add(q);
        if w as usize != RG_START {
            (*s).wptr = w;
        }
        (*s).nwords += *r.nw.add(q);
        *r.nw.add(q) = 0;
    }
}

fn dedup_thread(cx: &Cx, tid: usize) {
    use std::arch::x86_64::_rdtsc;
    let nt = cx.t.len();
    let t = cx.t(tid);
    let me = p3(tid);
    let b2 = cx.b2;
    let np = 1usize << cx.b1;
    let sh = unsafe { ESH };
    let np2 = 1usize << b2;
    let rb = 42 - b2;
    if b2 > 16 {
        fallback("dedup: user partitions over 2^16");
    }
    me.seen.resize(2 << (BMB - 6), 0);
    let rs = unsafe { rg_init(&mut me.ms, &mut me.msf, np2) };
    let rw = unsafe { rg_init(&mut me.mw, &mut me.mwf, np2) };
    me.ow.resize(DREC + 8, 0);
    me.oq.resize(DREC + 8, 0);
    me.so.resize(2 * (DREC + 8), 0);
    me.sq.resize(DREC + 8, 0);
    me.wo.resize(4 * (DREC + 8), 0);
    me.wq.resize(DREC + 8, 0);
    let (ow, oq) = (me.ow.as_mut_ptr(), me.oq.as_mut_ptr());
    let wout = unsafe {
        let (so, wo) = (me.so.as_mut_ptr(), me.wo.as_mut_ptr());
        WOut { sw: so, sp: so.add(DREC + 8), sq: me.sq.as_mut_ptr(), wa: wo, wl: wo.add(DREC + 8), wh: wo.add(2 * (DREC + 8)), wp: wo.add(3 * (DREC + 8)), wq: me.wq.as_mut_ptr() }
    };
    let mut kept = 0u64;
    let mut rel_tmp: Vec<*mut u8> = Vec::with_capacity(4096);
    #[inline(always)]
    fn pf_slice(sl: &[(*const u64, usize)], i: usize) {
        if let Some(&(p, n)) = sl.get(i) {
            let mut o = 0;
            while o < n.min(32) {
                unsafe { std::arch::x86_64::_mm_prefetch(p.add(o) as *const i8, std::arch::x86_64::_MM_HINT_T0) };
                o += 8;
            }
        }
    }
    loop {
        let p = cx.next.fetch_add(1, Ordering::Relaxed);
        if p >= np {
            break;
        }
        let mut tk = std::time::Instant::now();
        let gs = unsafe { _rdtsc() };
        me.ssl.clear();
        me.wsl.clear();
        me.xsl.clear();
        let mut nw = 0;
        for o in 0..nt {
            let src = p3(o);
            nw += src.es.slices(p, &mut me.ssl);
            nw += src.ew.slices(p, &mut me.wsl);
            nw += src.ex.slices(p, &mut me.xsl);
        }
        let g0 = unsafe { _rdtsc() };
        me.dc[0] += g0 - gs;
        me.seen.fill(0);
        let bm = me.seen.as_mut_ptr();
        me.cptr.clear();
        me.cptr.reserve(nw / 2 + 16);
        let cl = me.cptr.as_mut_ptr();
        let mut ncl = 0usize;
        pf_slice(&me.ssl, 0);
        for (i, &(ptr, n)) in me.ssl.iter().enumerate() {
            let (nptr, nn) = me.ssl.get(i + 1).copied().unwrap_or((ptr, 0));
            ncl = unsafe { dd_mark_s(ptr, n, nptr, nn, bm, cl, ncl) };
        }
        pf_slice(&me.wsl, 0);
        for (i, &(ptr, n)) in me.wsl.iter().enumerate() {
            let (nptr, nn) = me.wsl.get(i + 1).copied().unwrap_or((ptr, 0));
            ncl = unsafe { dd_mark_w(ptr, n, nptr, nn, bm, cl, ncl) };
        }
        for &(ptr, n) in me.xsl.iter() {
            let mut k = 0;
            while k < n {
                unsafe {
                    ncl = bm_mark(bm, cl, ncl, x_index(ptr.add(k), sh));
                }
                k += 8;
            }
        }
        unsafe { bm_twice(bm, cl, ncl) };
        let twice = |ix: u64| unsafe { (*bm.add(BMW + (ix >> 6) as usize) >> (ix & 63)) & 1 };
        me.dc[1] += unsafe { _rdtsc() } - g0;
        t.prof[0] += tick(&mut tk);
        me.cptr.clear();
        me.cptr.reserve(nw / 4 + 8);
        let cptr = me.cptr.as_mut_ptr();
        let mut nc = 0usize;
        me.ca.clear();
        me.ca.reserve(nw / 2 + 8);
        me.cb.clear();
        me.cb.reserve(nw / 2 + 8);
        let (ca, cb) = (me.ca.as_mut_ptr(), me.cb.as_mut_ptr());
        let mut ncs = 0usize;
        let c0 = unsafe { _rdtsc() };
        for (i, &(ptr, n)) in me.ssl.iter().enumerate() {
            let (nptr, nn) = me.ssl.get(i + 1).copied().unwrap_or((ptr, 0));
            let mut o = 0;
            while o < n {
                let nb = (n - o).min(2 * DREC);
                unsafe {
                    let (c, m) = dd_scan_s(ptr.add(o), nb, bm, ca.add(ncs), cb.add(ncs), ow, oq, rb, nptr.wrapping_add(o), nn.saturating_sub(o));
                    ncs += c;
                    kept += m as u64;
                    rg_push_s(&mut t.u2.s, rs, ow, oq, m);
                }
                o += nb;
            }
        }
        let c1 = unsafe { _rdtsc() };
        me.cyc[3] += c1 - c0;
        for (i, &(ptr, n)) in me.wsl.iter().enumerate() {
            let (nptr, nn) = me.wsl.get(i + 1).copied().unwrap_or((ptr, 0));
            let mut o = 0;
            while o < n {
                let nb = (n - o).min(4 * DREC);
                unsafe {
                    let (c, m1, m2) = dd_scan_w(ptr.add(o), nb, bm, cptr.add(nc), &wout, b2, nptr.wrapping_add(o), nn.saturating_sub(o));
                    nc += c;
                    kept += (m1 + m2) as u64;
                    rg_push_s2(&mut t.u2.s, t.u2.np.as_mut_ptr(), rs, &wout, m1);
                    rg_push_w(&mut t.u2.w, rw, &wout, m2);
                }
                o += nb;
            }
        }
        for &(ptr, n) in me.xsl.iter() {
            let mut k = 0;
            while k < n {
                unsafe {
                    let r = ptr.add(k);
                    if twice(x_index(r, sh)) != 0 {
                        *cptr.add(nc) = r as usize | 1;
                        nc += 1;
                    } else if *r.add(3) & WT_INV == 0 {
                        let (w0, uh, ul) = (*r.add(3) & !15, *r.add(2), *r.add(4));
                        if uh == 0 && ul < SHORT_END {
                            wout.put_s(0, w0, ul, b2);
                            rg_push_s2(&mut t.u2.s, t.u2.np.as_mut_ptr(), rs, &wout, 1);
                        } else {
                            wout.put_w(0, w0, uh, ul, b2);
                            rg_push_w(&mut t.u2.w, rw, &wout, 1);
                        }
                        kept += 1;
                    }
                }
                k += 8;
            }
        }
        me.cyc[5] += unsafe { _rdtsc() } - c1;
        t.prof[1] += tick(&mut tk);
        let d0 = unsafe { _rdtsc() };
        let ntot = ncs + nc;
        if ntot >= u32::MAX as usize / 2 {
            fallback("dedup partition too large");
        }
        let cap = (ntot * 2).next_power_of_two().max(64);
        let mask = cap - 1;
        me.ctab.clear();
        me.ctab.resize(cap, u32::MAX);
        me.dead.clear();
        me.dead.resize(ntot + 1, 0);
        me.cands.clear();
        me.cands.reserve(nc);
        for i in 0..nc {
            unsafe {
                std::arch::x86_64::_mm_prefetch((*cptr.add((i + 8).min(nc - 1)) & !1) as *const i8, std::arch::x86_64::_MM_HINT_T0);
                me.cands.push(cand_of(*cptr.add(i), sh));
            }
        }
        let d1 = unsafe { _rdtsc() };
        me.dc[2] += d1 - d0;
        let (cands, ctab, dead) = (me.cands.as_ptr(), me.ctab.as_mut_ptr(), me.dead.as_mut_ptr());
        #[inline(always)]
        fn ch_s(rem: u64) -> usize {
            (rem.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize
        }
        for i in 0..ncs {
            unsafe {
                let a = *ca.add(i);
                let mut s = ch_s(a >> 32) & mask;
                loop {
                    let e = *ctab.add(s);
                    let empty = e == u32::MAX;
                    let e2 = if empty { i as u32 } else { e };
                    let o = *ca.add(e2 as usize);
                    if (o ^ a) >> 32 != 0 {
                        s = (s + 1) & mask;
                        continue;
                    }
                    let wins = (a as u32 >> 11) < (o as u32 >> 11);
                    let best = if wins { i as u32 } else { e2 };
                    let loser = if wins { e2 } else { i as u32 };
                    *ctab.add(s) = best;
                    *dead.add(loser as usize) = !empty as u8;
                    break;
                }
            }
        }
        for j in 0..nc {
            unsafe {
                let i = ncs + j;
                let (k, g) = ((*cands.add(j)).k, (*cands.add(j)).ing);
                let mut s = if k.hi == 0 && k.lo < 1 << 32 { ch_s(k.lo) } else { (hash(k) >> 8) as usize } & mask;
                loop {
                    let e = *ctab.add(s);
                    let empty = e == u32::MAX;
                    let e2 = if empty { i } else { e as usize };
                    let (ok, og) = if e2 < ncs {
                        let o = *ca.add(e2);
                        (Key { hi: 0, lo: o >> 32 }, (o as u32 >> 11) as i64 * 1000)
                    } else {
                        ((*cands.add(e2 - ncs)).k, (*cands.add(e2 - ncs)).ing)
                    };
                    if ok != k {
                        s = (s + 1) & mask;
                        continue;
                    }
                    let wins = g < og;
                    let best = if wins { i } else { e2 };
                    let loser = if wins { e2 } else { i };
                    *ctab.add(s) = best as u32;
                    *dead.add(loser) = !empty as u8;
                    break;
                }
            }
        }
        let d2 = unsafe { _rdtsc() };
        me.dc[3] += d2 - d1;
        let rmask = (1u64 << rb) - 1;
        let mut i = 0;
        while i < ncs {
            let e = (i + DREC).min(ncs);
            let mut m = 0usize;
            while i < e {
                unsafe {
                    let (a, b) = (*ca.add(i), *cb.add(i));
                    let mx = mix42((a & 0x7FF) << 31 | ((b >> 2) & 0x7FFF_FFFF));
                    *ow.add(m) = (mx & rmask) << 32 | (b >> 33) << 1;
                    *oq.add(m) = (mx >> rb) as u32;
                    m += (*dead.add(i) == 0 && b & 2 == 0) as usize;
                }
                i += 1;
            }
            kept += m as u64;
            unsafe {
                rg_push_s(&mut t.u2.s, rs, ow, oq, m);
            }
        }
        let mut j = 0;
        while j < nc {
            let e = (j + DREC).min(nc);
            let (mut m1, mut m2) = (0usize, 0usize);
            while j < e {
                unsafe {
                    let c = &*cands.add(j);
                    if *dead.add(ncs + j) == 0 && c.w0 != W0_INVALID {
                        if c.uh == 0 && c.ul < SHORT_END {
                            wout.put_s(m1, c.w0, c.ul, b2);
                            m1 += 1;
                        } else {
                            wout.put_w(m2, c.w0, c.uh, c.ul, b2);
                            m2 += 1;
                        }
                    }
                }
                j += 1;
            }
            kept += (m1 + m2) as u64;
            unsafe {
                rg_push_s2(&mut t.u2.s, t.u2.np.as_mut_ptr(), rs, &wout, m1);
                rg_push_w(&mut t.u2.w, rw, &wout, m2);
            }
        }
        let d3 = unsafe { _rdtsc() };
        me.dc[4] += d3 - d2;
        for o in 0..nt {
            let src = p3(o);
            src.es.release(p, &mut rel_tmp);
            src.ew.release(p, &mut rel_tmp);
            src.ex.release(p, &mut rel_tmp);
        }
        for c in rel_tmp.drain(..) {
            if ext_kept(c as usize) {
                continue;
            }
            if t.u2.w.free.len() < t.u2.s.free.len() {
                t.u2.w.free.push(c as *mut u64);
            } else {
                t.u2.s.free.push(c as *mut u64);
            }
        }
        me.dc[5] += unsafe { _rdtsc() } - d3;
        t.prof[2] += tick(&mut tk);
    }
    let d4 = unsafe { _rdtsc() };
    unsafe {
        rg_fin(&mut t.u2.s, rs, np2);
        rg_fin(&mut t.u2.w, rw, np2);
    }
    me.dc[6] += unsafe { _rdtsc() } - d4;
    t.n_rows[3] += kept;
    t.u2.finish();
}

#[inline(always)]
unsafe fn cand_of(rec: usize, sh: u32) -> Cand {
    let r = (rec & !1) as *const u64;
    let a = *r;
    if rec & 1 == 0 {
        let b = *r.add(1);
        let w0 = if b & 2 == 0 { b & !((1u64 << 33) - 1) } else { W0_INVALID };
        return Cand { k: Key { hi: 0, lo: a >> 32 }, ing: ((a >> 11) & 0x1F_FFFF) as i64 * 1000, w0, uh: 0, ul: (a & 0x7FF) << 31 | ((b >> 2) & 0x7FFF_FFFF) };
    }
    let w3 = *r.add(3);
    let w0 = if w3 & WT_INV == 0 { w3 & !15 } else { W0_INVALID };
    let tag = w3 & 3;
    if tag == WT_SE {
        Cand { k: Key { hi: 0, lo: a >> 32 }, ing: ((a >> 11) & 0x1F_FFFF) as i64 * 1000, w0, uh: *r.add(1), ul: *r.add(2) }
    } else if tag == WT_LE {
        let c = *r.add(2);
        Cand { k: Key { hi: a, lo: *r.add(1) }, ing: (c >> 42) as i64 * 1000, w0, uh: 0, ul: c & (SHORT_END - 1) }
    } else {
        let mut k = Key { hi: a, lo: *r.add(1) };
        if k.hi == 0 && k.lo < SHORT_END {
            k.lo = e_rem(k.lo, sh);
        }
        Cand { k, ing: *r.add(5) as i64, w0, uh: *r.add(2), ul: *r.add(4) }
    }
}

fn link_tab(tab: &mut Tab, l: &[(Key, Key)]) {
    tab.reset(l.len());
    for (i, e) in l.iter().enumerate() {
        let h = hash(e.0);
        match tab.find(h, |j| l[j as usize].0 == e.0) {
            Ok(_) => {}
            Err(slot) => tab.set(slot, h, i as u32),
        }
    }
}

const RBB: u32 = 14;
struct Back {
    lkf: Vec<(Key, Key)>,
    lko: Vec<usize>,
    rep: Vec<AtomicU64>,
}
#[inline(always)]
fn rep_ix_short(rem: u32) -> usize {
    (rem.wrapping_mul(0x9E37_79B1) >> (32 - RBB)) as usize
}
#[inline(always)]
fn rep_slot(k: Key, b2: u32) -> (usize, u64) {
    let (q, ix) = if is_short(k) {
        let m = mix42(k.lo);
        let rb = 42 - b2;
        ((m >> rb) as usize, rep_ix_short((m & ((1u64 << rb) - 1)) as u32))
    } else {
        let h = hash(k);
        (part(h, b2), (h >> 16) as usize & ((1 << RBB) - 1))
    };
    ((q << (RBB - 6)) + (ix >> 6), 1u64 << (ix & 63))
}

struct B3 {
    blk: LB,
    es: Vec<u64>,
    es2: Vec<u64>,
    ps: Vec<[u64; 2]>,
    ps2: Vec<[u64; 2]>,
    cs: Vec<[u64; 2]>,
    cs2: Vec<[u64; 2]>,
    ew: Vec<[u64; 3]>,
    ew2: Vec<[u64; 3]>,
    pw: Vec<[u64; 4]>,
    pw2: Vec<[u64; 4]>,
    cw: Vec<[u64; 4]>,
    cw2: Vec<[u64; 4]>,
    cside: Vec<Key>,
    gt: GTab,
    stab: Vec<u64>,
    wtab: Tab,
    flat: Vec<u64>,
    sl: Vec<(*const u64, usize)>,
    bw: Vec<u64>,
    w: Walk,
    pm: [[u64; 3]; 16],
    kt: KTab,
    names: Vec<Key>,
    evr: Vec<Run>,
    ckr: Vec<Run>,
    evo: Vec<Run>,
    cko: Vec<Run>,
    ecnt: Vec<u32>,
    ccnt: Vec<u32>,
    eoff: Vec<u32>,
    coff: Vec<u32>,
    ut: UTab,
    e32: Vec<u32>,
    pv: Vec<u64>,
    lkix: Vec<u32>,
    homes: Vec<u32>,
    lrep: Vec<u32>,
    ldir: Vec<u32>,
    ub: Vec<u64>,
    base: Vec<u32>,
    rn: Vec<Run2>,
    ro: Vec<Run2>,
    c4: Vec<[u64; 4]>,
    lmov: Vec<u32>,
}
impl B3 {
    fn new(np: usize, ncamp: usize, nbins: usize) -> B3 {
        B3 {
            blk: LB::new(np),
            es: Vec::new(),
            es2: Vec::new(),
            ps: Vec::new(),
            ps2: Vec::new(),
            cs: Vec::new(),
            cs2: Vec::new(),
            ew: Vec::new(),
            ew2: Vec::new(),
            pw: Vec::new(),
            pw2: Vec::new(),
            cw: Vec::new(),
            cw2: Vec::new(),
            cside: Vec::new(),
            gt: GTab::new(),
            stab: Vec::new(),
            wtab: Tab::default(),
            flat: Vec::new(),
            sl: Vec::new(),
            bw: Vec::new(),
            w: Walk::new(ncamp, nbins),
            pm: [[0; 3]; 16],
            kt: KTab::default(),
            names: Vec::new(),
            evr: Vec::new(),
            ckr: Vec::new(),
            evo: Vec::new(),
            cko: Vec::new(),
            ecnt: Vec::new(),
            ccnt: Vec::new(),
            eoff: Vec::new(),
            coff: Vec::new(),
            ut: UTab::new(),
            e32: Vec::new(),
            pv: Vec::new(),
            lkix: Vec::new(),
            homes: Vec::new(),
            lrep: Vec::new(),
            ldir: Vec::new(),
            ub: Vec::new(),
            base: Vec::new(),
            rn: Vec::new(),
            ro: Vec::new(),
            c4: Vec::new(),
            lmov: Vec::new(),
        }
    }
}
static mut B3S: Vec<B3> = Vec::new();
#[inline(always)]
fn b3(tid: usize) -> *mut B3 {
    unsafe { (*std::ptr::addr_of_mut!(B3S)).as_mut_ptr().add(tid) }
}

mod pm3 {
    extern "C" {
        fn syscall(n: i64, ...) -> i64;
        fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    }
    pub static mut ON: bool = false;
    pub const NE: usize = 3;
    thread_local! {
        static FD: [i32; NE] = {
            let mut fd = [-1; NE];
            for (k, cfg) in [1u64, 5, 0].iter().enumerate() {
                let mut a = [0u64; 16];
                a[0] = 128u64 << 32;
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
        FD.with(|fd| {
            for k in 0..NE {
                unsafe { read(fd[k], &mut v[k] as *mut u64 as *mut u8, 8) };
            }
        });
        v
    }
    #[inline(always)]
    pub fn lap(acc: &mut [[u64; NE]; 16], last: &mut [u64; NE], k: usize) {
        if unsafe { ON } {
            let now = rd();
            for e in 0..NE {
                acc[k][e] += now[e].wrapping_sub(last[e]);
            }
            *last = rd();
        }
    }
}
static mut PF: bool = false;
static mut HIST: [[u64; 3]; 8] = [[0; 3]; 8];
#[inline(always)]
fn pf_now() -> u64 {
    if unsafe { PF } {
        unsafe { core::arch::x86_64::_rdtsc() }
    } else {
        0
    }
}

#[derive(Clone, Copy)]
struct RunD {
    i: u32,
    j: u32,
    pi: u32,
    pj: u32,
    ci: u32,
    cj: u32,
    home: u32,
    pk: Key,
}
const DIRECT: u32 = u32::MAX;

enum Lk {
    Moved(Key),
    Rep,
    No,
}

#[inline(always)]
unsafe fn pf_words(p: *const u64, n: usize) {
    use std::arch::x86_64::*;
    let mut a = p as *const i8;
    let e = a.add(n * 8);
    while a < e {
        _mm_prefetch(a, _MM_HINT_T0);
        a = a.add(64);
    }
}
#[inline(never)]
fn copy_slices(sl: &[(*const u64, usize)], out: &mut Vec<u64>) {
    let mut tot = 0usize;
    for s in sl.iter() {
        tot = opq64((tot + s.1) as u64) as usize;
    }
    out.reserve(tot);
    let pd = match unsafe { KN[0] & 15 } {
        0 => 2,
        15 => 0,
        v => v,
    };
    unsafe {
        let mut d = out.as_mut_ptr().add(out.len());
        for (k, &(ptr, n)) in sl.iter().enumerate() {
            if pd != 0 && k + pd < sl.len() {
                pf_words(sl.get_unchecked(k + pd).0, sl.get_unchecked(k + pd).1);
            }
            std::ptr::copy_nonoverlapping(ptr, d, n);
            d = d.add(n);
        }
        out.set_len(out.len() + tot);
    }
}

#[inline(never)]
fn gather_events(sl: &[(*const u64, usize)], gt: &mut GTab, sh: u32, ep: *mut u64, pp: *mut [u64; 2]) -> (usize, usize) {
    let c = gt.cnt.as_mut_ptr();
    let um = gt.used.as_mut_ptr();
    let (mut epp, mut ppp) = (ep, pp);
    let mut pend: Option<u64> = None;
    unsafe {
        for &(ptr, n) in sl.iter() {
            if n == 0 {
                if pend.is_some() {
                    fallback("event stream out of step");
                }
                continue;
            }
            let mut p = ptr;
            let end = ptr.add(n);
            if let Some(w) = pend.take() {
                *ppp = [w, *p];
                ppp = ppp.add(1);
                p = p.add(1);
            }
            while p < end {
                let w = *p;
                p = p.add(1);
                *epp = w;
                epp = epp.add(1);
                let b = (w >> sh) as usize;
                *c.add(b) += 1;
                *um.add(b) = 1;
                if w & 1 != 0 {
                    if p == end {
                        pend = Some(w);
                        break;
                    }
                    *ppp = [w, *p];
                    ppp = ppp.add(1);
                    p = p.add(1);
                }
            }
        }
        if pend.is_some() {
            fallback("event stream out of step");
        }
        (epp.offset_from(ep) as usize, ppp.offset_from(pp) as usize)
    }
}

#[inline(never)]
fn gather_clicks(flat: &[u64], gt: &mut GTab, sh: u32, cs: *mut [u64; 2], cside: &mut Vec<Key>) -> usize {
    let c = gt.cnt.as_mut_ptr();
    let um = gt.used.as_mut_ptr();
    unsafe {
        let mut p = flat.as_ptr();
        let end = p.add(flat.len());
        let mut csp = cs;
        while p < end {
            let a = *p;
            let mut x = *p.add(1);
            p = p.add(2);
            if a & 1 != 0 {
                x |= cside.len() as u64;
                cside.push(Key { lo: *p, hi: *p.add(1) });
                p = p.add(2);
            }
            *csp = [a, x];
            csp = csp.add(1);
            let b = (a >> sh) as usize;
            *c.add(b) += 1;
            *um.add(b) = 1;
        }
        csp.offset_from(cs) as usize
    }
}

struct UTab {
    tab: Vec<u64>,
    bits: u32,
    rb: u32,
    sh: u32,
    ns: usize,
    wt: Vec<u64>,
    wbits: u32,
    wsh: u32,
    nw: usize,
    nu: usize,
    cap: usize,
    rems: Vec<u32>,
    keys: Vec<Key>,
    ec: Vec<u32>,
    pc: Vec<u32>,
    cc: Vec<u32>,
}
impl UTab {
    fn new() -> UTab {
        UTab { tab: Vec::new(), bits: 0, rb: 0, sh: 0, ns: 0, wt: Vec::new(), wbits: 0, wsh: 0, nw: 0, nu: 0, cap: 0, rems: Vec::new(), keys: Vec::new(), ec: Vec::new(), pc: Vec::new(), cc: Vec::new() }
    }
    #[cold]
    #[inline(never)]
    fn ids(&mut self, cap: usize) {
        self.cap = cap;
        self.rems.resize(cap, 0);
        self.keys.resize(cap, Key { hi: 0, lo: 0 });
        self.ec.resize(cap, 0);
        self.pc.resize(cap, 0);
        self.cc.resize(cap, 0);
    }
    fn slots(&mut self) {
        self.sh = self.rb - self.bits;
        self.tab.clear();
        self.tab.resize((1 << self.bits) + 1, EMPTY);
        if self.rb < 32 {
            self.tab[1 << self.bits] = (1u64 << self.rb) << 32;
        }
    }
    fn reset(&mut self, users: usize, wusers: usize, rb: u32) {
        let f = match unsafe { KN[0] >> 4 & 15 } {
            0 => 4,
            v => v,
        };
        self.bits = (usize::BITS - (users * f).leading_zeros()).clamp(9, 24).min(rb);
        self.rb = rb;
        self.slots();
        self.ns = 0;
        let wbits = (usize::BITS - (wusers * f).leading_zeros()).clamp(6, 24);
        self.wbits = wbits;
        self.wsh = 32 - wbits;
        self.wt.clear();
        self.wt.resize(1 << wbits, EMPTY);
        self.nw = 0;
        self.nu = 0;
        if self.cap < 1024 {
            self.ids(1024);
        }
        self.rems[0] = 0;
        self.keys[0] = Key { hi: 0, lo: 0 };
        self.ec[0] = 0;
        self.pc[0] = 0;
        self.cc[0] = 0;
    }
    #[inline(always)]
    unsafe fn place(tab: &mut [u64], mask: usize, sh: u32, x: u64, id: u64) {
        let mut s = (x >> sh) as usize;
        while *tab.get_unchecked(s) != EMPTY {
            s = (s + 1) & mask;
        }
        *tab.get_unchecked_mut(s) = x << 32 | id;
    }
    #[inline(always)]
    unsafe fn new_id(&mut self, x: u64, k: Key) -> u64 {
        let id = self.nu + 1;
        if id >= self.cap {
            if id >= (1 << 31) - 2 {
                fallback("too many users in a partition");
            }
            self.ids(self.cap * 2);
        }
        self.nu = id;
        *self.rems.get_unchecked_mut(id) = x as u32;
        *self.keys.get_unchecked_mut(id) = k;
        *self.ec.get_unchecked_mut(id) = 0;
        *self.pc.get_unchecked_mut(id) = 0;
        *self.cc.get_unchecked_mut(id) = 0;
        id as u64
    }
    #[inline(always)]
    unsafe fn miss(&mut self, rem: u64, e: u64) -> u64 {
        if e == EMPTY && self.nu + 2 < self.cap && (self.ns + 1) * 2 <= self.tab.len() - 1 {
            let id = self.nu + 1;
            self.nu = id;
            self.ns += 1;
            *self.rems.get_unchecked_mut(id) = rem as u32;
            *self.keys.get_unchecked_mut(id) = Key { hi: 0, lo: rem };
            *self.ec.get_unchecked_mut(id) = 0;
            *self.pc.get_unchecked_mut(id) = 0;
            *self.cc.get_unchecked_mut(id) = 0;
            let v = rem << 32 | id as u64;
            *self.tab.get_unchecked_mut((rem >> self.sh) as usize) = v;
            v
        } else {
            self.slow(rem, true)
        }
    }
    #[inline(always)]
    unsafe fn entry(&mut self, rem: u64) -> u64 {
        let e = *self.tab.get_unchecked((rem >> self.sh) as usize);
        if e >> 32 == rem {
            e
        } else {
            self.miss(rem, e)
        }
    }
    #[cold]
    #[inline(never)]
    unsafe fn slow(&mut self, rem: u64, insert: bool) -> u64 {
        let mask = self.tab.len() - 2;
        let mut s = (rem >> self.sh) as usize;
        loop {
            let e = *self.tab.get_unchecked(s);
            if e == EMPTY {
                break;
            }
            if e >> 32 == rem {
                return e;
            }
            s = (s + 1) & mask;
        }
        if !insert {
            return 0;
        }
        let id = self.new_id(rem, Key { hi: 0, lo: rem });
        self.ns += 1;
        if self.ns * 2 > self.tab.len() - 1 {
            if self.bits >= self.rb.min(28) {
                fallback("too many users in a partition");
            }
            self.bits += 1;
            self.slots();
            let (mask, sh) = (self.tab.len() - 2, self.sh);
            for i in 1..=self.nu {
                if is_short(*self.keys.get_unchecked(i)) {
                    Self::place(&mut self.tab, mask, sh, *self.rems.get_unchecked(i) as u64, i as u64);
                }
            }
        } else {
            *self.tab.get_unchecked_mut(s) = rem << 32 | id;
        }
        rem << 32 | id
    }
    #[cold]
    #[inline(never)]
    unsafe fn slow_w(&mut self, hb: u64, k: Key, insert: bool) -> u64 {
        let mask = self.wt.len() - 1;
        let mut s = (hb >> self.wsh) as usize;
        loop {
            let e = *self.wt.get_unchecked(s);
            if e == EMPTY {
                break;
            }
            if e >> 32 == hb && *self.keys.get_unchecked(e as u32 as usize) == k {
                return e;
            }
            s = (s + 1) & mask;
        }
        if !insert {
            return 0;
        }
        let id = self.new_id(hb, k);
        self.nw += 1;
        if self.nw * 2 > self.wt.len() {
            if self.wbits >= 28 {
                fallback("too many users in a partition");
            }
            self.wbits += 1;
            self.wsh = 32 - self.wbits;
            self.wt.clear();
            self.wt.resize(1 << self.wbits, EMPTY);
            let (mask, sh) = (self.wt.len() - 1, self.wsh);
            for i in 1..=self.nu {
                if !is_short(*self.keys.get_unchecked(i)) {
                    Self::place(&mut self.wt, mask, sh, *self.rems.get_unchecked(i) as u64, i as u64);
                }
            }
        } else {
            *self.wt.get_unchecked_mut(s) = hb << 32 | id;
        }
        hb << 32 | id
    }
    #[cold]
    #[inline(never)]
    unsafe fn slow_click(&mut self, rem: u64, repq: &[AtomicU64]) -> u64 {
        let e = self.slow(rem, false);
        if e != 0 {
            return e;
        }
        let ix = rep_ix_short(rem as u32);
        if (repq.get_unchecked(ix >> 6).load(Ordering::Relaxed) >> (ix & 63)) & 1 != 0 {
            self.slow(rem, true)
        } else {
            0
        }
    }
    #[cold]
    #[inline(never)]
    unsafe fn slow_click_w(&mut self, hb: u64, k: Key, repq: &[AtomicU64]) -> u64 {
        let e = self.slow_w(hb, k, false);
        if e != 0 {
            return e;
        }
        let ix = (hash(k) >> 16) as usize & ((1 << RBB) - 1);
        if (repq.get_unchecked(ix >> 6).load(Ordering::Relaxed) >> (ix & 63)) & 1 != 0 {
            self.slow_w(hb, k, true)
        } else {
            0
        }
    }
}
#[inline(always)]
fn u32p_of(p: *mut u64) -> *const u32 {
    p as *const u32
}
#[inline(always)]
fn prefix(c: &mut [u32]) -> usize {
    let mut s = 0u32;
    for x in c.iter_mut() {
        let v = *x;
        *x = s;
        s = s.wrapping_add(v);
    }
    s as usize
}

#[inline(never)]
unsafe fn ua_events(sl: &[(*const u64, usize)], u: &mut UTab, es: *mut u64, ps: *mut [u64; 2]) -> (usize, usize) {
    let (mut esp, mut psp) = (es, ps);
    let mut pend: Option<(u64, u64)> = None;
    for &(ptr, n) in sl.iter() {
        if n == 0 {
            if pend.is_some() {
                fallback("event stream out of step");
            }
            continue;
        }
        let mut p = ptr;
        let end = ptr.add(n);
        if let Some((id, w)) = pend.take() {
            *psp = [id, ((w as u32 >> 1) as u64) << 32 | *p];
            psp = psp.add(1);
            p = p.add(1);
        }
        while p < end {
            let (kind, w, e): (u64, u64, u64);
            let d = (esp as usize).wrapping_sub(p as usize);
            std::arch::asm!(
                "2:",
                "mov {w}, qword ptr [{p}]",
                "shrx {e}, {w}, {sh}",
                "mov {e}, qword ptr [{tab} + {e}*8]",
                "mov {t}, {e}",
                "xor {t}, {w}",
                "shr {t}, 32",
                "jnz 6f",
                "mov dword ptr [{p} + {d}], {w:e}",
                "mov dword ptr [{p} + {d} + 4], {e:e}",
                "mov {e:e}, {e:e}",
                "inc dword ptr [{ec} + {e}*4]",
                "add {p}, 8",
                "test {w:l}, 1",
                "jnz 7f",
                "cmp {p}, {end}",
                "jb 2b",
                "xor {t:e}, {t:e}",
                "jmp 9f",
                "6:",
                "mov {t:e}, 1",
                "jmp 9f",
                "7:",
                "mov {t:e}, 2",
                "9:",
                p = inout(reg) p,
                end = in(reg) end,
                d = in(reg) d,
                sh = in(reg) (32 + u.sh) as u64,
                tab = in(reg) u.tab.as_ptr(),
                ec = in(reg) u.ec.as_mut_ptr(),
                w = out(reg) w,
                e = out(reg) e,
                t = out(reg) kind,
                options(nostack),
            );
            esp = (p as usize).wrapping_add(d) as *mut u64;
            let (id, w) = match kind {
                0 => continue,
                1 => {
                    let w = *p;
                    p = p.add(1);
                    let rem = w >> 32;
                    let e = u.miss(rem, *u.tab.get_unchecked((rem >> u.sh) as usize));
                    let id = (e & 0xFFFF_FFFF) as usize;
                    *esp = e << 32 | (w & 0xFFFF_FFFF);
                    esp = esp.add(1);
                    *u.ec.get_unchecked_mut(id) += 1;
                    if w & 1 == 0 {
                        continue;
                    }
                    (id, w)
                }
                _ => (e as usize, w),
            };
            *u.pc.get_unchecked_mut(id) += 1;
            if p == end {
                pend = Some((id as u64, w));
                break;
            }
            *psp = [id as u64, ((w as u32 >> 1) as u64) << 32 | *p];
            psp = psp.add(1);
            p = p.add(1);
        }
    }
    if pend.is_some() {
        fallback("event stream out of step");
    }
    (esp.offset_from(es) as usize, psp.offset_from(ps) as usize)
}

#[inline(never)]
unsafe fn ua_clicks(f: *const u64, n: usize, u: &mut UTab, repq: &[AtomicU64], cs: *mut [u64; 4]) -> usize {
    let mut p = f;
    let end = f.add(n);
    let mut csp = cs;
    while p < end {
        let kind: u64;
        std::arch::asm!(
            "2:",
            "mov {a}, qword ptr [{p}]",
            "shrx {e}, {a}, {sh}",
            "mov {e}, qword ptr [{tab} + {e}*8]",
            "mov {t}, {e}",
            "xor {t}, {a}",
            "shr {t}, 32",
            "jnz 6f",
            "vmovdqu {y}, ymmword ptr [{p}]",
            "vmovdqu ymmword ptr [{cs}], {y}",
            "mov dword ptr [{cs} + 4], {e:e}",
            "mov {e:e}, {e:e}",
            "add {cs}, 32",
            "add {p}, 16",
            "test {a:l}, 1",
            "jnz 7f",
            "add dword ptr [{cc} + {e}*4], 2",
            "cmp {p}, {end}",
            "jb 2b",
            "jmp 8f",
            "7:",
            "add dword ptr [{cc} + {e}*4], 4",
            "add {p}, 16",
            "cmp {p}, {end}",
            "jb 2b",
            "8:",
            "xor {t:e}, {t:e}",
            "jmp 9f",
            "6:",
            "mov {t:e}, 1",
            "9:",
            p = inout(reg) p,
            cs = inout(reg) csp,
            end = in(reg) end,
            sh = in(reg) (32 + u.sh) as u64,
            tab = in(reg) u.tab.as_ptr(),
            cc = in(reg) u.cc.as_mut_ptr(),
            a = out(reg) _,
            e = out(reg) _,
            t = out(reg) kind,
            y = out(ymm_reg) _,
            options(nostack),
        );
        if kind == 0 {
            break;
        }
        let a = *p;
        let r = [*p.add(1), *p.add(2), *p.add(3)];
        p = p.add(2 + 2 * (a & 1) as usize);
        let e = u.slow_click(a >> 32, repq);
        let id = (e & 0xFFFF_FFFF) as usize;
        *csp = [e << 32 | (a & 0xFFFF_FFFF), r[0], r[1], r[2]];
        csp = csp.add(1);
        *u.cc.get_unchecked_mut(id) += 2 + 2 * (a & 1) as u32;
    }
    if p != end {
        fallback("click stream out of step");
    }
    csp.offset_from(cs) as usize
}

#[inline(never)]
unsafe fn ua_events_w(flat: &[u64], u: &mut UTab, es: *mut u64, ps: *mut [u64; 2]) -> (usize, usize) {
    let (mut esp, mut psp) = (es, ps);
    let n = flat.len();
    let f = flat.as_ptr();
    let mut i = 0;
    while i + 3 <= n {
        let a = *f.add(i);
        let k = Key { lo: *f.add(i + 1), hi: *f.add(i + 2) };
        i += 3;
        let hb = a >> 32;
        let mut e = *u.wt.get_unchecked((hb >> u.wsh) as usize);
        if e == EMPTY || e >> 32 != hb || *u.keys.get_unchecked(e as u32 as usize) != k {
            e = if k.lo | k.hi == 0 { 0 } else { u.slow_w(hb, k, true) };
        }
        let id = (e & 0xFFFF_FFFF) as usize;
        *esp = e << 32 | (a & 0xFFFF_FFFF);
        esp = esp.add(1);
        *u.ec.get_unchecked_mut(id) += 1;
        if a & 1 != 0 {
            if i >= n {
                fallback("event stream out of step");
            }
            *u.pc.get_unchecked_mut(id) += 1;
            *psp = [id as u64, ((a as u32 >> 1) as u64) << 32 | *f.add(i)];
            psp = psp.add(1);
            i += 1;
        }
    }
    if i != n {
        fallback("event stream out of step");
    }
    (esp.offset_from(es) as usize, psp.offset_from(ps) as usize)
}

#[inline(never)]
unsafe fn ua_clicks_w(f: *const u64, n: usize, u: &mut UTab, repq: &[AtomicU64], cs: *mut [u64; 4]) -> usize {
    let mut csp = cs;
    let mut i = 0;
    while i + 4 <= n {
        let a = *f.add(i);
        let k = Key { lo: *f.add(i + 1), hi: *f.add(i + 2) };
        let r = [*f.add(i + 3), *f.add(i + 4), *f.add(i + 5)];
        i += 4;
        if a & 1 != 0 {
            i += 2;
        }
        let hb = a >> 32;
        let mut e = *u.wt.get_unchecked((hb >> u.wsh) as usize);
        if e == EMPTY || e >> 32 != hb || *u.keys.get_unchecked(e as u32 as usize) != k {
            e = u.slow_click_w(hb, k, repq);
        }
        let id = (e & 0xFFFF_FFFF) as usize;
        *csp = [e << 32 | (a & 0xFFFF_FFFF), r[0], r[1], r[2]];
        csp = csp.add(1);
        *u.cc.get_unchecked_mut(id) += 2 + 2 * (a & 1) as u32;
    }
    if i != n {
        fallback("click stream out of step");
    }
    csp.offset_from(cs) as usize
}

#[inline(always)]
unsafe fn clicks_in(cl: &mut Vec<[u64; 2]>, cside: &mut Vec<Key>, mut p: *const u64, n: usize) {
    let pe = p.add(n);
    cl.reserve(n / 2);
    let mut cp = cl.as_mut_ptr().add(cl.len());
    while p < pe {
        let a = *p;
        let mut x = *p.add(1);
        p = p.add(2);
        if a & 1 != 0 {
            x = (x >> 42 << 42) | cside.len() as u64;
            cside.push(Key { lo: *p, hi: *p.add(1) });
            p = p.add(2);
        }
        *cp = [a, x];
        cp = cp.add(1);
    }
    if p != pe {
        fallback("click stream out of step");
    }
    cl.set_len(cp.offset_from(cl.as_ptr()) as usize);
}

#[inline(always)]
fn ev_block<E: Rec, P: Rec>(blk: &mut VB, home: usize, pk: Key, ev: &[E], pp: &[P]) {
    let (n, x) = (ev.len(), pp.len());
    if n >= 1 << 31 || x >= 1 << 30 {
        fallback("a user run over 2^31 events");
    }
    let wide = pk.hi != 0;
    let hdr = [n as u64 | (x as u64) << 32 | (wide as u64) << 62, pk.lo, pk.hi];
    blk.put_events(home, &hdr[..2 + wide as usize], ev, pp);
}
#[inline(always)]
fn ck_block<C: Rec>(blk: &mut VB, home: usize, pk: Key, ck: &[C], cside: &[Key]) {
    let n = ck.len();
    if n >= 1 << 30 {
        fallback("a user run over 2^30 clicks");
    }
    let wide = pk.hi != 0;
    let x = ck.iter().filter(|c| c.lo32() & 1 != 0).count() as u64;
    let hdr = [n as u64 | x << 32 | (wide as u64) << 62 | 1 << 63, pk.lo, pk.hi];
    blk.put_clicks(home, &hdr[..2 + wide as usize], ck, cside);
}

#[inline(never)]
fn lane<E: Rec, P: Rec, C: Rec>(
    g: &G,
    odds: &[Vec<u8>],
    b2: u32,
    q: usize,
    t: &mut T3,
    blk: &mut VB,
    bw: &mut Vec<u64>,
    w: &mut Walk,
    cside: &[Key],
    name: &mut Vec<u8>,
    ev: &[E],
    pp: &[P],
    ck: &[C],
    link: impl Fn((u32, u64, u64)) -> Lk,
    key_of: impl Fn((u32, u64, u64)) -> Key,
) {
    let (n, np, nc) = (ev.len(), pp.len(), ck.len());
    let (mut i, mut pi, mut ci) = (0usize, 0usize, 0usize);
    let _ = &bw;
    loop {
        let eu = if i < n { Some(ev[i].ukey()) } else { None };
        while ci < nc {
            let cr = ck[ci];
            let cu = cr.ukey();
            if eu.is_some_and(|u| cu >= u) {
                break;
            }
            let mut cj = ci + 1;
            while cj < nc && unsafe { ck.get_unchecked(cj) }.usame(&cr) {
                cj += 1;
            }
            match link(cu) {
                Lk::Moved(pk) => ck_block(blk, upart(pk, b2), pk, &ck[ci..cj], cside),
                Lk::Rep => ck_block(blk, q, key_of(cu), &ck[ci..cj], cside),
                Lk::No => {}
            }
            ci = cj;
        }
        let Some(u) = eu else { break };
        let e0 = ev[i];
        let mut j = i + 1;
        while j < n && unsafe { ev.get_unchecked(j) }.usame(&e0) {
            j += 1;
        }
        while pi < np && pp[pi].ukey() < u {
            pi += 1;
        }
        let mut pj = pi;
        while pj < np && pp[pj].ukey() == u {
            pj += 1;
        }
        let mut cj = ci;
        while cj < nc && ck[cj].ukey() == u {
            cj += 1;
        }
        let dst = match link(u) {
            Lk::Moved(pk) => Some((upart(pk, b2), pk)),
            Lk::Rep => Some((q, key_of(u))),
            Lk::No => None,
        };
        match dst {
            Some((home, pk)) => {
                ev_block(blk, home, pk, &ev[i..j], &pp[pi..pj]);
                if cj > ci {
                    ck_block(blk, home, pk, &ck[ci..cj], cside);
                }
            }
            None => {
                t.sb.add(ev[i..j].iter().map(|e| e.lo32() >> 1));
                for p in &pp[pi..pj] {
                    w.ppv.push(((p.lo32() >> 1) as u64) << 32 | p.last());
                }
                for c in &ck[ci..cj] {
                    w.cl.push([c.lo32() as u64, c.last()]);
                }
                finish_person(g, odds, t, w, cside, key_of(u), name);
            }
        }
        i = j;
        pi = pj;
        ci = cj;
    }
}

fn p1_thread(cx: &Cx, tid: usize) {
    let nt = cx.t.len();
    let t = cx.t(tid);
    let b = unsafe { &mut *b3(tid) };
    let g = cx.g;
    let b2 = cx.b2;
    let np = 1usize << b2;
    let rb = 42 - b2;
    let rmask = (1u64 << rb) - 1;
    let odds = cx.odds();
    let bk = unsafe { &*cx.bk.get() };
    let mut name = Vec::with_capacity(64);
    if unsafe { KN[1] } == 0 {
        let n = g.ev_bytes / 72 / nt * ROWS_PM / 1000 + 1024;
        let r = &mut t.rows;
        let e = ROWS_N.load(Ordering::Relaxed);
        if e != 0 {
            fn cc<T>(v: &mut Vec<T>, n: usize) {
                let c = (n * std::mem::size_of::<T>()).next_power_of_two() / std::mem::size_of::<T>();
                if c > v.len() {
                    v.reserve_exact(c - v.len());
                }
            }
            cc(&mut r.names.off, e);
            cc(&mut r.names.data, e * 6);
            cc(&mut r.ukeys, e);
            cc(&mut r.day, e);
            cc(&mut r.ses, e);
            cc(&mut r.evs, e);
            cc(&mut r.rev, e);
            cc(&mut r.top, e);
        } else {
        r.names.off.reserve(n);
        r.names.data.reserve(n * 6);
        r.ukeys.reserve(n);
        r.day.reserve(n);
        r.ses.reserve(n);
        r.evs.reserve(n);
        r.rev.reserve(n);
        r.top.reserve(n);
        }
    }
    loop {
        let q = cx.next.fetch_add(1, Ordering::Relaxed);
        if q >= np {
            break;
        }
        let mut tk = std::time::Instant::now();
        let B3 { blk, es, ps, cside, flat, sl, w, pm, ut, lkix, homes, lrep, ldir, ub, base, c4, lmov, .. } = &mut *b;
        let mut pl = [0u64; 3];
        pm3::lap(pm, &mut pl, 15);
        let l = &bk.lkf[bk.lko[q]..bk.lko[q + 1]];
        if l.len() >= u32::MAX as usize {
            fallback("too many links in a partition");
        }
        let tot: usize = (0..nt).map(|o| unsafe { (*cx.t[o].get()).u2.s.words(q) }).sum();
        let npur: usize = (0..nt).map(|o| unsafe { (&(*cx.t[o].get()).u2.np)[q] as usize }).sum();
        let wtot: usize = (0..nt).map(|o| unsafe { (*cx.t[o].get()).u2.w.words(q) }).sum();
        if tot >= 1 << 31 || npur > tot || wtot >= 1 << 31 {
            fallback("a user partition with 2^31 records");
        }
        ut.reset(l.len() + (tot - npur) / 8 + 64, l.len() / 4 + wtot / 24 + 16, rb);
        lkix.clear();
        lkix.push(0);
        homes.clear();
        homes.push(0);
        for (i, e) in l.iter().enumerate() {
            let id = if is_short(e.0) {
                unsafe { ut.entry(mix42(e.0.lo) & rmask) }
            } else {
                unsafe { ut.slow_w(h32(hash(e.0), b2), e.0, true) }
            } as u32 as usize;
            if id == lkix.len() {
                lkix.push(i as u32);
                homes.push(upart(e.1, b2) as u32);
            }
        }
        let nls = ut.nu;
        if rb == 32 {
            unsafe { ut.slow(0xFFFF_FFFF, true) };
        }
        let repq = &bk.rep[q << (RBB - 6)..(q + 1) << (RBB - 6)];
        t.prof[3] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 3);

        flat.clear();
        sl.clear();
        for o in 0..nt {
            unsafe { (*cx.t[o].get()).u2.w.slices(q, sl) };
        }
        copy_slices(sl, flat);
        es.clear();
        ps.clear();
        es.reserve(tot + flat.len() / 3 + 2);
        ps.reserve(npur + flat.len() / 4 + 2);
        sl.clear();
        for o in 0..nt {
            unsafe { (*cx.t[o].get()).u2.s.slices(q, sl) };
            sl.push((std::ptr::null(), 0));
        }
        let (mut ne, mut nps) = unsafe { ua_events(sl, ut, es.as_mut_ptr(), ps.as_mut_ptr()) };
        if ne != tot - npur || nps != npur {
            fallback("event stream out of step");
        }
        if unsafe { KN[5] } == 0 {
            for &(ptr, n) in sl.iter() {
                if n > 0 {
                    blk.free.push(ptr as *mut u64);
                }
            }
        }
        unsafe {
            let (a, b) = ua_events_w(flat, ut, es.as_mut_ptr().add(ne), ps.as_mut_ptr().add(nps));
            ne += a;
            nps += b;
            es.set_len(ne);
            ps.set_len(nps);
        }
        flat.clear();
        sl.clear();
        for o in 0..nt {
            unsafe {
                if !(*cx.t[o].get()).ck.s.cur.is_null() {
                    (*cx.t[o].get()).ck.s.slices(q, sl);
                }
            }
        }
        copy_slices(sl, flat);
        let n1 = flat.len();
        sl.clear();
        for o in 0..nt {
            unsafe {
                if !(*cx.t[o].get()).ck.w.cur.is_null() {
                    (*cx.t[o].get()).ck.w.slices(q, sl);
                }
            }
        }
        copy_slices(sl, flat);
        let n2 = flat.len() - n1;
        if flat.len() >= 1 << 30 {
            fallback("a user partition with 2^31 records");
        }
        flat.reserve(2);
        unsafe {
            *flat.as_mut_ptr().add(n1 + n2) = 0;
            *flat.as_mut_ptr().add(n1 + n2 + 1) = 0;
        }
        c4.clear();
        c4.reserve(n1 / 2 + n2 / 4 + 2);
        let (ncs, ncw) = unsafe {
            let a = ua_clicks(flat.as_ptr(), n1, ut, repq, c4.as_mut_ptr());
            let b = ua_clicks_w(flat.as_ptr().add(n1), n2, ut, repq, c4.as_mut_ptr().add(a));
            c4.set_len(a + b);
            (a + b, n1 + n2 - 2 * b)
        };
        t.prof[4] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 4);
        let nu = ut.nu;
        let bound = 4 * (nu + 2) + ne + nps + ncw + 16;
        if bound >= 1 << 31 {
            fallback("a user partition with 2^31 records");
        }
        ub.clear();
        ub.reserve(bound);
        base.clear();
        base.reserve(nu + 2);
        lmov.clear();
        lmov.reserve(nls + 1);
        lrep.clear();
        lrep.reserve(nu - nls + 1);
        ldir.clear();
        ldir.reserve(nu - nls + 1);
        let ubp = ub.as_mut_ptr();
        let bp = base.as_mut_ptr();
        let (mp, rp, dp) = (lmov.as_mut_ptr(), lrep.as_mut_ptr(), ldir.as_mut_ptr());
        let (mut nm, mut nr, mut ndr) = (0usize, 0usize, 0usize);
        unsafe {
            let (ec, pc, cc) = (ut.ec.as_mut_ptr(), ut.pc.as_mut_ptr(), ut.cc.as_mut_ptr());
            let (rems, keys) = (ut.rems.as_ptr(), ut.keys.as_ptr());
            let (mut se, mut sp, mut sc) = (0usize, 0usize, 0usize);
            let mut pos = 0usize;
            macro_rules! place {
                ($id:expr) => {{
                    let id = $id;
                    let (n, x, c) = (*ec.add(id) as usize, *pc.add(id) as usize, *cc.add(id) as usize);
                    se += n;
                    sp += x;
                    sc += c;
                    *bp.add(id) = pos as u32;
                    let d = pos + 4;
                    *ec.add(id) = (2 * d) as u32;
                    *ubp.add(d + n / 2) = 0;
                    let p0 = d + (n + 1) / 2;
                    *pc.add(id) = p0 as u32;
                    let c0 = p0 + x;
                    *cc.add(id) = c0 as u32;
                    pos = c0 + c;
                    (n != 0, (n | x | c) != 0)
                }};
            }
            place!(0);
            for id in 1..=nls {
                let (_, any) = place!(id);
                *mp.add(nm) = id as u32;
                nm += any as usize;
            }
            for id in nls + 1..=nu {
                let (he, any) = place!(id);
                let k = *keys.add(id);
                let ix = if is_short(k) { rep_ix_short(*rems.add(id)) } else { (hash(k) >> 16) as usize & ((1 << RBB) - 1) };
                let rep = (repq.get_unchecked(ix >> 6).load(Ordering::Relaxed) >> (ix & 63)) & 1 != 0;
                *rp.add(nr) = id as u32;
                nr += (rep & any) as usize;
                *dp.add(ndr) = id as u32;
                ndr += (!rep & he) as usize;
            }
            *bp.add(nu + 1) = pos as u32;
            base.set_len(nu + 2);
            if se != ne || sp != nps || sc != ncw {
                fallback("user counts out of step");
            }
            let u32p = ubp as *mut u32;
            for &x in es.iter() {
                let c = ec.add((x >> 32) as usize);
                *u32p.add(*c as usize) = x as u32;
                *c += 1;
            }
            for x in ps.iter() {
                let c = pc.add(x[0] as usize);
                *ubp.add(*c as usize) = x[1];
                *c += 1;
            }
            for x in c4.iter() {
                let c = cc.add((x[0] >> 32) as usize);
                let d = ubp.add(*c as usize);
                *d = x[0] & 0xFFFF_FFFF;
                *d.add(1) = x[1];
                *d.add(2) = x[2];
                *d.add(3) = x[3];
                *c += 2 + 2 * (x[0] & 1) as u32;
            }
        }
        let _ = ncs;
        t.prof[5] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 5);
        unsafe {
            let (ec, pc, keys) = (ut.ec.as_ptr(), ut.pc.as_ptr(), ut.keys.as_ptr());
            let rec = |id: usize| -> (usize, usize, usize, usize, usize) {
                let d = *bp.add(id) as usize + 4;
                let n = *ec.add(id) as usize - 2 * d;
                let c0 = *pc.add(id) as usize;
                (d, n, c0 - (d + (n + 1) / 2), c0, *bp.add(id + 1) as usize)
            };
            let blkp: *mut LB = &mut *blk;
            let emit = |home: usize, pk: Key, id: usize| {
                let (d, n, x, c0, end) = rec(id);
                let wide = (pk.hi != 0) as usize;
                let ext = (x != 0) as usize;
                *ubp.add(d - 1) = pk.hi;
                *ubp.add(d - 1 - wide) = pk.lo;
                *ubp.add(d - 2 - wide) = x as u64;
                let h = d - 2 - wide - ext;
                *ubp.add(h) = n as u64 | ((end - c0) as u64) << 31 | (wide as u64) << 62 | (ext as u64) << 63;
                (*blkp).put(home, ubp.add(h), end - h);
            };
            for k in 0..nm {
                if k + 4 < nm {
                    (*blkp).prefetch(*homes.get_unchecked(*mp.add(k + 4) as usize) as usize);
                }
                let id = *mp.add(k) as usize;
                let pk = l.get_unchecked(*lkix.get_unchecked(id) as usize).1;
                emit(*homes.get_unchecked(id) as usize, pk, id);
            }
            let key_of = |id: usize| -> Key {
                let k = *keys.add(id);
                if is_short(k) {
                    Key { hi: 0, lo: unmix42((q as u64) << rb | k.lo) }
                } else {
                    k
                }
            };
            for k in 0..nr {
                let id = *rp.add(k) as usize;
                emit(q, key_of(id), id);
            }
            for k in 0..ndr {
                let id = *dp.add(k) as usize;
                let (d, n, x, c0, end) = rec(id);
                t.sb.add(std::slice::from_raw_parts(u32p_of(ubp).add(2 * d), n).iter().map(|&v| v >> 1));
                w.ppv.extend_from_slice(std::slice::from_raw_parts(ubp.add(c0 - x), x));
                cside.clear();
                clicks_in(&mut w.cl, cside, ubp.add(c0), end - c0);
                finish_person(g, odds, t, w, cside, key_of(id), &mut name);
            }
        }
        t.prof[6] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 6);
        t.prof[7] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 7);
    }
    b.w.apply(&mut t.acc);
    b.blk.finish();
}

#[derive(Clone, Copy)]
struct Run {
    pid: u32,
    off: u32,
    n: u32,
    x: u32,
}

fn order_runs(src: &[Run], dst: &mut Vec<Run>, cnt: &mut [u32], off: &mut Vec<u32>) {
    off.clear();
    let mut s = 0u32;
    for c in cnt.iter_mut() {
        off.push(s);
        let v = *c;
        *c = s;
        s += v;
    }
    off.push(s);
    dst.clear();
    dst.reserve(src.len());
    let d = dst.as_mut_ptr();
    for r in src {
        unsafe {
            let p = cnt.get_unchecked_mut(r.pid as usize);
            *d.add(*p as usize) = *r;
            *p += 1;
        }
    }
    unsafe { dst.set_len(src.len()) };
}

#[derive(Clone, Copy)]
struct Run2 {
    pid: u32,
    off: u32,
    n: u32,
    x: u32,
    c: u32,
}
fn order_runs2(src: &[Run2], dst: &mut Vec<Run2>, cnt: &mut [u32], off: &mut Vec<u32>) {
    off.clear();
    let mut s = 0u32;
    for c in cnt.iter_mut() {
        off.push(s);
        let v = *c;
        *c = s;
        s += v;
    }
    off.push(s);
    dst.clear();
    dst.reserve(src.len());
    let d = dst.as_mut_ptr();
    for r in src {
        unsafe {
            let p = cnt.get_unchecked_mut(r.pid as usize);
            *d.add(*p as usize) = *r;
            *p += 1;
        }
    }
    unsafe { dst.set_len(src.len()) };
}

fn p2_thread(cx: &Cx, tid: usize) {
    let nt = cx.t.len();
    let t = cx.t(tid);
    let b = unsafe { &mut *b3(tid) };
    let g = cx.g;
    let np = 1usize << cx.b2;
    let rmask = (1u64 << (42 - cx.b2)) - 1;
    let odds = cx.odds();
    let mut name = Vec::with_capacity(64);
    const HM: u64 = 0x9E37_79B9_7F4A_7C15;
    loop {
        let q = cx.next.fetch_add(1, Ordering::Relaxed);
        if q >= np {
            break;
        }
        let mut tk = std::time::Instant::now();
        let B3 { cside, flat, w, kt, names, ecnt, eoff, stab, pm, sl, rn, ro, .. } = &mut *b;
        let mut pl = [0u64; 3];
        pm3::lap(pm, &mut pl, 15);
        flat.clear();
        sl.clear();
        for o in 0..nt {
            unsafe { (*std::ptr::addr_of!((*b3(o)).blk)).slices(q, sl) };
        }
        copy_slices(sl, flat);
        if flat.is_empty() {
            continue;
        }
        if flat.len() >= u32::MAX as usize {
            fallback("too many block words in a partition");
        }
        t.prof[8] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 8);
        kt.clear();
        names.clear();
        rn.clear();
        ecnt.clear();
        let cap = if unsafe { KN[6] } == 1 { 16 } else { (flat.len() * 2).next_power_of_two().clamp(1 << 10, 1 << 15) };
        let smask = cap - 1;
        let ssh = 64 - cap.trailing_zeros();
        let mut used = 0usize;
        stab.clear();
        stab.resize(cap, EMPTY);
        let fp = flat.as_ptr();
        let mut i = 0;
        let e = flat.len();
        while i < e {
            if i + 2 > e {
                fallback("block stream out of step");
            }
            let h = unsafe { *fp.add(i) };
            let n = (h & 0x7FFF_FFFF) as u32;
            let c = ((h >> 31) & 0x7FFF_FFFF) as u32;
            let wide = ((h >> 62) & 1) as usize;
            let ext = (h >> 63) as usize;
            let x = if ext != 0 { unsafe { *fp.add(i + 1) } } else { 0 };
            if x >= 1 << 32 {
                fallback("block stream out of step");
            }
            let x = x as u32;
            i += 1 + ext;
            if i + 1 + wide > e {
                fallback("block stream out of step");
            }
            let lo = unsafe { *fp.add(i) };
            let mut pid = u32::MAX;
            if wide == 0 && lo < SHORT_END {
                let rem = mix42(lo) & rmask;
                let mut s = (rem.wrapping_mul(HM) >> ssh) as usize;
                loop {
                    let v = unsafe { *stab.get_unchecked(s) };
                    if v == EMPTY {
                        if used * 2 < cap {
                            pid = names.len() as u32;
                            unsafe { *stab.get_unchecked_mut(s) = rem << 32 | pid as u64 };
                            used += 1;
                            names.push(Key { hi: 0, lo });
                            ecnt.push(0);
                        }
                        break;
                    }
                    if v >> 32 == rem {
                        pid = v as u32;
                        break;
                    }
                    s = (s + 1) & smask;
                }
            }
            if pid == u32::MAX {
                let pk = Key { lo, hi: if wide != 0 { flat[i + 1] } else { 0 } };
                let hh = hash(pk);
                pid = match kt.get(pk, hh) {
                    Some(v) => v,
                    None => {
                        let v = names.len() as u32;
                        if v == u32::MAX {
                            fallback("too many persons in a partition");
                        }
                        names.push(pk);
                        ecnt.push(0);
                        kt.insert(pk, hh, v);
                        v
                    }
                };
            }
            i += 1 + wide;
            rn.push(Run2 { pid, off: i as u32, n, x, c });
            ecnt[pid as usize] += 1;
            i += (n as usize).div_ceil(2) + x as usize + c as usize;
        }
        if i != e {
            fallback("block stream out of step");
        }
        let npers = names.len();
        order_runs2(rn, ro, ecnt, eoff);
        t.prof[9] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 9);
        for pid in 0..npers {
            cside.clear();
            let mut tn = 0usize;
            for r in &ro[eoff[pid] as usize..eoff[pid + 1] as usize] {
                let (off, n, x, c) = (r.off as usize, r.n as usize, r.x as usize, r.c as usize);
                tn += n;
                let s = unsafe { std::slice::from_raw_parts(fp.add(off) as *const u32, n) };
                t.sb.add(s.iter().map(|&v| v >> 1));
                let po = off + n.div_ceil(2);
                if x != 0 {
                    w.ppv.extend_from_slice(&flat[po..po + x]);
                }
                if c != 0 {
                    unsafe { clicks_in(&mut w.cl, cside, fp.add(po + x), c) };
                }
            }
            if tn == 0 {
                w.cl.clear();
                w.ppv.clear();
                continue;
            }
            finish_person(g, odds, t, w, cside, names[pid], &mut name);
        }
        t.prof[13] += tick(&mut tk);
        pm3::lap(pm, &mut pl, 13);
    }
    if t.st[0] > (u32::MAX / 48) as u64 {
        fallback("too many persons for 32-bit assist sums");
    }
    b.w.apply(&mut t.acc);
}

const LOGN: usize = 2048;
struct Walk {
    ppv: Vec<u64>,
    cl: Vec<[u64; 2]>,
    ses: Vec<u32>,
    asst: Vec<u32>,
    pur: Vec<[u64; 2]>,
    sk: Vec<u64>,
    sk2: Vec<u64>,
    sidx: Vec<u32>,
    lidx: Vec<u32>,
    rs: Vec<(u32, u32, u32)>,
    rowof: Vec<u32>,
    sr: Vec<u32>,
    um: Vec<u64>,
    ul: Vec<u32>,
}
impl Walk {
    fn new(ncamp: usize, nbins: usize) -> Walk {
        Walk {
            ppv: Vec::new(),
            cl: Vec::new(),
            ses: Vec::with_capacity(LOGN + nbins + 16),
            asst: Vec::with_capacity(LOGN + 64 * nbins + 64),
            pur: Vec::with_capacity(LOGN + nbins + 16),
            sk: Vec::new(),
            sk2: Vec::new(),
            sidx: Vec::new(),
            lidx: vec![0; ncamp],
            rs: Vec::new(),
            rowof: Vec::new(),
            sr: Vec::new(),
            um: vec![0; ncamp],
            ul: Vec::new(),
        }
    }
    fn apply(&mut self, acc: &mut Acc) {
        for &s in &self.ses {
            Acc::add16(&mut acc.ses, &mut acc.ses_hi, s as usize, 1);
        }
        for &e in &self.asst {
            acc.asst[(e >> 8) as usize] += e & 255;
        }
        for p in &self.pur {
            let a = &mut acc.pur[p[0] as usize];
            a[0] += 1;
            a[1] += p[1];
        }
        self.ses.clear();
        self.asst.clear();
        self.pur.clear();
    }
}

#[cold]
#[inline(never)]
fn tie_camp<'a>(it: impl Iterator<Item = &'a [u64; 2]>, tsr: u64, cside: &[Key], odds: &[Vec<u8>]) -> u32 {
    let mut best: Option<(Key, u32)> = None;
    for c in it.filter(|c| (c[0] as u32 >> 1) as u64 == tsr) {
        let k = if c[0] & 1 != 0 { cside[(c[1] & M42) as usize] } else { Key { hi: 0, lo: c[1] & M42 } };
        if best.is_none_or(|b| id_cmp(k, b.0, b'k', odds) == std::cmp::Ordering::Less) {
            best = Some((k, (c[1] >> 42) as u32));
        }
    }
    best.unwrap().1
}

#[cfg(feature = "w512")]
#[inline(never)]
unsafe fn credit_mask<const NV: usize>(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let ns = t.sess.len();
    debug_assert!(ns >= 1 && ns <= 8 * NV);
    let nd = g.nd as u32;
    let day0 = g.day0;
    let cbase = g.cbase;
    let mut sr = [std::mem::MaybeUninit::<u32>::uninit(); 64];
    let mut rm = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let mut rd = [std::mem::MaybeUninit::<u32>::uninit(); 64];
    let (srp, rmp, rdp) = (sr.as_mut_ptr() as *mut u32, rm.as_mut_ptr() as *mut u64, rd.as_mut_ptr() as *mut u32);
    const NS16: [usize; 9] = [1, 1, 1, 2, 2, 3, 3, 4, 4];
    let nv16 = NS16[NV];
    for v in 0..nv16 {
        _mm512_storeu_si512(srp.add(16 * v) as *mut _, _mm512_set1_epi32(-1));
    }
    let sp = t.sess.as_ptr();
    let nrows = (*sp.add(ns - 1)).row as usize + 1;
    let ng = nrows.div_ceil(8);
    for v in 0..ng {
        _mm512_storeu_si512(rmp.add(8 * v) as *mut _, _mm512_setzero_si512());
        _mm256_storeu_si256(rdp.add(8 * v) as *mut _, _mm256_setzero_si256());
    }
    let (mut f, mut pr) = (0usize, u32::MAX);
    for s in 0..ns {
        let x = &*sp.add(s);
        *srp.add(s) = (x.start - cbase) as u32;
        f = std::hint::select_unpredictable(x.row != pr, s, f);
        pr = x.row;
        *rmp.add(x.row as usize) = _bzhi_u64(!0, s as u32 + 1) & !_bzhi_u64(!0, f as u32);
        *rdp.add(x.row as usize) = (x.day - day0) as u32;
    }
    let mut srv = [_mm512_setzero_si512(); 4];
    for v in 0..nv16 {
        srv[v] = _mm512_loadu_si512(srp.add(16 * v) as *const _);
    }
    t.serial += 1;
    let ser = t.serial;
    let stamp = t.stamp.as_mut_ptr();
    let um = w.um.as_mut_ptr();
    let n = w.cl.len();
    w.ul.clear();
    w.ul.reserve(n);
    let ulp = w.ul.as_mut_ptr();
    let mut k = 0usize;
    let mut b1 = [_mm512_setzero_si512(); NV];
    let mut b2 = [_mm512_setzero_si512(); NV];
    let cp = w.cl.as_ptr();
    for j in 0..n {
        let c = *cp.add(j);
        let ts = (c[0] as u32) >> 1;
        let camp = (c[1] >> 42) as u32;
        let (bl, bh) = (_mm512_set1_epi32(ts as i32), _mm512_set1_epi32((ts + WEEK_MS as u32) as i32));
        let (mut a, mut b) = (0u32, 0u32);
        for v in 0..nv16 {
            a += (_mm512_cmplt_epu32_mask(srv[v], bl) as u32).count_ones();
            b += (_mm512_cmple_epu32_mask(srv[v], bh) as u32).count_ones();
        }
        let r = _bzhi_u64(!0, b) & !_bzhi_u64(!0, a);
        let fresh = *stamp.add(camp as usize) != ser;
        let old = *um.add(camp as usize);
        *um.add(camp as usize) = std::hint::select_unpredictable(fresh, 0, old) | r;
        *stamp.add(camp as usize) = ser;
        *ulp.add(k) = camp;
        k += fresh as usize;
        let hi = (ts as u64 + 1) << 32;
        let (k1, k2) = (_mm512_set1_epi64((hi | camp as u64) as i64), _mm512_set1_epi64((hi | (!camp) as u64) as i64));
        for v in 0..NV {
            let m = (r >> (8 * v)) as u8;
            b1[v] = _mm512_mask_max_epu64(b1[v], m, b1[v], k1);
            b2[v] = _mm512_mask_max_epu64(b2[v], m, b2[v], k2);
        }
    }
    w.ul.set_len(k);
    let mut x1 = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let mut x2 = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let (x1, x2) = (x1.as_mut_ptr() as *mut u64, x2.as_mut_ptr() as *mut u64);
    for v in 0..NV {
        _mm512_storeu_si512(x1.add(8 * v) as *mut _, b1[v]);
        _mm512_storeu_si512(x2.add(8 * v) as *mut _, b2[v]);
    }
    let organic = g.nc as u32;
    w.ses.reserve(ns);
    w.pur.reserve(ns);
    let mut spp = w.ses.as_mut_ptr().add(w.ses.len());
    let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
    for s in 0..ns {
        let (y1, y2) = (*x1.add(s), *x2.add(s));
        let mut camp = std::hint::select_unpredictable(y1 == 0, organic, y1 as u32);
        if y1 as u32 != !(y2 as u32) && y1 != 0 {
            t.st[11] += 1;
            camp = tie_camp(w.cl.iter(), (y1 >> 32) - 1, cside, odds);
        }
        let x = &*sp.add(s);
        let sl = camp * nd + (x.day - day0) as u32;
        *spp = sl;
        spp = spp.add(1);
        *pp = [sl as u64, x.revenue()];
        pp = pp.add(x.purch() as usize);
    }
    w.ses.set_len(spp.offset_from(w.ses.as_ptr()) as usize);
    w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
    w.asst.reserve(k * 8 * ng + 8);
    let mut ap = w.asst.as_mut_ptr().add(w.asst.len());
    let lut = _mm512_broadcast_i32x4(_mm_setr_epi8(0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4));
    let m0f = _mm512_set1_epi8(0x0F);
    let ndv = _mm256_set1_epi32(nd as i32);
    for i in 0..k {
        let c = *ulp.add(i);
        let ub = _mm512_set1_epi64(*um.add(c as usize) as i64);
        let base = _mm256_mullo_epi32(_mm256_set1_epi32(c as i32), ndv);
        for v in 0..ng {
            let x = _mm512_and_si512(ub, _mm512_loadu_si512(rmp.add(8 * v) as *const _));
            let pc = _mm512_add_epi8(_mm512_shuffle_epi8(lut, _mm512_and_si512(x, m0f)), _mm512_shuffle_epi8(lut, _mm512_and_si512(_mm512_srli_epi16::<4>(x), m0f)));
            let cnt = _mm512_cvtepi64_epi32(_mm512_sad_epu8(pc, _mm512_setzero_si512()));
            let ent = _mm256_or_si256(_mm256_slli_epi32::<8>(_mm256_add_epi32(base, _mm256_loadu_si256(rdp.add(8 * v) as *const _))), cnt);
            let km = _mm256_test_epi32_mask(cnt, cnt);
            _mm256_storeu_si256(ap as *mut _, _mm256_maskz_compress_epi32(km, ent));
            ap = ap.add(km.count_ones() as usize);
        }
    }
    w.asst.set_len(ap.offset_from(w.asst.as_ptr()) as usize);
}
#[cfg(not(feature = "w512"))]
#[inline(never)]
unsafe fn credit_mask<const NV: usize>(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let ns = t.sess.len();
    debug_assert!(ns >= 1 && ns <= 8 * NV);
    let nd = g.nd as u32;
    let day0 = g.day0;
    let cbase = g.cbase;
    let mut sr = [std::mem::MaybeUninit::<u32>::uninit(); 64];
    let mut rm = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let mut rd = [std::mem::MaybeUninit::<u32>::uninit(); 64];
    let (srp, rmp, rdp) = (sr.as_mut_ptr() as *mut u32, rm.as_mut_ptr() as *mut u64, rd.as_mut_ptr() as *mut u32);
    for v in 0..NV {
        _mm256_storeu_si256(srp.add(8 * v) as *mut _, _mm256_set1_epi32(-1));
    }
    let sp = t.sess.as_ptr();
    let nrows = (*sp.add(ns - 1)).row as usize + 1;
    let ng = nrows.div_ceil(4);
    for v in 0..ng {
        _mm256_storeu_si256(rmp.add(4 * v) as *mut _, _mm256_setzero_si256());
        _mm_storeu_si128(rdp.add(4 * v) as *mut _, _mm_setzero_si128());
    }
    let (mut f, mut pr) = (0usize, u32::MAX);
    for s in 0..ns {
        let x = &*sp.add(s);
        *srp.add(s) = (x.start - cbase) as u32;
        f = std::hint::select_unpredictable(x.row != pr, s, f);
        pr = x.row;
        *rmp.add(x.row as usize) = _bzhi_u64(!0, s as u32 + 1) & !_bzhi_u64(!0, f as u32);
        *rdp.add(x.row as usize) = (x.day - day0) as u32;
    }
    let mut srv = [_mm256_setzero_si256(); NV];
    for v in 0..NV {
        srv[v] = _mm256_loadu_si256(srp.add(8 * v) as *const _);
    }
    t.serial += 1;
    let ser = t.serial;
    let stamp = t.stamp.as_mut_ptr();
    let um = w.um.as_mut_ptr();
    let n = w.cl.len();
    w.ul.clear();
    w.ul.reserve(n);
    let ulp = w.ul.as_mut_ptr();
    let mut k = 0usize;
    let mut b1 = [_mm256_setzero_si256(); NV];
    let mut b2 = [_mm256_setzero_si256(); NV];
    let mut b1h = [_mm256_setzero_si256(); NV];
    let mut b2h = [_mm256_setzero_si256(); NV];
    let cp = w.cl.as_ptr();
    for j in 0..n {
        let c = *cp.add(j);
        let ts = (c[0] as u32) >> 1;
        let camp = (c[1] >> 42) as u32;
        let (bl, bh) = (_mm256_set1_epi32(ts as i32), _mm256_set1_epi32((ts + WEEK_MS as u32) as i32));
        let (mut a, mut b) = (0u32, 0u32);
        for v in 0..NV {
            a += (_mm256_cmplt_epu32_mask(srv[v], bl) as u32).count_ones();
            b += (_mm256_cmple_epu32_mask(srv[v], bh) as u32).count_ones();
        }
        let r = _bzhi_u64(!0, b) & !_bzhi_u64(!0, a);
        let fresh = *stamp.add(camp as usize) != ser;
        let old = *um.add(camp as usize);
        *um.add(camp as usize) = std::hint::select_unpredictable(fresh, 0, old) | r;
        *stamp.add(camp as usize) = ser;
        *ulp.add(k) = camp;
        k += fresh as usize;
        let hi = (ts as u64 + 1) << 32;
        let (k1, k2) = (_mm256_set1_epi64x((hi | camp as u64) as i64), _mm256_set1_epi64x((hi | (!camp) as u64) as i64));
        for v in 0..NV {
            let m = (r >> (8 * v)) as u8;
            b1[v] = _mm256_mask_max_epu64(b1[v], m, b1[v], k1);
            b2[v] = _mm256_mask_max_epu64(b2[v], m, b2[v], k2);
            b1h[v] = _mm256_mask_max_epu64(b1h[v], m >> 4, b1h[v], k1);
            b2h[v] = _mm256_mask_max_epu64(b2h[v], m >> 4, b2h[v], k2);
        }
    }
    w.ul.set_len(k);
    let mut x1 = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let mut x2 = [std::mem::MaybeUninit::<u64>::uninit(); 64];
    let (x1, x2) = (x1.as_mut_ptr() as *mut u64, x2.as_mut_ptr() as *mut u64);
    for v in 0..NV {
        _mm256_storeu_si256(x1.add(8 * v) as *mut _, b1[v]);
        _mm256_storeu_si256(x2.add(8 * v) as *mut _, b2[v]);
        _mm256_storeu_si256(x1.add(8 * v + 4) as *mut _, b1h[v]);
        _mm256_storeu_si256(x2.add(8 * v + 4) as *mut _, b2h[v]);
    }
    let organic = g.nc as u32;
    w.ses.reserve(ns);
    w.pur.reserve(ns);
    let mut spp = w.ses.as_mut_ptr().add(w.ses.len());
    let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
    for s in 0..ns {
        let (y1, y2) = (*x1.add(s), *x2.add(s));
        let mut camp = std::hint::select_unpredictable(y1 == 0, organic, y1 as u32);
        if y1 as u32 != !(y2 as u32) && y1 != 0 {
            t.st[11] += 1;
            camp = tie_camp(w.cl.iter(), (y1 >> 32) - 1, cside, odds);
        }
        let x = &*sp.add(s);
        let sl = camp * nd + (x.day - day0) as u32;
        *spp = sl;
        spp = spp.add(1);
        *pp = [sl as u64, x.revenue()];
        pp = pp.add(x.purch() as usize);
    }
    w.ses.set_len(spp.offset_from(w.ses.as_ptr()) as usize);
    w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
    w.asst.reserve(k * 4 * ng + 8);
    let mut ap = w.asst.as_mut_ptr().add(w.asst.len());
    let lut = _mm256_broadcastsi128_si256(_mm_setr_epi8(0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4));
    let m0f = _mm256_set1_epi8(0x0F);
    for i in 0..k {
        let c = *ulp.add(i);
        let ub = _mm256_set1_epi64x(*um.add(c as usize) as i64);
        let base = _mm_set1_epi32(c.wrapping_mul(nd) as i32);
        for v in 0..ng {
            let x = _mm256_and_si256(ub, _mm256_loadu_si256(rmp.add(4 * v) as *const _));
            let pc = _mm256_add_epi8(_mm256_shuffle_epi8(lut, _mm256_and_si256(x, m0f)), _mm256_shuffle_epi8(lut, _mm256_and_si256(_mm256_srli_epi16::<4>(x), m0f)));
            let cnt = _mm256_cvtepi64_epi32(_mm256_sad_epu8(pc, _mm256_setzero_si256()));
            let ent = _mm_or_si128(_mm_slli_epi32::<8>(_mm_add_epi32(base, _mm_loadu_si128(rdp.add(4 * v) as *const _))), cnt);
            let km = _mm_test_epi32_mask(cnt, cnt);
            _mm_storeu_si128(ap as *mut _, _mm_maskz_compress_epi32(km, ent));
            ap = ap.add(km.count_ones() as usize);
        }
    }
    w.asst.set_len(ap.offset_from(w.asst.as_ptr()) as usize);
}

#[cfg(feature = "w512")]
#[inline(always)]
unsafe fn credit16(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let n = w.cl.len();
    let nd = g.nd as u32;
    let clp = w.cl.as_ptr() as *const i64;
    let lm = _bzhi_u32(!0, 2 * n as u32);
    let a0 = _mm512_maskz_loadu_epi64(lm as u8, clp as *const _);
    let a1 = _mm512_maskz_loadu_epi64((lm >> 8) as u8, clp.add(8) as *const _);
    let a2 = _mm512_maskz_loadu_epi64((lm >> 16) as u8, clp.add(16) as *const _);
    let a3 = _mm512_maskz_loadu_epi64((lm >> 24) as u8, clp.add(24) as *const _);
    let ie = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
    let io = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
    let (lo0, lo1) = (_mm512_permutex2var_epi64(a0, ie, a1), _mm512_permutex2var_epi64(a2, ie, a3));
    let (hi0, hi1) = (_mm512_srli_epi64::<42>(_mm512_permutex2var_epi64(a0, io, a1)), _mm512_srli_epi64::<42>(_mm512_permutex2var_epi64(a2, io, a3)));
    let m32 = _mm512_set1_epi64(0xFFFF_FFFF);
    let vm = _bzhi_u32(!0, n as u32);
    let mx = _mm512_set1_epi64(-1);
    let kv0 = _mm512_mask_blend_epi64(vm as u8, mx, _mm512_or_si512(_mm512_slli_epi64::<32>(_mm512_srli_epi64::<1>(_mm512_and_si512(lo0, m32))), hi0));
    let kv1 = _mm512_mask_blend_epi64((vm >> 8) as u8, mx, _mm512_or_si512(_mm512_slli_epi64::<32>(_mm512_srli_epi64::<1>(_mm512_and_si512(lo1, m32))), hi1));
    let (c0, c1) = (_mm512_cvtepi64_epi32(hi0), _mm512_cvtepi64_epi32(hi1));
    let cpv = _mm512_mask_blend_epi32(vm as u16, _mm512_set1_epi32(-1), _mm512_inserti64x4::<1>(_mm512_castsi256_si512(c0), c1));
    let conf = _mm512_conflict_epi32(cpv);
    let later = _mm512_reduce_or_epi32(conf) as u32;
    let rep = (!later & ((1u32 << n) - 1)) as u16;
    let cmv = _mm512_or_si512(conf, _mm512_sllv_epi32(_mm512_set1_epi32(1), _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15)));
    let ndv = _mm256_set1_epi32(nd as i32);
    let (s0, s1): (__m256i, __m256i);
    std::arch::asm!("vpmulld {0}, {1}, {2}", out(ymm_reg) s0, in(ymm_reg) c0, in(ymm_reg) ndv, options(pure, nomem, nostack, preserves_flags));
    std::arch::asm!("vpmulld {0}, {1}, {2}", out(ymm_reg) s1, in(ymm_reg) c1, in(ymm_reg) ndv, options(pure, nomem, nostack, preserves_flags));
    let slotv = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(s0), s1);
    let day0 = g.day0;
    let organic = g.nc as u32;
    let cbase = g.cbase;
    let week = (WEEK_MS as u64) << 32;
    let one64 = _mm512_set1_epi64(1);
    let one32 = _mm512_set1_epi32(1);
    let lim = _mm512_set1_epi64(0xFFFF_FFFF);
    let mut ap = w.asst.as_mut_ptr().add(w.asst.len());
    let mut sp = w.ses.as_mut_ptr().add(w.ses.len());
    let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
    let ns = t.sess.len();
    for si in 0..ns {
        let (start, day, purch, rev) = {
            let s = t.sess.get_unchecked(si);
            (s.start, s.day, s.purch(), s.revenue())
        };
        let dayoff = (day - day0) as u32;
        let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
        let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
        let (bt, bb) = (_mm512_set1_epi64(top as i64), _mm512_set1_epi64(bot as i64));
        let m0 = _mm512_cmple_epu64_mask(kv0, bt) & _mm512_cmpge_epu64_mask(kv0, bb);
        let m1 = _mm512_cmple_epu64_mask(kv1, bt) & _mm512_cmpge_epu64_mask(kv1, bb);
        let m = m0 as u32 | (m1 as u32) << 8;
        let maxk = _mm512_reduce_max_epu64(_mm512_max_epu64(_mm512_maskz_mov_epi64(m0, kv0), _mm512_maskz_mov_epi64(m1, kv1)));
        let mut camp = if m != 0 { maxk as u32 } else { organic };
        let bmax = _mm512_set1_epi64(maxk as i64);
        let tie = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(_mm512_xor_si512(kv0, bmax), one64), lim) | _mm512_cmplt_epu64_mask(_mm512_sub_epi64(_mm512_xor_si512(kv1, bmax), one64), lim);
        if tie != 0 && m != 0 {
            t.st[11] += 1;
            camp = tie_camp(w.cl.iter(), maxk >> 32, cside, odds);
        }
        let a = _mm512_test_epi32_mask(cmv, _mm512_set1_epi32(m as i32)) & rep;
        let ent = _mm512_or_si512(_mm512_slli_epi32::<8>(_mm512_add_epi32(slotv, _mm512_set1_epi32(dayoff as i32))), one32);
        _mm512_storeu_si512(ap as *mut _, _mm512_maskz_compress_epi32(a, ent));
        ap = ap.add(a.count_ones() as usize);
        let sl = camp * nd + dayoff;
        *sp = sl;
        sp = sp.add(1);
        *pp = [sl as u64, rev];
        pp = pp.add(purch as usize);
    }
    w.asst.set_len(ap.offset_from(w.asst.as_ptr()) as usize);
    w.ses.set_len(sp.offset_from(w.ses.as_ptr()) as usize);
    w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn credit16<const H: usize>(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let n = w.cl.len();
    debug_assert!(n >= 1 && n <= 8 * H && H <= 2);
    let nd = g.nd as u32;
    let clp = w.cl.as_ptr() as *const i64;
    let lm = _bzhi_u32(!0, 2 * n as u32);
    let vm = _bzhi_u32(!0, n as u32);
    let ie = _mm256_setr_epi64x(0, 2, 4, 6);
    let io = _mm256_setr_epi64x(1, 3, 5, 7);
    let m32 = _mm256_set1_epi64x(0xFFFF_FFFF);
    let mx = _mm256_set1_epi64x(-1);
    let ndv = _mm256_set1_epi32(nd as i32);
    let lane = _mm256_setr_epi32(1, 2, 4, 8, 16, 32, 64, 128);
    let z = _mm256_setzero_si256();
    let mut kv = [z; 4];
    let mut cv = [z; 2];
    let mut cpv = [z; 2];
    let mut slot = [z; 2];
    for h in 0..H {
        let (l, v) = (lm >> (16 * h), vm >> (8 * h));
        let p = clp.add(16 * h);
        let a0 = _mm256_maskz_loadu_epi64(l as u8, p as *const _);
        let a1 = _mm256_maskz_loadu_epi64((l >> 4) as u8, p.add(4) as *const _);
        let a2 = _mm256_maskz_loadu_epi64((l >> 8) as u8, p.add(8) as *const _);
        let a3 = _mm256_maskz_loadu_epi64((l >> 12) as u8, p.add(12) as *const _);
        let (lo0, lo1) = (_mm256_permutex2var_epi64(a0, ie, a1), _mm256_permutex2var_epi64(a2, ie, a3));
        let (hi0, hi1) = (_mm256_srli_epi64::<42>(_mm256_permutex2var_epi64(a0, io, a1)), _mm256_srli_epi64::<42>(_mm256_permutex2var_epi64(a2, io, a3)));
        kv[2 * h] = _mm256_mask_blend_epi64(v as u8, mx, _mm256_or_si256(_mm256_slli_epi64::<32>(_mm256_srli_epi64::<1>(_mm256_and_si256(lo0, m32))), hi0));
        kv[2 * h + 1] = _mm256_mask_blend_epi64((v >> 4) as u8, mx, _mm256_or_si256(_mm256_slli_epi64::<32>(_mm256_srli_epi64::<1>(_mm256_and_si256(lo1, m32))), hi1));
        let c = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm256_cvtepi64_epi32(hi0)), opqx(_mm256_cvtepi64_epi32(hi1)));
        cv[h] = c;
        cpv[h] = _mm256_mask_blend_epi32(v as u8, _mm256_set1_epi32(-1), c);
        let sl: __m256i;
        std::arch::asm!("vpmulld {0}, {1}, {2}", out(ymm_reg) sl, in(ymm_reg) c, in(ymm_reg) ndv, options(pure, nomem, nostack, preserves_flags));
        slot[h] = sl;
    }
    let conf0 = _mm256_conflict_epi32(cpv[0]);
    let mut cmv = [_mm256_or_si256(conf0, lane), z];
    let mut later = or8_epi32(conf0);
    if H == 2 {
        let conf1 = _mm256_conflict_epi32(cpv[1]);
        let mut x = z;
        for j in 0..8 {
            let e = _mm256_cmpeq_epi32_mask(cpv[1], _mm256_permutevar8x32_epi32(cv[0], _mm256_set1_epi32(j)));
            x = _mm256_mask_or_epi32(x, e, x, _mm256_set1_epi32(1 << j));
        }
        cmv[1] = _mm256_or_si256(_mm256_slli_epi32::<8>(_mm256_or_si256(conf1, lane)), x);
        later |= or8_epi32(x) | or8_epi32(conf1) << 8;
    }
    let rep = !later & vm;
    let day0 = g.day0;
    let organic = g.nc as u32;
    let cbase = g.cbase;
    let week = (WEEK_MS as u64) << 32;
    let one64 = _mm256_set1_epi64x(1);
    let one32 = _mm256_set1_epi32(1);
    let lim = _mm256_set1_epi64x(0xFFFF_FFFF);
    let mut ap = w.asst.as_mut_ptr().add(w.asst.len());
    let mut sp = w.ses.as_mut_ptr().add(w.ses.len());
    let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
    let ns = t.sess.len();
    for si in 0..ns {
        let (start, day, purch, rev) = {
            let s = t.sess.get_unchecked(si);
            (s.start, s.day, s.purch(), s.revenue())
        };
        let dayoff = (day - day0) as u32;
        let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
        let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
        let (bt, bb) = (_mm256_set1_epi64x(top as i64), _mm256_set1_epi64x(bot as i64));
        let mut m = 0u32;
        let mut vmax = z;
        for q in 0..2 * H {
            let mq = _mm256_cmple_epu64_mask(kv[q], bt) & _mm256_cmpge_epu64_mask(kv[q], bb);
            vmax = _mm256_mask_max_epu64(vmax, mq, vmax, kv[q]);
            m |= opq(mq as u32) << (4 * q);
        }
        let maxk = max4_epu64(vmax);
        let mut camp = if m != 0 { maxk as u32 } else { organic };
        let bmax = _mm256_set1_epi64x(maxk as i64);
        let mut tie = 0u8;
        for q in 0..2 * H {
            tie |= _mm256_cmplt_epu64_mask(_mm256_sub_epi64(_mm256_xor_si256(kv[q], bmax), one64), lim);
        }
        if tie != 0 && m != 0 {
            t.st[11] += 1;
            camp = tie_camp(w.cl.iter(), maxk >> 32, cside, odds);
        }
        let (mv, dv) = (_mm256_set1_epi32(m as i32), _mm256_set1_epi32(dayoff as i32));
        for h in 0..H {
            let a = _mm256_test_epi32_mask(cmv[h], mv) & (rep >> (8 * h)) as u8;
            let ent = _mm256_or_si256(_mm256_slli_epi32::<8>(_mm256_add_epi32(slot[h], dv)), one32);
            _mm256_storeu_si256(ap as *mut _, _mm256_maskz_compress_epi32(a, ent));
            ap = ap.add(a.count_ones() as usize);
        }
        let sl = camp * nd + dayoff;
        *sp = sl;
        sp = sp.add(1);
        *pp = [sl as u64, rev];
        pp = pp.add(purch as usize);
    }
    w.asst.set_len(ap.offset_from(w.asst.as_ptr()) as usize);
    w.ses.set_len(sp.offset_from(w.ses.as_ptr()) as usize);
    w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
}

fn radix_ts(v: &mut Vec<u64>, tmp: &mut Vec<u64>) {
    let n = v.len();
    tmp.clear();
    tmp.resize(n, 0);
    let mut h = [[0u32; 256]; 4];
    for &x in v.iter() {
        let k = x >> 33;
        h[0][(k & 255) as usize] += 1;
        h[1][((k >> 8) & 255) as usize] += 1;
        h[2][((k >> 16) & 255) as usize] += 1;
        h[3][((k >> 24) & 255) as usize] += 1;
    }
    for (d, hd) in h.iter_mut().enumerate() {
        let mut s = 0u32;
        for c in hd.iter_mut() {
            let x = *c;
            *c = s;
            s += x;
        }
        let sh = 33 + 8 * d as u32;
        let (src, dst) = if d % 2 == 0 { (&*v, &mut *tmp) } else { (&*tmp, &mut *v) };
        let dp = dst.as_mut_ptr();
        for &x in src.iter() {
            let c = &mut hd[((x >> sh) & 255) as usize];
            unsafe { *dp.add(*c as usize) = x };
            *c += 1;
        }
    }
}

#[cfg(feature = "w512")]
#[inline(never)]
fn credit_sorted(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let mut cpl = [0u64; 3];
    let n = w.cl.len();
    let nd = g.nd as u32;
    let day0 = g.day0;
    let organic = g.nc as u32;
    let cbase = g.cbase;
    let week = (WEEK_MS as u64) << 32;
    let ns = t.sess.len();
    w.sk.clear();
    if n <= 64 {
        unsafe {
            let nv = n.div_ceil(8);
            let mut k = [u64::MAX; 64];
            let mut ki = [u64::MAX; 64];
            for (j, c) in w.cl.iter().enumerate() {
                let ts = (c[0] as u32 >> 1) as u64;
                *k.get_unchecked_mut(j) = ts << 32 | c[1] >> 42;
                *ki.get_unchecked_mut(j) = ts << 32 | j as u64;
            }
            let one64 = _mm512_set1_epi64(1);
            let lim = _mm512_set1_epi64(0xFFFF_FFFF);
            let mut sp = w.ses.as_mut_ptr().add(w.ses.len());
            let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
            for si in 0..ns {
                let (start, day, purch, rev) = {
                    let s = t.sess.get_unchecked(si);
                    (s.start, s.day, s.purch(), s.revenue())
                };
                let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
                let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
                let (bt, bb) = (_mm512_set1_epi64(top as i64), _mm512_set1_epi64(bot as i64));
                let mut m = 0u8;
                let mut vmax = _mm512_setzero_si512();
                for v in 0..nv {
                    let kv = _mm512_loadu_si512(k.as_ptr().add(8 * v) as *const _);
                    let m8 = _mm512_cmple_epu64_mask(kv, bt) & _mm512_cmpge_epu64_mask(kv, bb);
                    m |= m8;
                    vmax = _mm512_mask_max_epu64(vmax, m8, vmax, kv);
                }
                let maxk = _mm512_reduce_max_epu64(vmax);
                let mut camp = if m != 0 { maxk as u32 } else { organic };
                let bmax = _mm512_set1_epi64(maxk as i64);
                let mut tie = 0u8;
                for v in 0..nv {
                    let kv = _mm512_loadu_si512(k.as_ptr().add(8 * v) as *const _);
                    tie |= _mm512_cmplt_epu64_mask(_mm512_sub_epi64(_mm512_xor_si512(kv, bmax), one64), lim);
                }
                if tie != 0 && m != 0 {
                    t.st[11] += 1;
                    camp = tie_camp(w.cl.iter(), maxk >> 32, cside, odds);
                }
                let sl = camp * nd + (day - day0) as u32;
                *sp = sl;
                sp = sp.add(1);
                *pp = [sl as u64, rev];
                pp = pp.add(purch as usize);
            }
            w.ses.set_len(sp.offset_from(w.ses.as_ptr()) as usize);
            w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
            w.sk.reserve(n);
            let d = w.sk.as_mut_ptr();
            for j in 0..n {
                let b = _mm512_set1_epi64(*ki.get_unchecked(j) as i64);
                let mut r = 0u32;
                for v in 0..nv {
                    r += (_mm512_cmplt_epu64_mask(_mm512_loadu_si512(ki.as_ptr().add(8 * v) as *const _), b) as u32).count_ones();
                }
                *d.add(r as usize) = *k.get_unchecked(j);
            }
            w.sk.set_len(n);
        }
    } else {
        if pkpm::on() {
            cpl = pkpm::rd();
        }
        w.sidx.clear();
        if n < 1 << 19 {
            w.sk.extend(w.cl.iter().enumerate().map(|(i, c)| ((c[0] as u32 >> 1) as u64) << 33 | (c[1] >> 42) << 19 | i as u64));
            if n >= unsafe { KN[3] } {
                radix_ts(&mut w.sk, &mut w.sk2);
            } else {
                w.sk.sort_unstable();
            }
            for p in w.sk.iter_mut() {
                w.sidx.push((*p & 0x7_FFFF) as u32);
                *p = (*p >> 33) << 32 | ((*p >> 19) & 0x3FFF);
            }
        } else {
            if n >= u32::MAX as usize {
                fallback("a person with 2^32 clicks");
            }
            let mut v: Vec<(u64, u32)> = w.cl.iter().enumerate().map(|(i, c)| (((c[0] as u32 >> 1) as u64) << 32 | c[1] >> 42, i as u32)).collect();
            v.sort_unstable();
            for (k, i) in v {
                w.sk.push(k);
                w.sidx.push(i);
            }
        }
        if pkpm::on() {
            pkpm::lap(&mut cpl, 11);
        }
        w.sk.push(u64::MAX);
        let sk = &w.sk;
        let skp = sk.as_ptr();
        let (mut lo, mut hi) = (0usize, 0usize);
        w.ses.reserve(ns);
        w.pur.reserve(ns);
        let mut spp = unsafe { w.ses.as_mut_ptr().add(w.ses.len()) };
        let mut pp = unsafe { w.pur.as_mut_ptr().add(w.pur.len()) };
        for si in 0..ns {
            let (start, day, purch, rev) = {
                let s = &t.sess[si];
                (s.start, s.day, s.purch(), s.revenue())
            };
            let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
            let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
            unsafe {
                hi += (*skp.add(hi) <= top) as usize;
                hi += (*skp.add(hi) <= top) as usize;
                while *skp.add(hi) <= top {
                    hi += 1;
                }
                lo += (*skp.add(lo) < bot) as usize;
                lo += (*skp.add(lo) < bot) as usize;
                while *skp.add(lo) < bot {
                    lo += 1;
                }
            }
            let last = unsafe { *skp.add(hi.saturating_sub(1)) };
            let mut camp = std::hint::select_unpredictable(hi > lo, last as u32, organic);
            if hi >= lo + 2 && sk[hi - 2] >> 32 == last >> 32 {
                let mut f = hi - 1;
                let mut tie = false;
                while f > lo && sk[f - 1] >> 32 == last >> 32 {
                    f -= 1;
                    tie |= sk[f] != last;
                }
                if tie {
                    t.st[11] += 1;
                    camp = tie_camp(w.sidx[f..hi].iter().map(|&i| &w.cl[i as usize]), last >> 32, cside, odds);
                }
            }
            let sl = camp * nd + (day - day0) as u32;
            unsafe {
                *spp = sl;
                spp = spp.add(1);
                *pp = [sl as u64, rev];
                pp = pp.add(purch as usize);
            }
        }
        unsafe {
            w.ses.set_len(spp.offset_from(w.ses.as_ptr()) as usize);
            w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
        }
        w.sk.pop();
    }
    if pkpm::on() && n > 64 {
        pkpm::lap(&mut cpl, 12);
    }
    let sk = &w.sk;
    let ndu = g.nd;
    t.serial += 1;
    let ser = t.serial;
    let asst = t.acc.asst.as_mut_ptr();
    w.sr.clear();
    w.sr.extend(t.sess.iter().map(|s| (s.start - cbase) as u32));
    w.sr.resize((ns + 1).next_multiple_of(16), u32::MAX);
    let sr = w.sr.as_ptr();
    if ndu <= 16 {
        let mut s0a = [0u32; 16];
        let mut s1a = [0u32; 16];
        let mut s0 = 0usize;
        for si in 0..ns {
            let day = t.sess[si].day;
            if si + 1 == ns || t.sess[si + 1].day != day {
                s0a[(day - day0) as usize] = s0 as u32;
                s1a[(day - day0) as usize] = si as u32 + 1;
                s0 = si + 1;
            }
        }
        unsafe {
            let s0v = _mm512_loadu_si512(s0a.as_ptr() as *const _);
            let s1v = _mm512_loadu_si512(s1a.as_ptr() as *const _);
            let km = ((1u32 << ndu) - 1) as u16;
            let stamp = t.stamp.as_mut_ptr();
            let lidx = w.lidx.as_mut_ptr();
            let nsv = ns.div_ceil(16);
            let (mut pa, mut pb) = (0usize, 0usize);
            for &k in sk.iter() {
                let ts = (k >> 32) as u32;
                let (a, b);
                if nsv <= 4 {
                    let (bl, bh) = (_mm512_set1_epi32(ts as i32), _mm512_set1_epi32((ts + WEEK_MS as u32) as i32));
                    let (mut ca, mut cb) = (0u32, 0u32);
                    for v in 0..nsv {
                        let sv = _mm512_loadu_si512(sr.add(16 * v) as *const _);
                        ca += (_mm512_cmplt_epu32_mask(sv, bl) as u32).count_ones();
                        cb += (_mm512_cmple_epu32_mask(sv, bh) as u32).count_ones();
                    }
                    a = ca as usize;
                    b = cb as usize;
                } else {
                    let tw = ts + WEEK_MS as u32;
                    pa += (*sr.add(pa) < ts) as usize;
                    while *sr.add(pa) < ts {
                        pa += 1;
                    }
                    pb += (*sr.add(pb) <= tw) as usize;
                    pb += (*sr.add(pb) <= tw) as usize;
                    while *sr.add(pb) <= tw {
                        pb += 1;
                    }
                    a = pa;
                    b = pb;
                }
                let c = k as u32 as usize;
                let covered = *lidx.add(c) as usize;
                let x = if *stamp.add(c) == ser { a.max(covered) } else { a };
                *stamp.add(c) = ser;
                *lidx.add(c) = b as u32;
                let cnt = _mm512_max_epi32(_mm512_setzero_si512(), _mm512_sub_epi32(_mm512_min_epi32(_mm512_set1_epi32(b as i32), s1v), _mm512_max_epi32(_mm512_set1_epi32(x as i32), s0v)));
                let p = asst.add(c * ndu) as *mut i32;
                _mm512_mask_storeu_epi32(p, km, _mm512_add_epi32(_mm512_maskz_loadu_epi32(km, p), cnt));
            }
        }
    } else {
        w.rs.clear();
        w.rowof.clear();
        let mut s0 = 0usize;
        for si in 0..ns {
            let day = t.sess[si].day;
            w.rowof.push(w.rs.len() as u32);
            if si + 1 == ns || t.sess[si + 1].day != day {
                w.rs.push((s0 as u32, si as u32 + 1, (day - day0) as u32));
                s0 = si + 1;
            }
        }
        let (mut a, mut b) = (0usize, 0usize);
        for &k in sk.iter() {
            let ts = (k >> 32) as u32;
            unsafe {
                while a < ns && *sr.add(a) < ts {
                    a += 1;
                }
                while b < ns && *sr.add(b) <= ts + WEEK_MS as u32 {
                    b += 1;
                }
            }
            if a >= b {
                continue;
            }
            let c = k as u32 as usize;
            let mut x = a;
            if t.stamp[c] == ser {
                x = x.max(w.lidx[c] as usize);
            } else {
                t.stamp[c] = ser;
            }
            w.lidx[c] = b as u32;
            if x >= b {
                continue;
            }
            let mut r = w.rowof[x] as usize;
            loop {
                let (r0, r1, dayoff) = w.rs[r];
                let cnt = (b.min(r1 as usize) - x.max(r0 as usize)) as u32;
                unsafe { *asst.add(c * ndu + dayoff as usize) += cnt };
                if r1 as usize >= b {
                    break;
                }
                r += 1;
            }
        }
    }
    if pkpm::on() && n > 64 {
        pkpm::lap(&mut cpl, 13);
    }
}
#[cfg(not(feature = "w512"))]
#[inline(never)]
fn credit_sorted(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key]) {
    use std::arch::x86_64::*;
    let mut cpl = [0u64; 3];
    let n = w.cl.len();
    let nd = g.nd as u32;
    let day0 = g.day0;
    let organic = g.nc as u32;
    let cbase = g.cbase;
    let week = (WEEK_MS as u64) << 32;
    let ns = t.sess.len();
    w.sk.clear();
    if n <= 64 {
        unsafe {
            let nv = n.div_ceil(4);
            let mut k = [u64::MAX; 64];
            let mut ki = [u64::MAX; 64];
            for (j, c) in w.cl.iter().enumerate() {
                let ts = opq64((c[0] as u32 >> 1) as u64);
                *k.get_unchecked_mut(j) = ts << 32 | c[1] >> 42;
                *ki.get_unchecked_mut(j) = ts << 32 | j as u64;
            }
            let one64 = _mm256_set1_epi64x(1);
            let lim = _mm256_set1_epi64x(0xFFFF_FFFF);
            let mut sp = w.ses.as_mut_ptr().add(w.ses.len());
            let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
            for si in 0..ns {
                let (start, day, purch, rev) = {
                    let s = t.sess.get_unchecked(si);
                    (s.start, s.day, s.purch(), s.revenue())
                };
                let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
                let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
                let (bt, bb) = (_mm256_set1_epi64x(top as i64), _mm256_set1_epi64x(bot as i64));
                let mut m = 0u8;
                let mut vmax = _mm256_setzero_si256();
                for v in 0..nv {
                    let kv = _mm256_loadu_si256(k.as_ptr().add(4 * v) as *const _);
                    let m8 = _mm256_cmple_epu64_mask(kv, bt) & _mm256_cmpge_epu64_mask(kv, bb);
                    m |= m8;
                    vmax = _mm256_mask_max_epu64(vmax, m8, vmax, kv);
                }
                let maxk = max4_epu64(vmax);
                let mut camp = if m != 0 { maxk as u32 } else { organic };
                let bmax = _mm256_set1_epi64x(maxk as i64);
                let mut tie = 0u8;
                for v in 0..nv {
                    let kv = _mm256_loadu_si256(k.as_ptr().add(4 * v) as *const _);
                    tie |= _mm256_cmplt_epu64_mask(_mm256_sub_epi64(_mm256_xor_si256(kv, bmax), one64), lim);
                }
                if tie != 0 && m != 0 {
                    t.st[11] += 1;
                    camp = tie_camp(w.cl.iter(), maxk >> 32, cside, odds);
                }
                let sl = camp * nd + (day - day0) as u32;
                *sp = sl;
                sp = sp.add(1);
                *pp = [sl as u64, rev];
                pp = pp.add(purch as usize);
            }
            w.ses.set_len(sp.offset_from(w.ses.as_ptr()) as usize);
            w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
            w.sk.reserve(n);
            let d = w.sk.as_mut_ptr();
            for j in 0..n {
                let b = _mm256_set1_epi64x(*ki.get_unchecked(j) as i64);
                let mut r = 0u32;
                for v in 0..nv {
                    r += (_mm256_cmplt_epu64_mask(_mm256_loadu_si256(ki.as_ptr().add(4 * v) as *const _), b) as u32).count_ones();
                }
                *d.add(r as usize) = *k.get_unchecked(j);
            }
            w.sk.set_len(n);
        }
    } else {
        if pkpm::on() {
            cpl = pkpm::rd();
        }
        w.sidx.clear();
        if n < 1 << 19 {
            w.sk.extend(w.cl.iter().enumerate().map(|(i, c)| opq64((c[0] as u32 >> 1) as u64) << 33 | (c[1] >> 42) << 19 | i as u64));
            if n >= unsafe { KN[3] } {
                radix_ts(&mut w.sk, &mut w.sk2);
            } else {
                w.sk.sort_unstable();
            }
            for p in w.sk.iter_mut() {
                w.sidx.push((*p & 0x7_FFFF) as u32);
                *p = (*p >> 33) << 32 | ((*p >> 19) & 0x3FFF);
            }
        } else {
            if n >= u32::MAX as usize {
                fallback("a person with 2^32 clicks");
            }
            let mut v: Vec<(u64, u32)> = w.cl.iter().enumerate().map(|(i, c)| (opq64((c[0] as u32 >> 1) as u64) << 32 | c[1] >> 42, i as u32)).collect();
            v.sort_unstable();
            for (k, i) in v {
                w.sk.push(k);
                w.sidx.push(i);
            }
        }
        if pkpm::on() {
            pkpm::lap(&mut cpl, 11);
        }
        w.sk.push(u64::MAX);
        let sk = &w.sk;
        let skp = sk.as_ptr();
        let (mut lo, mut hi) = (0usize, 0usize);
        w.ses.reserve(ns);
        w.pur.reserve(ns);
        let mut spp = unsafe { w.ses.as_mut_ptr().add(w.ses.len()) };
        let mut pp = unsafe { w.pur.as_mut_ptr().add(w.pur.len()) };
        for si in 0..ns {
            let (start, day, purch, rev) = {
                let s = &t.sess[si];
                (s.start, s.day, s.purch(), s.revenue())
            };
            let top = ((start - cbase) as u64) << 32 | 0xFFFF_FFFF;
            let bot = (top - 0xFFFF_FFFF).saturating_sub(week);
            unsafe {
                hi += (*skp.add(hi) <= top) as usize;
                hi += (*skp.add(hi) <= top) as usize;
                while *skp.add(hi) <= top {
                    hi += 1;
                }
                lo += (*skp.add(lo) < bot) as usize;
                lo += (*skp.add(lo) < bot) as usize;
                while *skp.add(lo) < bot {
                    lo += 1;
                }
            }
            let last = unsafe { *skp.add(hi.saturating_sub(1)) };
            let mut camp = std::hint::select_unpredictable(hi > lo, last as u32, organic);
            if hi >= lo + 2 && sk[hi - 2] >> 32 == last >> 32 {
                let mut f = hi - 1;
                let mut tie = false;
                while f > lo && sk[f - 1] >> 32 == last >> 32 {
                    f -= 1;
                    tie |= sk[f] != last;
                }
                if tie {
                    t.st[11] += 1;
                    camp = tie_camp(w.sidx[f..hi].iter().map(|&i| &w.cl[i as usize]), last >> 32, cside, odds);
                }
            }
            let sl = camp * nd + (day - day0) as u32;
            unsafe {
                *spp = sl;
                spp = spp.add(1);
                *pp = [sl as u64, rev];
                pp = pp.add(purch as usize);
            }
        }
        unsafe {
            w.ses.set_len(spp.offset_from(w.ses.as_ptr()) as usize);
            w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
        }
        w.sk.pop();
    }
    if pkpm::on() && n > 64 {
        pkpm::lap(&mut cpl, 12);
    }
    let sk = &w.sk;
    let ndu = g.nd;
    t.serial += 1;
    let ser = t.serial;
    let asst = t.acc.asst.as_mut_ptr();
    w.sr.clear();
    w.sr.extend(t.sess.iter().map(|s| opq((s.start - cbase) as u32)));
    w.sr.resize((ns + 1).next_multiple_of(16), u32::MAX);
    let sr = w.sr.as_ptr();
    if ndu <= 16 {
        let mut s0a = [0u32; 16];
        let mut s1a = [0u32; 16];
        let mut s0 = 0usize;
        for si in 0..ns {
            let day = t.sess[si].day;
            if si + 1 == ns || t.sess[si + 1].day != day {
                s0a[(day - day0) as usize] = s0 as u32;
                s1a[(day - day0) as usize] = si as u32 + 1;
                s0 = si + 1;
            }
        }
        unsafe {
            let (s0v, s0h) = (_mm256_loadu_si256(s0a.as_ptr() as *const _), _mm256_loadu_si256(s0a.as_ptr().add(8) as *const _));
            let (s1v, s1h) = (_mm256_loadu_si256(s1a.as_ptr() as *const _), _mm256_loadu_si256(s1a.as_ptr().add(8) as *const _));
            let km = ((1u32 << ndu) - 1) as u16;
            let (kml, kmh) = (km as u8, (km >> 8) as u8);
            let z = _mm256_setzero_si256();
            let stamp = t.stamp.as_mut_ptr();
            let lidx = w.lidx.as_mut_ptr();
            let nsv = ns.div_ceil(8);
            let (mut pa, mut pb) = (0usize, 0usize);
            for &k in sk.iter() {
                let ts = (k >> 32) as u32;
                let (a, b);
                if nsv <= 8 {
                    let (bl, bh) = (_mm256_set1_epi32(ts as i32), _mm256_set1_epi32((ts + WEEK_MS as u32) as i32));
                    let (mut ca, mut cb) = (0u32, 0u32);
                    for v in 0..nsv {
                        let sv = _mm256_loadu_si256(sr.add(8 * v) as *const _);
                        ca += (_mm256_cmplt_epu32_mask(sv, bl) as u32).count_ones();
                        cb += (_mm256_cmple_epu32_mask(sv, bh) as u32).count_ones();
                    }
                    a = ca as usize;
                    b = cb as usize;
                } else {
                    let tw = ts + WEEK_MS as u32;
                    pa += (*sr.add(pa) < ts) as usize;
                    while *sr.add(pa) < ts {
                        pa += 1;
                    }
                    pb += (*sr.add(pb) <= tw) as usize;
                    pb += (*sr.add(pb) <= tw) as usize;
                    while *sr.add(pb) <= tw {
                        pb += 1;
                    }
                    a = pa;
                    b = pb;
                }
                let c = k as u32 as usize;
                let covered = *lidx.add(c) as usize;
                let x = if *stamp.add(c) == ser { a.max(covered) } else { a };
                *stamp.add(c) = ser;
                *lidx.add(c) = b as u32;
                let (bv, xv) = (_mm256_set1_epi32(b as i32), _mm256_set1_epi32(x as i32));
                let cnt = _mm256_max_epi32(z, _mm256_sub_epi32(_mm256_min_epi32(bv, s1v), _mm256_max_epi32(xv, s0v)));
                let cnth = _mm256_max_epi32(z, _mm256_sub_epi32(_mm256_min_epi32(bv, s1h), _mm256_max_epi32(xv, s0h)));
                let p = asst.add(c * ndu) as *mut i32;
                _mm256_mask_storeu_epi32(p, kml, _mm256_add_epi32(_mm256_maskz_loadu_epi32(kml, p), cnt));
                _mm256_mask_storeu_epi32(p.add(8), kmh, _mm256_add_epi32(_mm256_maskz_loadu_epi32(kmh, p.add(8)), cnth));
            }
        }
    } else {
        w.rs.clear();
        w.rowof.clear();
        let mut s0 = 0usize;
        for si in 0..ns {
            let day = t.sess[si].day;
            w.rowof.push(w.rs.len() as u32);
            if si + 1 == ns || t.sess[si + 1].day != day {
                w.rs.push((s0 as u32, si as u32 + 1, (day - day0) as u32));
                s0 = si + 1;
            }
        }
        let (mut a, mut b) = (0usize, 0usize);
        for &k in sk.iter() {
            let ts = (k >> 32) as u32;
            unsafe {
                while a < ns && *sr.add(a) < ts {
                    a += 1;
                }
                while b < ns && *sr.add(b) <= ts + WEEK_MS as u32 {
                    b += 1;
                }
            }
            if a >= b {
                continue;
            }
            let c = k as u32 as usize;
            let mut x = a;
            if t.stamp[c] == ser {
                x = x.max(w.lidx[c] as usize);
            } else {
                t.stamp[c] = ser;
            }
            w.lidx[c] = b as u32;
            if x >= b {
                continue;
            }
            let mut r = w.rowof[x] as usize;
            loop {
                let (r0, r1, dayoff) = w.rs[r];
                let cnt = (b.min(r1 as usize) - x.max(r0 as usize)) as u32;
                unsafe { *asst.add(c * ndu + dayoff as usize) += cnt };
                if r1 as usize >= b {
                    break;
                }
                r += 1;
            }
        }
    }
    if pkpm::on() && n > 64 {
        pkpm::lap(&mut cpl, 13);
    }
}

#[inline(always)]
fn flush_row(t: &mut T3, day: i32, sessions: u64, events: u64) {
    let r = &mut t.rows;
    let n = r.day.len();
    debug_assert!(n < r.day.capacity());
    unsafe {
        *r.ukeys.as_mut_ptr().add(n) = (r.names.off.len() - 2) as u32;
        *r.day.as_mut_ptr().add(n) = day;
        *r.ses.as_mut_ptr().add(n) = sessions as i64;
        *r.evs.as_mut_ptr().add(n) = events as i64;
        *r.rev.as_mut_ptr().add(n) = 0;
        *r.top.as_mut_ptr().add(n) = 255;
        r.ukeys.set_len(n + 1);
        r.day.set_len(n + 1);
        r.ses.set_len(n + 1);
        r.evs.set_len(n + 1);
        r.rev.set_len(n + 1);
        r.top.set_len(n + 1);
    }
}

mod pkpm {
    use std::sync::atomic::{AtomicU64, Ordering};
    extern "C" {
        fn syscall(n: i64, ...) -> i64;
    }
    static STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    #[inline(always)]
    pub fn on() -> bool {
        let s = STATE.load(Ordering::Relaxed);
        if s == 0 {
            return init();
        }
        s == 2
    }
    #[cold]
    fn init() -> bool {
        let on = std::env::var("XL_PKPM").is_ok();
        STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
        on
    }
    static KB: AtomicU64 = AtomicU64::new(u64::MAX);
    #[inline(always)]
    pub fn kb() -> u64 {
        let v = KB.load(Ordering::Relaxed);
        if v != u64::MAX {
            return v;
        }
        let v = std::env::var("XL_PK").ok().and_then(|v| v.parse().ok()).unwrap_or(0u64) & 0xFFFF;
        KB.store(v, Ordering::Relaxed);
        v
    }
    pub const NK: usize = 14;
    pub static SUM: [[AtomicU64; 4]; NK] = [const { [const { AtomicU64::new(0) }; 4] }; NK];
    pub const NAMES: [&str; NK] = ["name", "scan", "purch", "organic", "credit16", "credit17_64", "bins_add", "credit65+", "null", "scan_bins", "c65_mask", "cs_sort", "cs_credit", "cs_asst"];
    thread_local! {
        static FD: i32 = {
            let mut lead = -1i32;
            for cfg in [0u64, 1, 5] {
                let mut a = [0u64; 16];
                a[0] = 128u64 << 32;
                a[1] = cfg;
                a[4] = 8;
                a[5] = (1 << 5) | (1 << 6);
                let fd = unsafe { syscall(298, a.as_ptr(), 0i32, -1i32, lead, 0u64) } as i32;
                if lead < 0 {
                    lead = fd;
                }
            }
            lead
        };
    }
    #[inline(never)]
    pub fn rd() -> [u64; 3] {
        let mut v = [0u64; 4];
        FD.with(|&fd| unsafe { syscall(0, fd as i64, v.as_mut_ptr(), 32usize) });
        [v[1], v[2], v[3]]
    }
    thread_local! {
        static LOC: std::cell::UnsafeCell<([[u64; 4]; NK], u64)> = const { std::cell::UnsafeCell::new(([[0; 4]; NK], 0)) };
    }
    #[inline(never)]
    pub fn lap(last: &mut [u64; 3], k: usize) {
        let now = rd();
        LOC.with(|l| {
            let l = unsafe { &mut *l.get() };
            for e in 0..3 {
                l.0[k][e] += now[e].wrapping_sub(last[e]);
            }
            l.0[k][3] += 1;
            l.1 += 1;
            if l.1 % 512 == 0 {
                for k in 0..NK {
                    for e in 0..4 {
                        SUM[k][e].fetch_add(l.0[k][e], Ordering::Relaxed);
                        l.0[k][e] = 0;
                    }
                }
            }
        });
        *last = rd();
    }
    pub fn report() {
        static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !on() || DONE.swap(true, Ordering::Relaxed) {
            return;
        }
        let mut s = String::new();
        for k in 0..NK {
            let v: Vec<u64> = (0..4).map(|e| SUM[k][e].load(Ordering::Relaxed)).collect();
            if v[3] != 0 {
                s.push_str(&format!("{}={}Mc/{}Mi/{:.2}Mbm/{}k ", NAMES[k], v[0] / 1_000_000, v[1] / 1_000_000, v[2] as f64 / 1e6, v[3] / 1000));
            }
        }
        eprintln!("[v3b] pkpm (cycles/instructions/branch misses/calls): {s}");
    }
}

#[inline(never)]
unsafe fn scan_bins(op: *mut u8, bp: *mut Bin, nw: usize, sm: u64, sp: *mut Sess, rp: *mut [u64; 2], dbase: i32, woff: u64, ws: i64) -> (usize, usize) {
    use std::arch::x86_64::*;
    let z = _mm256_setzero_si256();
    let mut ms = [std::mem::MaybeUninit::<u64>::uninit(); 65];
    let mut mb = [std::mem::MaybeUninit::<u32>::uninit(); 65];
    let msp = ms.as_mut_ptr() as *mut u64;
    let mbp = mb.as_mut_ptr() as *mut u32;
    let mut k = 0usize;
    let mut total = 0usize;
    macro_rules! word {
        ($wi:expr) => {{
            let p = op.add($wi * 64);
            let m = (_mm256_movemask_epi8(_mm256_loadu_si256(p as *const _)) as u32 as u64) | opq64(_mm256_movemask_epi8(_mm256_loadu_si256(p.add(32) as *const _)) as u32 as u64) << 32;
            _mm256_storeu_si256(p as *mut _, z);
            _mm256_storeu_si256(p.add(32) as *mut _, z);
            m
        }};
    }
    if nw > 64 {
        let mut ns = 0usize;
        let mut nrow = 0usize;
        let (mut ds, mut ev, mut prev) = (0u64, 0u64, 0u32);
        for wi in 0..nw {
            let mut m = word!(wi);
            while m != 0 {
                let b = bp.add(wi * 64 + m.trailing_zeros() as usize);
                m &= m - 1;
                let Bin { mn: lo, mx: hi, cnt: c, .. } = *b;
                if ns == 0 || lo - prev > GAP_MS as u32 {
                    let day = dbase + ((woff + lo as u64) / DAY_MS as u64) as i32;
                    if ns == 0 {
                        ds = day as u32 as u64;
                    } else if day as u32 != ds as u32 {
                        *rp.add(nrow) = [ds, ev];
                        nrow += 1;
                        ds = day as u32 as u64;
                        ev = 0;
                    }
                    ds += 1 << 32;
                    *sp.add(ns) = Sess { start: ws + lo as i64, day, row: nrow as u32, rev: 0 };
                    ns += 1;
                }
                ev += c as u64;
                prev = hi;
                *b = Bin { mn: u32::MAX, mx: 0, cnt: 0, si: (ns - 1) as u32 };
            }
        }
        if ns == 0 {
            return (0, 0);
        }
        *rp.add(nrow) = [ds, ev];
        return (ns, nrow + 1);
    }
    let mut wm = [std::mem::MaybeUninit::<[u64; 2]>::uninit(); 65];
    let wp = wm.as_mut_ptr() as *mut [u64; 2];
    let mut cur_sm = sm;
    while cur_sm != 0 {
        let wi = cur_sm.trailing_zeros() as usize;
        cur_sm &= cur_sm - 1;
        let m = word!(wi);
        *wp.add(k) = [m, bp.add(wi * 64) as u64];
        k += (m != 0) as usize;
        total += m.count_ones() as usize;
    }
    let _ = (msp, mbp);
    *wp.add(k) = [0, bp as u64];
    if total == 0 {
        return (0, 0);
    }
    let mut q = wp;
    let [mut m, mut base] = *q;
    macro_rules! next_bin {
        () => {{
            let b = (base as *mut Bin).add(m.trailing_zeros() as usize);
            m &= m.wrapping_sub(1);
            let zz = m == 0;
            q = q.add(zz as usize);
            m = std::hint::select_unpredictable(zz, (*q)[0], m);
            base = std::hint::select_unpredictable(zz, (*q)[1], base);
            b
        }};
    }
    let b = next_bin!();
    let Bin { mn: lo, mx: hi, cnt: c, .. } = *b;
    let day = dbase + ((woff + lo as u64) / DAY_MS as u64) as i32;
    let mut ds = 1u64 << 32 | day as u32 as u64;
    let mut ev = c as u64;
    *sp = Sess { start: ws + lo as i64, day, row: 0, rev: 0 };
    *b = BIN0;
    let mut prev = hi;
    let mut ns = 1usize;
    let mut nrow = 0usize;
    let mut left = total - 1;
    while left != 0 {
        left -= 1;
        let b = next_bin!();
        let Bin { mn: lo, mx: hi, cnt: c, .. } = *b;
        let nsf = lo - prev > GAP_MS as u32;
        let day = ((woff as u32 + lo) / DAY_MS as u32) as i32 + dbase;
        let nr = nsf & (day as u32 != ds as u32);
        *rp.add(nrow) = [ds, ev];
        nrow += nr as usize;
        ds = std::hint::select_unpredictable(nr, day as u32 as u64, ds) + ((nsf as u64) << 32);
        ev = std::hint::select_unpredictable(nr, 0, ev) + c as u64;
        let q = sp.add(ns) as *mut u64;
        *q = (ws + lo as i64) as u64;
        *q.add(1) = (nrow as u64) << 32 | day as u32 as u64;
        *q.add(2) = 0;
        ns += nsf as usize;
        *b = Bin { mn: u32::MAX, mx: 0, cnt: 0, si: (ns - 1) as u32 };
        prev = hi;
    }
    *rp.add(nrow) = [ds, ev];
    (ns, nrow + 1)
}

fn finish_person(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key], key: Key, name: &mut Vec<u8>) {
    let row0 = t.rows.day.len();
    walk_person(g, odds, t, w, cside, key, name);
    unsafe { expand_rows(t, row0) };
}

#[cfg(feature = "w512")]
#[inline(always)]
unsafe fn expand_rows(t: &mut T3, row0: usize) {
    let r = &mut t.rows;
    let nr = r.day.len() - row0;
    let name_ix = (r.names.off.len() - 2) as u32;
    let (rp, vp, tq) = (t.sb.rw.as_ptr(), t.sb.rv.as_ptr(), t.sb.tp.as_ptr());
    let (pu, pd, pse, pev, pr, pt) = (r.ukeys.as_mut_ptr().add(row0), r.day.as_mut_ptr().add(row0), r.ses.as_mut_ptr().add(row0), r.evs.as_mut_ptr().add(row0), r.rev.as_mut_ptr().add(row0), r.top.as_mut_ptr().add(row0));
    if nr <= 16 {
        use std::arch::x86_64::*;
        let km = _bzhi_u32(!0, nr as u32);
        let (k0, k1) = (km as u8, (km >> 8) as u8);
        let rq = rp as *const __m512i;
        let (p0, p1, p2, p3) = (_mm512_loadu_si512(rq as *const _), _mm512_loadu_si512(rq.add(1) as *const _), _mm512_loadu_si512(rq.add(2) as *const _), _mm512_loadu_si512(rq.add(3) as *const _));
        let ie = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
        let io = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
        let (ds0, ds1) = (_mm512_permutex2var_epi64(p0, ie, p1), _mm512_permutex2var_epi64(p2, ie, p3));
        let (ev0, ev1) = (_mm512_permutex2var_epi64(p0, io, p1), _mm512_permutex2var_epi64(p2, io, p3));
        _mm512_mask_storeu_epi64(pse as *mut _, k0, _mm512_srli_epi64::<32>(ds0));
        _mm512_mask_storeu_epi64(pse.add(8) as *mut _, k1, _mm512_srli_epi64::<32>(ds1));
        _mm512_mask_storeu_epi64(pev as *mut _, k0, ev0);
        _mm512_mask_storeu_epi64(pev.add(8) as *mut _, k1, ev1);
        _mm512_mask_storeu_epi64(pr as *mut _, k0, _mm512_loadu_si512(vp as *const _));
        _mm512_mask_storeu_epi64(pr.add(8) as *mut _, k1, _mm512_loadu_si512(vp.add(8) as *const _));
        let dv = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(ds0)), _mm512_cvtepi64_epi32(ds1));
        _mm512_mask_storeu_epi32(pd as *mut _, km as u16, dv);
        _mm512_mask_storeu_epi32(pu as *mut _, km as u16, _mm512_set1_epi32(name_ix as i32));
        _mm_mask_storeu_epi8(pt as *mut _, km as u16, _mm_loadu_si128(tq as *const _));
    } else {
        for k in 0..nr {
            let [ds, ev] = *rp.add(k);
            *pu.add(k) = name_ix;
            *pd.add(k) = ds as i32;
            *pse.add(k) = (ds >> 32) as i64;
            *pev.add(k) = ev as i64;
            *pr.add(k) = *vp.add(k) as i64;
            *pt.add(k) = *tq.add(k);
        }
    }
}
#[cfg(not(feature = "w512"))]
#[inline(always)]
unsafe fn expand_rows(t: &mut T3, row0: usize) {
    let r = &mut t.rows;
    let nr = r.day.len() - row0;
    let name_ix = (r.names.off.len() - 2) as u32;
    let (rp, vp, tq) = (t.sb.rw.as_ptr(), t.sb.rv.as_ptr(), t.sb.tp.as_ptr());
    let (pu, pd, pse, pev, pr, pt) = (r.ukeys.as_mut_ptr().add(row0), r.day.as_mut_ptr().add(row0), r.ses.as_mut_ptr().add(row0), r.evs.as_mut_ptr().add(row0), r.rev.as_mut_ptr().add(row0), r.top.as_mut_ptr().add(row0));
    if nr <= 16 {
        use std::arch::x86_64::*;
        let km = _bzhi_u32(!0, nr as u32);
        let rq = rp as *const __m256i;
        let ie = _mm256_setr_epi64x(0, 2, 4, 6);
        let io = _mm256_setr_epi64x(1, 3, 5, 7);
        let nv = _mm_set1_epi32(name_ix as i32);
        let mut q = 0usize;
        while 4 * q < nr {
            let k = (km >> (4 * q)) as u8 & 15;
            let (p0, p1) = (_mm256_loadu_si256(rq.add(2 * q)), opqv(_mm256_loadu_si256(rq.add(2 * q + 1))));
            let ds = _mm256_permutex2var_epi64(p0, ie, p1);
            let ev = _mm256_permutex2var_epi64(p0, io, p1);
            _mm256_mask_storeu_epi64(pse.add(4 * q) as *mut _, k, _mm256_srli_epi64::<32>(ds));
            _mm256_mask_storeu_epi64(pev.add(4 * q) as *mut _, k, ev);
            _mm256_mask_storeu_epi64(pr.add(4 * q) as *mut _, k, _mm256_loadu_si256(vp.add(4 * q) as *const _));
            _mm_mask_storeu_epi32(pd.add(4 * q) as *mut _, k, _mm256_cvtepi64_epi32(ds));
            _mm_mask_storeu_epi32(pu.add(4 * q) as *mut _, k, nv);
            q += 1;
        }
        _mm_mask_storeu_epi8(pt as *mut _, km as u16, _mm_loadu_si128(tq as *const _));
    } else {
        for k in 0..nr {
            let [ds, ev] = *rp.add(k);
            let ds = opq64(ds);
            *pu.add(k) = name_ix;
            *pd.add(k) = ds as i32;
            *pse.add(k) = (ds >> 32) as i64;
            *pev.add(k) = ev as i64;
            *pr.add(k) = *vp.add(k) as i64;
            *pt.add(k) = *tq.add(k);
        }
    }
}

#[inline(always)]
fn walk_person(g: &G, odds: &[Vec<u8>], t: &mut T3, w: &mut Walk, cside: &[Key], key: Key, name: &mut Vec<u8>) {
    if w.ses.len() >= LOGN || w.asst.len() >= LOGN {
        w.apply(&mut t.acc);
    }
    let c0 = pf_now();
    let pm = pkpm::on();
    let mut pl = [0u64; 3];
    if pm {
        pl = pkpm::rd();
        pkpm::lap(&mut pl, 8);
    }
    let row0 = t.rows.day.len();
    {
        let r = &mut t.rows;
        let room = g.nd + 2;
        if r.day.capacity() - row0 < room {
            r.ukeys.reserve(room);
            r.day.reserve(room);
            r.ses.reserve(room);
            r.evs.reserve(room);
            r.rev.reserve(room);
            r.top.reserve(room);
        }
        if is_short(key) {
            let (v, len) = enc36(key.lo);
            r.names.data.reserve(16);
            unsafe {
                let l = r.names.data.len();
                let p = r.names.data.as_mut_ptr().add(l);
                *p = b'u';
                (p.add(1) as *mut u64).write_unaligned(v.to_be());
                r.names.data.set_len(l + 1 + len);
            }
            r.names.off.push(r.names.data.len() as u32);
        } else {
            id_text(key, b'u', odds, name);
            r.names.push(name);
        }
    }
    let c1 = pf_now();
    t.st[14] += c1 - c0;
    if pm {
        pkpm::lap(&mut pl, 0);
    }
    let nbins = t.sb.b.len();
    t.sess.clear();
    t.sess.reserve(nbins + 1);
    t.sb.rw.clear();
    t.sb.rw.reserve(nbins + 2);
    unsafe {
        let r = &mut t.rows;
        if r.ukeys.capacity() - row0 < g.nd + 2 || r.ses.capacity() - row0 < g.nd + 2 || r.evs.capacity() - row0 < g.nd + 2 || r.rev.capacity() - row0 < g.nd + 2 || r.top.capacity() - row0 < g.nd + 2 || r.ukeys.len() != row0 {
            fallback("row columns out of step");
        }
        let name_ix = (r.names.off.len() - 2) as u32;
        let ws = g.ws;
        let dbase = ws.div_euclid(DAY_MS) as i32;
        let woff = ws.rem_euclid(DAY_MS) as u64;
        let rp = t.sb.rw.as_mut_ptr();
        if pm {
            pkpm::lap(&mut pl, 1);
        }
        let (ns, nr) = scan_bins(t.sb.occ.as_mut_ptr(), t.sb.b.as_mut_ptr(), nbins / 64, t.sb.sm, t.sess.as_mut_ptr(), rp, dbase, woff, ws);
        t.sb.sm = 0;
        if pm {
            pkpm::lap(&mut pl, 9);
        }
        if ns == 0 || nr > g.nd + 2 || ns > nbins {
            fallback("more day rows than days");
        }
        let _ = name_ix;
        let sb = &mut t.sb;
        if sb.rv.len() < nr.max(16) {
            sb.rv.resize(nr.max(16), 0);
            sb.tp.resize(nr.max(16), 255);
        }
        {
            use std::arch::x86_64::*;
            let (vp, tq) = (sb.rv.as_mut_ptr(), sb.tp.as_mut_ptr());
            #[cfg(feature = "w512")]
            {
                _mm512_storeu_si512(vp as *mut _, _mm512_setzero_si512());
                _mm512_storeu_si512(vp.add(8) as *mut _, _mm512_setzero_si512());
            }
            #[cfg(not(feature = "w512"))]
            {
                let z = _mm256_setzero_si256();
                _mm256_storeu_si256(vp as *mut _, z);
                _mm256_storeu_si256(vp.add(4) as *mut _, z);
                _mm256_storeu_si256(vp.add(8) as *mut _, z);
                _mm256_storeu_si256(vp.add(12) as *mut _, z);
            }
            _mm_storeu_si128(tq as *mut _, _mm_set1_epi8(-1));
            for k in 16..nr {
                *vp.add(k) = 0;
                *tq.add(k) = 255;
            }
        }
        let nrow = row0 + nr;
        t.sess.set_len(ns);
        r.ukeys.set_len(nrow);
        r.day.set_len(nrow);
        r.ses.set_len(nrow);
        r.evs.set_len(nrow);
        r.rev.set_len(nrow);
        r.top.set_len(nrow);
    }
    let c2 = pf_now();
    t.st[15] += c2 - c1;
    if pm {
        pkpm::lap(&mut pl, 1);
    }
    if unsafe { KN[4] } & 2 != 0 {
        w.ppv.clear();
        w.cl.clear();
        return;
    }
    if w.ppv.len() == 1 {
        let v = w.ppv[0];
        let bi = ((v >> 32) as u32 / GAP_MS as u32) as usize;
        let price = (v as u32 >> 5) as u64;
        let s = &mut t.sess[t.sb.b[bi].si as usize];
        s.rev = PURCH | price;
        let r = s.row as usize;
        t.sb.rv[r] = price;
        t.sb.tp[r] = (v & 31) as u8;
        w.ppv.clear();
    } else if !w.ppv.is_empty() {
        let nrows = t.rows.day.len() - row0;
        let sb = &mut t.sb;
        sb.cs.clear();
        sb.cs.resize(nrows * 32, 0);
        for &v in w.ppv.iter() {
            let rel = (v >> 32) as u32;
            let bi = (rel / GAP_MS as u32) as usize;
            let price = (v as u32 >> 5) as u64;
            let c = (v & 31) as usize;
            let s = &mut t.sess[sb.b[bi].si as usize];
            s.rev = (s.rev | PURCH) + price;
            let r = s.row as usize;
            sb.rv[r] += price;
            sb.cs[r * 32 + c] += price;
        }
        for r in 0..nrows {
            let rv = sb.rv[r];
            if rv != 0 {
                unsafe {
                    use std::arch::x86_64::*;
                    let p = sb.cs.as_ptr().add(r * 32);
                    #[cfg(feature = "w512")]
                    let m = {
                        let v: [__m512i; 4] = [_mm512_loadu_si512(p as *const _), _mm512_loadu_si512(p.add(8) as *const _), _mm512_loadu_si512(p.add(16) as *const _), _mm512_loadu_si512(p.add(24) as *const _)];
                        let best = _mm512_set1_epi64(_mm512_reduce_max_epu64(_mm512_max_epu64(_mm512_max_epu64(v[0], v[1]), _mm512_max_epu64(v[2], v[3]))) as i64);
                        let m = _mm512_cmpeq_epu64_mask(v[0], best) as u32 | (_mm512_cmpeq_epu64_mask(v[1], best) as u32) << 8 | (_mm512_cmpeq_epu64_mask(v[2], best) as u32) << 16 | (_mm512_cmpeq_epu64_mask(v[3], best) as u32) << 24;
                        m
                    };
                    #[cfg(not(feature = "w512"))]
                    let m = {
                        let mut v = [_mm256_setzero_si256(); 8];
                        let mut mx = _mm256_setzero_si256();
                        for q in 0..8 {
                            v[q] = _mm256_loadu_si256(p.add(4 * q) as *const _);
                            mx = _mm256_max_epu64(mx, v[q]);
                        }
                        let best = _mm256_set1_epi64x(max4_epu64(mx) as i64);
                        let mut m = 0u32;
                        for q in 0..8 {
                            m |= opq(_mm256_cmpeq_epu64_mask(v[q], best) as u32) << (4 * q);
                        }
                        m
                    };
                    sb.tp[r] = m.trailing_zeros() as u8;
                }
            }
        }
        w.ppv.clear();
    }
    t.st[0] += 1;
    t.st[1] += t.sess.len() as u64;
    t.st[2] += w.cl.len() as u64;
    let c3 = pf_now();
    t.st[8] += c3 - c2;
    if pm {
        pkpm::lap(&mut pl, 2);
    }
    let n = w.cl.len();
    if unsafe { PF } {
        let b = if n == 0 { 0 } else if n <= 8 { 1 } else if n <= 16 { 2 } else if n <= 32 { 3 } else if n <= 64 { 4 } else if n <= 1024 { 5 } else { 6 };
        unsafe {
            HIST[b][0] += 1;
            HIST[b][1] += n as u64;
            HIST[b][2] += t.sess.len() as u64;
        }
    }
    if n == 0 {
        let nd = g.nd as u32;
        let base = g.nc as u32 * nd;
        unsafe {
            let mut sp = w.ses.as_mut_ptr().add(w.ses.len());
            let mut pp = w.pur.as_mut_ptr().add(w.pur.len());
            for s in &t.sess {
                let sl = base + (s.day - g.day0) as u32;
                *sp = sl;
                sp = sp.add(1);
                *pp = [sl as u64, s.revenue()];
                pp = pp.add(s.purch() as usize);
            }
            w.ses.set_len(sp.offset_from(w.ses.as_ptr()) as usize);
            w.pur.set_len(pp.offset_from(w.pur.as_ptr()) as usize);
        }
        t.st[9] += pf_now() - c3;
        t.st[3] += 1;
        if pm {
            pkpm::lap(&mut pl, 3);
        }
        return;
    }
    let kn7 = unsafe { KN[7] };
    if kn7 & 1 != 0 || (kn7 & 2 != 0 && n <= 16) || (kn7 & 4 != 0 && n > 16 && n <= 64) || (kn7 & 8 != 0 && n > 64) {
        w.cl.clear();
        return;
    }
    let ns = t.sess.len();
    if (n > 16 || pkpm::kb() & 2 != 0) && ns <= 64 && pkpm::kb() & 1 == 0 {
        unsafe {
            if ns <= 8 {
                credit_mask::<1>(g, odds, t, w, cside)
            } else if ns <= 16 {
                credit_mask::<2>(g, odds, t, w, cside)
            } else if ns <= 32 {
                credit_mask::<4>(g, odds, t, w, cside)
            } else {
                credit_mask::<8>(g, odds, t, w, cside)
            }
        };
        if pm {
            pkpm::lap(&mut pl, if n <= 16 { 9 } else if n <= 64 { 5 } else { 10 });
        }
    } else if n <= 16 {
        #[cfg(feature = "w512")]
        unsafe {
            credit16(g, odds, t, w, cside)
        };
        #[cfg(not(feature = "w512"))]
        unsafe {
            if n <= 8 {
                credit16::<1>(g, odds, t, w, cside)
            } else {
                credit16::<2>(g, odds, t, w, cside)
            }
        };
        t.st[6] += pf_now() - c3;
        if pm {
            pkpm::lap(&mut pl, 4);
        }
    } else {
        credit_sorted(g, odds, t, w, cside);
        t.st[5] += pf_now() - c3;
        if pm {
            pkpm::lap(&mut pl, if n <= 64 { 5 } else { 7 });
        }
    }
    w.cl.clear();
}

fn die(what: &str) -> ! {
    eprintln!("[v3b] output error: {what}");
    unsafe { _exit(1) }
}

fn clear_parquet(dir: &std::path::Path) {
    if let Ok(mut rd) = std::fs::read_dir(dir) {
        if rd.next().is_some() {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "parquet") {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }
}

const STREAM: pqw::Method = pqw::Method::Stream(1 << 20);

fn write_users(path: &std::path::Path, r: &Rows, cats: &StrBuf) {
    use pqw::*;
    pkpm::report();
    let cols = [
        Column { name: "user_id", data: Data::Dict { dict: r.names.col(), keys: Keys::U32(&r.ukeys), nullable: false, plain: false } },
        Column { name: "day", data: Data::Date(&r.day, IntEnc::Dense) },
        Column { name: "sessions", data: Data::I64(&r.ses, IntEnc::Auto) },
        Column { name: "events", data: Data::I64(&r.evs, IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I64(&r.rev, IntEnc::Plain) },
        Column { name: "top_category", data: Data::Dict { dict: cats.col(), keys: Keys::U8(&r.top), nullable: true, plain: false } },
    ];
    if write_file(path, &cols, &Opts::default(), STREAM).is_err() {
        die("write user_daily");
    }
}

fn write_campaigns(path: &std::path::Path, g: &G, acc: &[[u64; 4]]) {
    use pqw::*;
    let mut cid = StrBuf::new();
    let mut ch = StrBuf::new();
    let (mut day, mut s, mut ps, mut rev, mut asst) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for c in 0..=g.nc {
        let mut idb = [0u8; 24];
        let mut at = idb.len();
        let mut x = c;
        loop {
            at -= 1;
            idb[at] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        at -= 2;
        idb[at] = b'c';
        idb[at + 1] = b'_';
        for d in 0..g.nd {
            let v = &acc[c * g.nd + d];
            if v[0] == 0 && v[3] == 0 {
                continue;
            }
            if c == g.nc {
                cid.push(b"organic");
                ch.push(b"organic");
            } else {
                cid.push(&idb[at..]);
                ch.push(&g.channels[g.camp[c] as usize]);
            }
            day.push(g.day0 + d as i32);
            s.push(v[0] as i64);
            ps.push(v[1] as i64);
            rev.push(v[2] as i64);
            asst.push(v[3] as i64);
        }
    }
    let cols = [
        Column { name: "campaign_id", data: Data::Str(cid.col()) },
        Column { name: "channel", data: Data::Str(ch.col()) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Dense) },
        Column { name: "sessions", data: Data::I64(&s, IntEnc::Auto) },
        Column { name: "purchasing_sessions", data: Data::I64(&ps, IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I64(&rev, IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&asst, IntEnc::Auto) },
    ];
    if write_file(path, &cols, &Opts::default(), STREAM).is_err() {
        die("write campaign_daily");
    }
}

fn pm_on(name: &str) -> bool {
    std::env::var_os(name).map_or(true, |v| v != "0")
}
fn np_of(b: u32) -> usize {
    1usize << b
}
fn bits_for(bytes: usize, target: usize, max: u32) -> u32 {
    let mut b = 0;
    while b < max && (bytes >> b) > target {
        b += 1;
    }
    b
}

pub fn run(g: &G, outdir: &str, timing: bool) {
    let t0 = std::time::Instant::now();
    let mut last = t0;
    let mut laps: Vec<(&str, f64)> = Vec::new();
    let mut mem: Vec<(usize, usize, i64)> = Vec::new();
    let mut cpu: Vec<(f64, f64)> = Vec::new();
    let mut last_ru = (0.0f64, 0.0f64);
    let mut lap = |name: &'static str| {
        let now = std::time::Instant::now();
        laps.push((name, (now - last).as_secs_f64() * 1e3));
        if timing {
            let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
            let mut w = s.split_whitespace().skip(1).map(|x| x.parse::<usize>().unwrap_or(0));
            let (res, sh) = (w.next().unwrap_or(0), w.next().unwrap_or(0));
            mem.push((res * 4096 >> 20, (res - sh.min(res)) * 4096 >> 20, crate::sys::minflt(0)));
            #[cfg(feature = "tl")]
            crate::sys::mp::report(name);
            #[cfg(feature = "tl")]
            crate::sys::cls_report(name);
            extern "C" {
                fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
            }
            let mut ru = [0i64; 18];
            unsafe { getrusage(0, &mut ru) };
            let cur = (ru[0] as f64 * 1e3 + ru[1] as f64 / 1e3, ru[2] as f64 * 1e3 + ru[3] as f64 / 1e3);
            cpu.push((cur.0 - last_ru.0, cur.1 - last_ru.1));
            last_ru = cur;
            last = std::time::Instant::now();
        } else {
            last = now;
        }
    };
    unsafe { SKIP = std::env::var("XL_V3SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(0) };
    unsafe { PF = std::env::var("XL_PF").is_ok() };
    unsafe { pm3::ON = std::env::var("XL_PM").is_ok() };
    for k in 0..8 {
        if let Some(v) = std::env::var(format!("XL_PB{k}")).ok().and_then(|v| v.parse().ok()) {
            unsafe { KN[k] = v };
        }
    }
    if g.wlen > 1 << 31 || g.clen > 1 << 31 {
        fallback("window too long");
    }
    let nt = g.nt;
    let rows_est = g.ev_bytes / 72 + 1;
    let knob = |n: &str, d: usize| std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let b1 = bits_for(rows_est, knob("XL_V3T1", 80) * 1000, knob("XL_V3M1", 12).clamp(10, 14) as u32).max(10);
    if g.wlen > 1 << 31 {
        fallback("window over 2^31 ms");
    }
    unsafe {
        EK = knob("XL_EK", 0) as u32;
        DDK = knob("XL_DD", 0) as u32;
        ESH = 40 - b1;
        let v = &mut *std::ptr::addr_of_mut!(P3S);
        v.clear();
        for _ in 0..nt {
            v.push(P3 { es: ELane::empty(), ew: ELane::empty(), ex: ELane::empty(), seen: Vec::new(), ssl: Vec::new(), wsl: Vec::new(), xsl: Vec::new(), cptr: Vec::new(), ca: Vec::new(), cb: Vec::new(), ms: Vec::new(), mw: Vec::new(), msf: Vec::new(), mwf: Vec::new(), fl: Vec::new(), flw: Vec::new(), ow: Vec::new(), oq: Vec::new(), so: Vec::new(), sq: Vec::new(), wo: Vec::new(), wq: Vec::new(), dc: [0; 8], cands: Vec::new(), ctab: Vec::new(), dead: Vec::new(), out: Vec::new(), out3: Vec::new(), cyc: [0; 8] });
        }
    }
    let b2 = bits_for(rows_est * 17, knob("XL_V3T2", 800) << 10, knob("XL_V3M2", 11).clamp(10, 16) as u32).max(10);
    unsafe {
        if KN[2] == 0 {
            KN[2] = 16;
        }

        let v = &mut *std::ptr::addr_of_mut!(B3S);
        v.clear();
        for _ in 0..nt {
            v.push(B3::new(np_of(b2), g.nc + 1, (g.wlen / GAP_MS as u64) as usize + 66));
        }
    }
    let (np1, np2) = (1usize << b1, 1usize << b2);
    let mut queue = g.queue.clone();
    queue.sort_by_key(|c| match g.files[c.file as usize].kind {
        Kind::Lk => 0,
        Kind::Ck => 1,
        Kind::Ev => 2,
    });
    let n_lk = queue.iter().filter(|c| g.files[c.file as usize].kind == Kind::Lk).count();
    let nacc = (g.nc + 1) * g.nd;
    if nacc >= 1 << 24 {
        fallback("too many (campaign, day) slots");
    }

    let mut ts = Vec::with_capacity(nt);
    let pmr = pm_on("XL_PMR");
    let ekeep = knob("XL_EKEEP", 250).min(1000);
    unsafe { EXT_BASE = crate::sys::a_base() & !(EXT_SZ - 1) };
    EXT_ON.store(ekeep > 0 && ck_late() && crate::sys::a_base() != 0, Ordering::Relaxed);
    let rcap = if pmr && !EXT_ON.load(Ordering::Relaxed) { rows_est / nt + 4096 } else { 0 };
    let lk_bytes: usize = g.files.iter().filter(|f| f.kind == Kind::Lk).map(|f| f.len).sum();
    let lcap = if pmr && lk_bytes > 0 { lk_bytes / 16 + 16 } else { 0 };
    for _ in 0..nt {
        ts.push(UnsafeCell::new(T3 {
            eb: EvBatch::new(),
            cb: CkBatch::new(),
            lb: LkBatch::new(),
            ev: WBins::new(np1),
            ck: CBins::new(np2),
            lk: Vec::with_capacity(lcap),
            u2: UBins::new(np2),
            rows: Rows { names: StrBuf { off: vec![0], data: Vec::with_capacity(rcap * 16) }, ukeys: Vec::with_capacity(rcap), day: Vec::with_capacity(rcap), ses: Vec::with_capacity(rcap), evs: Vec::with_capacity(rcap), rev: Vec::with_capacity(rcap), top: Vec::with_capacity(rcap) },
            acc: Acc::new(nacc),
            tab: Tab::default(),
            sl: Vec::new(),
            tab4: Vec::new(),
            kt: KTab::default(),
            w0s: Vec::new(),
            ck64: Vec::new(),
            stamp: vec![0; g.nc + 1],
            serial: 0,
            lkv: Vec::new(),
            prv: Vec::new(),
            names: Vec::new(),
            pid: Vec::new(),
            off: Vec::new(),
            pos: Vec::new(),
            pe: Vec::new(),
            cpid: Vec::new(),
            coff: Vec::new(),
            sess: Vec::new(),
            starts: Vec::new(),
            days: Vec::new(),
            day_end: Vec::new(),
            catsum: [0; 32],
            ct: Vec::new(),
            rs: Vec::new(),
            n_rows: [0; 4],
            prof: [0; 16],
            st: [0; 16],
            sb: SBins::new(g.wlen),
        }));
    }
    let cx = Cx {
        g,
        b1,
        b2,
        queue,
        qnext: AtomicUsize::new(0),
        next: AtomicUsize::new(0),
        lk_left: AtomicUsize::new(n_lk),
        lkh: Mutex::new(None),
        t: ts,
        odd: Mutex::new(Odd { map: std::collections::HashMap::new(), strs: Vec::new() }),
        odds: UnsafeCell::new(Vec::new()),
        bk: UnsafeCell::new(Back { lkf: Vec::new(), lko: vec![0; np2 + 1], rep: Vec::new() }),
    };
    lap("init");
    run_par(nt, |tid| parse_thread(&cx, tid));
    unsafe { *cx.odds.get() = std::mem::take(&mut cx.odd.lock().unwrap_or_else(|e| e.into_inner()).strs) };
    lap("parse");
    ext_decide(ekeep);
    let mut cen = String::new();
    let vbc = |v: &VB| -> (usize, usize) {
        let (mut c, mut w) = (0, 0);
        for p in 0..v.np {
            let n = v.words(p);
            c += n.div_ceil(CW);
            w += n;
        }
        (c, w)
    };
    if timing {
        let (mut a, mut b, mut e) = ((0, 0), (0, 0), [0u64; 3]);
        for tid in 0..nt {
            let (x, y) = ((0usize, 0usize), (0usize, 0usize)); let _ = &vbc;
            a = (a.0 + x.0, a.1 + x.1);
            b = (b.0 + y.0, b.1 + y.1);
            e[0] += p3(tid).es.nch as u64;
            e[1] += p3(tid).ew.nch as u64;
            e[2] += p3(tid).ex.nch as u64;
        }
        cen.push_str(&format!("parse: lane chunks es={} ew={} ex={} (x4kB) | ck.s chunks={} words={} ck.w chunks={} words={} | lk rows={} cap={}\n", e[0], e[1], e[2], a.0, a.1, b.0, b.1, (0..nt).map(|t| cx.t(t).lk.len()).sum::<usize>(), (0..nt).map(|t| cx.t(t).lk.capacity()).sum::<usize>()));
    }
    cx.next.store(0, Ordering::Relaxed);
    run_par(nt, |tid| dedup_thread(&cx, tid));
    lap("dedup");
    cx.next.store(0, Ordering::Relaxed);
    run_par(nt, |tid| click_thread(&cx, tid));
    unsafe { (*cx.odds.get()).append(&mut cx.odd.lock().unwrap_or_else(|e| e.into_inner()).strs) };
    lap("clicks");
    if timing {
        let (mut a, mut b, mut f) = ((0, 0), (0, 0), [0usize; 2]);
        for tid in 0..nt {
            let (x, y) = (vbc(&cx.t(tid).u2.s), vbc(&cx.t(tid).u2.w));
            a = (a.0 + x.0, a.1 + x.1);
            b = (b.0 + y.0, b.1 + y.1);
            f[0] += cx.t(tid).u2.s.free.len();
            f[1] += cx.t(tid).u2.w.free.len();
        }
        cen.push_str(&format!("dedup: u2.s chunks={} words={} u2.w chunks={} words={} | free lists left: s={} w={} chunks\n", a.0, a.1, b.0, b.1, f[0], f[1]));
    }
    if timing {
        let mut c = [0u64; 8];
        for tid in 0..nt {
            for k in 0..8 {
                c[k] += p3(tid).dc[k];
            }
        }
        let names = ["gather", "pass1", "cbuild", "ctab", "cemit", "release", "runs", "x7"];
        let mut s = String::new();
        for k in 0..8 {
            s.push_str(&format!("{}={} ", names[k], c[k] / 1_000_000));
        }
        eprintln!("[v3b] pe: dedup Mcycles: {s}");
    }
    if timing {
        let mut c = [0u64; 8];
        for tid in 0..nt {
            for k in 0..8 {
                c[k] += p3(tid).cyc[k];
            }
        }
        let flt = std::fs::read_to_string("/proc/self/stat").ok().and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().nth(7).map(|x| x.to_string()))).unwrap_or_default();
        eprintln!("[v3b] pa Mcycles: ev_simd={} ev_spush={} ev_sink={} ck_sink={} p2s_scan={} p2s_emit={} p2w={} | minflt={flt} chunks: s={} w={}", c[6] / 1_000_000, c[7] / 1_000_000, c[0] / 1_000_000, c[1] / 1_000_000, c[3] / 1_000_000, c[4] / 1_000_000, c[5] / 1_000_000, (0..nt).map(|t| p3(t).es.nch as u64).sum::<u64>(), (0..nt).map(|t| p3(t).ew.nch as u64).sum::<u64>());
    }
    links_join(&cx);
    lap("links");
    if EXT_ON.load(Ordering::Relaxed) {
        let kept: u64 = (0..nt).map(|t| cx.t(t).n_rows[3]).sum();
        ROWS_N.store(kept as usize * knob("XL_ERN", 140) / 1000 / nt + 4096, Ordering::Relaxed);
        if knob("XL_EKCHK", 0) == 1 {
            ext_check(&cx);
        }
        if knob("XL_EKPOISON", 0) == 1 {
            ext_poison();
        }
        let d = ext_donate();
        if timing {
            cen.push_str(&format!("e2we: extents={} kept={} donated_MB={} rows_cap={}\n", EXT_N.load(Ordering::Relaxed), unsafe { EXT_NK }, d >> 20, ROWS_N.load(Ordering::Relaxed)));
        }
    }
    if pm_on("XL_PMH") {
        let mut all: Vec<*mut u64> = Vec::new();
        for tid in 0..nt {
            let t = cx.t(tid);
            all.append(&mut t.u2.s.free);
            all.append(&mut t.u2.w.free);
        }
        let n = all.len();
        for tid in 0..nt {
            let b = unsafe { &mut *b3(tid) };
            b.blk.free.extend_from_slice(&all[n * tid / nt..n * (tid + 1) / nt]);
        }
    }
    cx.next.store(0, Ordering::Relaxed);
    run_par(nt, |tid| p1_thread(&cx, tid));
    lap("p1");
    let out = std::path::Path::new(outdir);
    let ud = out.join("user_daily");
    let cd = out.join("campaign_daily");
    cx.next.store(0, Ordering::Relaxed);
    run_par(nt, |tid| {
        if tid == nt - 1 {
            if std::fs::create_dir_all(&ud).is_err() || std::fs::create_dir_all(&cd).is_err() {
                die("create output dirs");
            }
            clear_parquet(&ud);
            clear_parquet(&cd);
        }
        p2_thread(&cx, tid)
    });
    lap("p2");
    let mut acc = vec![[0u64; 4]; nacc];
    for tid in 0..nt {
        let t = cx.t(tid);
        for (i, a) in acc.iter_mut().enumerate() {
            a[0] += t.acc.ses[i] as u64 + t.acc.ses_hi[i];
            a[1] += t.acc.pur[i][0];
            a[2] += t.acc.pur[i][1];
            a[3] += t.acc.asst[i] as u64;
        }
    }
    let mut cats = StrBuf::new();
    for c in &g.cats {
        cats.push(c);
    }
    lap("wprep");
    let cd_file = cd.join("part-0.parquet");
    let acc_ptr = &acc as *const Vec<[u64; 4]> as usize;
    let gp = g as *const G as usize;
    let camp_h = std::thread::spawn(move || unsafe {
        let acc = &*(acc_ptr as *const Vec<[u64; 4]>);
        let g = &*(gp as *const G);
        write_campaigns(&cd_file, g, acc);
    });
    run_par(nt, |tid| {
        let mut tk = std::time::Instant::now();
        write_users(&ud.join(format!("part-{tid}.parquet")), &cx.t(tid).rows, &cats);
        cx.t(tid).prof[12] += tick(&mut tk);
    });
    let _ = camp_h.join();
    lap("write");
    if timing {
        let mut s = String::new();
        for (n, ms) in &laps {
            s.push_str(&format!("{n}={ms:.1} "));
        }
        let mut cs = String::new();
        for (k, (n, _)) in laps.iter().enumerate() {
            cs.push_str(&format!("{n}={:.0}/{:.0} ", cpu[k].0, cpu[k].1));
        }
        eprintln!("[v3b] cpu ms (user/system, all threads): {cs}");
        let mut c = [0u64; 4];
        let mut rows = 0;
        for tid in 0..nt {
            for k in 0..4 {
                c[k] += cx.t(tid).n_rows[k];
            }
            rows += cx.t(tid).rows.day.len();
        }
        let hwm = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())).unwrap_or_default();
        eprintln!("[v3b] ms: {s}v3total={:.1} peak_rss_kb={hwm}", t0.elapsed().as_secs_f64() * 1e3);
        let mut s = String::new();
        for (i, (n, _)) in laps.iter().enumerate() {
            s.push_str(&format!("{n}={}/{}/{} ", mem[i].0, mem[i].1, mem[i].2));
        }
        let roll = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
        let f = |k: &str| roll.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse::<usize>().ok()).unwrap_or(0) >> 10;
        let r = &cx.t(0).rows;
        cen.push_str(&format!("end: rows per thread {:?}; thread 0 caps: day={} ses={} evs={} rev={} top={} ukeys={} names.off={} names.data={}\n", (0..nt).map(|t| cx.t(t).rows.day.len()).collect::<Vec<_>>(), r.day.capacity(), r.ses.capacity(), r.evs.capacity(), r.rev.capacity(), r.top.capacity(), r.ukeys.capacity(), r.names.off.capacity(), r.names.data.capacity()));
        for l in cen.lines() {
            eprintln!("[pm] census {l}");
        }
        eprintln!("[pm] after lap rss_MB/anon_MB/minflt: {s}| end: Rss={} Anonymous={} AnonHugePages={} MB", f("Rss:"), f("Anonymous:"), f("AnonHugePages:"));
        let mut pr = [0u64; 16];
        for tid in 0..nt {
            for k in 0..16 {
                pr[k] += cx.t(tid).prof[k];
            }
        }
        let names = ["dd.gather", "dd.table", "dd.emit", "p1.links", "p1.gather", "p1.group", "p1.short", "p1.wide", "p2.gather", "p2.runs", "lk.uf", "wr.camp", "wr.users", "p2.walk", "", ""];
        let mut s = String::new();
        for k in 0..14 {
            s.push_str(&format!("{}={:.0} ", names[k], pr[k] as f64 / 1e6));
        }
        eprintln!("[v3b] prof (thread-ms): {s}");
        let mut st = [0u64; 16];
        for tid in 0..nt {
            for k in 0..16 {
                st[k] += cx.t(tid).st[k];
            }
        }
        eprintln!("[v3b] st: {:?}", st);
        if unsafe { pm3::ON } {
            let mut s = String::new();
            for (k, n) in names.iter().enumerate() {
                let mut v = [0u64; 3];
                for tid in 0..nt {
                    for e in 0..3 {
                        v[e] += unsafe { (*b3(tid)).pm[k][e] };
                    }
                }
                if v[0] != 0 {
                    s.push_str(&format!("{}={}Mi/{:.2}Mbm/{}Mc ", n, v[0] / 1_000_000, v[1] as f64 / 1e6, v[2] / 1_000_000));
                }
            }
            eprintln!("[v3b] pm: {s}");
        }
        if unsafe { PF } {
            eprintln!("[v3b] pf persons by clicks (0, <=8, <=16, <=32, <=64, <=1024, more): [persons, clicks, sessions] {:?}", unsafe { HIST });
            let m = |k: usize| st[k] / 1_000_000;
            eprintln!("[v3b] pf Mcycles: blocks={} bins={} name={} scan={} purch={} organic={} credit16={} credit64={} big={} | persons={} sessions={} clicks={} ties={}", m(12), m(13), m(14), m(15), m(8), m(9), m(6), m(10), m(5), st[0], st[1], st[2], st[11]);
        }
        eprintln!("[v3b] rows: events={} clicks={} links={} kept={} user_rows={rows} odd={} p1={np1} p2={np2}", c[0], c[1], c[2], c[3], cx.odds().len());
    }
}
