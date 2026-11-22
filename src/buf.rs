//! Append-only record streams built from 4 KB blocks carved out of one
//! per-thread arena (a single lazily committed reservation), so a record is
//! addressable by a 32-bit index into its thread's arena.
//!
//! A stream's whole write state is one 32-bit arena offset. A thousand streams
//! then keep their state in 4 KB, which stays in the first-level cache: the
//! only store of a push that leaves it is the record itself.

use crate::sys;

pub struct Arena {
    base: usize,
    used: usize,
    cap: usize,
}

unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    pub fn new(cap: usize, huge: bool) -> Arena {
        let cap = (cap + (2 << 20)) & !((2 << 20) - 1);
        Arena { base: sys::anon(cap, huge) as usize, used: 0, cap }
    }
    /// Gives the memory back. Nothing may point into the arena any more.
    #[allow(dead_code)]
    pub fn free(&mut self) {
        if self.cap > 0 {
            sys::unmap(self.base as *const u8, self.cap);
            self.cap = 0;
            self.used = 0;
        }
    }
    #[inline]
    pub fn alloc(&mut self, bytes: usize) -> *mut u8 {
        let bytes = (bytes + 63) & !63;
        if self.used + bytes > self.cap {
            eprintln!("arena exhausted ({} bytes)", self.cap);
            sys::exit_now(4);
        }
        let p = self.base + self.used;
        self.used += bytes;
        p as *mut u8
    }
    /// Bytes handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }
    /// A fresh stream block: its byte offset in the arena, a multiple of `BB`.
    #[inline]
    pub fn alloc_block(&mut self) -> u32 {
        let at = (self.used + BB - 1) & !(BB - 1);
        if at + BB > self.cap.min(1 << 32) {
            eprintln!("arena exhausted ({} bytes)", self.cap);
            sys::exit_now(4);
        }
        self.used = at + BB;
        at as u32
    }
    #[inline(always)]
    pub fn base(&self) -> *mut u8 {
        self.base as *mut u8
    }
}

pub static AHEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub static READ_AHEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static AHEAD_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);

/// Bytes in a stream block.
pub const BB: usize = 4096;

/// `n` streams of `T`, each a list of blocks.
pub struct Streams<T> {
    pos: Vec<u32>,         // arena offset of the next free slot; `full` within its block when a new block is due
    blocks: Vec<Vec<u32>>, // arena offsets of each stream's blocks
    base: usize,           // the arena
    per: usize,            // records in a block
    full: u32,             // offset within a block that means "no room"
    _t: std::marker::PhantomData<T>,
}

unsafe impl<T> Send for Streams<T> {}
unsafe impl<T> Sync for Streams<T> {}

impl<T: Copy> Streams<T> {
    pub fn new(n: usize, arena: &Arena) -> Self {
        let size = std::mem::size_of::<T>();
        let per = BB / size;
        let full = ((per * size) & (BB - 1)) as u32;
        Streams { pos: vec![full; n], blocks: (0..n).map(|_| Vec::new()).collect(), base: arena.base() as usize, per, full, _t: std::marker::PhantomData }
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self, s: usize, arena: &mut Arena) -> u32 {
        let b = arena.alloc_block();
        self.blocks[s].push(b);
        b
    }
    /// Appends to stream `s`; returns where the record went.
    #[inline(always)]
    pub fn push(&mut self, s: usize, v: T, arena: &mut Arena) -> *mut T {
        unsafe {
            let mut at = *self.pos.get_unchecked(s);
            if at & (BB as u32 - 1) == self.full {
                at = self.grow(s, arena);
            }
            let p = (self.base + at as usize) as *mut T;
            p.write(v);
            // The stream comes back to this spot a thousand pushes from now. Ask
            // for its next line today: fetching a fresh line from memory at the
            // moment of the write holds up every store behind it. (Measured on
            // the scoring machine; a knob only in benchmark builds.)
            if !cfg!(feature = "selfbench") || AHEAD.load(std::sync::atomic::Ordering::Relaxed) {
                #[cfg(target_arch = "x86_64")]
                {
                    let d = if cfg!(feature = "selfbench") { AHEAD_BYTES.load(std::sync::atomic::Ordering::Relaxed) } else { 64 };
                    std::arch::x86_64::_mm_prefetch((p as *const i8).add(d), std::arch::x86_64::_MM_HINT_T0);
                }
            }
            *self.pos.get_unchecked_mut(s) = at + std::mem::size_of::<T>() as u32;
            p
        }
    }
    /// Records in the last block of stream `s` (which has at least one block).
    #[inline(always)]
    fn tail(&self, s: usize, last: u32) -> usize {
        (self.pos[s] - last) as usize / std::mem::size_of::<T>()
    }
    pub fn len(&self, s: usize) -> usize {
        match self.blocks[s].last() {
            None => 0,
            Some(&last) => (self.blocks[s].len() - 1) * self.per + self.tail(s, last),
        }
    }
    /// `for_slices` for a first pass over blocks that are not in the cache:
    /// asks for the whole next block while working on the current one.
    #[inline(always)]
    pub fn for_slices_ahead(&self, s: usize, mut f: impl FnMut(&[T])) {
        let on = cfg!(feature = "selfbench") && READ_AHEAD.load(std::sync::atomic::Ordering::Relaxed);
        let bl = &self.blocks[s];
        let n = bl.len();
        for (k, &b) in bl.iter().enumerate() {
            if on && k + 1 < n {
                let nx = (self.base + bl[k + 1] as usize) as *const u8;
                for l in 0..BB / 64 {
                    #[cfg(target_arch = "x86_64")]
                    unsafe {
                        std::arch::x86_64::_mm_prefetch(nx.add(l * 64) as *const i8, std::arch::x86_64::_MM_HINT_T0);
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    let _ = (nx, l);
                }
            }
            let len = if k + 1 == n { self.tail(s, b) } else { self.per };
            f(unsafe { std::slice::from_raw_parts((self.base + b as usize) as *const T, len) });
        }
    }
    /// Calls `f` on each block's filled part of stream `s`.
    #[inline(always)]
    pub fn for_slices(&self, s: usize, mut f: impl FnMut(&[T])) {
        let bl = &self.blocks[s];
        let n = bl.len();
        for (k, &b) in bl.iter().enumerate() {
            let len = if k + 1 == n { self.tail(s, b) } else { self.per };
            f(unsafe { std::slice::from_raw_parts((self.base + b as usize) as *const T, len) });
        }
    }
}
