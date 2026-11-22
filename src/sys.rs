#![allow(dead_code)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

extern "C" {
    pub fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    #[cfg(not(feature = "sp"))]
    pub fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    #[cfg(feature = "sp")]
    #[link_name = "madvise"]
    fn madvise_real(addr: *mut u8, len: usize, advice: i32) -> i32;
    pub fn _exit(code: i32) -> !;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> i32;
    static environ: *const *const c_char;
}
use std::ffi::{c_char, CString};
pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;
pub const MAP_PRIVATE: i32 = 2;
pub const MAP_FIXED: i32 = 0x10;
pub const MAP_ANONYMOUS: i32 = 0x20;
pub const MAP_NORESERVE: i32 = 0x4000;
pub const MADV_DONTNEED: i32 = 4;
pub const MADV_HUGEPAGE: i32 = 14;
pub const MADV_NOHUGEPAGE: i32 = 15;
const HP: usize = 2 << 20;

pub static HUGE: AtomicBool = AtomicBool::new(true);
pub static SMALLPAGES: AtomicBool = AtomicBool::new(false);

pub static FD_ALL: AtomicBool = AtomicBool::new(false);

pub fn fd_reserve() {
    extern "C" {
        fn dup2(a: i32, b: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    if std::env::var("XL_FDALL").is_ok_and(|v| v == "0") {
        return;
    }
    unsafe {
        if dup2(2, 1000) == 1000 {
            close(1000);
            FD_ALL.store(true, Ordering::Relaxed);
        }
    }
}

pub fn minflt(who: i32) -> i64 {
    extern "C" {
        fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
    }
    let mut ru = [0i64; 18];
    unsafe { getrusage(who, &mut ru) };
    ru[8]
}

pub fn detect_pages() {
    let never = std::fs::read("/sys/kernel/mm/transparent_hugepage/enabled").map_or(false, |s| s.windows(7).any(|w| w == b"[never]"));
    SMALLPAGES.store(!HUGE.load(Ordering::Relaxed) || never, Ordering::Relaxed);
}

pub unsafe fn release(p: *const u8, bytes: usize) {
    let a = (p as usize + 4095) & !4095;
    let b = (p as usize + bytes) & !4095;
    if b > a {
        madvise(a as *mut u8, b - a, MADV_DONTNEED);
    }
}

#[cfg(xm)]
pub static mut XM: [u64; 64] = [0; 64];
#[cfg(xm)]
static mut XM_PAGE: usize = 0;
#[cfg(xm)]
pub static mut XT: [u64; 128] = [0; 128];
#[inline(always)]
pub fn xt(_i: usize, _end: bool) {
    #[cfg(xm)]
    unsafe {
        if _i < 64 {
            XT[2 * _i + _end as usize] = std::arch::x86_64::_rdtsc()
        }
    }
}
#[inline(always)]
pub fn xm(_i: usize) {
    #[cfg(xm)]
    unsafe {
        XM[_i] = std::arch::x86_64::_rdtsc()
    }
}
pub fn xm_page() {
    #[cfg(xm)]
    unsafe {
        let p = mmap(std::ptr::null_mut(), 4096, 3, 1, 9, 0);
        if (p as isize) > 4095 {
            XM_PAGE = p as usize;
        }
    }
}
#[inline(always)]
pub fn xm_end() {
    #[cfg(xm)]
    unsafe {
        let t = std::arch::x86_64::_rdtsc();
        if XM_PAGE != 0 {
            let p = XM_PAGE as *mut u64;
            for i in 0..63 {
                p.add(8 + i).write_volatile(XM[i]);
            }
            for i in 0..128 {
                p.add(128 + i).write_volatile(XT[i]);
            }
            p.add(8 + 63).write_volatile(t);
        }
    }
}

pub fn exit_release(nt: usize, fds: &[i32]) {
    extern "C" {
        fn close(fd: i32) -> i32;
    }
    if nt < 2 || POOL_N.load(Ordering::SeqCst) + 1 < nt {
        return;
    }
    let mut fd = [-1i32; 2048];
    let nf = fds.len().min(fd.len());
    fd[..nf].copy_from_slice(&fds[..nf]);
    let base = A_BASE.load(Ordering::Relaxed);
    let zend = Z_END.load(Ordering::Relaxed);
    let (z0, z1) = if base != 0 && zend != 0 { (zend - Z_SIZE, (Z_NEXT.load(Ordering::Relaxed) + 4095) & !4095) } else { (0, 0) };
    let (h0, h1) = if base != 0 { (if zend != 0 { zend } else { base }, (A_NEXT.load(Ordering::Relaxed) + HP - 1) & !(HP - 1)) } else { (0, 0) };
    let (fd, nh, nz) = (&fd, h1.saturating_sub(h0) / HP, z1.saturating_sub(z0) / 4096);
    let kt = std::env::var("XL_XRELT").ok().and_then(|v| v.trim().parse::<usize>().ok()).map_or(nt, |k| k.clamp(1, nt));
    let job = |tid: usize| unsafe {
        for &x in &fd[nf * tid / nt..nf * (tid + 1) / nt] {
            if x >= 0 {
                close(x);
            }
        }
        if tid >= kt {
            return;
        }
        let (x, y) = (h0 + nh * tid / kt * HP, h0 + nh * (tid + 1) / kt * HP);
        if y > x {
            madvise(x as *mut u8, y - x, MADV_DONTNEED);
        }
        let (x, y) = (z0 + nz * tid / kt * 4096, z0 + nz * (tid + 1) / kt * 4096);
        if y > x {
            madvise(x as *mut u8, y - x, MADV_DONTNEED);
        }
    };
    let n = POOL_N.load(Ordering::SeqCst);
    unsafe {
        let r: &(dyn Fn(usize) + Sync) = &job;
        JOB = Some(std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(r));
        JOB_NT = nt;
    }
    DONE.store(0, Ordering::SeqCst);
    GEN.fetch_add(1, Ordering::SeqCst);
    if SLEEPERS.load(Ordering::SeqCst) != 0 {
        unsafe { syscall(SYS_FUTEX, GEN.as_ptr(), FUTEX_WAKE_PRIVATE, i32::MAX as i64, 0usize, 0usize, 0usize) };
    }
    let t0 = rdtsc();
    job(0);
    let t1 = rdtsc();
    let bound = 3 * (t1 - t0) + 200_000;
    while DONE.load(Ordering::Acquire) != n && rdtsc() - t1 < bound {
        std::hint::spin_loop();
    }
}

pub fn exit_release_pieces(nt: usize, fds: &[i32]) {
    extern "C" {
        fn close(fd: i32) -> i32;
    }
    if nt < 2 || POOL_N.load(Ordering::SeqCst) + 1 < nt {
        return;
    }
    let mut fd = [-1i32; 2048];
    let nf = fds.len().min(fd.len());
    fd[..nf].copy_from_slice(&fds[..nf]);
    let base = A_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let zend = Z_END.load(Ordering::Relaxed);
    let (z0, z1) = if zend != 0 { (zend - Z_SIZE, (Z_NEXT.load(Ordering::Relaxed) + 4095) & !4095) } else { (0, 0) };
    let (h0, h1) = (if zend != 0 { zend } else { base }, (A_NEXT.load(Ordering::Relaxed) + HP - 1) & !(HP - 1));
    let piece = ((h1.saturating_sub(h0) / (4 * nt)).max(32 << 20) + HP - 1) & !(HP - 1);
    let np = h1.saturating_sub(h0).div_ceil(piece);
    let next = AtomicUsize::new(0);
    let (fd, next) = (&fd, &next);
    let job = |tid: usize| unsafe {
        for &x in &fd[nf * tid / nt..nf * (tid + 1) / nt] {
            if x >= 0 {
                close(x);
            }
        }
        if tid == 0 && z1 > z0 {
            madvise(z0 as *mut u8, z1 - z0, MADV_DONTNEED);
        }
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= np {
                break;
            }
            let (x, y) = (h0 + i * piece, (h0 + (i + 1) * piece).min(h1));
            madvise(x as *mut u8, y - x, MADV_DONTNEED);
        }
    };
    let n = POOL_N.load(Ordering::SeqCst);
    unsafe {
        let r: &(dyn Fn(usize) + Sync) = &job;
        JOB = Some(std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(r));
        JOB_NT = nt;
    }
    DONE.store(0, Ordering::SeqCst);
    GEN.fetch_add(1, Ordering::SeqCst);
    if SLEEPERS.load(Ordering::SeqCst) != 0 {
        unsafe { syscall(SYS_FUTEX, GEN.as_ptr(), FUTEX_WAKE_PRIVATE, i32::MAX as i64, 0usize, 0usize, 0usize) };
    }
    let t0 = rdtsc();
    job(0);
    let t1 = rdtsc();
    let bound = 3 * (t1 - t0) + 200_000;
    while DONE.load(Ordering::Acquire) != n && rdtsc() - t1 < bound {
        std::hint::spin_loop();
    }
}

pub unsafe fn release_heap_part(tid: usize, nt: usize) {
    let base = A_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let used = (A_NEXT.load(Ordering::Relaxed) - base) / HP + 1;
    let (a, b) = (used * tid / nt * HP, used * (tid + 1) / nt * HP);
    release((base + a) as *const u8, b - a);
}
pub static RELEASE: AtomicBool = AtomicBool::new(true);
pub static ANON_REGION: AtomicBool = AtomicBool::new(true);
pub static ANON_TINY: AtomicBool = AtomicBool::new(false);
pub static PREFAULT: AtomicBool = AtomicBool::new(false);
pub static FIRST_SMALL: AtomicBool = AtomicBool::new(false);
pub static PIN_AT_START: AtomicBool = AtomicBool::new(false);
pub static PINNED: [AtomicBool; 16] = [const { AtomicBool::new(false) }; 16];
pub static SPIN_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100_000);

pub fn pin_to(cpu: usize) -> bool {
    let mut m = [0u64; 16];
    if cpu >= 1024 {
        return false;
    }
    m[cpu / 64] |= 1 << (cpu % 64);
    unsafe { syscall(203, 0i32, 128usize, m.as_ptr()) == 0 }
}

pub static WORKERS: AtomicBool = AtomicBool::new(false);

struct ExecInfo {
    argv0: CString,
    _args: Vec<CString>,
    ptrs: Vec<*const c_char>,
    data: String,
    out: String,
    ncpu: usize,
}
static mut EXEC: Option<ExecInfo> = None;

pub fn exec_init(args: &[String], data: &str, out: &str, ncpu: usize) {
    let cs = |s: &str| CString::new(s).unwrap_or_else(|_| CString::new("pipeline").unwrap());
    let argv0 = cs(args.first().map_or("pipeline", |s| s.as_str()));
    let mut a = vec![argv0.clone(), cs("--generic")];
    if args.len() >= 3 {
        a.push(cs(data));
        a.push(cs(out));
    }
    let mut ptrs: Vec<*const c_char> = a.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe {
        EXEC = Some(ExecInfo { argv0, _args: a, ptrs, data: data.to_string(), out: out.to_string(), ncpu });
    }
}

#[cold]
#[inline(never)]
pub fn fallback(why: &str) -> ! {
    eprintln!("[v2b] FALLBACK: {why}");
    unsafe {
        #[allow(static_mut_refs)]
        if let Some(e) = EXEC.as_ref() {
            execve(c"/proc/self/exe".as_ptr(), e.ptrs.as_ptr(), environ);
            execve(e.argv0.as_ptr(), e.ptrs.as_ptr(), environ);
            eprintln!("[v2b] exec failed");
            if !WORKERS.load(Ordering::SeqCst) && std::thread::current().name() == Some("main") {
                crate::run_generic(&e.data, &e.out, e.ncpu);
            }
        }
        _exit(3)
    }
}

