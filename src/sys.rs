//! Thin wrappers over the few libc calls the pipeline needs (no crates).
#![allow(dead_code)]

use std::fs::File;
use std::os::fd::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn _exit(code: i32) -> !;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(target_os = "linux")]
const MAP_NORESERVE: i32 = 0x4000;
#[cfg(not(target_os = "linux"))]
const MAP_NORESERVE: i32 = 0x40;
#[cfg(target_os = "linux")]
const MAP_POPULATE: i32 = 0x8000;
#[cfg(not(target_os = "linux"))]
const MAP_POPULATE: i32 = 0;

pub const MADV_SEQUENTIAL: i32 = 2;
pub const MADV_WILLNEED: i32 = 3;
pub const MADV_HUGEPAGE: i32 = 14;

pub fn exit_now(code: i32) -> ! {
    // an instrumented build writes its profile here: `_exit` skips the usual hook
    #[cfg(feature = "profgen")]
    unsafe {
        extern "C" {
            fn __llvm_profile_write_file() -> i32;
        }
        __llvm_profile_write_file();
    }
    unsafe { _exit(code) }
}

/// A read-only file mapping. Empty files map to an empty slice.
pub struct Mapped {
    pub ptr: *const u8,
    pub len: usize,
}
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    pub fn open(path: &std::path::Path, populate: bool) -> std::io::Result<(Mapped, File)> {
        let f = File::open(path)?;
        let len = f.metadata()?.len() as usize;
        if len == 0 {
            return Ok((Mapped { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 }, f));
        }
        let flags = MAP_PRIVATE | if populate { MAP_POPULATE } else { 0 };
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, flags, f.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((Mapped { ptr: p, len }, f))
    }
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    pub fn advise(&self, advice: i32) {
        if self.len > 0 {
            unsafe { madvise(self.ptr as *mut u8, self.len, advice) };
        }
    }
}

/// Anonymous zeroed memory, lazily committed. Never freed (the process exits).
pub fn anon(len: usize, huge: bool) -> *mut u8 {
    let len = (len + 4095) & !4095;
    if len == 0 {
        return std::ptr::NonNull::<u8>::dangling().as_ptr();
    }
    let p = unsafe {
        mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0)
    };
    if p as isize == -1 {
        eprintln!("anon mmap of {len} bytes failed: {}", std::io::Error::last_os_error());
        exit_now(3);
    }
    if huge && cfg!(target_os = "linux") {
        unsafe { madvise(p, len, MADV_HUGEPAGE) };
    }
    p
}

/// Unmaps `[addr, addr + len)`; both must be page-aligned.
pub fn unmap(addr: *const u8, len: usize) {
    unsafe { munmap(addr as *mut u8, len) };
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
extern "C" {
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
}

/// CPU time used by the calling thread, in milliseconds.
pub fn thread_cpu_ms() -> f64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let clk = if cfg!(target_os = "linux") { 3 } else { 16 };
    unsafe { clock_gettime(clk, &mut ts) };
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 * 1e-6
}

pub const MADV_DONTNEED: i32 = 4;

/// Drops the page-table entries of `[addr, addr + len)` (page-aligned). Unlike
/// munmap this only needs the address-space lock shared, so threads can do it
/// side by side.
pub fn drop_pages(addr: *const u8, len: usize) {
    unsafe { madvise(addr as *mut u8, len, MADV_DONTNEED) };
}

// ---------------------------------------------------------------------------
// The global allocator: a lock-free bump pointer over one lazily committed
// reservation. Nothing is ever freed (the process is short-lived), so there is
// no lock to fight over and no mmap/munmap churn from worker threads.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Bump;

static BUMP_BASE: AtomicUsize = AtomicUsize::new(0);
static BUMP_NEXT: AtomicUsize = AtomicUsize::new(0);
const BUMP_CAP: usize = 24 << 30;

#[cold]
#[inline(never)]
fn bump_init() -> usize {
    let p = unsafe { mmap(std::ptr::null_mut(), BUMP_CAP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
    if p as isize == -1 {
        exit_now(5);
    }
    if cfg!(target_os = "linux") {
        unsafe { madvise(p, BUMP_CAP, MADV_HUGEPAGE) };
    }
    match BUMP_BASE.compare_exchange(0, p as usize, Relaxed, Relaxed) {
        Ok(_) => p as usize,
        Err(winner) => {
            unsafe { munmap(p, BUMP_CAP) };
            winner
        }
    }
}

unsafe impl GlobalAlloc for Bump {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let mut base = BUMP_BASE.load(Relaxed);
        if base == 0 {
            base = bump_init();
        }
        let align = l.align().max(16);
        let mut cur = BUMP_NEXT.load(Relaxed);
        loop {
            let start = (cur + align - 1) & !(align - 1);
            let end = start + l.size();
            if end > BUMP_CAP {
                return std::ptr::null_mut();
            }
            match BUMP_NEXT.compare_exchange_weak(cur, end, Relaxed, Relaxed) {
                Ok(_) => return (base + start) as *mut u8,
                Err(x) => cur = x,
            }
        }
    }
    #[inline]
    unsafe fn dealloc(&self, _p: *mut u8, _l: Layout) {}
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // fresh pages of the reservation are zero and never handed out twice
        self.alloc(l)
    }
    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if new_size <= l.size() {
            return p;
        }
        // grow in place when this is the newest block
        let base = BUMP_BASE.load(Relaxed);
        let off = p as usize - base;
        let old_end = off + l.size();
        if off + new_size <= BUMP_CAP && BUMP_NEXT.compare_exchange(old_end, off + new_size, Relaxed, Relaxed).is_ok() {
            return p;
        }
        let n = self.alloc(Layout::from_size_align_unchecked(new_size, l.align()));
        if !n.is_null() {
            std::ptr::copy_nonoverlapping(p, n, l.size());
        }
        n
    }
}

/// Benchmark only: the allocator's high-water mark, and giving back everything
/// allocated since a mark (nothing allocated since may still be in use).
#[cfg(feature = "selfbench")]
pub fn bump_mark() -> usize {
    BUMP_NEXT.load(Relaxed)
}

#[cfg(feature = "selfbench")]
pub fn bump_reset(mark: usize) {
    let base = BUMP_BASE.load(Relaxed);
    let start = (mark + 4095) & !4095;
    let end = (BUMP_NEXT.load(Relaxed) + 4095) & !4095;
    if end > start {
        drop_pages((base + start) as *const u8, end - start);
    }
    BUMP_NEXT.store(start, Relaxed);
}
