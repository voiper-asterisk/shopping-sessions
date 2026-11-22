//! Linked IDs, in parallel: a concurrent hash table of 4-slot buckets (one
//! cache line each) whose slot index is the node id, and a lock-free
//! union-find over slots. A root is only ever linked under a root with a
//! byte-wise smaller ID, so there are no cycles and every set's root is its
//! smallest ID: the person's name.
//!
//! While the sets are being built a slot's second word is its parent (0 for a
//! root, else parent + 1), next to its key in the same cache line; when they
//! are complete it is overwritten with the person's name.

use crate::ids::{cmp_ids, SLOW_BIT};
use crate::sys;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

const K2: u64 = 0xD6E8_FEB8_6659_FD93;

pub struct PersonTable {
    slots: &'static [[AtomicU64; 2]], // (key, parent while building then person name); key 0 = empty
    strs: &'static [AtomicU64],       // where a hashed key's bytes are (0 for packed keys)
    nb: u64,    // buckets
    cap: usize, // slots: 4 a bucket
}

impl PersonTable {
    /// A table for at most `max_keys` keys: two slots in three taken at the
    /// very worst, fewer in practice (an ID is usually in several links).
    fn new(max_keys: usize) -> Self {
        let nb = (max_keys * 3 / 2 / 4 + 1).max(256);
        let cap = nb * 4;
        let slots = unsafe { std::slice::from_raw_parts(sys::anon(cap * 16, true) as *const [AtomicU64; 2], cap) };
        let strs = unsafe { std::slice::from_raw_parts(sys::anon(cap * 8, true) as *const AtomicU64, cap) };
        PersonTable { slots, strs, nb: nb as u64, cap }
    }

    /// Touches the `part`-th of `of` shares of the table, so the kernel hands
    /// out (and zeroes) its pages on every core at once, not page by page as
    /// the first insertions happen to land on them.
    fn touch(&self, part: usize, of: usize) {
        for (base, bytes) in [(self.slots.as_ptr() as usize, self.cap * 16), (self.strs.as_ptr() as usize, self.cap * 8)] {
            let (lo, hi) = (bytes / of * part & !4095, if part + 1 == of { bytes } else { bytes / of * (part + 1) & !4095 });
            let mut o = lo;
            while o < hi {
                // a write that changes nothing: other threads may already be inserting here
                unsafe { (*((base + o) as *const AtomicU64)).fetch_or(0, Relaxed) };
                o += 4096;
            }
        }
    }