extern "C" fn on_fault(_sig: i32) {
    extern "C" {
        fn write(fd: i32, buf: *const core::ffi::c_void, n: usize) -> isize;
    }
    unsafe {
        let msg = b"[v2b] FALLBACK: fault signal\n";
        write(2, msg.as_ptr() as *const core::ffi::c_void, msg.len());
        #[allow(static_mut_refs)]
        if let Some(e) = EXEC.as_ref() {
            execve(c"/proc/self/exe".as_ptr(), e.ptrs.as_ptr(), environ);
            execve(e.argv0.as_ptr(), e.ptrs.as_ptr(), environ);
        }
        _exit(3)
    }
}
pub fn fault_fallback_init() {
    #[repr(C)]
    struct SigAction {
        handler: usize,
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    extern "C" {
        fn sigaction(sig: i32, act: *const SigAction, old: *mut SigAction) -> i32;
    }
    const SA_ONSTACK: i32 = 0x0800_0000;
    const SA_NODEFER: i32 = 0x4000_0000;
    let act = SigAction { handler: on_fault as *const () as usize, mask: [0; 16], flags: SA_ONSTACK | SA_NODEFER, restorer: 0 };
    for sig in [4, 6, 7, 8, 11] {
        unsafe { sigaction(sig, &act, std::ptr::null_mut()) };
    }
}

#[cfg_attr(feature = "tl", track_caller)]
pub fn anon(len: usize) -> *mut u8 {
    if ANON_TINY.load(Ordering::Relaxed) && len <= 4096 && A_BASE.load(Ordering::Relaxed) != 0 {
        let p = a_region(4096, 4096);
        if p != 0 {
            #[cfg(feature = "tl")]
            mp::add(p, 4096, 0, 4);
            return p as *mut u8;
        }
    }
    let len = (len + HP - 1) & !(HP - 1);
    if ANON_REGION.load(Ordering::Relaxed) && A_BASE.load(Ordering::Relaxed) != 0 {
        let p = a_region(len, HP);
        if p != 0 {
            #[cfg(feature = "tl")]
            mp::add(p, len, std::panic::Location::caller() as *const _ as usize, 0);
            return p as *mut u8;
        }
    }
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + HP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if p as isize == -1 {
            fallback("mmap failed");
        }
        let a = ((p as usize + HP - 1) & !(HP - 1)) as *mut u8;
        madvise(a, len, if HUGE.load(Ordering::Relaxed) { MADV_HUGEPAGE } else { MADV_NOHUGEPAGE });
        a
    }
}

#[cfg_attr(feature = "tl", track_caller)]
pub fn anon_pack(len: usize) -> *mut u8 {
    if len < HP && ANON_REGION.load(Ordering::Relaxed) && A_BASE.load(Ordering::Relaxed) != 0 {
        let len = (len + 4095) & !4095;
        let p = a_region(len, 4096);
        if p != 0 {
            #[cfg(feature = "tl")]
            mp::add(p, len, std::panic::Location::caller() as *const _ as usize, 0);
            return p as *mut u8;
        }
    }
    anon(len)
}

#[cfg_attr(feature = "tl", track_caller)]
pub fn anon_small(len: usize) -> *mut u8 {
    let len = (len + HP - 1) & !(HP - 1);
    let p = z_take(len, HP);
    if p != 0 {
        #[cfg(feature = "tl")]
        mp::add(p, len, std::panic::Location::caller() as *const _ as usize, 0);
        return p as *mut u8;
    }
    let p = anon(len);
    unsafe { madvise(p, len, MADV_NOHUGEPAGE) };
    p
}

const R3_DEF: bool = crate::env_u32(option_env!("XL_R3_DEF"), 1) != 0;
static R3: AtomicBool = AtomicBool::new(R3_DEF);
pub fn r3_on() -> bool {
    R3.load(Ordering::Relaxed)
}
static Z_NEXT: AtomicUsize = AtomicUsize::new(0);
static Z_END: AtomicUsize = AtomicUsize::new(0);
const Z_SIZE: usize = 1 << 30;
fn z_take(sz: usize, align: usize) -> usize {
    let end = Z_END.load(Ordering::Relaxed);
    if end == 0 {
        return 0;
    }
    let mut cur = Z_NEXT.load(Ordering::Relaxed);
    loop {
        let a = (cur + align - 1) & !(align - 1);
        if a + sz > end {
            return 0;
        }
        match Z_NEXT.compare_exchange_weak(cur, a + sz, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return a,
            Err(x) => cur = x,
        }
    }
}

pub struct Arena {
    base: *mut u8,
    cap: usize,
    next: AtomicUsize,
}
unsafe impl Sync for Arena {}
unsafe impl Send for Arena {}
impl Arena {
    #[cfg_attr(feature = "tl", track_caller)]
    pub fn new(cap: usize) -> Arena {
        Arena { base: anon(cap), cap, next: AtomicUsize::new(0) }
    }
    #[inline]
    pub fn alloc(&self, n: usize) -> *mut u8 {
        let off = self.next.fetch_add(n, Ordering::Relaxed);
        if off + n > self.cap {
            fallback("arena full");
        }
        unsafe { self.base.add(off) }
    }
    pub fn used(&self) -> usize {
        self.next.load(Ordering::Relaxed)
    }
    pub fn span(&self) -> (*mut u8, usize) {
        (self.base, self.cap)
    }
    pub fn release_part(&self, tid: usize, nt: usize) {
        if !RELEASE.load(Ordering::Relaxed) {
            return;
        }
        let used = self.used().min(self.cap) / HP;
        let (a, mut b) = (used * tid / nt * HP, used * (tid + 1) / nt * HP);
        let div = DONATE_DIV.load(Ordering::Relaxed);
        if div != 0 && b > a {
            let keep = ((b - a) / div + 2 * HP).min(b - a) & !(HP - 1);
            donate(self.base as usize + a, keep);
            #[cfg(feature = "tl")]
            mp::add(self.base as usize + a, keep, 0, 5);
            b = b.max(a + keep);
            let a2 = a + keep;
            if b > a2 {
                unsafe { madvise(self.base.add(a2), b - a2, MADV_DONTNEED) };
            }
            return;
        }
        if b > a {
            unsafe { madvise(self.base.add(a), b - a, MADV_DONTNEED) };
        }
    }
}

pub struct AVec<T> {
    p: *mut T,
    len: usize,
    cap: usize,
}
unsafe impl<T> Send for AVec<T> {}
impl<T: Copy> AVec<T> {
    #[cfg_attr(feature = "tl", track_caller)]
    pub fn new(cap: usize) -> AVec<T> {
        let cap = cap.max(16);
        AVec { p: anon(cap * std::mem::size_of::<T>()) as *mut T, len: 0, cap }
    }
    pub const fn empty() -> AVec<T> {
        AVec { p: std::ptr::NonNull::dangling().as_ptr(), len: 0, cap: 0 }
    }
    #[inline(always)]
    pub fn push(&mut self, v: T) {
        if self.len >= self.cap {
            fallback("avec full");
        }
        unsafe { self.p.add(self.len).write(v) };
        self.len += 1;
    }
    #[inline(always)]
    pub fn extend(&mut self, s: &[T]) {
        if self.len + s.len() > self.cap {
            fallback("avec full");
        }
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), self.p.add(self.len), s.len()) };
        self.len += s.len();
    }
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    pub fn as_slice(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.p, self.len) }
    }
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.p
    }
    #[inline(always)]
    pub fn as_ptr(&self) -> *const T {
        self.p
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    pub fn release(&self) {
        unsafe { release(self.p as *const u8, self.len * std::mem::size_of::<T>()) };
    }
    pub fn truncate(&mut self, n: usize) {
        self.len = n;
    }
}

static GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static SLEEPERS: AtomicUsize = AtomicUsize::new(0);
static POOL_N: AtomicUsize = AtomicUsize::new(0);
static mut JOB: Option<&'static (dyn Fn(usize) + Sync)> = None;
static mut JOB_NT: usize = 0;

extern "C" {
    fn syscall(n: i64, ...) -> i64;
}
const SYS_FUTEX: i64 = 202;
const FUTEX_WAIT_PRIVATE: i64 = 128;
const FUTEX_WAKE_PRIVATE: i64 = 129;

#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { std::arch::x86_64::_rdtsc() }
}

pub static CORE_NOTOUCH: AtomicBool = AtomicBool::new(true);
pub static SETUP_KP: AtomicUsize = AtomicUsize::new(0);
pub static SETUP_PARTS: AtomicBool = AtomicBool::new(true);
pub fn pool_start(nt: usize) {
    if nt < 2 || POOL_N.load(Ordering::SeqCst) != 0 {
        return;
    }
    WORKERS.store(true, Ordering::SeqCst);
    POOL_N.store(nt - 1, Ordering::SeqCst);
    spawn_small(move || worker(1, nt));
}

pub const STACK: usize = (2 << 20) - (64 << 10);
fn spawn_small<F: FnOnce() + Send + 'static>(f: F) {
    if std::thread::Builder::new().stack_size(STACK).spawn(f).is_err() {
        fallback("thread spawn");
    }
}

