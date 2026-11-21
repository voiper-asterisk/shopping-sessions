use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const K0: u64 = 0x243f_6a88_85a3_08d3;
const K1: u64 = 0x1319_8a2e_0370_7344;
const K2: u64 = 0xa409_3822_299f_31d0;
const K3: u64 = 0x082e_fa98_ec4e_6c89;
const K4: u64 = 0x4528_21e6_38d0_1377;

#[inline(always)]
pub fn mix(a: u64, b: u64) -> u64 {
    let r = (a as u128) * (b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}
#[inline(always)]
fn r64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
#[inline(always)]
fn r32(b: &[u8], i: usize) -> u64 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64
}

#[inline]
pub fn hash(b: &[u8]) -> u64 {
    let n = b.len();
    if n <= 16 {
        let (lo, hi) = if n >= 8 {
            (r64(b, 0), r64(b, n - 8))
        } else if n >= 4 {
            (r32(b, 0), r32(b, n - 4))
        } else if n > 0 {
            (((b[0] as u64) << 16) | ((b[n >> 1] as u64) << 8) | b[n - 1] as u64, 0)
        } else {
            (0, 0)
        };
        let h = mix(lo ^ K0, hi ^ K1 ^ (n as u64));
        return mix(h ^ K2, K3);
    }
    let mut h = K4 ^ n as u64;
    let mut i = 0;
    while i + 16 < n {
        h = mix(r64(b, i) ^ K0, r64(b, i + 8) ^ h);
        i += 16;
    }
    h = mix(r64(b, n - 16) ^ K1, r64(b, n - 8) ^ h ^ K2);
    mix(h ^ K2, K3)
}

#[derive(Default, Clone, Copy)]
pub struct H(u64);
impl Hasher for H {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, b: &[u8]) {
        self.0 = mix(self.0 ^ hash(b), K3);
    }
    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.0 = mix(self.0 ^ x ^ K0, K3);
    }
    #[inline]
    fn write_u32(&mut self, x: u32) {
        self.write_u64(x as u64)
    }
    #[inline]
    fn write_usize(&mut self, x: usize) {
        self.write_u64(x as u64)
    }
}
pub type Map<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<H>>;
pub fn new_map<K, V>(n: usize) -> Map<K, V> {
    Map::with_capacity_and_hasher(n, BuildHasherDefault::default())
}

pub struct Tab {
    slots: Vec<u64>,
    mask: usize,
}
impl Default for Tab {
    fn default() -> Self {
        Tab { slots: Vec::new(), mask: 0 }
    }
}
impl Tab {
    pub fn reset(&mut self, n: usize) {
        let cap = (n.saturating_mul(2)).max(16).next_power_of_two();
        if self.slots.capacity() < cap {
            self.slots = vec![0u64; cap];
        } else {
            self.slots.clear();
            self.slots.resize(cap, 0);
        }
        self.mask = cap - 1;
    }
    pub fn with_capacity(n: usize) -> Tab {
        let cap = (n.saturating_mul(2)).max(16).next_power_of_two();
        Tab { slots: vec![0u64; cap], mask: cap - 1 }
    }
    #[inline]
    pub fn find(&self, h: u64, mut eq: impl FnMut(u32) -> bool) -> Result<(usize, u32), usize> {
        let tag = h >> 32;
        let mut i = (h as usize) & self.mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                return Err(i);
            }
            if s >> 32 == tag {
                let idx = (s as u32).wrapping_sub(1);
                if eq(idx) {
                    return Ok((i, idx));
                }
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline]
    pub fn set(&mut self, slot: usize, h: u64, idx: u32) {
        self.slots[slot] = ((h >> 32) << 32) | (idx as u64 + 1);
    }
}


#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
#[inline(always)]
pub fn eq_mask64(p: &[u8], c: u8) -> u64 {
    use std::arch::x86_64::*;
    assert!(p.len() >= 64);
    #[allow(unused_unsafe)]
    unsafe {
        let v = _mm512_loadu_si512(p.as_ptr() as *const _);
        _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(c as i8))
    }
}
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
#[inline(always)]
pub fn eq_mask64(p: &[u8], c: u8) -> u64 {
    let p = &p[..64];
    let mut m = 0u64;
    for i in 0..64 {
        m |= ((p[i] == c) as u64) << i;
    }
    m
}

#[inline]
pub fn find_byte(h: &[u8], c: u8) -> Option<usize> {
    let mut i = 0;
    while i + 64 <= h.len() {
        let m = eq_mask64(&h[i..], c);
        if m != 0 {
            return Some(i + m.trailing_zeros() as usize);
        }
        i += 64;
    }
    h[i..].iter().position(|&b| b == c).map(|p| p + i)
}

#[inline]
pub fn count_byte(h: &[u8], c: u8) -> usize {
    let mut i = 0;
    let mut n = 0usize;
    while i + 64 <= h.len() {
        n += eq_mask64(&h[i..], c).count_ones() as usize;
        i += 64;
    }
    n + h[i..].iter().filter(|&&b| b == c).count()
}

pub fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || n.len() > h.len() {
        return if n.is_empty() { Some(0) } else { None };
    }
    (0..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}


extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

pub struct Mapped {
    ptr: *mut u8,
    len: usize,
}
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}
impl Mapped {
    pub fn open(path: &Path) -> io::Result<Mapped> {
        let ctx = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
        let f = File::open(path).map_err(ctx)?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0) as usize;
        if len == 0 {
            return Ok(Mapped { ptr: std::ptr::null_mut(), len: 0 });
        }
        let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(ctx(io::Error::last_os_error()));
        }
        Ok(Mapped { ptr: p, len })
    }
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}
impl Drop for Mapped {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe {
                munmap(self.ptr, self.len);
            }
        }
    }
}

pub fn map_dir(dir: &Path) -> io::Result<Vec<Mapped>> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.file_name().map_or(false, |n| n.as_encoded_bytes().ends_with(b".csv")))
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths.iter().map(|p| Mapped::open(p)).collect()
}


pub fn par_run<S: Send, T: Send>(
    n: usize,
    threads: usize,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, usize) -> T + Sync,
) -> (Vec<T>, Vec<S>) {
    let threads = threads.max(1).min(n.max(1));
    let next = AtomicUsize::new(0);
    let (next, init, f) = (&next, &init, &f);
    let work = move || {
        let mut s = init();
        let mut out = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            out.push((i, f(&mut s, i)));
        }
        (out, s)
    };
    let parts: Vec<(Vec<(usize, T)>, S)> = if threads == 1 {
        vec![work()]
    } else {
        std::thread::scope(|sc| {
            let hs: Vec<_> = (0..threads).map(|_| sc.spawn(work)).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        })
    };
    let mut slots: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let mut states = Vec::with_capacity(parts.len());
    for (out, s) in parts {
        for (i, t) in out {
            slots[i] = Some(t);
        }
        states.push(s);
    }
    (slots.into_iter().map(|o| o.expect("job result")).collect(), states)
}