    /// First slot of the key's bucket (4 slots, one cache line).
    #[inline(always)]
    fn slot(&self, k: u64) -> usize {
        ((((k.wrapping_mul(K2) as u128) * (self.nb as u128)) >> 64) as usize) << 2
    }

    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_prefetch(self.slots.as_ptr().add(self.slot(k)) as *const i8, std::arch::x86_64::_MM_HINT_T0);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) self.slots.as_ptr().add(self.slot(k)), options(nostack, preserves_flags));
        }
    }

    /// Asks for what a join reads of a root: its key and where its bytes are.
    #[inline(always)]
    fn prefetch_root(&self, i: usize) {
        let (a, b) = unsafe { (self.slots.as_ptr().add(i) as *const u8, self.strs.as_ptr().add(i) as *const u8) };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_prefetch(a as *const i8, std::arch::x86_64::_MM_HINT_T0);
            std::arch::x86_64::_mm_prefetch(b as *const i8, std::arch::x86_64::_MM_HINT_T0);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{0}]", "prfm pldl1keep, [{1}]", in(reg) a, in(reg) b, options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    fn prefetch_parent(&self, i: usize) {
        let p = unsafe { self.slots.as_ptr().add(i) };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags));
        }
    }

    /// The person name of an ID (the ID itself when it is in no link). Only
    /// for use once the table is complete. Slots of a bucket fill in order, so
    /// an empty last slot means the key is absent; the common cases take no
    /// data-dependent branch.
    #[inline(always)]
    pub fn person(&self, k: u64) -> u64 {
        let mut i = self.slot(k);
        loop {
            // names are never 0, so a zero value means "not in this bucket"
            let (_, v, full) = unsafe { probe_bucket(self.slots.as_ptr().add(i) as *const u64, k) };
            let missing = (v == 0) as u64;
            if (missing & full as u64) == 0 {
                return v | (k & missing.wrapping_neg());
            }
            i = if i + 4 >= self.cap { 0 } else { i + 4 };
        }
    }

    /// Returns (slot, newly inserted by this call). `loc` says where the ID's
    /// bytes are; every caller records it for a hashed key before using the
    /// slot, so it is set by the time anyone compares through this slot.
    #[inline]
    fn insert(&self, k: u64, loc: u64) -> (usize, bool) {
        let mut i = self.slot(k);
        loop {
            for j in i..i + 4 {
                let s = &self.slots[j][0];
                let x = s.load(Relaxed);
                let (hit, new) = if x == k {
                    (true, false)
                } else if x == 0 {
                    match s.compare_exchange(0, k, Relaxed, Relaxed) {
                        Ok(_) => (true, true),
                        Err(y) => (y == k, false),
                    }
                } else {
                    (false, false)
                };
                if hit {
                    if k & SLOW_BIT != 0 && self.strs[j].load(Relaxed) == 0 {
                        self.strs[j].store(loc, Relaxed);
                    }
                    return (j, new);
                }
            }
            i = if i + 4 >= self.cap { 0 } else { i + 4 };
        }
    }

    /// Where the bytes of a hashed key in the table are, if it is in the table.
    pub fn name_loc(&self, k: u64) -> Option<u64> {
        let mut i = self.slot(k);
        loop {
            for j in i..i + 4 {
                let x = self.slots[j][0].load(Relaxed);
                if x == k {
                    return Some(self.strs[j].load(Relaxed));
                }
                if x == 0 {
                    return None;
                }
            }
            i = if i + 4 >= self.cap { 0 } else { i + 4 };
        }
    }

    #[inline]
    fn find(&self, mut x: usize) -> usize {
        loop {
            let p = self.slots[x][1].load(Relaxed);
            if p == 0 {
                return x;
            }
            let p = (p - 1) as usize;
            let gp = self.slots[p][1].load(Relaxed);
            if gp == 0 {
                return p;
            }
            self.slots[x][1].store(gp, Relaxed);
            x = (gp - 1) as usize;
        }
    }

    #[inline]
    fn union(&self, a: usize, b: usize) {
        loop {
            let (ra, rb) = (self.find(a), self.find(b));
            if ra == rb {
                return;
            }
            let (ka, kb) = (self.slots[ra][0].load(Relaxed), self.slots[rb][0].load(Relaxed));
            let (la, lb) = if (ka | kb) & SLOW_BIT != 0 { (self.strs[ra].load(Relaxed), self.strs[rb].load(Relaxed)) } else { (0, 0) };
            let less = cmp_ids(ka, la, kb, lb) == Ordering::Less;
            let (lo, hi) = if less { (ra, rb) } else { (rb, ra) };
            if self.slots[hi][1].compare_exchange(0, lo as u64 + 1, Relaxed, Relaxed).is_ok() {
                return;
            }
        }
    }

    /// Builds the table from link pairs (one Vec per producer), with `n` threads.
    pub fn build(pairs: &[Vec<(u64, u64, u64, u64)>], n: usize) -> PersonTable {
        let total: usize = pairs.iter().map(|v| v.len()).sum();
        let t0 = std::time::Instant::now();
        let t = PersonTable::new(total * 2 + 16);
        let prof = std::env::var("PROF").is_ok();

        const JOB: usize = 1 << 14;
        let jobs: Vec<(usize, usize)> = pairs.iter().enumerate().flat_map(|(v, p)| (0..p.len()).step_by(JOB).map(move |s| (v, s))).collect();
        let next = AtomicUsize::new(0);
        let inserted: Vec<Vec<u32>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..n)
                .map(|th| {
                    let t = &t;
                    let (next, jobs) = (&next, &jobs);
                    s.spawn(move || {
                        t.touch(th, n);
                        let mut mine: Vec<u32> = Vec::new();
                        let mut ids: Vec<(u32, u32)> = Vec::with_capacity(JOB);
                        loop {
                            let j = next.fetch_add(1, Relaxed);
                            let Some(&(v, st)) = jobs.get(j) else { break };
                            let sl = &pairs[v][st..(st + JOB).min(pairs[v].len())];
                            // A few hundred pairs at a time, so the lines a pair's
                            // insertion brought in are still in the cache when it
                            // is joined: insert both ends (misses hidden by
                            // prefetch), walk to the roots and ask for the lines a
                            // join reads of them, then join. A join started from
                            // an old root just walks on if the root has moved.
                            let mut lo = 0;
                            while lo < sl.len() {
                                let hi = (lo + 256).min(sl.len());
                                ids.clear();
                                for i in lo..hi {
                                    if let Some(&(c, _, d, _)) = sl.get(i + 8) {
                                        t.prefetch(c);
                                        t.prefetch(d);
                                    }
                                    let (a, la, b, lb) = sl[i];
                                    let (ia, na) = t.insert(a, la);
                                    let (ib, nb) = t.insert(b, lb);
                                    if na {
                                        mine.push(ia as u32);
                                    }
                                    if nb {
                                        mine.push(ib as u32);
                                    }
                                    ids.push((ia as u32, ib as u32));
                                }
                                for x in ids.iter_mut() {
                                    let (ra, rb) = (t.find(x.0 as usize), t.find(x.1 as usize));
                                    t.prefetch_root(ra);
                                    t.prefetch_root(rb);
                                    *x = (ra as u32, rb as u32);
                                }
                                for &(ra, rb) in ids.iter() {
                                    if ra != rb {
                                        t.union(ra as usize, rb as usize);
                                    }
                                }
                                lo = hi;
                            }
                        }
                        mine
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        if prof {
            eprintln!("  links: insert+union done at {:.1} ms ({} ids)", t0.elapsed().as_secs_f64() * 1e3, inserted.iter().map(|v| v.len()).sum::<usize>());
        }
        // Every slot learns its root's key, in two rounds: the second word of
        // a slot is a parent until all the roots are found, a name after.
        let roots: Vec<Vec<u32>> = std::thread::scope(|s| {
            let hs: Vec<_> = inserted
                .iter()
                .map(|mine| {
                    let t = &t;
                    s.spawn(move || {
                        let mut r = Vec::with_capacity(mine.len());
                        for (k, &i) in mine.iter().enumerate() {
                            if let Some(&j) = mine.get(k + 8) {
                                t.prefetch_parent(j as usize);
                            }
                            r.push(t.find(i as usize) as u32);
                        }
                        r
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        if prof {
            eprintln!("  links: roots done at {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
        }
        std::thread::scope(|s| {
            for (mine, r) in inserted.iter().zip(&roots) {
                let t = &t;
                s.spawn(move || {
                    for (k, (&i, &root)) in mine.iter().zip(r).enumerate() {
                        if let (Some(&j), Some(&rj)) = (mine.get(k + 8), r.get(k + 8)) {
                            t.prefetch_parent(j as usize);
                            t.prefetch_parent(rj as usize);
                        }
                        // a root's own second word is still 0 here: only non-roots were ever written
                        let name = t.slots[root as usize][0].load(Relaxed);
                        if i != root {
                            t.slots[i as usize][1].store(name, Relaxed);
                        }
                    }
                });
            }
        });
        // the roots last: nothing reads a parent any more
        std::thread::scope(|s| {
            for (mine, r) in inserted.iter().zip(&roots) {
                let t = &t;
                s.spawn(move || {
                    for (&i, &root) in mine.iter().zip(r) {
                        if i == root {
                            t.slots[i as usize][1].store(t.slots[i as usize][0].load(Relaxed), Relaxed);
                        }
                    }
                });
            }
        });
        if prof {
            eprintln!("  links: names done at {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
        }
        t
    }
}

/// Looks for `k` among the four (key, value) pairs of a bucket: (found, value,
/// bucket is full). No data-dependent branches.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn probe_bucket(b: *const u64, k: u64) -> (bool, u64, bool) {
    use std::arch::x86_64::*;
    let kk = _mm256_set1_epi64x(k as i64);
    let even = _mm256_setr_epi64x(-1, 0, -1, 0);
    let x = _mm256_loadu_si256(b as *const __m256i);
    let y = _mm256_loadu_si256(b.add(4) as *const __m256i);
    let mx = _mm256_and_si256(_mm256_cmpeq_epi64(x, kk), even);
    let my = _mm256_and_si256(_mm256_cmpeq_epi64(y, kk), even);
    // move each key's mask onto its value lane, keep the matching value
    let vx = _mm256_and_si256(x, _mm256_slli_si256(mx, 8));
    let vy = _mm256_and_si256(y, _mm256_slli_si256(my, 8));
    let v = _mm256_or_si256(vx, vy);
    let v128 = _mm_or_si128(_mm256_castsi256_si128(v), _mm256_extracti128_si256(v, 1));
    let value = _mm_extract_epi64(v128, 1) as u64;
    let found = _mm256_testz_si256(_mm256_or_si256(mx, my), _mm256_or_si256(mx, my)) == 0;
    (found, value, *b.add(6) != 0)
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn probe_bucket(b: *const u64, k: u64) -> (bool, u64, bool) {
    let b = &*(b as *const [u64; 8]);
    let (m0, m1, m2, m3) = ((b[0] == k) as u64, (b[2] == k) as u64, (b[4] == k) as u64, (b[6] == k) as u64);
    let v = (b[1] & m0.wrapping_neg()) | (b[3] & m1.wrapping_neg()) | (b[5] & m2.wrapping_neg()) | (b[7] & m3.wrapping_neg());
    ((m0 | m1 | m2 | m3) != 0, v, b[6] != 0)
}