fn worker(tid: usize, nt: usize) -> ! {
    for c in [2 * tid, 2 * tid + 1] {
        if c < nt {
            spawn_small(move || worker(c, nt));
        }
    }
    #[cfg(feature = "tl")]
    tl::mark(tid, 10);
    if PIN_AT_START.load(Ordering::Relaxed) && tid < 16 {
        let order = crate::parse::cpu_order();
        if !order.is_empty() && pin_to(order[tid % order.len()]) {
            PINNED[tid].store(true, Ordering::Release);
        }
    }
    if PREFAULT.load(Ordering::Relaxed) {
        prefault_chunk();
    }
    #[cfg(feature = "tl")]
    tl::mark(tid, 11);
    let spin = SPIN_TICKS.load(Ordering::Relaxed);
    let mut seen = 0u32;
    loop {
        let t0 = rdtsc();
        loop {
            let g = GEN.load(Ordering::Acquire);
            if g != seen {
                seen = g;
                break;
            }
            if rdtsc() - t0 < spin {
                std::hint::spin_loop();
                continue;
            }
            SLEEPERS.fetch_add(1, Ordering::SeqCst);
            unsafe { syscall(SYS_FUTEX, GEN.as_ptr(), FUTEX_WAIT_PRIVATE, g, 0usize, 0usize, 0usize) };
            SLEEPERS.fetch_sub(1, Ordering::SeqCst);
        }
        unsafe {
            #[allow(static_mut_refs)]
            if let Some(f) = JOB {
                if tid < JOB_NT {
                    #[cfg(feature = "tl")]
                    let c = tl::CUR;
                    #[cfg(feature = "tl")]
                    {
                        tl::ST[c][tid] = tl::now();
                    }
                    f(tid);
                    #[cfg(feature = "tl")]
                    {
                        tl::EN[c][tid] = tl::now();
                    }
                }
            }
        }
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

pub fn pool_size() -> usize {
    POOL_N.load(Ordering::SeqCst)
}

pub fn run_par<F: Fn(usize) + Sync>(nt: usize, f: F) {
    WORKERS.store(true, Ordering::SeqCst);
    let n = POOL_N.load(Ordering::SeqCst);
    if n == 0 || nt > n + 1 {
        std::thread::scope(|s| {
            for t in 1..nt {
                let f = &f;
                s.spawn(move || f(t));
            }
            f(0);
        });
        return;
    }
    unsafe {
        let r: &(dyn Fn(usize) + Sync) = &f;
        JOB = Some(std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(r));
        JOB_NT = nt;
    }
    #[cfg(feature = "tl")]
    let c = unsafe {
        let c = tl::CALL.min(tl::MAXC - 1);
        tl::CALL = c + 1;
        tl::CUR = c;
        tl::NTH[c] = nt;
        tl::POST[c] = tl::now();
        c
    };
    DONE.store(0, Ordering::SeqCst);
    GEN.fetch_add(1, Ordering::SeqCst);
    if SLEEPERS.load(Ordering::SeqCst) != 0 {
        unsafe { syscall(SYS_FUTEX, GEN.as_ptr(), FUTEX_WAKE_PRIVATE, i32::MAX as i64, 0usize, 0usize, 0usize) };
    }
    #[cfg(feature = "tl")]
    unsafe {
        tl::ST[c][0] = tl::now();
    }
    f(0);
    #[cfg(feature = "tl")]
    unsafe {
        tl::EN[c][0] = tl::now();
    }
    while DONE.load(Ordering::Acquire) != n {
        std::hint::spin_loop();
    }
    #[cfg(feature = "tl")]
    unsafe {
        tl::DONE_T[c] = tl::now();
    }
    unsafe { JOB = None };
}

#[cfg(feature = "tl")]
pub mod tl {
    pub const MAXC: usize = 64;
    pub static mut CALL: usize = 0;
    pub static mut CUR: usize = 0;
    pub static mut POST: [u64; MAXC] = [0; MAXC];
    pub static mut DONE_T: [u64; MAXC] = [0; MAXC];
    pub static mut NTH: [usize; MAXC] = [0; MAXC];
    pub static mut ST: [[u64; 16]; MAXC] = [[0; 16]; MAXC];
    pub static mut EN: [[u64; 16]; MAXC] = [[0; 16]; MAXC];
    pub static mut MK: [[u64; 12]; 16] = [[0; 12]; 16];
    pub static mut LAPT: Vec<(&'static str, u64)> = Vec::new();
    pub static mut T0: u64 = 0;
    pub static mut T0_NS: u64 = 0;
    pub static mut REL: u64 = 0;
    pub static WR_IO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static WR_OPEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static WR_CLOSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static WR_COL: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];
    pub static mut TASK: [(u64, u64, u64); 64] = [(0, 0, 0); 64];
    pub static mut SH: [(u64, u64, u64); 64] = [(0, 0, 0); 64];
    pub static mut STASK: [(u64, u64); 64] = [(0, 0); 64];
    pub static mut CHAIN_END: u64 = 0;
    #[inline(always)]
    pub fn now() -> u64 {
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[inline(always)]
    pub fn mark(tid: usize, k: usize) {
        unsafe {
            if tid < 16 {
                MK[tid][k] = now();
            }
        }
    }
    pub fn realtime_ns() -> u64 {
        let mut t = [0i64; 2];
        extern "C" {
            fn clock_gettime(clk: i32, ts: *mut i64) -> i32;
        }
        unsafe { clock_gettime(0, t.as_mut_ptr()) };
        t[0] as u64 * 1_000_000_000 + t[1] as u64
    }
    fn exit_prof(tpm: f64) {
        extern "C" {
            fn munmap(p: *mut u8, n: usize) -> i32;
        }
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let ab = super::A_BASE.load(std::sync::atomic::Ordering::Relaxed);
        let mut v: Vec<(usize, usize, u8)> = Vec::with_capacity(4096);
        for l in maps.lines() {
            let mut it = l.split_whitespace();
            let r = it.next().unwrap_or("");
            let perm = it.next().unwrap_or("");
            let _off = it.next();
            let _dev = it.next();
            let ino = it.next().unwrap_or("0");
            let name = it.next().unwrap_or("");
            let mut rr = r.split('-');
            let a = usize::from_str_radix(rr.next().unwrap_or("0"), 16).unwrap_or(0);
            let b = usize::from_str_radix(rr.next().unwrap_or("0"), 16).unwrap_or(0);
            if name.starts_with('[') || name.contains("pipeline") || name.contains("s_tl") || (ab != 0 && a <= ab && ab < b) {
                continue;
            }
            let kind = if ino != "0" { 0 } else if perm.starts_with("---") { 2 } else { 1 };
            v.push((a, b, kind));
        }
        let mut tk = [0u64; 3];
        let mut nk = [0usize; 3];
        let mut sz = [0usize; 3];
        for &(a, b, k) in &v {
            let t = now();
            unsafe { munmap(a as *mut u8, b - a) };
            tk[k as usize] += now() - t;
            nk[k as usize] += 1;
            sz[k as usize] += b - a;
        }
        eprintln!("[tl] exitprof: file {} vmas {:.0} MB {:.3} ms | anon {} vmas {:.0} MB {:.3} ms | none {} vmas {:.3} ms", nk[0], sz[0] as f64 / 1e6, tk[0] as f64 / tpm, nk[1], sz[1] as f64 / 1e6, tk[1] as f64 / tpm, nk[2], tk[2] as f64 / tpm);
    }
    #[allow(static_mut_refs)]
    pub fn report(t0: std::time::Instant) {
        unsafe {
            let tn = now();
            let el = t0.elapsed().as_secs_f64() * 1e3;
            let tpm = (tn - T0) as f64 / el;
            let ms = |a: u64, b: u64| (b as f64 - a as f64) / tpm;
            eprintln!("[tl] realtime_ns start={} end={} endrel_ms={:.3}", T0_NS, realtime_ns(), REL as f64 / tpm);
            let mut prev = T0;
            let mut c = 0usize;
            let mut tot_idle = 0.0;
            for &(name, t) in LAPT.iter() {
                let lw = ms(prev, t);
                let mut par = 0.0;
                let mut s = String::new();
                while c < CALL && POST[c] < t {
                    let n = NTH[c];
                    let w = ms(POST[c], DONE_T[c]);
                    let mut busy = 0.0;
                    let (mut lag, mut mine, mut maxe) = (0.0f64, f64::MAX, 0.0f64);
                    for k in 0..n {
                        busy += ms(ST[c][k], EN[c][k]);
                        lag = lag.max(ms(POST[c], ST[c][k]));
                        mine = mine.min(ms(POST[c], EN[c][k]));
                        maxe = maxe.max(ms(POST[c], EN[c][k]));
                    }
                    let idle = w * n as f64 - busy;
                    tot_idle += idle;
                    s.push_str(&format!(" | job{c}: wall {w:.2} busy {busy:.1} idle {idle:.2} ({:.0}%) wake_lag {lag:.3} first_end {mine:.2} last_end {maxe:.2} done {:.3}", 100.0 * idle / (w * n as f64), w - maxe));
                    par += w;
                    c += 1;
                }
                let ser = lw - par;
                eprintln!("[tl] {name:>10} wall {lw:6.2} serial {ser:5.2}{s}");
                prev = t;
            }
            eprintln!("[tl] total {el:.2} ms; idle in jobs {tot_idle:.1} thread-ms");
            let o = std::sync::atomic::Ordering::Relaxed;
            eprintln!("[tl] write syscalls (thread-ms): create {:.2} write {:.2} close {:.2}", WR_OPEN.load(o) as f64 / tpm, WR_IO.load(o) as f64 / tpm, WR_CLOSE.load(o) as f64 / tpm);
            eprintln!("[tl] write columns (thread-ms, incl. flushes inside): {:?} plan {:.2}", (0..7).map(|j| format!("{:.2}", WR_COL[j].load(o) as f64 / tpm)).collect::<Vec<_>>(), WR_COL[7].load(o) as f64 / tpm);
            let mut s = String::new();
            for t in 0..15 {
                if MK[t][0] == 0 {
                    continue;
                }
                let b = POST[0];
                s.push_str(&format!("\n[tl] parse t{t}: start {:.3} pin {:.3} parts {:.3} core {:.3} tabs {:.3} init {:.3} c1 {:.3}..{:.3} qempty {:.3} fin {:.3}", ms(b, MK[t][0]), ms(b, MK[t][6]), ms(b, MK[t][7]), ms(b, MK[t][8]), ms(b, MK[t][9]), ms(b, MK[t][1]), ms(b, MK[t][2]), ms(b, MK[t][3]), ms(b, MK[t][4]), ms(b, MK[t][5])));
            }
            eprintln!("[tl] parse marks (ms after post){s}");
            let mut s = String::new();
            for t in 1..15 {
                if MK[t][10] != 0 {
                    s.push_str(&format!(" t{t} {:.3}/{:.3}", ms(T0, MK[t][10]), ms(T0, MK[t][11])));
                }
            }
            eprintln!("[tl] worker start/ready (ms after t0):{s}");
            let mut s = String::new();
            for (i, &(a, b, th)) in TASK.iter().enumerate() {
                if a != 0 {
                    s.push_str(&format!(" {i}:t{th} {:.3}-{:.3}", ms(T0, a), ms(T0, b)));
                }
            }
            eprintln!("[tl] setup tasks (ms after t0):{s}");
            let mut s = String::new();
            for (i, &(a, b, sh)) in SH.iter().enumerate() {
                if a != 0 && ms(a, b) > 0.05 {
                    s.push_str(&format!(" {i}:s{sh} {:.3}+{:.3}", ms(T0, a), ms(a, b)));
                }
            }
            eprintln!("[tl] shards over 0.05 ms (start+dur):{s}");
            let mut s = String::new();
            for (i, &(a, b)) in STASK.iter().enumerate() {
                if a != 0 {
                    s.push_str(&format!(" {i}:{:.3}-{:.3}", ms(T0, a), ms(T0, b)));
                }
            }
            eprintln!("[tl] u8 setup queue (ms after t0; chain end {:.3}, post0 {:.3}, post1 {:.3}):{s}", ms(T0, CHAIN_END), ms(T0, POST[0]), ms(T0, POST[1]));
            eprintln!("[tl] campaign: agg {:.3} cols {:.3} file {:.3}", ms(TASK[8].0, MK[15][4]), ms(MK[15][4], MK[15][5]), ms(MK[15][5], MK[15][6]));
            eprintln!("[tl] lookups (ms after t0): start {:.3} prod read {:.3} prod parsed {:.3} done {:.3}", ms(T0, MK[15][0]), ms(T0, MK[15][1]), ms(T0, MK[15][2]), ms(T0, MK[15][3]));
            if std::env::var("XL_EXITPROF").is_ok() {
                exit_prof(tpm);
            }
        }
    }
}

pub struct U64Map {
    slots: Vec<[u64; 2]>,
    mask: usize,
    len: usize,
}
impl U64Map {
    pub fn new() -> U64Map {
        U64Map::with_capacity(0)
    }
    pub fn cap(&self) -> usize {
        self.slots.len()
    }
    pub fn with_capacity(n: usize) -> U64Map {
        let m = (2 * n).next_power_of_two().max(1024);
        U64Map { slots: vec![[0; 2]; m], mask: m - 1, len: 0 }
    }
    #[inline(always)]
    fn h(k: u64, mask: usize) -> usize {
        (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20) as usize & mask
    }
    #[inline]
    pub fn get(&self, k: u64) -> Option<u32> {
        let k1 = k + 1;
        let mut i = Self::h(k, self.mask);
        loop {
            let s = unsafe { *self.slots.get_unchecked(i) };
            if s[0] == k1 {
                return Some(s[1] as u32);
            }
            if s[0] == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline]
    pub fn get_or_insert(&mut self, k: u64, v: u32) -> u32 {
        if self.len * 2 >= self.slots.len() {
            self.grow();
        }
        let k1 = k + 1;
        let mut i = Self::h(k, self.mask);
        loop {
            let s = unsafe { self.slots.get_unchecked_mut(i) };
            if s[0] == k1 {
                return s[1] as u32;
            }
            if s[0] == 0 {
                *s = [k1, v as u64];
                self.len += 1;
                return v;
            }
            i = (i + 1) & self.mask;
        }
    }
    fn grow(&mut self) {
        let n = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![[0; 2]; n]);
        self.mask = n - 1;
        for s in old {
            if s[0] != 0 {
                let mut i = Self::h(s[0] - 1, self.mask);
                while self.slots[i][0] != 0 {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = s;
            }
        }
    }
}

#[repr(C)]
struct Timespec {
    s: i64,
    ns: i64,
}
extern "C" {
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
}
pub fn thread_cpu_ms() -> f64 {
    let mut t = Timespec { s: 0, ns: 0 };
    unsafe { clock_gettime(3, &mut t) };
    t.s as f64 * 1e3 + t.ns as f64 / 1e6
}

#[cfg(feature = "tl")]
pub mod mp {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    pub const MAX: usize = 16384;
    pub static mut R: [(usize, usize, usize, u32, u8, u32); MAX] = [(0, 0, 0, 0, 0, 0); MAX];
    pub static mut BT: [[usize; 8]; MAX] = [[0; 8]; MAX];
    pub static N: AtomicUsize = AtomicUsize::new(0);
    pub static LAPN: AtomicUsize = AtomicUsize::new(0);
    pub static ON: AtomicBool = AtomicBool::new(false);
    pub static LOCK: AtomicBool = AtomicBool::new(false);
    pub static DON_USED: AtomicUsize = AtomicUsize::new(0);
    static mut MC: [u8; 1 << 20] = [0; 1 << 20];
    static mut MAXRES: [usize; MAX] = [0; MAX];
    extern "C" {
        fn mincore(a: *mut u8, n: usize, v: *mut u8) -> i32;
        fn getrusage(who: i32, r: *mut i64) -> i32;
    }
    #[inline(never)]
    pub fn add(a: usize, n: usize, loc: usize, kind: u8) {
        if !ON.load(Relaxed) {
            return;
        }
        let i = N.fetch_add(1, Relaxed);
        if i < MAX {
            unsafe {
                R[i] = (a, n, loc, LAPN.load(Relaxed) as u32, kind, 0);
                let mut fp: usize;
                std::arch::asm!("mov {}, rbp", out(reg) fp);
                let sp: usize;
                std::arch::asm!("mov {}, rsp", out(reg) sp);
                for k in 0..8 {
                    if fp < sp || fp > sp + (64 << 20) || fp & 7 != 0 {
                        break;
                    }
                    BT[i][k] = *((fp + 8) as *const usize);
                    let nf = *(fp as *const usize);
                    if nf <= fp {
                        break;
                    }
                    fp = nf;
                }
            }
        }
    }
    #[inline(never)]
    pub fn free(a: usize) {
        if !ON.load(Relaxed) {
            return;
        }
        while LOCK.swap(true, std::sync::atomic::Ordering::Acquire) {}
        let n = N.load(Relaxed).min(MAX);
        unsafe {
            for i in (0..n).rev() {
                if R[i].0 == a && (R[i].4 == 2 || R[i].4 == 3) && R[i].5 == 0 {
                    R[i].5 = LAPN.load(Relaxed) as u32 + 1;
                    break;
                }
            }
        }
        LOCK.store(false, std::sync::atomic::Ordering::Release);
    }
    pub fn resident(a: usize, n: usize) -> usize {
        let s = a & !4095;
        let e = (a + n + 4095) & !4095;
        let mut p = s;
        let mut r = 0;
        while p < e {
            let k = ((e - p) / 4096).min(1 << 20);
            unsafe {
                if mincore(p as *mut u8, k * 4096, MC.as_mut_ptr()) != 0 {
                    for i in 0..k {
                        if mincore((p + i * 4096) as *mut u8, 4096, MC.as_mut_ptr()) == 0 {
                            r += (MC[0] & 1) as usize;
                        }
                    }
                } else {
                    for i in 0..k {
                        r += (MC[i] & 1) as usize;
                    }
                }
            }
            p += k * 4096;
        }
        r * 4096
    }
    pub fn minflt() -> i64 {
        let mut r = [0i64; 18];
        unsafe { getrusage(0, r.as_mut_ptr()) };
        r[8]
    }
    fn locstr(l: usize) -> String {
        if l == 0 {
            return "alloc".into();
        }
        let l = unsafe { &*(l as *const std::panic::Location<'static>) };
        format!("{}:{}", l.file().trim_start_matches("src/"), l.line())
    }
    #[allow(static_mut_refs)]
    pub fn report(name: &str) {
        if !ON.load(Relaxed) {
            return;
        }
        let lapn = LAPN.fetch_add(1, Relaxed);
        let n = N.load(Relaxed).min(MAX);
        let mut groups: Vec<(String, usize, usize, usize, usize)> = Vec::new();
        let mut tot = 0usize;
        unsafe {
            for i in 0..n {
                let (a, len, loc, l0, kind, fr) = R[i];
                if len == 0 {
                    continue;
                }
                let res = if kind == 3 { 0 } else { resident(a, len) };
                MAXRES[i] = MAXRES[i].max(res);
                let key = match kind {
                    1 => format!("alloc 4MB chunk @{}", l0),
                    2 | 3 => format!("alloc big{} {}KB @{}{} bt {:x?}", if kind == 3 { "(reuse)" } else { "" }, len >> 10, l0, if fr != 0 { format!(" freed@{}", fr - 1) } else { String::new() }, &BT[i][1..6]),
                    0 => format!("anon {} bt {:x?}", locstr(loc), &BT[i][1..6]),
                    5 => "donated (ev_arena part kept)".to_string(),
                    _ => "anon tiny".to_string(),
                };
                if let Some(g) = groups.iter_mut().find(|g| g.0 == key) {
                    g.1 += 1;
                    g.2 += len;
                    g.3 += res;
                    g.4 += MAXRES[i];
                } else {
                    groups.push((key, 1, len, res, MAXRES[i]));
                }
                tot += res;
            }
        }
        let fresh: usize = unsafe { (0..n).filter(|&i| R[i].4 != 3 && R[i].4 != 5).map(|i| MAXRES[i]).sum() };
        let don: usize = unsafe { (0..n).filter(|&i| R[i].4 == 5).map(|i| R[i].1).sum() };
        eprintln!("[mp] fresh so far (sum of max resident of every record): {:.1} MB; donated {:.1} MB, of it taken {:.1} MB", fresh as f64 / 1e6, don as f64 / 1e6, DON_USED.load(Relaxed) as f64 / 1e6);
        let base = super::A_BASE.load(Relaxed);
        let reg = if base != 0 { resident(base, super::A_NEXT.load(Relaxed) - base) } else { 0 };
        let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let f = |k: &str| st.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("?").to_string();
        eprintln!("[mp] === lap {lapn} {name}: RssAnon {} kB RssFile {} kB HWM {} kB minflt {} | region resident {:.1} MB, records resident {:.1} MB (overlaps possible), records {n}", f("RssAnon"), f("RssFile"), f("VmHWM"), minflt(), reg as f64 / 1e6, tot as f64 / 1e6);
        groups.sort_by(|a, b| b.4.cmp(&a.4));
        for g in &groups {
            if g.4 >= 64 << 10 {
                eprintln!("[mp]   {:<44} n={:<4} reserved {:9.1} MB resident {:8.2} MB maxres {:8.2} MB", g.0, g.1, g.2 as f64 / 1e6, g.3 as f64 / 1e6, g.4 as f64 / 1e6);
            }
        }
        let sm = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
        let mut head = String::new();
        let mut rss = 0u64;
        let mut out = Vec::new();
        for l in sm.lines() {
            let w0 = l.split_whitespace().next().unwrap_or("");
            if w0.contains('-') && !w0.ends_with(':') {
                head = l.to_string();
            } else if w0 == "Rss:" {
                rss = l.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
            } else if w0 == "AnonHugePages:" {
                let ah: u64 = l.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
                let a = usize::from_str_radix(head.split('-').next().unwrap_or("0"), 16).unwrap_or(0);
                if rss >= 256 && !(base != 0 && a <= base && base < a + (65usize << 30)) {
                    out.push(format!("{} rss {} kB huge {} kB", head.split_whitespace().take(6).collect::<Vec<_>>().join(" "), rss, ah));
                } else if rss >= 256 {
                    out.push(format!("REGION rss {} kB huge {} kB", rss, ah));
                }
            }
        }
        for o in out {
            eprintln!("[mp]   vma {o}");
        }
    }
}

pub struct Alloc;
const BIG_SLOTS: usize = 64;
static BIG_LOCK: AtomicBool = AtomicBool::new(false);
static mut BIG_N: [usize; 48] = [0; 48];
static mut BIG: [[usize; BIG_SLOTS]; 48] = [[0; BIG_SLOTS]; 48];
pub static BIG_ON: AtomicBool = AtomicBool::new(true);
#[inline(always)]
fn big_lock() {
    while BIG_LOCK.swap(true, Ordering::Acquire) {
        std::hint::spin_loop();
    }
}
#[inline(never)]
unsafe fn big_put(p: usize, c: usize) -> bool {
    if c >= 48 || !BIG_ON.load(Ordering::Relaxed) {
        return false;
    }
    big_lock();
    let n = BIG_N[c];
    let ok = n < BIG_SLOTS;
    if ok {
        BIG[c][n] = p;
        BIG_N[c] = n + 1;
    }
    BIG_LOCK.store(false, Ordering::Release);
    ok
}
#[inline(never)]
unsafe fn big_take(c: usize) -> usize {
    if c >= 48 || !BIG_ON.load(Ordering::Relaxed) {
        return 0;
    }
    big_lock();
    let mut p = 0;
    let mut k = c;
    while k < 48 && BIG_N[k] == 0 {
        k += 1;
    }
    if k < 48 {
        let room = (c..k).all(|j| BIG_N[j] < BIG_SLOTS);
        if room {
            BIG_N[k] -= 1;
            p = BIG[k][BIG_N[k]];
            for j in c..k {
                BIG[j][BIG_N[j]] = p + (1usize << j);
                BIG_N[j] += 1;
            }
        }
    }
    BIG_LOCK.store(false, Ordering::Release);
    p
}
#[cfg(feature = "tl")]
pub static CLS_N: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
#[cfg(feature = "tl")]
pub static CLS_FRESH: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
#[cfg(feature = "tl")]
pub fn cls_report(name: &str) {
    let mut s = String::new();
    for c in 0..40 {
        let (n, f) = (CLS_N[c].load(Ordering::Relaxed), CLS_FRESH[c].load(Ordering::Relaxed));
        if f > 0 {
            s.push_str(&format!("2^{c}:{n}/{f}/{:.1}MB ", (f << c) as f64 / 1048576.0));
        }
    }
    eprintln!("[pm] alloc classes after {name} (calls/fresh/fresh MB): {s}");
}
static A_BASE: AtomicUsize = AtomicUsize::new(0);
static A_END: AtomicUsize = AtomicUsize::new(0);
static A_NEXT: AtomicUsize = AtomicUsize::new(0);
const A_SMALL_MAX: usize = 16;
const A_CHUNK: usize = 4 << 20;
const A_REGION: usize = 64 << 30;

struct Tl {
    cur: usize,
    end: usize,
    free: [usize; 64],
    dirty: bool,
    don_slot: usize,
}
thread_local! {
    static TL: std::cell::UnsafeCell<Tl> = const { std::cell::UnsafeCell::new(Tl { cur: 0, end: 0, free: [0; 64], dirty: false, don_slot: 0 }) };
}

static DON: [[AtomicUsize; 2]; 16] = [const { [AtomicUsize::new(0), AtomicUsize::new(0)] }; 16];
static DON_N: AtomicUsize = AtomicUsize::new(0);

pub fn donate(p: usize, len: usize) {
    let base = A_BASE.load(Ordering::Relaxed);
    if base == 0 || p < base || p + len > A_END.load(Ordering::Relaxed) || len < A_CHUNK || p & (HP - 1) != 0 {
        return;
    }
    let s = DON_N.load(Ordering::Acquire);
    if s >= DON.len() {
        return;
    }
    let s = DON_N.fetch_add(1, Ordering::AcqRel);
    if s >= DON.len() {
        return;
    }
    DON[s][1].store(p + len, Ordering::Relaxed);
    DON[s][0].store(p, Ordering::Release);
    TL.with(|t| unsafe {
        let t = &mut *t.get();
        t.don_slot = s;
        if t.cur != 0 && !t.dirty && t.cur < t.end - A_CHUNK + HP {
            let d = a_donated(t, HP, HP);
            if d != 0 {
                #[cfg(feature = "tl")]
                mp::add(d, HP, 0, 3);
                t.cur = d;
                t.end = d + HP;
                t.dirty = true;
            }
        }
    });
}
pub static DONATE_DIV: AtomicUsize = AtomicUsize::new(DONATE_DIV_DEF);
pub const DONATE_DIV_DEF: usize = crate::env_u32(option_env!("XL_DONATE_DEF"), 16) as usize;

#[inline(always)]
fn a_donated(t: &mut Tl, sz: usize, align: usize) -> usize {
    let n = DON_N.load(Ordering::Acquire).min(DON.len());
    if n == 0 {
        return 0;
    }
    for k in 0..n {
        let s = (t.don_slot + k) % n;
        let end = DON[s][1].load(Ordering::Relaxed);
        let mut cur = DON[s][0].load(Ordering::Acquire);
        loop {
            if cur == 0 {
                break;
            }
            let a = (cur + align - 1) & !(align - 1);
            if a + sz > end {
                break;
            }
            match DON[s][0].compare_exchange_weak(cur, a + sz, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    t.don_slot = s;
                    #[cfg(feature = "tl")]
                    mp::DON_USED.fetch_add(sz, Ordering::Relaxed);
                    return a;
                }
                Err(x) => cur = x,
            }
        }
    }
    0
}

pub fn alloc_enable() {
    unsafe {
        let p = mmap(std::ptr::null_mut(), A_REGION + HP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if p as isize == -1 {
            return;
        }
        let a = (p as usize + HP - 1) & !(HP - 1);
        let huge = HUGE.load(Ordering::Relaxed);
        let r3 = std::env::var("XL_R3").map_or(R3_DEF, |v| v.trim() == "1");
        R3.store(r3, Ordering::Relaxed);
        let mut z = 0;
        if r3 && huge {
            z = Z_SIZE;
            madvise(a as *mut u8, z, MADV_NOHUGEPAGE);
            Z_NEXT.store(a, Ordering::SeqCst);
            Z_END.store(a + z, Ordering::SeqCst);
        }
        madvise((a + z) as *mut u8, A_REGION - z, if huge { MADV_HUGEPAGE } else { MADV_NOHUGEPAGE });
        A_NEXT.store(a + z, Ordering::SeqCst);
        A_END.store(a + A_REGION, Ordering::SeqCst);
        A_BASE.store(a, Ordering::SeqCst);
    }
}

pub fn prefault_chunk() {
    if A_BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let v: Vec<u64> = Vec::with_capacity(8);
    std::hint::black_box(&v);
    drop(v);
    TL.with(|t| unsafe {
        let t = &*t.get();
        let mut p = (t.cur + 4095) & !4095;
        let step = if HUGE.load(Ordering::Relaxed) { HP } else { 4096 };
        while p < t.end {
            std::ptr::write_volatile(p as *mut u8, 0);
            p = (p + step) & !(step - 1);
        }
    });
}

#[inline(always)]
fn a_class(size: usize, align: usize) -> usize {
    let s = size.max(align).max(16);
    (usize::BITS - (s - 1).leading_zeros()) as usize
}

#[inline(always)]
fn a_mine(p: *mut u8) -> bool {
    let base = A_BASE.load(Ordering::Relaxed);
    base != 0 && p as usize >= base && (p as usize) < A_END.load(Ordering::Relaxed)
}

pub fn a_base() -> usize {
    A_BASE.load(Ordering::Relaxed)
}
pub fn anon_ext(len: usize, align: usize) -> *mut u8 {
    if A_BASE.load(Ordering::Relaxed) == 0 {
        return std::ptr::null_mut();
    }
    a_region(len, align) as *mut u8
}

fn a_region(sz: usize, align: usize) -> usize {
    let end = A_END.load(Ordering::Relaxed);
    let mut cur = A_NEXT.load(Ordering::Relaxed);
    loop {
        let a = (cur + align - 1) & !(align - 1);
        if a + sz > end {
            return 0;
        }
        match A_NEXT.compare_exchange_weak(cur, a + sz, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return a,
            Err(x) => cur = x,
        }
    }
}

impl Alloc {
    #[inline(always)]
    unsafe fn get(&self, l: std::alloc::Layout, zero: bool) -> *mut u8 {
        use std::alloc::GlobalAlloc;
        if A_BASE.load(Ordering::Relaxed) == 0 || l.align() > 4096 {
            return if zero { std::alloc::System.alloc_zeroed(l) } else { std::alloc::System.alloc(l) };
        }
        let c = a_class(l.size(), l.align());
        let sz = 1usize << c;
        #[cfg(feature = "tl")]
        {
            CLS_N[c].fetch_add(1, Ordering::Relaxed);
        }
        let p = TL.with(|t| {
            let t = &mut *t.get();
            let h = t.free[c];
            #[cfg(feature = "tl")]
            if h == 0 {
                CLS_FRESH[c].fetch_add(1, Ordering::Relaxed);
            }
            if h != 0 {
                t.free[c] = *(h as *const usize);
                #[cfg(feature = "tl")]
                if c > A_SMALL_MAX {
                    mp::add(h, sz, 0, 3);
                }
                if zero {
                    std::ptr::write_bytes(h as *mut u8, 0, l.size());
                }
                return h;
            }
            if c <= A_SMALL_MAX {
                let al = sz.min(4096);
                let mut a = (t.cur + al - 1) & !(al - 1);
                if t.cur == 0 || a + sz > t.end {
                    let first = t.cur == 0;
                    let d = a_donated(t, A_CHUNK, HP);
                    if d != 0 {
                        t.cur = d;
                        t.end = d + A_CHUNK;
                        t.dirty = true;
                        #[cfg(feature = "tl")]
                        mp::add(d, A_CHUNK, 0, 3);
                        let a = (d + al - 1) & !(al - 1);
                        t.cur = a + sz;
                        if zero {
                            std::ptr::write_bytes(a as *mut u8, 0, l.size());
                        }
                        return a;
                    }
                    let small = first && FIRST_SMALL.load(Ordering::Relaxed) && HUGE.load(Ordering::Relaxed);
                    let zc = if small { z_take(A_CHUNK, A_CHUNK) } else { 0 };
                    let ch = if zc != 0 { zc } else { a_region(A_CHUNK, A_CHUNK) };
                    if ch == 0 {
                        return 0;
                    }
                    t.dirty = false;
                    #[cfg(feature = "tl")]
                    mp::add(ch, A_CHUNK, 0, 1);
                    if small && zc == 0 {
                        madvise(ch as *mut u8, A_CHUNK, MADV_NOHUGEPAGE);
                    }
                    t.cur = ch;
                    t.end = ch + A_CHUNK;
                    a = ch;
                }
                t.cur = a + sz;
                if zero && t.dirty {
                    std::ptr::write_bytes(a as *mut u8, 0, l.size());
                }
                return a;
            }
            let h = big_take(c);
            if h != 0 {
                #[cfg(feature = "tl")]
                mp::add(h, sz, 0, 3);
                #[cfg(feature = "tl")]
                CLS_FRESH[c].fetch_sub(1, Ordering::Relaxed);
                if zero {
                    std::ptr::write_bytes(h as *mut u8, 0, l.size());
                }
                return h;
            }
            if !zero {
                let d = a_donated(t, sz, if sz >= HP { HP } else { 4096 });
                if d != 0 {
                    #[cfg(feature = "tl")]
                    mp::add(d, sz, 0, 3);
                    return d;
                }
            }
            let r = a_region(sz, if sz >= HP { HP } else { 4096 });
            #[cfg(feature = "tl")]
            if r != 0 {
                mp::add(r, sz, 0, 2);
            }
            r
        });
        if p == 0 {
            return if zero { std::alloc::System.alloc_zeroed(l) } else { std::alloc::System.alloc(l) };
        }
        p as *mut u8
    }
}

unsafe impl std::alloc::GlobalAlloc for Alloc {
    #[inline]
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        self.get(l, false)
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        self.get(l, true)
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        if a_mine(p) {
            let c = a_class(l.size(), l.align());
            #[cfg(feature = "tl")]
            if c > A_SMALL_MAX {
                mp::free(p as usize);
            }
            if c > A_SMALL_MAX && big_put(p as usize, c) {
                return;
            }
            TL.with(|t| {
                let t = &mut *t.get();
                *(p as *mut usize) = t.free[c];
                t.free[c] = p as usize;
            });
        } else {
            std::alloc::System.dealloc(p, l)
        }
    }
    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if !a_mine(p) {
            return std::alloc::System.realloc(p, l, new_size);
        }
        let c = a_class(l.size(), l.align());
        let nc = a_class(new_size, l.align());
        if nc <= c {
            return p;
        }
        let nl = std::alloc::Layout::from_size_align_unchecked(new_size, l.align());
        let q = self.get(nl, false);
        if !q.is_null() {
            std::ptr::copy_nonoverlapping(p, q, l.size());
            self.dealloc(p, l);
        }
        q
    }
}

#[cfg(feature = "sp")]
pub unsafe fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32 {
    let t = sp::ns();
    let r = madvise_real(addr, len, advice);
    let k = sp::cls(advice);
    sp::MADV[k][0].fetch_add(1, Ordering::Relaxed);
    sp::MADV[k][1].fetch_add(sp::ns() - t, Ordering::Relaxed);
    sp::MADV[k][2].fetch_add(len as u64, Ordering::Relaxed);
    r
}
#[cfg(feature = "sp")]
pub mod sp {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static MADV: [[AtomicU64; 3]; 6] = [const { [const { AtomicU64::new(0) }; 3] }; 6];
    pub const NAMES: [&str; 6] = ["DONTNEED", "POPW", "HUGE", "NOHUGE", "SEQ", "other"];
    pub fn cls(a: i32) -> usize {
        match a {
            4 => 0,
            23 => 1,
            14 => 2,
            15 => 3,
            2 => 4,
            _ => 5,
        }
    }
    pub fn ns() -> u64 {
        let mut t = super::Timespec { s: 0, ns: 0 };
        unsafe { super::clock_gettime(1, &mut t) };
        t.s as u64 * 1_000_000_000 + t.ns as u64
    }
    pub fn ru(who: i32) -> [i64; 3] {
        extern "C" {
            fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
        }
        let mut r = [0i64; 18];
        unsafe { getrusage(who, &mut r) };
        [r[0] * 1_000_000 + r[1], r[2] * 1_000_000 + r[3], r[8]]
    }
    const VK: [&str; 5] = ["thp_fault_alloc", "nr_tlb_remote_flush", "nr_tlb_remote_flush_received", "pgfree", "thp_fault_fallback"];
    fn vm() -> [i64; 5] {
        let mut o = [0i64; 5];
        if let Ok(s) = std::fs::read_to_string("/proc/vmstat") {
            for l in s.lines() {
                let mut it = l.split_whitespace();
                let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or("0"));
                if let Some(i) = VK.iter().position(|x| *x == k) {
                    o[i] = v.parse().unwrap_or(0);
                }
            }
        }
        o
    }
    pub struct Row {
        name: &'static str,
        t: u64,
        ru: [i64; 3],
        vm: [i64; 5],
        madv: [[u64; 3]; 6],
    }
    pub static mut ROWS: Vec<Row> = Vec::new();
    pub static PT: [[AtomicU64; 3]; 16] = [const { [const { AtomicU64::new(0) }; 3] }; 16];
    pub static ZT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    #[inline(never)]
    pub fn zero_touch(p: *mut u8) {
        let t = ns();
        unsafe { std::ptr::write_volatile(p, 0) };
        ZT[0].fetch_add(ns() - t, Ordering::Relaxed);
        ZT[1].fetch_add(1, Ordering::Relaxed);
    }
    #[inline(never)]
    pub fn input_touch(p: *const u8, n: usize) {
        let t = ns();
        let mut o = 0;
        let mut x = 0u8;
        while o < n {
            x = x.wrapping_add(unsafe { std::ptr::read_volatile(p.add(o)) });
            o += 4096;
        }
        std::hint::black_box(x);
        ZT[2].fetch_add(ns() - t, Ordering::Relaxed);
        ZT[3].fetch_add((n / 4096) as u64, Ordering::Relaxed);
    }
    pub fn thread_add(tid: usize, a: [i64; 3], b: [i64; 3]) {
        if tid < 16 {
            for k in 0..3 {
                PT[tid][k].fetch_add((b[k] - a[k]) as u64, Ordering::Relaxed);
            }
        }
    }
    #[allow(static_mut_refs)]
    pub fn snap(name: &'static str) {
        let mut madv = [[0u64; 3]; 6];
        for k in 0..6 {
            for j in 0..3 {
                madv[k][j] = MADV[k][j].load(Ordering::Relaxed);
            }
        }
        let r = Row { name, t: ns(), ru: ru(0), vm: vm(), madv };
        unsafe { ROWS.push(r) };
    }
    #[allow(static_mut_refs)]
    pub fn report() {
        unsafe {
            if ROWS.capacity() < 64 {
            }
            for w in ROWS.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let mut s = String::new();
                for k in 0..6 {
                    let n = b.madv[k][0] - a.madv[k][0];
                    if n > 0 {
                        s.push_str(&format!(" {}:{}x/{:.2}ms/{:.0}MB", NAMES[k], n, (b.madv[k][1] - a.madv[k][1]) as f64 / 1e6, (b.madv[k][2] - a.madv[k][2]) as f64 / 1048576.0));
                    }
                }
                eprintln!(
                    "[sp] {:>8} wall {:7.2} user {:7.2} sys {:6.2} minflt {:6} thp {:4} tlbfl {:4} tlbrecv {:5} pgfree {:7} thpfb {}{}",
                    b.name,
                    (b.t - a.t) as f64 / 1e6,
                    (b.ru[0] - a.ru[0]) as f64 / 1e3,
                    (b.ru[1] - a.ru[1]) as f64 / 1e3,
                    b.ru[2] - a.ru[2],
                    b.vm[0] - a.vm[0],
                    b.vm[1] - a.vm[1],
                    b.vm[2] - a.vm[2],
                    b.vm[3] - a.vm[3],
                    b.vm[4] - a.vm[4],
                    s
                );
            }
            let mut s = String::new();
            for t in 0..16 {
                let v: Vec<u64> = (0..3).map(|k| PT[t][k].load(Ordering::Relaxed)).collect();
                if v[0] + v[1] > 0 {
                    s.push_str(&format!(" t{t}:{:.1}/{:.1}/{}", v[0] as f64 / 1e3, v[1] as f64 / 1e3, v[2]));
                }
            }
            eprintln!("[sp] parse per thread user/sys ms/minflt:{s}");
            eprintln!("[sp] block first touch: {} blocks {:.2} thread-ms ({:.1} us each); input pre-touch {} pages {:.2} thread-ms", ZT[1].load(Ordering::Relaxed), ZT[0].load(Ordering::Relaxed) as f64 / 1e6, ZT[0].load(Ordering::Relaxed) as f64 / 1e3 / ZT[1].load(Ordering::Relaxed).max(1) as f64, ZT[3].load(Ordering::Relaxed), ZT[2].load(Ordering::Relaxed) as f64 / 1e6);
        }
    }
}

pub static mut LAPS: Vec<(&'static str, f64)> = Vec::new();
pub static mut LAST: Option<std::time::Instant> = None;
#[allow(static_mut_refs)]
pub fn lap(name: &'static str) {
    unsafe {
        let now = std::time::Instant::now();
        if let Some(l) = LAST {
            LAPS.push((name, (now - l).as_secs_f64() * 1e3));
        }
        #[cfg(feature = "tl")]
        tl::LAPT.push((name, tl::now()));
        #[cfg(feature = "tl")]
        if std::env::var("XL_RSSAT").is_ok() {
            if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
                let f: Vec<&str> = s.lines().filter(|l| l.starts_with("RssFile") || l.starts_with("RssAnon")).collect();
                eprintln!("[tl] after {name}: {:?}", f);
            }
        }
        LAST = Some(now);
        #[cfg(feature = "sp")]
        sp::snap(name);
        #[cfg(feature = "tl")]
        mp::report(name);
    }
}

#[inline(always)]
unsafe fn cp_small(d: *mut u8, s: *const u8, n: usize) {
    use std::arch::x86_64::*;
    if n <= 16 {
        if n >= 8 {
            let a = (s as *const u64).read_unaligned();
            let b = (s.add(n - 8) as *const u64).read_unaligned();
            (d as *mut u64).write_unaligned(a);
            (d.add(n - 8) as *mut u64).write_unaligned(b);
        } else if n >= 4 {
            let a = (s as *const u32).read_unaligned();
            let b = (s.add(n - 4) as *const u32).read_unaligned();
            (d as *mut u32).write_unaligned(a);
            (d.add(n - 4) as *mut u32).write_unaligned(b);
        } else if n >= 2 {
            let a = (s as *const u16).read_unaligned();
            let b = (s.add(n - 2) as *const u16).read_unaligned();
            (d as *mut u16).write_unaligned(a);
            (d.add(n - 2) as *mut u16).write_unaligned(b);
        } else if n == 1 {
            *d = *s;
        }
    } else if n <= 32 {
        let a = _mm_loadu_si128(s as *const __m128i);
        let b = _mm_loadu_si128(s.add(n - 16) as *const __m128i);
        _mm_storeu_si128(d as *mut __m128i, a);
        _mm_storeu_si128(d.add(n - 16) as *mut __m128i, b);
    } else if n <= 64 {
        let a = _mm256_loadu_si256(s as *const __m256i);
        let b = _mm256_loadu_si256(s.add(n - 32) as *const __m256i);
        _mm256_storeu_si256(d as *mut __m256i, a);
        _mm256_storeu_si256(d.add(n - 32) as *mut __m256i, b);
    } else if n <= 128 {
        let a = _mm256_loadu_si256(s as *const __m256i);
        let b = _mm256_loadu_si256(s.add(32) as *const __m256i);
        let c = _mm256_loadu_si256(s.add(n - 64) as *const __m256i);
        let e = _mm256_loadu_si256(s.add(n - 32) as *const __m256i);
        _mm256_storeu_si256(d as *mut __m256i, a);
        _mm256_storeu_si256(d.add(32) as *mut __m256i, b);
        _mm256_storeu_si256(d.add(n - 64) as *mut __m256i, c);
        _mm256_storeu_si256(d.add(n - 32) as *mut __m256i, e);
    } else {
        let a = _mm256_loadu_si256(s as *const __m256i);
        let b = _mm256_loadu_si256(s.add(32) as *const __m256i);
        let c = _mm256_loadu_si256(s.add(64) as *const __m256i);
        let e = _mm256_loadu_si256(s.add(96) as *const __m256i);
        let f = _mm256_loadu_si256(s.add(n - 128) as *const __m256i);
        let g = _mm256_loadu_si256(s.add(n - 96) as *const __m256i);
        let h = _mm256_loadu_si256(s.add(n - 64) as *const __m256i);
        let i = _mm256_loadu_si256(s.add(n - 32) as *const __m256i);
        _mm256_storeu_si256(d as *mut __m256i, a);
        _mm256_storeu_si256(d.add(32) as *mut __m256i, b);
        _mm256_storeu_si256(d.add(64) as *mut __m256i, c);
        _mm256_storeu_si256(d.add(96) as *mut __m256i, e);
        _mm256_storeu_si256(d.add(n - 128) as *mut __m256i, f);
        _mm256_storeu_si256(d.add(n - 96) as *mut __m256i, g);
        _mm256_storeu_si256(d.add(n - 64) as *mut __m256i, h);
        _mm256_storeu_si256(d.add(n - 32) as *mut __m256i, i);
    }
}

#[cfg(not(feature = "w512"))]
const NT_MIN: usize = 1 << 20;
#[inline(always)]
unsafe fn cp_fwd(d: *mut u8, s: *const u8, n: usize) {
    #[cfg(feature = "w512")]
    std::arch::asm!("rep movsb", inout("rcx") n => _, inout("rdi") d => _, inout("rsi") s => _, options(nostack, preserves_flags));
    #[cfg(not(feature = "w512"))]
    {
        if n >= NT_MIN && (d as usize).wrapping_sub(s as usize) >= n && (s as usize).wrapping_sub(d as usize) >= n {
            std::arch::asm!(
                "vmovdqu {t}, ymmword ptr [{s} + {n} - 32]",
                "lea {e}, [{d} + {n} - 32]",
                "vmovdqu {a}, ymmword ptr [{s}]",
                "vmovdqu ymmword ptr [{d}], {a}",
                "mov {k}, {d}",
                "add {d}, 32",
                "and {d}, -32",
                "sub {k}, {d}",
                "sub {s}, {k}",
                "add {n}, {k}",
                "2:",
                "vmovdqu {a}, ymmword ptr [{s}]",
                "vmovdqu {b}, ymmword ptr [{s} + 32]",
                "vmovdqu {c}, ymmword ptr [{s} + 64]",
                "vmovdqu {f}, ymmword ptr [{s} + 96]",
                "vmovntdq ymmword ptr [{d}], {a}",
                "vmovntdq ymmword ptr [{d} + 32], {b}",
                "vmovntdq ymmword ptr [{d} + 64], {c}",
                "vmovntdq ymmword ptr [{d} + 96], {f}",
                "add {s}, 128",
                "add {d}, 128",
                "sub {n}, 128",
                "cmp {n}, 128",
                "jae 2b",
                "3:",
                "cmp {n}, 32",
                "jbe 4f",
                "vmovdqu {a}, ymmword ptr [{s}]",
                "vmovntdq ymmword ptr [{d}], {a}",
                "add {s}, 32",
                "add {d}, 32",
                "sub {n}, 32",
                "jmp 3b",
                "4:",
                "sfence",
                "vmovdqu ymmword ptr [{e}], {t}",
                s = inout(reg) s => _,
                d = inout(reg) d => _,
                n = inout(reg) n => _,
                e = out(reg) _,
                k = out(reg) _,
                t = out(ymm_reg) _,
                a = out(ymm_reg) _,
                b = out(ymm_reg) _,
                c = out(ymm_reg) _,
                f = out(ymm_reg) _,
                options(nostack),
            );
        } else {
            std::arch::asm!(
                "vmovdqu {t}, ymmword ptr [{s} + {n} - 32]",
                "lea {e}, [{d} + {n} - 32]",
                "2:",
                "vmovdqu {a}, ymmword ptr [{s}]",
                "vmovdqu {b}, ymmword ptr [{s} + 32]",
                "vmovdqu {c}, ymmword ptr [{s} + 64]",
                "vmovdqu {f}, ymmword ptr [{s} + 96]",
                "vmovdqu ymmword ptr [{d}], {a}",
                "vmovdqu ymmword ptr [{d} + 32], {b}",
                "vmovdqu ymmword ptr [{d} + 64], {c}",
                "vmovdqu ymmword ptr [{d} + 96], {f}",
                "add {s}, 128",
                "add {d}, 128",
                "sub {n}, 128",
                "cmp {n}, 128",
                "jae 2b",
                "3:",
                "cmp {n}, 32",
                "jbe 4f",
                "vmovdqu {a}, ymmword ptr [{s}]",
                "vmovdqu ymmword ptr [{d}], {a}",
                "add {s}, 32",
                "add {d}, 32",
                "sub {n}, 32",
                "jmp 3b",
                "4:",
                "vmovdqu ymmword ptr [{e}], {t}",
                s = inout(reg) s => _,
                d = inout(reg) d => _,
                n = inout(reg) n => _,
                e = out(reg) _,
                t = out(ymm_reg) _,
                a = out(ymm_reg) _,
                b = out(ymm_reg) _,
                c = out(ymm_reg) _,
                f = out(ymm_reg) _,
                options(nostack),
            );
        }
    }
}
#[inline(always)]
unsafe fn cp_bwd(d: *mut u8, s: *const u8, n: usize) {
    #[cfg(feature = "w512")]
    std::arch::asm!("std", "rep movsb", "cld", inout("rcx") n => _, inout("rdi") d.add(n - 1) => _, inout("rsi") s.add(n - 1) => _, options(nostack));
    #[cfg(not(feature = "w512"))]
    std::arch::asm!(
        "vmovdqu {t}, ymmword ptr [{s}]",
        "mov {e}, {d}",
        "add {s}, {n}",
        "add {d}, {n}",
        "2:",
        "vmovdqu {a}, ymmword ptr [{s} - 32]",
        "vmovdqu {b}, ymmword ptr [{s} - 64]",
        "vmovdqu {c}, ymmword ptr [{s} - 96]",
        "vmovdqu {f}, ymmword ptr [{s} - 128]",
        "vmovdqu ymmword ptr [{d} - 32], {a}",
        "vmovdqu ymmword ptr [{d} - 64], {b}",
        "vmovdqu ymmword ptr [{d} - 96], {c}",
        "vmovdqu ymmword ptr [{d} - 128], {f}",
        "sub {s}, 128",
        "sub {d}, 128",
        "sub {n}, 128",
        "cmp {n}, 128",
        "jae 2b",
        "3:",
        "cmp {n}, 32",
        "jbe 4f",
        "vmovdqu {a}, ymmword ptr [{s} - 32]",
        "vmovdqu ymmword ptr [{d} - 32], {a}",
        "sub {s}, 32",
        "sub {d}, 32",
        "sub {n}, 32",
        "jmp 3b",
        "4:",
        "vmovdqu ymmword ptr [{e}], {t}",
        s = inout(reg) s => _,
        d = inout(reg) d => _,
        n = inout(reg) n => _,
        e = out(reg) _,
        t = out(ymm_reg) _,
        a = out(ymm_reg) _,
        b = out(ymm_reg) _,
        c = out(ymm_reg) _,
        f = out(ymm_reg) _,
        options(nostack),
    );
}
#[inline(always)]
unsafe fn set_big(d: *mut u8, b: u8, n: usize) {
    #[cfg(feature = "w512")]
    std::arch::asm!("rep stosb", inout("rcx") n => _, inout("rdi") d => _, in("al") b, options(nostack, preserves_flags));
    #[cfg(not(feature = "w512"))]
    {
        use std::arch::x86_64::*;
        let v = _mm256_set1_epi8(b as i8);
        if n >= NT_MIN {
            std::arch::asm!(
                "vmovdqu ymmword ptr [{d}], {v}",
                "vmovdqu ymmword ptr [{d} + {n} - 32], {v}",
                "lea {e}, [{d} + {n} - 32]",
                "add {d}, 32",
                "and {d}, -32",
                "2:",
                "cmp {d}, {e}",
                "jae 3f",
                "vmovntdq ymmword ptr [{d}], {v}",
                "add {d}, 32",
                "jmp 2b",
                "3:",
                "sfence",
                d = inout(reg) d => _,
                n = in(reg) n,
                e = out(reg) _,
                v = in(ymm_reg) v,
                options(nostack),
            );
        } else {
            std::arch::asm!(
                "vmovdqu ymmword ptr [{d}], {v}",
                "vmovdqu ymmword ptr [{d} + {n} - 32], {v}",
                "lea {e}, [{d} + {n} - 32]",
                "add {d}, 32",
                "and {d}, -32",
                "2:",
                "cmp {d}, {e}",
                "jae 3f",
                "vmovdqa ymmword ptr [{d}], {v}",
                "add {d}, 32",
                "jmp 2b",
                "3:",
                d = inout(reg) d => _,
                n = in(reg) n,
                e = out(reg) _,
                v = in(ymm_reg) v,
                options(nostack),
            );
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(d: *mut std::ffi::c_void, s: *const std::ffi::c_void, n: usize) -> *mut std::ffi::c_void {
    let (d, s) = (d as *mut u8, s as *const u8);
    if n <= 256 {
        cp_small(d, s, n);
    } else {
        cp_fwd(d, s, n);
    }
    d as *mut std::ffi::c_void
}

#[no_mangle]
pub unsafe extern "C" fn memmove(d: *mut std::ffi::c_void, s: *const std::ffi::c_void, n: usize) -> *mut std::ffi::c_void {
    let (d, s) = (d as *mut u8, s as *const u8);
    if n <= 256 {
        cp_small(d, s, n);
    } else if (d as usize).wrapping_sub(s as usize) >= n {
        cp_fwd(d, s, n);
    } else {
        cp_bwd(d, s, n);
    }
    d as *mut std::ffi::c_void
}

#[no_mangle]
pub unsafe extern "C" fn memset(d: *mut std::ffi::c_void, c: i32, n: usize) -> *mut std::ffi::c_void {
    use std::arch::x86_64::*;
    let d = d as *mut u8;
    let b = c as u8;
    if n <= 32 {
        let v = 0x0101_0101_0101_0101u64 * b as u64;
        if n >= 16 {
            (d as *mut u64).write_unaligned(v);
            (d.add(8) as *mut u64).write_unaligned(v);
            (d.add(n - 16) as *mut u64).write_unaligned(v);
            (d.add(n - 8) as *mut u64).write_unaligned(v);
        } else if n >= 8 {
            (d as *mut u64).write_unaligned(v);
            (d.add(n - 8) as *mut u64).write_unaligned(v);
        } else if n >= 4 {
            (d as *mut u32).write_unaligned(v as u32);
            (d.add(n - 4) as *mut u32).write_unaligned(v as u32);
        } else if n >= 2 {
            (d as *mut u16).write_unaligned(v as u16);
            (d.add(n - 2) as *mut u16).write_unaligned(v as u16);
        } else if n == 1 {
            *d = b;
        }
    } else if n <= 128 {
        let v = _mm256_set1_epi8(b as i8);
        _mm256_storeu_si256(d as *mut __m256i, v);
        _mm256_storeu_si256(d.add(n - 32) as *mut __m256i, v);
        if n > 64 {
            _mm256_storeu_si256(d.add(32) as *mut __m256i, v);
            _mm256_storeu_si256(d.add(n - 64) as *mut __m256i, v);
        }
    } else {
        set_big(d, b, n);
    }
    d as *mut std::ffi::c_void
}

#[inline(never)]
pub fn mem_test() -> bool {
    use z::nvq;
    let mut src = vec![0u8; 4096];
    for (i, b) in src.iter_mut().enumerate() {
        *b = (nvq(i as u64) as usize * 131 % 251) as u8 + 1;
    }
    let mut ok = true;
    for n in 0..1100usize {
        for off in 0..9usize {
            let mut d = vec![0u8; 1200 + 16];
            unsafe { memcpy(d.as_mut_ptr().add(off) as *mut _, src.as_ptr().add(3) as *const _, std::hint::black_box(n)) };
            for i in 0..d.len() {
                let i = nvq(i as u64) as usize;
                let want = if i >= off && i < off + n { src[3 + i - off] } else { 0 };
                ok &= d[i] == want;
            }
            let mut d = vec![7u8; 1200 + 16];
            unsafe { memset(d.as_mut_ptr().add(off) as *mut _, 0x5A, std::hint::black_box(n)) };
            for i in 0..d.len() {
                let i = nvq(i as u64) as usize;
                ok &= d[i] == if i >= off && i < off + n { 0x5A } else { 7 };
            }
            for dir in 0..2 {
                let mut a = src[..2400].to_vec();
                let r = a.clone();
                let (s0, d0) = if dir == 0 { (100, 100 + off) } else { (100 + off, 100) };
                unsafe { memmove(a.as_mut_ptr().add(d0) as *mut _, a.as_ptr().add(s0) as *const _, std::hint::black_box(n)) };
                let mut e = r.clone();
                for i in 0..n {
                    let i = nvq(i as u64) as usize;
                    e[d0 + i] = r[s0 + i];
                }
                ok &= a == e;
            }
        }
    }
    {
        let big = 3usize << 20;
        let src: Vec<u8> = (0..big + 4096).map(|i| (nvq(i as u64) as usize * 31 % 253) as u8 + 1).collect();
        for &n in &[257usize, 300, 383, 384, 385, 511, 4032, 65_537, (1 << 20) - 1, 1 << 20, (1 << 20) + 1, (1 << 20) + 127, (1 << 20) + 160, (2 << 20) + 4095, big] {
            for off in [0usize, 1, 31, 32, 33, 63] {
                let mut d = vec![0u8; big + 4096 + 64];
                unsafe { memcpy(d.as_mut_ptr().add(off) as *mut _, src.as_ptr().add(5) as *const _, std::hint::black_box(n)) };
                ok &= d[..off].iter().all(|&x| x == 0) && d[off..off + n] == src[5..5 + n] && d[off + n..off + n + 64].iter().all(|&x| x == 0);
                unsafe { memset(d.as_mut_ptr().add(off) as *mut _, 0xA7, std::hint::black_box(n)) };
                ok &= d[..off].iter().all(|&x| x == 0) && d[off..off + n].iter().all(|&x| x == 0xA7) && d[off + n..off + n + 64].iter().all(|&x| x == 0);
                for (d0, s0) in [(off, off + 97), (off + 97, off), (off, off + 3), (off + 3, off)] {
                    if d0.max(s0) + n > src.len() {
                        continue;
                    }
                    let mut a = src.clone();
                    unsafe { memmove(a.as_mut_ptr().add(d0) as *mut _, a.as_ptr().add(s0) as *const _, std::hint::black_box(n)) };
                    ok &= a[d0..d0 + n] == src[s0..s0 + n] && a[..d0] == src[..d0] && a[d0 + n..] == src[d0 + n..];
                }
            }
        }
    }
    ok
}

pub mod z {
    use std::arch::x86_64::*;
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct Z(pub __m256i, pub __m256i);

    #[inline(always)]
    pub unsafe fn loadu<T>(p: *const T) -> Z {
        let p = p as *const __m256i;
        Z(yb(_mm256_loadu_si256(p)), yb(_mm256_loadu_si256(p.add(1))))
    }
    #[inline(always)]
    pub unsafe fn load<T>(p: *const T) -> Z {
        let p = p as *const __m256i;
        Z(yb(_mm256_load_si256(p)), yb(_mm256_load_si256(p.add(1))))
    }
    #[inline(always)]
    pub unsafe fn storeu<T>(p: *mut T, v: Z) {
        let p = p as *mut __m256i;
        _mm256_storeu_si256(p, yb(v.0));
        _mm256_storeu_si256(p.add(1), yb(v.1));
    }
    #[inline(always)]
    pub unsafe fn store<T>(p: *mut T, v: Z) {
        let p = p as *mut __m256i;
        _mm256_store_si256(p, yb(v.0));
        _mm256_store_si256(p.add(1), yb(v.1));
    }
    #[inline(always)]
    pub unsafe fn maskz_loadu_epi64<T>(m: u8, p: *const T) -> Z {
        let p = p as *const i64;
        Z(_mm256_maskz_loadu_epi64(m, p), _mm256_maskz_loadu_epi64(m >> 4, p.add(4)))
    }
    #[inline(always)]
    pub unsafe fn maskz_loadu_epi32<T>(m: u16, p: *const T) -> Z {
        let p = p as *const i32;
        Z(_mm256_maskz_loadu_epi32(m as u8, p), _mm256_maskz_loadu_epi32((m >> 8) as u8, p.add(8)))
    }
    #[inline(always)]
    pub unsafe fn mask_storeu_epi64<T>(p: *mut T, m: u8, v: Z) {
        let p = p as *mut i64;
        _mm256_mask_storeu_epi64(p, m, v.0);
        _mm256_mask_storeu_epi64(p.add(4), m >> 4, v.1);
    }
    #[inline(always)]
    pub unsafe fn compress_store_epi32(p: *mut u32, m: u16, v: Z) -> usize {
        let n0 = (m as u8).count_ones() as usize;
        _mm256_storeu_si256(p as *mut __m256i, _mm256_maskz_compress_epi32(m as u8, v.0));
        _mm256_storeu_si256(p.add(n0) as *mut __m256i, _mm256_maskz_compress_epi32((m >> 8) as u8, v.1));
        m.count_ones() as usize
    }
    #[inline(always)]
    pub unsafe fn compress_store_epi64(p: *mut u64, m: u8, v: Z) -> usize {
        let n0 = (m & 15).count_ones() as usize;
        _mm256_storeu_si256(p as *mut __m256i, _mm256_maskz_compress_epi64(m, v.0));
        _mm256_storeu_si256(p.add(n0) as *mut __m256i, _mm256_maskz_compress_epi64(m >> 4, v.1));
        m.count_ones() as usize
    }

    #[inline(always)]
    pub unsafe fn setzero() -> Z {
        Z(_mm256_setzero_si256(), _mm256_setzero_si256())
    }
    #[inline(always)]
    pub unsafe fn set1_epi32(x: i32) -> Z {
        let v = _mm256_set1_epi32(x);
        Z(v, v)
    }
    #[inline(always)]
    pub unsafe fn set1_epi64(x: i64) -> Z {
        let v = _mm256_set1_epi64x(x);
        Z(v, v)
    }
    #[inline(always)]
    pub unsafe fn setr_epi64(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64) -> Z {
        Z(_mm256_setr_epi64x(a, b, c, d), _mm256_setr_epi64x(e, f, g, h))
    }
    #[inline(always)]
    pub unsafe fn iota_epi32() -> Z {
        Z(_mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7), _mm256_setr_epi32(8, 9, 10, 11, 12, 13, 14, 15))
    }
    #[inline(always)]
    pub unsafe fn iota_epi64() -> Z {
        setr_epi64(0, 1, 2, 3, 4, 5, 6, 7)
    }

    macro_rules! bin {
        ($($name:ident = $f:ident;)*) => {$(
            #[inline(always)]
            pub unsafe fn $name(a: Z, b: Z) -> Z {
                Z($f(a.0, b.0), $f(a.1, b.1))
            }
        )*};
    }
    bin! {
        and = _mm256_and_si256;
        or = _mm256_or_si256;
        xor = _mm256_xor_si256;
        andnot = _mm256_andnot_si256;
        add_epi32 = _mm256_add_epi32;
        sub_epi32 = _mm256_sub_epi32;
        add_epi64 = _mm256_add_epi64;
        sub_epi64 = _mm256_sub_epi64;
        sllv_epi32 = _mm256_sllv_epi32;
        sllv_epi64 = _mm256_sllv_epi64;
        mullo_epi64 = _mm256_mullo_epi64;
        mul_epu32 = _mm256_mul_epu32;
        min_epu64 = _mm256_min_epu64;
        max_epu64 = _mm256_max_epu64;
    }
    #[inline(always)]
    pub unsafe fn srli_epi64<const N: i32>(a: Z) -> Z {
        Z(_mm256_srli_epi64::<N>(a.0), _mm256_srli_epi64::<N>(a.1))
    }
    #[inline(always)]
    pub unsafe fn slli_epi64<const N: i32>(a: Z) -> Z {
        Z(_mm256_slli_epi64::<N>(a.0), _mm256_slli_epi64::<N>(a.1))
    }
    #[inline(always)]
    pub unsafe fn srli_epi32<const N: i32>(a: Z) -> Z {
        Z(_mm256_srli_epi32::<N>(a.0), _mm256_srli_epi32::<N>(a.1))
    }
    #[inline(always)]
    pub unsafe fn slli_epi32<const N: i32>(a: Z) -> Z {
        Z(_mm256_slli_epi32::<N>(a.0), _mm256_slli_epi32::<N>(a.1))
    }
    #[inline(always)]
    pub unsafe fn lzcnt_epi64(a: Z) -> Z {
        Z(_mm256_lzcnt_epi64(a.0), _mm256_lzcnt_epi64(a.1))
    }
    #[inline(always)]
    pub unsafe fn ternarylogic_epi64<const I: i32>(a: Z, b: Z, c: Z) -> Z {
        Z(_mm256_ternarylogic_epi64::<I>(a.0, b.0, c.0), _mm256_ternarylogic_epi64::<I>(a.1, b.1, c.1))
    }

    #[inline(always)]
    pub fn kb(mut m: u8) -> u8 {
        unsafe { std::arch::asm!("/* {0} */", inout(kreg) m, options(pure, nomem, nostack, preserves_flags)) };
        m
    }
    #[inline(always)]
    pub fn nv<T>(x: T) -> T {
        let mut v = std::mem::MaybeUninit::<T>::new(x);
        unsafe { std::arch::asm!("/* {0} */", in(reg) v.as_mut_ptr(), options(nostack, preserves_flags)) };
        unsafe { v.assume_init() }
    }
    #[inline(always)]
    pub fn nvq(mut x: u64) -> u64 {
        unsafe { std::arch::asm!("/* {0} */", inout(reg) x, options(pure, nomem, nostack, preserves_flags)) };
        x
    }
    #[inline(always)]
    pub fn yb(mut v: __m256i) -> __m256i {
        unsafe { std::arch::asm!("/* {0} */", inout(ymm_reg) v, options(pure, nomem, nostack, preserves_flags)) };
        v
    }
    #[inline(always)]
    pub fn m64(lo: u8, hi: u8) -> u8 {
        let o: u8;
        unsafe { std::arch::asm!("kshiftlb {t}, {h}, 4", "korb {o}, {t}, {l}", h = in(kreg) hi, l = in(kreg) lo, t = out(kreg) _, o = lateout(kreg) o, options(pure, nomem, nostack, preserves_flags)) };
        o
    }
    #[inline(always)]
    pub fn m32(lo: u8, hi: u8) -> u16 {
        let o: u16;
        unsafe { std::arch::asm!("kunpckbw {o}, {h}, {l}", h = in(kreg) hi, l = in(kreg) lo, o = lateout(kreg) o, options(pure, nomem, nostack, preserves_flags)) };
        o
    }
    macro_rules! cmp64 {
        ($($name:ident = $f:ident;)*) => {$(
            #[inline(always)]
            pub unsafe fn $name(a: Z, b: Z) -> u8 {
                m64($f(a.0, b.0), $f(a.1, b.1))
            }
        )*};
    }
    macro_rules! cmp32 {
        ($($name:ident = $f:ident;)*) => {$(
            #[inline(always)]
            pub unsafe fn $name(a: Z, b: Z) -> u16 {
                m32($f(a.0, b.0), $f(a.1, b.1))
            }
        )*};
    }
    cmp64! {
        test_epi64_mask = _mm256_test_epi64_mask;
        testn_epi64_mask = _mm256_testn_epi64_mask;
        cmpgt_epu64_mask = _mm256_cmpgt_epu64_mask;
        cmplt_epu64_mask = _mm256_cmplt_epu64_mask;
        cmpge_epu64_mask = _mm256_cmpge_epu64_mask;
        cmpneq_epu64_mask = _mm256_cmpneq_epu64_mask;
    }
    cmp32! {
        test_epi32_mask = _mm256_test_epi32_mask;
        cmpgt_epu32_mask = _mm256_cmpgt_epu32_mask;
        cmplt_epu32_mask = _mm256_cmplt_epu32_mask;
        cmple_epu32_mask = _mm256_cmple_epu32_mask;
        cmpge_epu32_mask = _mm256_cmpge_epu32_mask;
        cmpeq_epi32_mask = _mm256_cmpeq_epi32_mask;
        cmpneq_epi32_mask = _mm256_cmpneq_epi32_mask;
    }
    #[inline(always)]
    pub unsafe fn mask_cmpneq_epu64_mask(k: u8, a: Z, b: Z) -> u8 {
        m64(_mm256_mask_cmpneq_epu64_mask(k, a.0, b.0), _mm256_mask_cmpneq_epu64_mask(k >> 4, a.1, b.1))
    }
    #[inline(always)]
    pub unsafe fn movepi64_mask(a: Z) -> u8 {
        m64(_mm256_movepi64_mask(a.0), _mm256_movepi64_mask(a.1))
    }
    #[inline(always)]
    pub unsafe fn movepi32_mask(a: Z) -> u16 {
        m32(_mm256_movepi32_mask(a.0), _mm256_movepi32_mask(a.1))
    }
    #[inline(always)]
    pub unsafe fn mask_mov_epi64(src: Z, k: u8, a: Z) -> Z {
        Z(_mm256_mask_mov_epi64(src.0, k, a.0), _mm256_mask_mov_epi64(src.1, k >> 4, a.1))
    }
    #[inline(always)]
    pub unsafe fn mask_mov_epi32(src: Z, k: u16, a: Z) -> Z {
        Z(_mm256_mask_mov_epi32(src.0, k as u8, a.0), _mm256_mask_mov_epi32(src.1, (k >> 8) as u8, a.1))
    }
    #[inline(always)]
    pub unsafe fn mask_or_epi64(src: Z, k: u8, a: Z, b: Z) -> Z {
        Z(_mm256_mask_or_epi64(src.0, k, a.0, b.0), _mm256_mask_or_epi64(src.1, k >> 4, a.1, b.1))
    }
    #[inline(always)]
    pub unsafe fn mask_or_epi32(src: Z, k: u16, a: Z, b: Z) -> Z {
        Z(_mm256_mask_or_epi32(src.0, k as u8, a.0, b.0), _mm256_mask_or_epi32(src.1, (k >> 8) as u8, a.1, b.1))
    }
    #[inline(always)]
    pub unsafe fn mask_add_epi64(src: Z, k: u8, a: Z, b: Z) -> Z {
        Z(_mm256_mask_add_epi64(src.0, k, a.0, b.0), _mm256_mask_add_epi64(src.1, k >> 4, a.1, b.1))
    }

    #[inline(always)]
    pub unsafe fn permutexvar_epi64(idx: Z, a: Z) -> Z {
        Z(_mm256_permutex2var_epi64(a.0, idx.0, a.1), _mm256_permutex2var_epi64(a.0, idx.1, a.1))
    }
    #[inline(always)]
    unsafe fn p2v64(a: Z, x: __m256i, b: Z) -> __m256i {
        let ta = _mm256_permutex2var_epi64(a.0, x, a.1);
        let tb = _mm256_permutex2var_epi64(b.0, x, b.1);
        _mm256_mask_blend_epi64(_mm256_test_epi64_mask(x, _mm256_set1_epi64x(8)), ta, tb)
    }
    #[inline(always)]
    pub unsafe fn permutex2var_epi64(a: Z, idx: Z, b: Z) -> Z {
        Z(p2v64(a, idx.0, b), p2v64(a, idx.1, b))
    }
    #[inline(always)]
    pub unsafe fn even_epi64(a: Z, b: Z) -> Z {
        Z(ev4(a.0, a.1), ev4(b.0, b.1))
    }
    #[inline(always)]
    pub unsafe fn odd_epi64(a: Z, b: Z) -> Z {
        Z(od4(a.0, a.1), od4(b.0, b.1))
    }
    #[inline(always)]
    pub unsafe fn ev4(x: __m256i, y: __m256i) -> __m256i {
        _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_unpacklo_epi64(x, y))
    }
    #[inline(always)]
    pub unsafe fn od4(x: __m256i, y: __m256i) -> __m256i {
        _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_unpackhi_epi64(x, y))
    }
    #[inline(always)]
    pub unsafe fn prev_epi64(a: Z, b: Z) -> Z {
        Z(_mm256_alignr_epi64::<3>(a.0, b.1), _mm256_alignr_epi64::<3>(a.1, a.0))
    }
    #[inline(always)]
    pub unsafe fn prev_epi32(a: Z, b: Z) -> Z {
        Z(_mm256_alignr_epi32::<7>(a.0, b.1), _mm256_alignr_epi32::<7>(a.1, a.0))
    }
    #[inline(always)]
    pub unsafe fn cvtepi64_epi32(a: Z) -> __m256i {
        _mm256_set_m128i(_mm256_cvtepi64_epi32(a.1), _mm256_cvtepi64_epi32(a.0))
    }
    #[inline(always)]
    pub unsafe fn reduce_or_epi64(a: Z) -> u64 {
        let v = _mm256_or_si256(a.0, a.1);
        let x = _mm_or_si128(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
        (_mm_cvtsi128_si64(x) | _mm_extract_epi64::<1>(x)) as u64
    }
    #[inline(always)]
    pub unsafe fn mask_reduce_min_epu64(k: u8, a: Z) -> u64 {
        let ones = _mm256_set1_epi64x(-1);
        let v = _mm256_min_epu64(_mm256_mask_mov_epi64(ones, k, a.0), _mm256_mask_mov_epi64(ones, k >> 4, a.1));
        let v = _mm256_min_epu64(v, _mm256_permute4x64_epi64::<0b01_00_11_10>(v));
        let v = _mm256_min_epu64(v, _mm256_permute4x64_epi64::<0b00_00_00_01>(v));
        _mm256_extract_epi64::<0>(v) as u64
    }
    #[inline(always)]
    pub fn bit_positions(m: u32) -> u64 {
        unsafe {
            let sel = _pdep_u64(m as u64, 0x1111_1111_1111_1111) * 15;
            _pext_u64(0xFEDC_BA98_7654_3210, sel)
        }
    }
}
