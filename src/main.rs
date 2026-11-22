// Shopping Sessions: fast implementation. Semantics follow kit/rules.md and the Python oracle.
//
// Pipeline (every phase runs on NCPU threads):
//   L. links: all threads parse link chunks straight into a shared lock-free union-find whose roots
//      are the byte-wise smallest ids (the person names); then every linked id gets its person.
//   A. parse clicks and events from ~equal byte chunks (two-stage SIMD line splitter, records wait
//      in small per-format lists so that the hot loops have predictable branches). The person of
//      every record is looked up here. Events go to buckets by event-id hash, clicks to buckets by
//      person key.
//   B. per event-id bucket: dedup (min ingest_ts wins, before validation) with a bitmap that sends
//      only the rare candidates to an exact table; valid survivors go to buckets by person key.
//   C. per person bucket (largest first): group by person, counting-sort by 17-minute time bin,
//      sessionize, attribute, and append the user_daily columns of this thread; each thread writes
//      its own Parquet part. campaign_daily is merged at the end.
//
// Contract: DATA_DIR (default /data), OUTPUT_DIR (default /output) or two positional arguments;
// NCPU threads. Inputs outside the fast path's ranges (more than 256 categories, a window wider
// than ~42 days) go to the legacy pipeline in legacy.rs.
// Optional switches (all leave the output unchanged): FAST_VERBOSE=1 phase times on stderr;
// FAST_THP=0 no transparent huge pages for the record arenas (on by default on Linux);
// FAST_POPULATE=1 pre-fault arena blocks (Linux); FAST_PAR_UNMAP=0 skip the parallel unmap before exit (on by default on Linux);
// FAST_PBITS=n number of buckets = 2^n; FAST_CHUNK_DIV=n parse chunks per thread; FAST_LEGACY=1
// force the legacy pipeline; FAST_BENCH=1 parser micro-benchmarks instead of a run.
mod legacy;
mod pq;

use legacy::{die, extract_json_str, hash128, list_csv, make_chunks, map_file, parse_digits, skip_header, BMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

type B = &'static [u8];

const MS_DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const SEVEN: i64 = 7 * MS_DAY;
const MAX_PRICE: u64 = 10_000_000;
const PTR48: u64 = 0xFFFF_FFFF_FFFF;

// ------------------------------------------------------------------ memory
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn clock_gettime(clk: i32, ts: *mut [i64; 2]) -> i32;
}

#[cfg(target_os = "linux")]
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
#[cfg(not(target_os = "linux"))]
const CLOCK_THREAD_CPUTIME_ID: i32 = 16;

/// CPU seconds used by the calling thread (for FAST_VERBOSE phase accounting).
fn thread_cpu() -> f64 {
    let mut ts = [0i64; 2];
    unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts[0] as f64 + ts[1] as f64 * 1e-9
}

/// (instructions retired, cycles) of the whole process so far, for FAST_VERBOSE (macOS only; 0 elsewhere).
fn insn_cycles() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buf: *mut u64) -> i32;
            fn getpid() -> i32;
        }
        let mut b = [0u64; 64];
        if unsafe { proc_pid_rusage(getpid(), 4, b.as_mut_ptr()) } == 0 {
            return (b[31], b[32]);
        }
    }
    (0, 0)
}

static CPU_ACC: Mutex<Vec<(&'static str, f64, f64)>> = Mutex::new(Vec::new());

/// Record (phase, thread cpu seconds, thread wall seconds) for the verbose summary.
fn acc(phase: &'static str, cpu: f64, wall: f64) {
    CPU_ACC.lock().unwrap().push((phase, cpu, wall));
}
#[cfg(target_os = "linux")]
const MAP_ANON_PRIVATE: i32 = 0x20 | 0x02;
#[cfg(not(target_os = "linux"))]
const MAP_ANON_PRIVATE: i32 = 0x1000 | 0x02;
#[cfg(target_os = "linux")]
const MAP_POPULATE: i32 = 0x8000;
#[cfg(not(target_os = "linux"))]
const MAP_POPULATE: i32 = 0;
const MADV_HUGEPAGE: i32 = 14;

/// Linux only. Transparent huge pages are requested for arena blocks and the link table unless
/// FAST_THP=0 (measured on a 2 vCPU amd64 runner at 20M events: 17% less CPU; the madvise call is
/// advisory, so a kernel with THP set to never just ignores it). FAST_POPULATE=1 (off by default)
/// pre-faults the blocks in the mmap call.
static ARENA_FLAGS: OnceLock<(bool, bool)> = OnceLock::new();

fn big_alloc(len: usize) -> usize {
    let (thp, populate) = *ARENA_FLAGS.get_or_init(|| {
        let on = |k: &str| std::env::var(k).map_or(false, |v| v == "1");
        let not_off = |k: &str| std::env::var(k).map_or(true, |v| v != "0");
        (cfg!(target_os = "linux") && not_off("FAST_THP"), cfg!(target_os = "linux") && on("FAST_POPULATE"))
    });
    const HUGE: usize = 2 << 20;
    let flags = MAP_ANON_PRIVATE | if populate { MAP_POPULATE } else { 0 };
    let p = unsafe { mmap(std::ptr::null_mut(), len + if thp { HUGE } else { 0 }, 3, flags, -1, 0) };
    if p as isize == -1 || p.is_null() {
        die("out of memory");
    }
    let mut a = p as usize;
    if thp {
        a = (a + HUGE - 1) & !(HUGE - 1);
        unsafe { madvise(a as *mut u8, len, MADV_HUGEPAGE) };
    }
    a
}

const CHUNK: usize = 16384; // bytes per bucket chunk (chunks are CHUNK-aligned)
const BLOCK: usize = 32 << 20; // arena block
const HDR: usize = 8; // chunk header: address of the bucket's previous chunk (0 = none)

/// Never-freed chunk allocator (one per thread); consumed chunks are recycled through `free`.
struct Arena {
    cur: usize,
    end: usize,
    free: Vec<usize>,
    blocks: Vec<usize>,
}

impl Arena {
    fn new() -> Arena {
        Arena { cur: 0, end: 0, free: Vec::new(), blocks: Vec::new() }
    }
    #[inline]
    fn chunk(&mut self) -> usize {
        if let Some(p) = self.free.pop() {
            return p;
        }
        if self.cur + CHUNK > self.end {
            let raw = big_alloc(BLOCK + CHUNK);
            self.cur = (raw + CHUNK - 1) & !(CHUNK - 1);
            self.end = self.cur + BLOCK;
            self.blocks.push(raw);
        }
        let p = self.cur;
        self.cur += CHUNK;
        p
    }
}

/// Append-only list of records in fixed-size chunks. The whole state is the write pointer: the
/// current chunk is found by rounding it down, and each chunk's header links to the previous one.
#[derive(Clone, Copy)]
struct Bucket<T> {
    wp: usize,
    _t: std::marker::PhantomData<T>,
}
unsafe impl<T> Send for Bucket<T> {}
unsafe impl<T> Sync for Bucket<T> {}

const BK_EMPTY: usize = CHUNK - 1;

impl<T: Copy> Bucket<T> {
    const SZ: usize = std::mem::size_of::<T>();
    const CAP: usize = (CHUNK - HDR) / Self::SZ;
    fn new() -> Self {
        Bucket { wp: BK_EMPTY, _t: std::marker::PhantomData }
    }
    /// Reserves room for one record and returns its address.
    #[inline(always)]
    fn slot(&mut self, ar: &mut Arena) -> *mut T {
        if ((self.wp.wrapping_sub(1)) & (CHUNK - 1)) + 1 + Self::SZ > CHUNK {
            self.grow(ar);
        }
        let p = self.wp as *mut T;
        self.wp += Self::SZ;
        p
    }
    #[inline(always)]
    fn push(&mut self, v: T, ar: &mut Arena) {
        let p = self.slot(ar);
        unsafe { p.write_unaligned(v) };
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self, ar: &mut Arena) {
        let prev = if self.wp == BK_EMPTY { 0 } else { (self.wp - 1) & !(CHUNK - 1) };
        let c = ar.chunk();
        unsafe { (c as *mut usize).write(prev) };
        self.wp = c + HDR;
    }
    fn len(&self) -> usize {
        let mut v = Vec::new();
        self.slices(&mut v);
        v.iter().map(|x| x.1).sum()
    }
    /// (first record, count) of every chunk; the chunk itself starts HDR bytes before the pointer.
    fn slices(&self, out: &mut Vec<(*mut T, usize)>) {
        if self.wp == BK_EMPTY {
            return;
        }
        let mut c = (self.wp - 1) & !(CHUNK - 1);
        let mut n = (self.wp - c - HDR) / Self::SZ;
        loop {
            out.push(((c + HDR) as *mut T, n));
            c = unsafe { (c as *const usize).read() };
            if c == 0 {
                break;
            }
            n = Self::CAP;
        }
    }
}

// ------------------------------------------------------------------ packed strings
// A string slice of the input is packed as ptr (48 bits) | len (16 bits). Strings of 65535 bytes or
// more go to a side registry: len field 0xFFFF and the ptr field holds the registry index.
static LONG: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

#[inline(always)]
fn pack_str(s: B) -> u64 {
    if s.len() < 0xFFFF {
        (s.as_ptr() as u64) | ((s.len() as u64) << 48)
    } else {
        pack_long(s)
    }
}
#[cold]
fn pack_long(s: B) -> u64 {
    let mut v = LONG.lock().unwrap();
    v.push((s.as_ptr() as usize, s.len()));
    ((v.len() - 1) as u64) | (0xFFFFu64 << 48)
}
#[inline(always)]
fn unpack_str(p: u64) -> B {
    let len = (p >> 48) as usize;
    if len != 0xFFFF {
        unsafe { std::slice::from_raw_parts((p & PTR48) as *const u8, len) }
    } else {
        unpack_long(p)
    }
}
#[cold]
fn unpack_long(p: u64) -> B {
    let v = LONG.lock().unwrap();
    let (a, l) = v[(p & PTR48) as usize];
    unsafe { std::slice::from_raw_parts(a as *const u8, l) }
}

const HK1: u64 = 0xa076_1d64_78bd_642f;
const HK2: u64 = 0xe703_7ed1_a0b4_28db;
const HK3: u64 = 0x8ebc_6af0_9c88_c6e3;
const HK4: u64 = 0x5899_65cc_7537_4cc3;

/// 64 + 64 hash bits of a string. Strings of 1..=16 bytes (every id in practice) take two 64x64
/// multiplies over two words that determine the string together with its length (wyhash's short
/// path); longer strings and the empty string use the general hash128.
#[inline(always)]
fn fhash(s: &[u8]) -> (u64, u64) {
    let n = s.len();
    let p = s.as_ptr();
    let (a, b);
    if n.wrapping_sub(8) <= 8 {
        a = ld8(p);
        b = ld8(unsafe { p.add(n - 8) });
    } else if n.wrapping_sub(4) <= 3 {
        a = ld4(p);
        b = ld4(unsafe { p.add(n - 4) });
    } else if n.wrapping_sub(1) <= 2 {
        a = unsafe { (*p as u64) | ((*p.add(n >> 1) as u64) << 8) | ((*p.add(n - 1) as u64) << 16) };
        b = 0;
    } else {
        return hash128(s);
    }
    let m = ((a ^ HK1) as u128).wrapping_mul((b ^ HK2) as u128);
    let (lo, hi) = (m as u64, (m >> 64) as u64);
    let m2 = ((lo ^ HK3 ^ (n as u64)) as u128).wrapping_mul((hi ^ HK4) as u128);
    ((m2 as u64) ^ ((m2 >> 64) as u64), lo ^ hi.rotate_left(23))
}

/// Byte equality without a call for the short strings that dominate.
#[inline(always)]
fn eq_bytes(x: &[u8], y: &[u8]) -> bool {
    let n = x.len();
    if n != y.len() {
        return false;
    }
    let (p, q) = (x.as_ptr(), y.as_ptr());
    if n.wrapping_sub(8) <= 8 {
        (ld8(p) == ld8(q)) & (ld8(unsafe { p.add(n - 8) }) == ld8(unsafe { q.add(n - 8) }))
    } else if n.wrapping_sub(4) <= 3 {
        (ld4(p) == ld4(q)) & (ld4(unsafe { p.add(n - 4) }) == ld4(unsafe { q.add(n - 4) }))
    } else {
        x == y
    }
}

/// User-id hash used as identity and person key; never 0 (0 marks empty table slots).
#[inline(always)]
fn uhash(s: &[u8]) -> u64 {
    let h = fhash(s).0;
    if h == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        h
    }
}

#[inline(always)]
fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

// ------------------------------------------------------------------ CSV splitting
/// Bitmasks of ',', '\n' and '"' over 64 bytes at p (bit i = byte i).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    #[cfg(target_feature = "avx512bw")]
    {
        let v = _mm512_loadu_si512(p as *const __m512i);
        let c = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8));
        let n = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8));
        let q = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8));
        (c, n, q)
    }
    #[cfg(all(target_feature = "avx2", not(target_feature = "avx512bw")))]
    {
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let (c, n, q) = (_mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b'"' as i8));
        let m = |x: __m256i, y: __m256i| -> u64 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32 as u64 };
        (m(a, c) | (m(b, c) << 32), m(a, n) | (m(b, n) << 32), m(a, q) | (m(b, q) << 32))
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        let v = [
            _mm_loadu_si128(p as *const __m128i),
            _mm_loadu_si128(p.add(16) as *const __m128i),
            _mm_loadu_si128(p.add(32) as *const __m128i),
            _mm_loadu_si128(p.add(48) as *const __m128i),
        ];
        let m = |ch: u8| -> u64 {
            let c = _mm_set1_epi8(ch as i8);
            let mut r = 0u64;
            for (i, x) in v.iter().enumerate() {
                r |= (_mm_movemask_epi8(_mm_cmpeq_epi8(*x, c)) as u32 as u64) << (16 * i);
            }
            r
        };
        (m(b','), m(b'\n'), m(b'"'))
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::aarch64::*;
    const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let bm = vld1q_u8(BITS.as_ptr());
    let v0 = vld1q_u8(p);
    let v1 = vld1q_u8(p.add(16));
    let v2 = vld1q_u8(p.add(32));
    let v3 = vld1q_u8(p.add(48));
    let m = |ch: u8| -> u64 {
        let c = vdupq_n_u8(ch);
        let t0 = vandq_u8(vceqq_u8(v0, c), bm);
        let t1 = vandq_u8(vceqq_u8(v1, c), bm);
        let t2 = vandq_u8(vceqq_u8(v2, c), bm);
        let t3 = vandq_u8(vceqq_u8(v3, c), bm);
        let s = vpaddq_u8(vpaddq_u8(t0, t1), vpaddq_u8(t2, t3));
        let s = vpaddq_u8(s, s);
        vgetq_lane_u64::<0>(vreinterpretq_u64_u8(s))
    };
    (m(b','), m(b'\n'), m(b'"'))
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    let (mut c, mut n, mut q) = (0u64, 0u64, 0u64);
    for i in 0..64 {
        let b = *p.add(i);
        c |= ((b == b',') as u64) << i;
        n |= ((b == b'\n') as u64) << i;
        q |= ((b == b'"') as u64) << i;
    }
    (c, n, q)
}

/// Reference splitter (one pass, branchy): used for chunks of 4 GB or more and by the tests.
/// Calls f with the N fields of every well-formed line of buf (quotes around whole fields removed).
/// Same dialect as the oracle's split_line: a line needs exactly N fields, and its quote count must
/// equal twice the number of fields wrapped in quotes.
#[inline(always)]
fn for_each_line_ref<const N: usize, F: FnMut(&Flds<N>)>(buf: B, mut f: F) {
    let len = buf.len();
    let base = buf.as_ptr();
    let mut ends = [0usize; 16]; // comma positions of the current line, then its end at [N - 1]
    let mut kc = 0usize; // commas of the current line seen in earlier blocks
    let mut ls = 0usize; // line start
    let mut qls = 0u32; // quotes before the line start
    let mut qb = 0u32; // quotes before the current block
    let mut pos = 0usize;
    while pos < len {
        let rem = len - pos;
        let (cm, mut nm, qm) = if rem >= 64 {
            unsafe { masks64(base.add(pos)) }
        } else {
            let mut tmp = [0u8; 64];
            tmp[..rem].copy_from_slice(&buf[pos..]);
            unsafe { masks64(tmp.as_ptr()) }
        };
        let mut cbits = cm;
        while nm != 0 {
            let i = nm.trailing_zeros() as usize;
            nm &= nm - 1;
            let below = (1u64 << i) - 1;
            let cl = cbits & below;
            cbits &= !below;
            let qh = qb + (qm & below).count_ones();
            let at = pos + i;
            if kc + cl.count_ones() as usize == N - 1 {
                put_bits(&mut ends, kc, cl, pos);
                ends[N - 1] = at;
                emit(buf, ls, &ends, qh - qls, &mut f);
            }
            kc = 0;
            ls = at + 1;
            qls = qh;
        }
        // commas after the block's last newline belong to the line that continues
        let rc = cbits.count_ones() as usize;
        if kc + rc <= N - 1 {
            put_bits(&mut ends, kc, cbits, pos);
        }
        kc += rc;
        qb += qm.count_ones();
        pos += 64;
    }
    if ls < len && kc == N - 1 {
        ends[N - 1] = len;
        emit(buf, ls, &ends, qb - qls, &mut f);
    }
}

/// Bytes of input indexed per batch by the two-stage splitter (a multiple of 64).
const SPLIT_BATCH: usize = 16 << 10;

/// Fields of one line: offsets from a base pointer and lengths, quotes around whole fields removed.
#[repr(C, align(32))]
struct Row {
    s: [u32; 16],
    l: [u32; 16],
}

/// The N fields of one line, handed to the line callbacks.
struct Flds<'a, const N: usize> {
    base: *const u8,
    row: &'a Row,
}

impl<const N: usize> Flds<'_, N> {
    #[inline(always)]
    fn get(&self, j: usize) -> B {
        debug_assert!(j < N);
        unsafe { std::slice::from_raw_parts(self.base.add(*self.row.s.get_unchecked(j) as usize), *self.row.l.get_unchecked(j) as usize) }
    }
}

/// Per-thread scratch of the two-stage splitter.
struct Split {
    seps: Vec<u32>, // [0]: start of the carried line - 1; then positions of ',' and '\n' of the batch
    nls: Vec<u64>,  // per '\n' of the batch: index into seps << 32 | quotes before it
    row: Row,
}

impl Split {
    fn new() -> Split {
        Split { seps: vec![0u32; SPLIT_BATCH + 129], nls: vec![0u64; SPLIT_BATCH + 8], row: Row { s: [0; 16], l: [0; 16] } }
    }
}

/// Stage 2 for one line of N <= 8 fields with AVX2: sepa[-1] is the line start - 1 and
/// sepa[0..N] are its separators. The first and last byte of every field are collected into two
/// vectors (plain byte loads; no gather, whose cost depends on microcode mitigations), then all
/// fields are unquoted at once and the row is written. Returns (fields wrapped in quotes, no field
/// is one lone quote).
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn fields_avx2<const N: usize>(base: *const u8, sepa: *const u32, row: &mut Row) -> (u32, bool) {
    use std::arch::x86_64::*;
    let mut c0 = _mm_setzero_si128();
    let mut c1 = _mm_setzero_si128();
    macro_rules! fld {
        ($j:literal) => {
            if $j < N {
                // start = previous separator + 1 (u32 arithmetic: the first line starts after "-1")
                let st = (*sepa.add($j).sub(1)).wrapping_add(1) as usize;
                let e = *sepa.add($j) as usize;
                c0 = _mm_insert_epi8::<$j>(c0, *base.add(st) as i32);
                c1 = _mm_insert_epi8::<$j>(c1, *base.add(e).sub(1) as i32);
            }
        };
    }
    fld!(0);
    fld!(1);
    fld!(2);
    fld!(3);
    fld!(4);
    fld!(5);
    fld!(6);
    fld!(7);
    let q = _mm_set1_epi8(0x22);
    let w8 = _mm_and_si128(_mm_cmpeq_epi8(c0, q), _mm_cmpeq_epi8(c1, q));
    let w = _mm256_cvtepi8_epi32(w8); // -1 in wrapped lanes
    let ep = _mm256_loadu_si256(sepa.sub(1) as *const __m256i);
    let e = _mm256_loadu_si256(sepa as *const __m256i);
    let s = _mm256_sub_epi32(ep, _mm256_set1_epi32(-1));
    // wrapped: start + 1, length - 2 (a lone quote gives length -1)
    let l = _mm256_add_epi32(_mm256_sub_epi32(e, s), _mm256_add_epi32(w, w));
    _mm256_storeu_si256(row.s.as_mut_ptr() as *mut __m256i, _mm256_sub_epi32(s, w));
    _mm256_storeu_si256(row.l.as_mut_ptr() as *mut __m256i, l);
    let lane_bits = (1u32 << N) - 1;
    let wm = _mm_movemask_epi8(w8) as u32 & lane_bits;
    let neg = _mm256_movemask_ps(_mm256_castsi256_ps(l)) as u32 & lane_bits;
    (wm.count_ones(), neg == 0)
}

/// Stage 2 for one line starting at ls whose separators are sepa[0..N], one field at a time.
#[inline(always)]
unsafe fn fields_scalar<const N: usize>(base: *const u8, ls: usize, sepa: *const u32, row: &mut Row) -> (u32, bool) {
    let mut s = ls;
    let mut wrapped = 0u32;
    let mut neg = 0u32;
    for j in 0..N {
        let e = *sepa.add(j) as usize;
        // s <= e < len. buf[e - 1] is the byte before the buffer when the first field of the buffer
        // is empty (readable by contract); w is 0 then because buf[s] is the separator.
        let c0 = *base.add(s);
        let c1 = *base.add(e).sub(1);
        let w = ((c0 == b'"') & (c1 == b'"')) as usize;
        // a field that is one lone quote gives -1: the line is malformed
        let raw = ((e - s) as u32).wrapping_sub(2 * w as u32);
        neg |= raw;
        *row.s.get_unchecked_mut(j) = (s + w) as u32;
        *row.l.get_unchecked_mut(j) = raw;
        wrapped += w as u32;
        s = e + 1;
    }
    (wrapped, (neg as i32) >= 0)
}

/// Calls f with the N fields of every well-formed line of buf (quotes around whole fields removed).
/// Same dialect as the oracle's split_line: a line needs exactly N fields, and its quote count must
/// equal twice the number of fields wrapped in quotes.
///
/// The byte before `buf` must be readable (every caller passes a buffer that follows a header line).
///
/// Two stages per 16 KB batch, so that neither has a data-dependent inner loop: (1) SIMD masks of
/// separators are flattened into `seps`, and every newline records its index in `seps` and the quote
/// count before it; (2) one iteration per line takes its N separators and strips quotes branch-free
/// (with AVX2: all fields at once).
#[inline(always)]
fn for_each_line<const N: usize, F: FnMut(&Flds<N>)>(buf: B, sp: &mut Split, mut f: F) {
    let len = buf.len();
    if len >= (1usize << 31) - 64 || N < 2 || N > 16 {
        return for_each_line_ref::<N, F>(buf, f);
    }
    let base = buf.as_ptr();
    let seps = unsafe { sp.seps.as_mut_ptr().add(1) }; // seps[-1] = start of the current line - 1
    let nls = sp.nls.as_mut_ptr();
    let row = &mut sp.row;
    unsafe { *seps.sub(1) = u32::MAX };
    let mut nsep = 0usize; // entries in seps
    let mut a = 0isize; // index in seps of the first comma of the current line (very negative: line is bad)
    let mut ls = 0usize; // start of the current line
    let mut qb = 0u32; // quotes before the current block
    let mut qls = 0u32; // quotes before the current line
    let mut pos = 0usize;
    while pos < len {
        // stage 1: index up to SPLIT_BATCH bytes
        let stop = (pos + SPLIT_BATCH).min(len);
        let mut nnl = 0usize;
        while pos < stop {
            let rem = len - pos;
            let (cm, nm, qm) = if rem >= 64 {
                unsafe { masks64(base.add(pos)) }
            } else {
                let mut tmp = [0u8; 64];
                tmp[..rem].copy_from_slice(&buf[pos..]);
                unsafe { masks64(tmp.as_ptr()) }
            };
            let sm = cm | nm;
            let cnt = sm.count_ones() as usize;
            unsafe {
                let out = seps.add(nsep);
                let p32 = pos as u32;
                let mut b = sm;
                for i in 0..8 {
                    *out.add(i) = p32 + b.trailing_zeros();
                    b &= b.wrapping_sub(1);
                }
                if cnt > 8 {
                    for i in 8..16 {
                        *out.add(i) = p32 + b.trailing_zeros();
                        b &= b.wrapping_sub(1);
                    }
                    let mut i = 16;
                    while b != 0 {
                        *out.add(i) = p32 + b.trailing_zeros();
                        b &= b.wrapping_sub(1);
                        i += 1;
                    }
                }
                // newlines: index in seps and quotes before each
                let mut n = nm;
                macro_rules! one_nl {
                    () => {{
                        let below = (1u64 << (n.trailing_zeros() & 63)).wrapping_sub(1);
                        *nls.add(nnl) = (((nsep as u64) + (sm & below).count_ones() as u64) << 32) | (qb + (qm & below).count_ones()) as u64;
                        nnl += (n != 0) as usize;
                        n &= n.wrapping_sub(1);
                    }};
                }
                one_nl!();
                if N <= 4 {
                    one_nl!();
                }
                while n != 0 {
                    one_nl!();
                }
            }
            nsep += cnt;
            qb += qm.count_ones();
            pos += 64;
        }
        // stage 2: one iteration per line that ends in this batch
        for k in 0..nnl {
            let v = unsafe { *nls.add(k) };
            let b = (v >> 32) as usize;
            let q = (v as u32).wrapping_sub(qls);
            qls = v as u32;
            let end = unsafe { *seps.add(b) } as usize;
            if b as isize - a == (N - 1) as isize {
                let sepa = unsafe { seps.add(a as usize) };
                if q == 0 {
                    // Fast path: no quotes anywhere in this line
                    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
                    unsafe {
                        use std::arch::x86_64::*;
                        let ep = _mm256_loadu_si256(sepa.sub(1) as *const __m256i);
                        let e = _mm256_loadu_si256(sepa as *const __m256i);
                        let s = _mm256_sub_epi32(ep, _mm256_set1_epi32(-1));
                        let l = _mm256_sub_epi32(e, s);
                        _mm256_storeu_si256(row.s.as_mut_ptr() as *mut __m256i, s);
                        _mm256_storeu_si256(row.l.as_mut_ptr() as *mut __m256i, l);
                    }
                    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
                    unsafe {
                        let mut s = ls;
                        for j in 0..N {
                            let e = *sepa.add(j) as usize;
                            *row.s.get_unchecked_mut(j) = s as u32;
                            *row.l.get_unchecked_mut(j) = (e - s) as u32;
                            s = e + 1;
                        }
                    }
                    f(&Flds { base, row });
                } else {
                    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
                    let (wrapped, ok) = if N <= 8 { unsafe { fields_avx2::<N>(base, sepa, row) } } else { unsafe { fields_scalar::<N>(base, ls, sepa, row) } };
                    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
                    let (wrapped, ok) = unsafe { fields_scalar::<N>(base, ls, sepa, row) };
                    if (2 * wrapped == q) & ok {
                        f(&Flds { base, row });
                    }
                }
            }
            a = b as isize + 1;
            ls = end + 1;
        }
        // carry the commas of the unfinished line to the front
        if a >= 0 {
            let c = nsep - a as usize;
            if c > N - 1 {
                a = isize::MIN / 2; // too many fields already: skipped at its newline
                nsep = 0;
            } else {
                unsafe {
                    std::ptr::copy(seps.add(a as usize), seps, c);
                    *seps.sub(1) = (ls as u32).wrapping_sub(1);
                }
                a = 0;
                nsep = c;
            }
        } else {
            nsep = 0;
        }
    }
    // last line without a newline
    if ls < len && a >= 0 && nsep - a as usize == N - 1 {
        let mut ends = [0usize; 16];
        for j in 0..N - 1 {
            ends[j] = unsafe { *seps.add(a as usize + j) } as usize;
        }
        ends[N - 1] = len;
        emit(buf, ls, &ends, qb - qls, &mut f);
    }
}

/// ends[k..] = positions of the set bits of `bits` (at most N - 1 - k of them; k + 8 <= 16).
#[inline(always)]
fn put_bits(ends: &mut [usize; 16], k: usize, mut bits: u64, pos: usize) {
    let mut k = k;
    while bits != 0 {
        unsafe { *ends.get_unchecked_mut(k) = pos + bits.trailing_zeros() as usize };
        bits &= bits - 1;
        k += 1;
    }
}

/// Careful path for one line [ls, ends[N - 1]) with commas at ends[..N - 1]: never reads outside
/// the line. Offsets in the row are relative to the line start.
#[inline(never)]
fn emit<const N: usize, F: FnMut(&Flds<N>)>(buf: B, ls: usize, ends: &[usize; 16], q: u32, f: &mut F) {
    if ends[N - 1] - ls >= u32::MAX as usize {
        die("line of 4 GB or more");
    }
    let mut row = Row { s: [0; 16], l: [0; 16] };
    let mut s = ls;
    let mut wrapped = 0usize;
    for j in 0..N {
        let e = ends[j];
        let mut w = 0usize;
        if e >= s + 2 {
            w = ((buf[s] == b'"') & (buf[e - 1] == b'"')) as usize;
        }
        row.s[j] = (s + w - ls) as u32;
        row.l[j] = (e - s - 2 * w) as u32;
        wrapped += 2 * w;
        s = e + 1;
    }
    if wrapped == q as usize {
        f(&Flds { base: buf[ls..].as_ptr(), row: &row });
    }
}

// ------------------------------------------------------------------ timestamps
const ZEROS: u64 = 0x3030_3030_3030_3030;

#[inline(always)]
fn ld8(p: *const u8) -> u64 {
    u64::from_le(unsafe { (p as *const u64).read_unaligned() })
}
#[inline(always)]
fn ld4(p: *const u8) -> u64 {
    u32::from_le(unsafe { (p as *const u32).read_unaligned() }) as u64
}

/// All 8 bytes are ASCII digits.
#[inline(always)]
fn digits8(x: u64) -> bool {
    ((x & 0xF0F0_F0F0_F0F0_F0F0) | ((x.wrapping_add(0x0606_0606_0606_0606) & 0xF0F0_F0F0_F0F0_F0F0) >> 4))
        == 0x3333_3333_3333_3333
}

/// Value of 8 ASCII digits (first digit in the lowest byte).
#[inline(always)]
fn val8(mut v: u64) -> u64 {
    v = (v & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    v = (v & 0x00FF_00FF_00FF_00FF).wrapping_mul(6553601) >> 16;
    (v & 0x0000_FFFF_0000_FFFF).wrapping_mul(42949672960001) >> 32
}

/// Memo of validated dates: the 10 bytes YYYY-MM-DD (separators already checked) -> days since epoch.
struct DateCache {
    key: [u64; 32],
    days: [i64; 32],
}

impl DateCache {
    fn new() -> DateCache {
        DateCache { key: [u64::MAX; 32], days: [0; 32] } // u64::MAX never matches a key with '-' checked
    }
}

/// Epoch ms, or None if bad (F1/F2/F3). Equivalent to legacy::parse_ts.
#[inline(always)]
fn parse_ts(s: &[u8], dc: &mut DateCache) -> Option<i64> {
    if s.len() >= 20 && s[4] == b'-' {
        parse_iso(s, dc)
    } else {
        parse_f3(s)
    }
}

#[inline(always)]
fn parse_f3(s: &[u8]) -> Option<i64> {
    let n = s.len();
    if (8..=16).contains(&n) {
        let p = s.as_ptr();
        let y = ld8(p);
        let x = ld8(unsafe { p.add(n - 8) });
        if !(digits8(x) & digits8(y)) {
            return None;
        }
        if n == 8 {
            return Some(val8(x) as i64);
        }
        let k = 16 - n; // digits of y that overlap x
        let z = if k == 0 { y } else { (y << (8 * k)) | (ZEROS >> (8 * (8 - k))) };
        return Some((val8(z) * 100_000_000 + val8(x)) as i64);
    }
    parse_f3_slow(s)
}

#[cold]
#[inline(never)]
fn parse_f3_slow(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as u64)?;
    }
    if v <= i64::MAX as u64 {
        Some(v as i64)
    } else {
        None
    }
}

#[inline(always)]
fn parse_iso(s: &[u8], dc: &mut DateCache) -> Option<i64> {
    let n = s.len();
    if n != 20 && n != 25 {
        return None;
    }
    let p = s.as_ptr();
    let a = ld8(p); // YYYY-MM-
    let b = ld8(unsafe { p.add(8) }); // DDThh:mm
    let c = ld4(unsafe { p.add(16) }); // :ssZ  or  :ss+ / :ss-
    const AS: u64 = 0xFF00_00FF_0000_0000; // bytes 4, 7
    const AV: u64 = 0x2D00_002D_0000_0000; // '-', '-'
    const BS: u64 = 0x0000_FF00_00FF_0000; // bytes 2, 5
    const BV: u64 = 0x0000_3A00_0054_0000; // 'T', ':'
    const CD: u64 = 0x00FF_FF00; // digit bytes 1, 2
    if (a & AS) != AV || (b & BS) != BV || (c & 0xFF) != b':' as u64 {
        return None;
    }
    let b2 = (b & !BS) | (ZEROS & BS);
    let c2 = (c & CD) | (ZEROS & !CD);
    if !(digits8(b2) & digits8(c2)) {
        return None;
    }
    let db = b2 - ZEROS;
    let dcs = c2 - ZEROS;
    let g = |x: u64, i: u32| ((x >> (8 * i)) & 0xFF) as i64;
    let h = g(db, 3) * 10 + g(db, 4);
    let mi = g(db, 6) * 10 + g(db, 7);
    let se = g(dcs, 1) * 10 + g(dcs, 2);
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    // date part: all 10 bytes of YYYY-MM-DD go into the key (the two '-' are fixed above)
    let key = (a & 0x00FF_FF00_FFFF_FFFF) | ((b & 0xFF) << 32) | ((b & 0xFF00) << 48);
    let slot = ((g(db, 0) * 10 + g(db, 1)) & 31) as usize; // the day of month
    let days = if dc.key[slot] == key {
        dc.days[slot]
    } else {
        let a2 = (a & !AS) | (ZEROS & AS);
        if !digits8(a2) {
            return None;
        }
        let da = a2 - ZEROS;
        let y = g(da, 0) * 1000 + g(da, 1) * 100 + g(da, 2) * 10 + g(da, 3);
        let mo = g(da, 5) * 10 + g(da, 6);
        let d = g(db, 0) * 10 + g(db, 1);
        if !(1..=12).contains(&mo) {
            return None;
        }
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let dim = match mo {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => {
                if leap {
                    29
                } else {
                    28
                }
            }
        };
        if d < 1 || d > dim {
            return None;
        }
        let days = legacy::days_from_civil(y, mo, d);
        dc.key[slot] = key;
        dc.days[slot] = days;
        days
    };
    let last = (c >> 24) as u8;
    let off: i64;
    if n == 20 {
        if last != b'Z' {
            return None;
        }
        off = 0;
    } else {
        if (last != b'+' && last != b'-') || s[22] != b':' {
            return None;
        }
        let d2 = |i: usize| -> Option<i64> {
            let x = s[i].wrapping_sub(b'0');
            let z = s[i + 1].wrapping_sub(b'0');
            if x < 10 && z < 10 {
                Some((x * 10 + z) as i64)
            } else {
                None
            }
        };
        let oh = d2(20)?;
        let om = d2(23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let o = (oh * 60 + om) * 60_000;
        off = if last == b'-' { -o } else { o };
    }
    Some((days * 86400 + h * 3600 + mi * 60 + se) * 1000 - off)
}

// ------------------------------------------------------------------ records
/// Parsed event, bucketed by event-id hash.
#[derive(Clone, Copy)]
#[repr(C)]
struct ERec {
    eh: u64,   // event-id hash (bucket: top bits; dedup bitmap: low bits)
    ik: u64,   // ingest (48 bits, biased) << 16 | 15 more event-id hash bits << 1 | valid
    pk: u64,   // person key (valid only)
    name: u64, // packed person name (valid only)
    w: u64,    // (ts - w0) << 32 | price << 8 | category (valid only)
}

/// Deduplicated valid event, bucketed by person key.
#[derive(Clone, Copy)]
#[repr(C)]
struct PRec {
    pk: u64,
    name: u64,
    w: u64,
}

/// Valid click, bucketed by person key.
#[derive(Clone, Copy)]
#[repr(C)]
struct CRec {
    k: u64,  // person key
    tc: u64, // (ts - (w0 - 7 days)) << 32 | campaign index
    cid: u64,
}

#[inline(always)]
fn ing48(ing: i64) -> u64 {
    const LIM: i64 = (1 << 47) - 1;
    if ing == ING_NONE {
        (1u64 << 48) - 1
    } else {
        (ing.clamp(-LIM - 1, LIM) + (1 << 47)) as u64
    }
}

/// Exact string -> value map for lookups on the hot path: open addressing on the 64-bit hash, with
/// the bytes compared on a hash match. Later inserts of an existing key overwrite its value.
struct StrTable<V: Copy> {
    slots: Vec<(u64, u64, V)>, // (hash, packed string, value); packed string 0 = empty
    mask: usize,
    len: usize,
}

impl<V: Copy> StrTable<V> {
    fn with_capacity(n: usize) -> StrTable<V> {
        let cap = (n * 2).max(16).next_power_of_two();
        StrTable { slots: Vec::with_capacity(cap), mask: cap - 1, len: 0 }
    }
    fn init(&mut self, zero: V) {
        if self.slots.is_empty() {
            self.slots.resize(self.mask + 1, (0, 0, zero));
        }
    }
    fn insert(&mut self, k: B, v: V) {
        self.init(v);
        if (self.len + 1) * 2 > self.slots.len() {
            let old = std::mem::take(&mut self.slots);
            let cap = old.len() * 2;
            self.mask = cap - 1;
            self.slots.resize(cap, (0, 0, v));
            for e in old {
                if e.1 != 0 {
                    let mut i = (e.0 as usize) & self.mask;
                    while self.slots[i].1 != 0 {
                        i = (i + 1) & self.mask;
                    }
                    self.slots[i] = e;
                }
            }
        }
        let h = fhash(k).0;
        let mut i = (h as usize) & self.mask;
        loop {
            let e = &mut self.slots[i];
            if e.1 == 0 {
                *e = (h, pack_str(k), v);
                self.len += 1;
                return;
            }
            if e.0 == h && eq_bytes(unpack_str(e.1), k) {
                e.2 = v;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn get(&self, k: &[u8]) -> Option<V> {
        if self.len == 0 {
            return None;
        }
        let h = fhash(k).0;
        let mut i = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.slots.get_unchecked(i) };
            if e.1 == 0 {
                return None;
            }
            if e.0 == h && eq_bytes(unpack_str(e.1), k) {
                return Some(e.2);
            }
            i = (i + 1) & self.mask;
        }
    }
}

// ------------------------------------------------------------------ links
#[inline(always)]
fn prefix8(s: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = s.len().min(8);
    b[..n].copy_from_slice(&s[..n]);
    u64::from_be_bytes(b)
}

/// One id of the links files, in a lock-free union-find keyed by the id hash.
///
/// Every thread that parses a link inserts both ids and unites them. The root of two trees with
/// the byte-wise smaller id becomes the parent, so a parent's id is always smaller than its
/// children's and the root of a component is its smallest id: the person's name.
#[repr(C, align(32))]
struct LEnt {
    key: AtomicU64,  // id hash; 0 = empty
    par: AtomicU32,  // parent slot + 1; 0 = root
    _pad: u32,
    name: AtomicU64, // packed id; after link_finish, for non-roots: the packed person name
    pre: AtomicU64,  // first 8 bytes of the id, big endian; after link_finish, for non-roots: the person key
}

struct LinkTable {
    ents: *const LEnt,
    mask: usize,
    filt: *const AtomicU64, // one bit per hashed key: clear means the id is certainly not a non-root
    fmask: usize,
    bytes: usize, // size of the ents mapping (0: static empty table)
}
unsafe impl Send for LinkTable {}
unsafe impl Sync for LinkTable {}

static EMPTY_ENT: LEnt = LEnt { key: AtomicU64::new(0), par: AtomicU32::new(0), _pad: 0, name: AtomicU64::new(0), pre: AtomicU64::new(0) };
static EMPTY_FILT: AtomicU64 = AtomicU64::new(0);

impl LinkTable {
    /// Table for at most `lines` link lines (2 ids each): load factor at most 8/9, usually ~0.3.
    fn with_lines(lines: usize) -> LinkTable {
        if lines == 0 {
            return LinkTable { ents: &EMPTY_ENT, mask: 0, filt: &EMPTY_FILT, fmask: 0, bytes: 0 };
        }
        let cap = (lines * 9 / 4 + 16).next_power_of_two();
        let bytes = cap * std::mem::size_of::<LEnt>();
        LinkTable { ents: big_alloc(bytes) as *const LEnt, mask: cap - 1, filt: &EMPTY_FILT, fmask: 0, bytes }
    }
    /// Allocates the filter for `ids` inserted ids (16 bits each).
    fn alloc_filter(&mut self, ids: usize) {
        if ids == 0 {
            return;
        }
        let fbits = (ids * 16).max(64).next_power_of_two();
        let v: Vec<AtomicU64> = (0..fbits / 64).map(|_| AtomicU64::new(0)).collect();
        self.filt = Box::leak(v.into_boxed_slice()).as_ptr();
        self.fmask = fbits - 1;
    }
    #[inline(always)]
    fn ent(&self, i: usize) -> &LEnt {
        unsafe { &*self.ents.add(i) }
    }
    #[inline(always)]
    fn fbit(&self, k: u64) -> usize {
        ((k >> 20) as usize) & self.fmask
    }
    /// False when k is certainly not a linked id below its person's name.
    #[inline(always)]
    fn maybe(&self, k: u64) -> bool {
        let b = self.fbit(k);
        (unsafe { &*self.filt.add(b >> 6) }.load(Ordering::Relaxed) >> (b & 63)) & 1 != 0
    }
    #[inline(always)]
    fn slot_addr(&self, k: u64) -> *const LEnt {
        unsafe { self.ents.add((k as usize) & self.mask) }
    }
    /// Slot of id hash k, inserting it if new (second value: this call inserted it).
    #[inline]
    fn insert(&self, k: u64, name: u64, pre: u64) -> (usize, bool) {
        let mut i = (k as usize) & self.mask;
        let mut new = false;
        loop {
            let e = self.ent(i);
            let mut cur = e.key.load(Ordering::Acquire);
            if cur == 0 {
                match e.key.compare_exchange(0, k, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        new = true;
                        cur = k;
                    }
                    Err(c) => cur = c,
                }
            }
            if cur == k {
                // Whoever uses the slot makes sure its name is there first (pre before name, so a
                // non-zero name implies pre is set). Racing writers store equal strings.
                if e.name.load(Ordering::Acquire) == 0 {
                    e.pre.store(pre, Ordering::Relaxed);
                    e.name.store(name, Ordering::Release);
                }
                return (i, new);
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Root of slot i, halving the path on the way.
    #[inline]
    fn find(&self, mut i: usize) -> usize {
        loop {
            let p = self.ent(i).par.load(Ordering::Acquire);
            if p == 0 {
                return i;
            }
            let g = self.ent(p as usize - 1).par.load(Ordering::Acquire);
            if g != 0 {
                let _ = self.ent(i).par.compare_exchange(p, g, Ordering::AcqRel, Ordering::Relaxed);
            }
            i = p as usize - 1;
        }
    }
    /// The id of slot a is byte-wise smaller than the id of slot b.
    #[inline]
    fn less(&self, a: usize, b: usize) -> bool {
        let (ea, eb) = (self.ent(a), self.ent(b));
        let (pa, pb) = (ea.pre.load(Ordering::Relaxed), eb.pre.load(Ordering::Relaxed));
        if pa != pb {
            pa < pb
        } else {
            unpack_str(ea.name.load(Ordering::Acquire)) < unpack_str(eb.name.load(Ordering::Acquire))
        }
    }
    fn unite(&self, a: usize, b: usize) {
        loop {
            let ra = self.find(a);
            let rb = self.find(b);
            if ra == rb {
                return;
            }
            let (child, parent) = if self.less(ra, rb) { (rb, ra) } else { (ra, rb) };
            if self.ent(child).par.compare_exchange(0, parent as u32 + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return;
            }
        }
    }
    /// (person key, packed person name) of a linked id that is not its person's name; None otherwise.
    /// Only after link_finish.
    #[inline(always)]
    fn lookup(&self, k: u64) -> Option<(u64, u64)> {
        let mut i = (k as usize) & self.mask;
        loop {
            let e = self.ent(i);
            let key = e.key.load(Ordering::Relaxed);
            if key == k {
                return if e.par.load(Ordering::Relaxed) != 0 { Some((e.pre.load(Ordering::Relaxed), e.name.load(Ordering::Relaxed))) } else { None };
            }
            if key == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Parses one chunk of a links file into the union-find; `mine` collects the slots this thread
/// inserted.
fn link_chunk(buf: B, lt: &LinkTable, sp: &mut Split, mine: &mut Vec<u32>) {
    const PB: usize = 16;
    let mut pend = [(0u64, 0u64, 0u64, 0u64, 0u64, 0u64); PB];
    let mut np = 0usize;
    let apply = |pend: &[(u64, u64, u64, u64, u64, u64)], mine: &mut Vec<u32>| {
        for &(ha, a, pa, hb, b, pb) in pend {
            let (ia, na) = lt.insert(ha, a, pa);
            if na {
                mine.push(ia as u32);
            }
            let (ib, nb) = lt.insert(hb, b, pb);
            if nb {
                mine.push(ib as u32);
            }
            if ia != ib {
                lt.unite(ia, ib);
            }
        }
    };
    for_each_line::<2, _>(buf, sp, |f| {
        if f.get(0).is_empty() || f.get(1).is_empty() {
            return;
        }
        let (ha, hb) = (uhash(f.get(0)), uhash(f.get(1)));
        prefetch(lt.slot_addr(ha));
        prefetch(lt.slot_addr(hb));
        pend[np] = (ha, pack_str(f.get(0)), prefix8(f.get(0)), hb, pack_str(f.get(1)), prefix8(f.get(1)));
        np += 1;
        if np == PB {
            apply(&pend, mine);
            np = 0;
        }
    });
    apply(&pend[..np], mine);
}

/// After all links are united: every non-root slot gets its person's key and name, and its filter
/// bit. Returns the number of non-roots.
fn link_finish(lt: &LinkTable, mine: &[u32]) -> usize {
    let mut n = 0usize;
    for (j, &i) in mine.iter().enumerate() {
        if let Some(&nx) = mine.get(j + 8) {
            prefetch(lt.ent(nx as usize) as *const LEnt);
        }
        let i = i as usize;
        let r = lt.find(i);
        if r != i {
            let (e, re) = (lt.ent(i), lt.ent(r));
            // roots are never written here, and nothing reads a non-root's name or pre any more
            e.pre.store(re.key.load(Ordering::Relaxed), Ordering::Relaxed);
            e.name.store(re.name.load(Ordering::Relaxed), Ordering::Relaxed);
            let b = lt.fbit(e.key.load(Ordering::Relaxed));
            unsafe { &*lt.filt.add(b >> 6) }.fetch_or(1u64 << (b & 63), Ordering::Relaxed);
            n += 1;
        }
    }
    n
}

struct Ctx {
    w0: i64,
    w1: i64,
    products: StrTable<u8>,
    unknown_cat: u8,
    campaigns: StrTable<u32>,
    links: LinkTable,
}

// ------------------------------------------------------------------ phase A: parse
/// Fast path for an F1 timestamp (20 bytes at p) whose date is in the cache. Some(epoch ms) only
/// when every byte has been verified; None means "ask the general parser", not "bad".
#[inline(always)]
fn f1_fast(p: *const u8, dc: &DateCache) -> Option<i64> {
    let a = ld8(p); // YYYY-MM-
    let b = ld8(unsafe { p.add(8) }); // DDThh:mm
    let t = ld8(unsafe { p.add(11) }); // hh:mm:ss
    let z = unsafe { *p.add(19) };
    const AS: u64 = 0xFF00_00FF_0000_0000; // bytes 4, 7
    const AV: u64 = 0x2D00_002D_0000_0000; // '-', '-'
    // the same key and slot as parse_iso: all 10 bytes of YYYY-MM-DD except the two '-'
    let key = (a & 0x00FF_FF00_FFFF_FFFF) | ((b & 0xFF) << 32) | ((b & 0xFF00) << 48);
    let slot = (((b & 0xF) * 10 + ((b >> 8) & 0xF)) & 31) as usize;
    // hh:mm:ss -> digit values; a byte gets its top bit set if a digit is > 9 or a ':' is missing
    let d = t ^ 0x3030_3A30_303A_3030;
    let bad = (d | d.wrapping_add(0x7676_7F76_767F_7676)) & 0x8080_8080_8080_8080;
    // bytes 0, 3, 6 = hh, mm, ss
    let pr = d.wrapping_mul(10).wrapping_add(d >> 8);
    let rng = pr.wrapping_add(0x0044_0000_4400_0068) & 0x0080_0000_8000_0080; // hh > 23, mm > 59, ss > 59
    let ok = ((a & AS) == AV) & (((b >> 16) & 0xFF) == b'T' as u64) & (bad == 0) & (rng == 0) & (z == b'Z') & (dc.key[slot] == key);
    if ok {
        let sec = (pr & 0xFF) * 3600 + ((pr >> 24) & 0xFF) * 60 + ((pr >> 48) & 0xFF);
        Some((dc.days[slot] * 86400 + sec as i64) * 1000)
    } else {
        None
    }
}

/// parse_ts for a field that is usually F1.
#[inline(always)]
fn parse_ts_f1(s: &[u8], dc: &mut DateCache) -> Option<i64> {
    if s.len() == 20 {
        if let Some(v) = f1_fast(s.as_ptr(), dc) {
            return Some(v);
        }
    }
    parse_ts(s, dc)
}

/// An event whose `ts` is not parsed and whose person is not looked up yet. Events wait in one of
/// three lists by the length of `ts` (20 = F1, 25 = F2, other = F3 or bad); a full list is split
/// again by whether the user may be a linked id, so that every loop that finishes records has
/// predictable branches. The wait also gives the prefetches of the link filter and slot time to
/// arrive, and no store address depends on a slow load.
#[derive(Clone, Copy)]
#[repr(C)]
struct Pend {
    eh: u64,
    h2v: u64, // 15 more event-id hash bits << 1 | everything but ts is valid
    ing: i64, // ING_NONE if bad
    uh: u64,
    un: u64,
    ts: u64, // packed ts field
    pc: u64, // price << 8 | category (purchases)
}

/// A click in the same waiting lists.
#[derive(Clone, Copy)]
#[repr(C)]
struct CPend {
    uh: u64,
    cid: u64,
    ts: u64,
    camp: u64,
}

const NCLS: usize = 3;
const PCAP: usize = 64;
const ING_NONE: i64 = i64::MIN;

/// Per-thread state of the parser.
struct ParseState {
    ep: Vec<Pend>, // NCLS lists of PCAP
    ecnt: [usize; NCLS],
    cp: Vec<CPend>,
    ccnt: [usize; NCLS],
    dc: DateCache,
}

impl ParseState {
    fn new() -> ParseState {
        ParseState {
            ep: vec![Pend { eh: 0, h2v: 0, ing: 0, uh: 0, un: 0, ts: 0, pc: 0 }; NCLS * PCAP],
            ecnt: [0; NCLS],
            cp: vec![CPend { uh: 0, cid: 0, ts: 0, camp: 0 }; NCLS * PCAP],
            ccnt: [0; NCLS],
            dc: DateCache::new(),
        }
    }
}

/// List of a record: 0 for a 20-byte ts, 1 for 25 bytes, 2 otherwise.
#[inline(always)]
fn class_of(ts_len: usize) -> usize {
    (ts_len != 20) as usize + ((ts_len != 20) & (ts_len != 25)) as usize
}

/// Splits the indices of a waiting list into (certainly not linked, may be linked) and prefetches
/// the link slots of the second kind. Branch-free.
#[inline(always)]
fn split_linked(links: &LinkTable, n: usize, uh: impl Fn(usize) -> u64, ix: &mut [[u8; PCAP]; 2]) -> (usize, usize) {
    let (mut n0, mut n1) = (0usize, 0usize);
    let hot = ix.as_ptr() as *const u8;
    for i in 0..n {
        let k = uh(i);
        let m = links.maybe(k);
        let a = if m { links.slot_addr(k) as *const u8 } else { hot };
        prefetch(a);
        ix[0][n0] = i as u8;
        ix[1][n1] = i as u8;
        n0 += !m as usize;
        n1 += m as usize;
    }
    (n0, n1)
}

#[inline(always)]
fn ev_one<const C: usize, const M: usize>(r: &Pend, dc: &mut DateCache, ctx: &Ctx, bk: &mut [Bucket<ERec>], ar: &mut Arena, shift: u32) {
    let (w0, w1) = (ctx.w0, ctx.w1);
    let s = unpack_str(r.ts);
    let ts = if C == 0 { parse_ts_f1(s, dc) } else { parse_ts(s, dc) };
    let (tv, tok) = match ts {
        Some(t) => (t, true),
        None => (0, false),
    };
    let valid = ((r.h2v & 1) != 0) & tok & (tv >= w0) & (tv < w1) & (r.ing != ING_NONE) & (r.ing.saturating_sub(tv) <= MS_DAY);
    let w = if valid { ((tv.wrapping_sub(w0) as u64) << 32) | r.pc } else { 0 };
    let ik = (ing48(r.ing) << 16) | (r.h2v & 0xFFFE) | valid as u64;
    let (pk, name) = if M == 1 { ctx.links.lookup(r.uh).unwrap_or((r.uh, r.un)) } else { (r.uh, r.un) };
    unsafe { bk.get_unchecked_mut((r.eh >> shift) as usize) }.push(ERec { eh: r.eh, ik, pk, name, w }, ar);
}

#[inline(always)]
fn ev_finish<const C: usize>(pend: &[Pend], dc: &mut DateCache, ctx: &Ctx, bk: &mut [Bucket<ERec>], ar: &mut Arena, shift: u32) {
    let mut ix = [[0u8; PCAP]; 2];
    let (n0, n1) = split_linked(&ctx.links, pend.len(), |i| pend[i].uh, &mut ix);
    for &i in &ix[0][..n0] {
        ev_one::<C, 0>(&pend[i as usize], dc, ctx, bk, ar, shift);
    }
    for &i in &ix[1][..n1] {
        ev_one::<C, 1>(&pend[i as usize], dc, ctx, bk, ar, shift);
    }
}

#[inline(never)]
fn ev_flush(c: usize, st: &mut ParseState, ctx: &Ctx, bk: &mut [Bucket<ERec>], ar: &mut Arena, shift: u32) {
    let n = std::mem::take(&mut st.ecnt[c]);
    let pend = &st.ep[c * PCAP..c * PCAP + n];
    match c {
        0 => ev_finish::<0>(pend, &mut st.dc, ctx, bk, ar, shift),
        1 => ev_finish::<1>(pend, &mut st.dc, ctx, bk, ar, shift),
        _ => ev_finish::<2>(pend, &mut st.dc, ctx, bk, ar, shift),
    }
}

fn parse_events(buf: B, ctx: &Ctx, bk: &mut [Bucket<ERec>], ar: &mut Arena, shift: u32, sp: &mut Split, st: &mut ParseState) {
    const VIEW: u64 = u32::from_le_bytes(*b"view") as u64;
    const CART: u64 = u32::from_le_bytes(*b"cart") as u64;
    const PURCHASE: u64 = u64::from_le_bytes(*b"purchase");
    for_each_line::<7, _>(buf, sp, |f| {
        let eid = f.get(0);
        if eid.is_empty() {
            return;
        }
        let (h1, h2) = fhash(eid);
        let ing = parse_ts_f1(f.get(3), &mut st.dc).unwrap_or(ING_NONE);
        let mut pre = !f.get(1).is_empty() & !f.get(5).is_empty();
        let mut pc = 0u64;
        let t = f.get(4);
        if t.len() == 4 {
            let x = ld4(t.as_ptr());
            pre &= (x == VIEW) | (x == CART);
        } else if t.len() == 8 && ld8(t.as_ptr()) == PURCHASE {
            match parse_digits(f.get(6)) {
                Some(p) if (1..=MAX_PRICE).contains(&p) => {
                    let cat = ctx.products.get(f.get(5)).unwrap_or(ctx.unknown_cat);
                    pc = (p << 8) | cat as u64;
                }
                _ => pre = false,
            }
        } else {
            pre = false;
        }
        let uh = uhash(f.get(1));
        let c = class_of(f.get(2).len());
        let i = st.ecnt[c];
        unsafe {
            *st.ep.get_unchecked_mut(c * PCAP + i) =
                Pend { eh: h1, h2v: ((h2 & 0x7FFF) << 1) | pre as u64, ing, uh, un: pack_str(f.get(1)), ts: pack_str(f.get(2)), pc };
        }
        st.ecnt[c] = i + 1;
        if i + 1 == PCAP {
            ev_flush(c, st, ctx, bk, ar, shift);
        }
    });
    for c in 0..NCLS {
        ev_flush(c, st, ctx, bk, ar, shift);
    }
}

#[inline(always)]
fn cl_one<const C: usize, const M: usize>(r: &CPend, dc: &mut DateCache, ctx: &Ctx, bk: &mut [Bucket<CRec>], ar: &mut Arena, shift: u32) {
    let lo = ctx.w0 - SEVEN;
    let s = unpack_str(r.ts);
    let ts = if C == 0 { parse_ts_f1(s, dc) } else { parse_ts(s, dc) };
    let t = match ts {
        Some(t) if t >= lo && t < ctx.w1 => t,
        _ => return,
    };
    let pk = if M == 1 { ctx.links.lookup(r.uh).map_or(r.uh, |x| x.0) } else { r.uh };
    unsafe { bk.get_unchecked_mut((pk >> shift) as usize) }.push(CRec { k: pk, tc: (((t - lo) as u64) << 32) | r.camp, cid: r.cid }, ar);
}

#[inline(always)]
fn cl_finish<const C: usize>(pend: &[CPend], dc: &mut DateCache, ctx: &Ctx, bk: &mut [Bucket<CRec>], ar: &mut Arena, shift: u32) {
    let mut ix = [[0u8; PCAP]; 2];
    let (n0, n1) = split_linked(&ctx.links, pend.len(), |i| pend[i].uh, &mut ix);
    for &i in &ix[0][..n0] {
        cl_one::<C, 0>(&pend[i as usize], dc, ctx, bk, ar, shift);
    }
    for &i in &ix[1][..n1] {
        cl_one::<C, 1>(&pend[i as usize], dc, ctx, bk, ar, shift);
    }
}

#[inline(never)]
fn cl_flush(c: usize, st: &mut ParseState, ctx: &Ctx, bk: &mut [Bucket<CRec>], ar: &mut Arena, shift: u32) {
    let n = std::mem::take(&mut st.ccnt[c]);
    let pend = &st.cp[c * PCAP..c * PCAP + n];
    match c {
        0 => cl_finish::<0>(pend, &mut st.dc, ctx, bk, ar, shift),
        1 => cl_finish::<1>(pend, &mut st.dc, ctx, bk, ar, shift),
        _ => cl_finish::<2>(pend, &mut st.dc, ctx, bk, ar, shift),
    }
}

fn parse_clicks(buf: B, ctx: &Ctx, bk: &mut [Bucket<CRec>], ar: &mut Arena, shift: u32, sp: &mut Split, st: &mut ParseState) {
    for_each_line::<4, _>(buf, sp, |f| {
        let cid = f.get(0);
        if cid.is_empty() || f.get(1).is_empty() {
            return;
        }
        let camp = match ctx.campaigns.get(f.get(2)) {
            Some(c) => c,
            None => return,
        };
        let uh = uhash(f.get(1));
        let c = class_of(f.get(3).len());
        let i = st.ccnt[c];
        unsafe {
            *st.cp.get_unchecked_mut(c * PCAP + i) = CPend { uh, cid: pack_str(cid), ts: pack_str(f.get(3)), camp: camp as u64 };
        }
        st.ccnt[c] = i + 1;
        if i + 1 == PCAP {
            cl_flush(c, st, ctx, bk, ar, shift);
        }
    });
    for c in 0..NCLS {
        cl_flush(c, st, ctx, bk, ar, shift);
    }
}

// ------------------------------------------------------------------ phase B: dedup
/// Per-thread scratch of the dedup.
struct Dedup {
    bm: Vec<u64>,          // one bit per hashed event id seen
    sb: Vec<u64>,          // bits that were hit twice
    sus: Vec<u32>,         // those bit indices
    fl: Vec<*const ERec>,  // records on a bit that was hit twice
    tab: Vec<u32>,         // exact table over `fl`: index + 1
}

impl Dedup {
    fn new() -> Dedup {
        Dedup { bm: Vec::new(), sb: Vec::new(), sus: Vec::new(), fl: Vec::new(), tab: Vec::new() }
    }
}

/// Dedups one event-id bucket (the copy with the smallest ingest wins, before validation) and
/// routes the valid survivors to person buckets.
///
/// Duplicates are rare, so they are first separated with a bitmap of 16 bits per record: a record
/// whose bit nobody else hit is unique and is emitted straight away; the others (duplicates and
/// the few bitmap collisions) go through an exact table.
fn dedup_part(slices: &[(*mut ERec, usize)], dd: &mut Dedup, out: &mut [Bucket<PRec>], ar: &mut Arena, pshift: u32) {
    let n: usize = slices.iter().map(|s| s.1).sum();
    if n == 0 {
        return;
    }
    let m = (n * 16).next_power_of_two().clamp(1024, 1 << 31); // bits
    let bmask = (m - 1) as u64;
    dd.bm.clear();
    dd.bm.resize(m / 64, 0);
    dd.sus.clear();
    dd.sus.reserve(n + 1);
    let bm = dd.bm.as_mut_ptr();
    let sus = dd.sus.as_mut_ptr();
    let mut ns = 0usize;
    for &(p, len) in slices {
        for i in 0..len {
            unsafe {
                let idx = (*p.add(i)).eh & bmask;
                let wd = bm.add((idx >> 6) as usize);
                let bit = 1u64 << (idx & 63);
                let old = *wd;
                *wd = old | bit;
                *sus.add(ns) = idx as u32;
                ns += ((old & bit) != 0) as usize;
            }
        }
    }
    let emit = |r: &ERec, out: &mut [Bucket<PRec>], ar: &mut Arena| {
        if r.ik & 1 != 0 {
            unsafe { out.get_unchecked_mut((r.pk >> pshift) as usize) }.push(PRec { pk: r.pk, name: r.name, w: r.w }, ar);
        }
    };
    if ns == 0 {
        for &(p, len) in slices {
            for i in 0..len {
                emit(unsafe { &*p.add(i) }, out, ar);
            }
        }
    } else {
        unsafe { dd.sus.set_len(ns) };
        dd.sb.clear();
        dd.sb.resize(m / 64, 0);
        for &idx in &dd.sus {
            dd.sb[(idx >> 6) as usize] |= 1u64 << (idx & 63);
        }
        dd.fl.clear();
        let sb = dd.sb.as_ptr();
        for &(p, len) in slices {
            for i in 0..len {
                let rp = unsafe { p.add(i) };
                let r = unsafe { &*rp };
                let idx = r.eh & bmask;
                if (unsafe { *sb.add((idx >> 6) as usize) } >> (idx & 63)) & 1 == 0 {
                    emit(r, out, ar);
                } else {
                    dd.fl.push(rp);
                }
            }
        }
        let nf = dd.fl.len();
        let cap = (nf * 2).next_power_of_two().max(16);
        let mask = cap - 1;
        dd.tab.clear();
        dd.tab.resize(cap, 0);
        for i in 0..nf {
            let r = unsafe { &*dd.fl[i] };
            let mut s = ((r.eh >> 28) as usize) & mask;
            loop {
                let t = dd.tab[s];
                if t == 0 {
                    dd.tab[s] = i as u32 + 1;
                    break;
                }
                let o = unsafe { &*dd.fl[t as usize - 1] };
                if o.eh == r.eh && ((o.ik ^ r.ik) & 0xFFFE) == 0 {
                    if (r.ik >> 16) < (o.ik >> 16) {
                        dd.tab[s] = i as u32 + 1;
                    }
                    break;
                }
                s = (s + 1) & mask;
            }
        }
        for &t in &dd.tab {
            if t != 0 {
                emit(unsafe { &*dd.fl[t as usize - 1] }, out, ar);
            }
        }
    }
    for &(p, _) in slices {
        ar.free.push(p as usize - HDR);
    }
}

// ------------------------------------------------------------------ phase C: sessionize
/// Events of one person are grouped by time bin (2^20 ms) instead of being sorted: a bin is shorter
/// than the session gap, so events of one bin are always in the same session, and whether two
/// consecutive non-empty bins are split depends only on the largest ts of the first and the
/// smallest ts of the second.
const BIN_SHIFT: u32 = 52; // w >> 52 = (ts - w0) >> 20

/// campaign_daily counters of one (campaign, day), together so that closing a session touches one
/// cache line.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
#[repr(C)]
struct AggRow {
    sessions: u64,
    purch: u64,
    rev: u64,
    assisted: u64,
}

struct Agg {
    v: Vec<AggRow>, // [campaign * ndays + day]; the last campaign is "organic"
}

impl Agg {
    fn new(size: usize) -> Agg {
        Agg { v: vec![AggRow::default(); size] }
    }
}

/// user_daily columns of one thread, already in Parquet PLAIN layout.
struct Cols {
    names: Vec<u8>,
    day: Vec<u8>,
    sess: Vec<u8>,
    ev: Vec<u8>,
    rev: Vec<u8>,
    lev: Vec<u8>, // definition levels of top_category, one bit per row
    top: Vec<u8>,
    n: usize,
}

struct Worker {
    w0: i64,
    day0: i64,
    ndays: usize,
    organic: usize,
    nbins: usize,
    cat_names: &'static [B],
    max_cat: usize, // longest category name
    cols: Cols,
    agg: Agg,
    // per-person scratch
    cat_sum: Vec<u64>,
    touched: Vec<u16>,
    cnt: Vec<u32>,
    pos: Vec<u32>,
    present: Vec<u32>,
    enter: Vec<u32>,
    // per-partition scratch
    ptab: Vec<(u64, u32)>,
    pname: Vec<u64>,
    pcnt: Vec<u32>,
    poff: Vec<u32>,
    pidv: Vec<u32>,
    wv: Vec<u64>,
    hist: Vec<u32>,
    tw: Vec<u64>,
    tp: Vec<u32>,
    ws: Vec<u64>,
    ccnt: Vec<u32>,
    coff: Vec<u32>,
    cpid: Vec<u32>,
    cs: Vec<CRec>,
}

#[inline(always)]
fn camp_of(c: &CRec) -> usize {
    (c.tc & 0xFFFF_FFFF) as usize
}

/// Clicks by (ts, campaign); ties in ts are resolved by click_id where a click is credited.
fn sort_clicks(v: &mut [CRec]) {
    if v.len() <= 16 {
        for i in 1..v.len() {
            let x = v[i];
            let mut j = i;
            while j > 0 && v[j - 1].tc > x.tc {
                v[j] = v[j - 1];
                j -= 1;
            }
            v[j] = x;
        }
    } else {
        v.sort_unstable_by_key(|c| c.tc);
    }
}

/// Appends K bytes; the capacity must have been reserved.
#[inline(always)]
unsafe fn put<const K: usize>(v: &mut Vec<u8>, b: [u8; K]) {
    let l = v.len();
    std::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr().add(l), K);
    v.set_len(l + K);
}

/// Appends a string; the capacity must have been reserved. Short strings are copied with two
/// (overlapping) word moves and never read beyond the source.
#[inline(always)]
unsafe fn put_str(v: &mut Vec<u8>, s: &[u8]) {
    let l = v.len();
    let n = s.len();
    let (src, dst) = (s.as_ptr(), v.as_mut_ptr().add(l));
    if n.wrapping_sub(8) <= 8 {
        let a = (src as *const u64).read_unaligned();
        let b = (src.add(n - 8) as *const u64).read_unaligned();
        (dst as *mut u64).write_unaligned(a);
        (dst.add(n - 8) as *mut u64).write_unaligned(b);
    } else if n.wrapping_sub(4) <= 3 {
        let a = (src as *const u32).read_unaligned();
        let b = (src.add(n - 4) as *const u32).read_unaligned();
        (dst as *mut u32).write_unaligned(a);
        (dst.add(n - 4) as *mut u32).write_unaligned(b);
    } else {
        std::ptr::copy_nonoverlapping(src, dst, n);
    }
    v.set_len(l + n);
}

/// Sets the length of a scratch vector without initializing (every element is written before use).
#[inline(always)]
fn scratch<T: Copy>(v: &mut Vec<T>, n: usize) {
    v.clear();
    v.reserve(n);
    unsafe { v.set_len(n) };
}

impl Worker {
    fn new(w0: i64, w1: i64, ncamps: usize, cat_names: &'static [B], est_rows: usize) -> Worker {
        let day0 = w0.div_euclid(MS_DAY);
        let ndays = ((w1 - 1).div_euclid(MS_DAY) - day0 + 1).max(1) as usize;
        Worker {
            w0,
            day0,
            ndays,
            organic: ncamps,
            nbins: (((w1 - w0 - 1) as u64) >> 20) as usize + 1,
            cat_names,
            max_cat: cat_names.iter().map(|c| c.len()).max().unwrap_or(0),
            cols: Cols {
                names: Vec::with_capacity(est_rows * 14),
                day: Vec::with_capacity(est_rows * 4),
                sess: Vec::with_capacity(est_rows * 8),
                ev: Vec::with_capacity(est_rows * 8),
                rev: Vec::with_capacity(est_rows * 8),
                lev: Vec::with_capacity(est_rows / 8 + 8),
                top: Vec::new(),
                n: 0,
            },
            agg: Agg::new((ncamps + 1) * ndays),
            cat_sum: vec![0; cat_names.len().max(256)],
            touched: Vec::new(),
            cnt: vec![0; ncamps + 1],
            pos: vec![0; ncamps + 1],
            present: Vec::new(),
            enter: vec![0; ncamps + 1],
            ptab: Vec::new(),
            pname: Vec::new(),
            pcnt: Vec::new(),
            poff: Vec::new(),
            pidv: Vec::new(),
            wv: Vec::new(),
            hist: Vec::new(),
            tw: Vec::new(),
            tp: Vec::new(),
            ws: Vec::new(),
            ccnt: Vec::new(),
            coff: Vec::new(),
            cpid: Vec::new(),
            cs: Vec::new(),
        }
    }

    fn flush_day(&mut self, name: u64, day: i64, sessions: u32, events: u32, rev: u64) {
        if sessions == 0 {
            return;
        }
        let mut top = 0xFFFFu16;
        if !self.touched.is_empty() {
            let mut best = 0u64;
            let mut bi = 0xFFFFu16;
            for &c in &self.touched {
                let v = self.cat_sum[c as usize];
                if v > best || (v == best && c < bi) {
                    best = v;
                    bi = c;
                }
            }
            top = bi;
            for &c in &self.touched {
                self.cat_sum[c as usize] = 0;
            }
            self.touched.clear();
        }
        // room for the row was reserved by `reserve_rows`
        let c = &mut self.cols;
        let s = unpack_str(name);
        unsafe {
            put(&mut c.names, (s.len() as u32).to_le_bytes());
            put_str(&mut c.names, s);
            put(&mut c.day, (day as i32).to_le_bytes());
            put(&mut c.sess, (sessions as i64).to_le_bytes());
            put(&mut c.ev, (events as i64).to_le_bytes());
            put(&mut c.rev, (rev as i64).to_le_bytes());
            if c.n & 7 == 0 {
                put(&mut c.lev, [0u8]);
            }
            if top != 0xFFFF {
                let l = c.lev.len() - 1;
                *c.lev.get_unchecked_mut(l) |= 1 << (c.n & 7);
                let t = *self.cat_names.get_unchecked(top as usize);
                put(&mut c.top, (t.len() as u32).to_le_bytes());
                put_str(&mut c.top, t);
            }
        }
        c.n += 1;
    }

    /// Reserves room in every column for `rows` more rows of the person called `name`.
    #[inline(always)]
    fn reserve_rows(&mut self, name: u64, rows: usize) {
        let c = &mut self.cols;
        let nl = if (name >> 48) != 0xFFFF { (name >> 48) as usize } else { unpack_str(name).len() };
        c.names.reserve(rows * (4 + nl));
        c.day.reserve(rows * 4);
        c.sess.reserve(rows * 8);
        c.ev.reserve(rows * 8);
        c.rev.reserve(rows * 8);
        c.lev.reserve(rows / 8 + 2);
        c.top.reserve(rows * (4 + self.max_cat));
    }

    /// ev: the person's events grouped by time bin, bins ascending (ts in the high 32 bits);
    /// cl: their clicks sorted by ts.
    fn person(&mut self, name: u64, ev: &[u64], cl: &[CRec]) {
        let w0 = self.w0;
        let day0 = self.day0;
        let ndays = self.ndays;
        let mut cur_day = i64::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0u32, 0u32, 0u64);
        let mut in_sess = false;
        let (mut s_start, mut s_rev, mut s_npur) = (0i64, 0u64, 0u32);
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut prev = 0i64; // largest ts so far
        let mut started = 0u32; // sessions started so far
        // A campaign is "present" for a session while one of its clicks is in the session's 7-day
        // window (cnt[c] counts them). It assists every session it is present for: enter[c] is the
        // first such session not yet added to `assisted`, which is settled when the campaign leaves
        // the window, when the day changes, and when the person ends.
        macro_rules! end_session {
            () => {{
                let si = started - 1; // index of the session being closed
                let dayidx = (cur_day - day0) as usize;
                let mut cred = self.organic;
                if !cl.is_empty() {
                    let sc = s_start + SEVEN; // session start on the clicks' time base
                    while hi < cl.len() && ((cl[hi].tc >> 32) as i64) <= sc {
                        let c = camp_of(&cl[hi]);
                        if self.cnt[c] == 0 {
                            self.pos[c] = self.present.len() as u32;
                            self.present.push(c as u32);
                            self.enter[c] = si;
                        }
                        self.cnt[c] += 1;
                        hi += 1;
                    }
                    while lo < hi && ((cl[lo].tc >> 32) as i64) < sc - SEVEN {
                        let c = camp_of(&cl[lo]);
                        self.cnt[c] -= 1;
                        if self.cnt[c] == 0 {
                            self.agg.v[c * ndays + dayidx].assisted += (si - self.enter[c]) as u64;
                            let p = self.pos[c] as usize;
                            let last = *self.present.last().unwrap();
                            self.present.swap_remove(p);
                            if p < self.present.len() {
                                self.pos[last as usize] = p as u32;
                            }
                        }
                        lo += 1;
                    }
                    if hi > lo {
                        // latest click at or before the start; on a tie in ts the smallest click_id
                        let ts = cl[hi - 1].tc >> 32;
                        let mut j = hi - 1;
                        while j > lo && (cl[j - 1].tc >> 32) == ts {
                            j -= 1;
                        }
                        let mut best = j;
                        for k in j + 1..hi {
                            if unpack_str(cl[k].cid) < unpack_str(cl[best].cid) {
                                best = k;
                            }
                        }
                        cred = camp_of(&cl[best]);
                    }
                }
                let ix = cred * ndays + dayidx;
                let g = &mut self.agg.v[ix];
                g.sessions += 1;
                g.purch += (s_npur > 0) as u64;
                g.rev += s_rev;
            }};
        }
        let n = ev.len();
        let mut i = 0usize;
        while i < n {
            // one run of events of the same bin: smallest and largest ts
            let e0 = unsafe { *ev.get_unchecked(i) };
            let bin = e0 >> BIN_SHIFT;
            let (mut mn, mut mx, mut any) = (e0 >> 32, e0 >> 32, e0);
            let mut j = i + 1;
            while j < n {
                let e = unsafe { *ev.get_unchecked(j) };
                if (e >> BIN_SHIFT) != bin {
                    break;
                }
                let t = e >> 32;
                mn = mn.min(t);
                mx = mx.max(t);
                any |= e;
                j += 1;
            }
            let (mn, mx) = (mn as i64, mx as i64);
            if !in_sess || mn - prev > GAP {
                if in_sess {
                    end_session!();
                }
                in_sess = true;
                s_start = mn;
                s_rev = 0;
                s_npur = 0;
                let day = (w0 + mn).div_euclid(MS_DAY);
                if day != cur_day {
                    if cur_day != i64::MIN {
                        self.flush_day(name, cur_day, d_sess, d_ev, d_rev);
                        let dayidx = (cur_day - day0) as usize;
                        for &c in &self.present {
                            let c = c as usize;
                            self.agg.v[c * ndays + dayidx].assisted += (started - self.enter[c]) as u64;
                            self.enter[c] = started;
                        }
                    }
                    cur_day = day;
                    d_sess = 0;
                    d_ev = 0;
                    d_rev = 0;
                }
                started += 1;
                d_sess += 1;
            }
            prev = mx;
            d_ev += (j - i) as u32;
            if (any & 0xFFFF_FF00) != 0 {
                for &e in &ev[i..j] {
                    let price = (e >> 8) & 0xFF_FFFF;
                    if price != 0 {
                        let cat = (e & 0xFF) as usize;
                        s_rev += price;
                        s_npur += 1;
                        d_rev += price;
                        if self.cat_sum[cat] == 0 {
                            self.touched.push(cat as u16);
                        }
                        self.cat_sum[cat] += price;
                    }
                }
            }
            i = j;
        }
        if in_sess {
            end_session!();
            self.flush_day(name, cur_day, d_sess, d_ev, d_rev);
            let dayidx = (cur_day - day0) as usize;
            for &c in &self.present {
                let c = c as usize;
                self.agg.v[c * ndays + dayidx].assisted += (started - self.enter[c]) as u64;
            }
        }
        for c in &cl[lo..hi] {
            self.cnt[camp_of(c)] = 0;
        }
        self.present.clear();
    }

    #[inline(always)]
    fn pfind(&self, pk: u64) -> u32 {
        let mask = self.ptab.len() - 1;
        let mut s = (pk as usize) & mask;
        loop {
            let e = unsafe { *self.ptab.get_unchecked(s) };
            if e.0 == pk {
                return e.1;
            }
            if e.0 == 0 {
                return u32::MAX;
            }
            s = (s + 1) & mask;
        }
    }

    fn pgrow(&mut self) {
        let cap = self.ptab.len() * 2;
        let old = std::mem::replace(&mut self.ptab, vec![(0u64, 0u32); cap]);
        let mask = cap - 1;
        for e in old {
            if e.0 != 0 {
                let mut s = (e.0 as usize) & mask;
                while self.ptab[s].0 != 0 {
                    s = (s + 1) & mask;
                }
                self.ptab[s] = e;
            }
        }
    }

    fn partition(&mut self, ev: &[(*mut PRec, usize)], cl: &[(*mut CRec, usize)]) {
        let n: usize = ev.iter().map(|s| s.1).sum();
        if n == 0 {
            return;
        }
        // pass 1: person id of every event, events per time bin
        let cap = (n / 8).max(64).next_power_of_two();
        self.ptab.clear();
        self.ptab.resize(cap, (0, 0));
        self.pname.clear();
        self.pcnt.clear();
        scratch(&mut self.pidv, n);
        scratch(&mut self.wv, n);
        self.hist.clear();
        self.hist.resize(self.nbins + 1, 0);
        let mut i = 0usize;
        for &(p, len) in ev {
            let recs = unsafe { std::slice::from_raw_parts(p, len) };
            for r in recs {
                let mask = self.ptab.len() - 1;
                let mut s = (r.pk as usize) & mask;
                let id;
                loop {
                    let e = unsafe { *self.ptab.get_unchecked(s) };
                    if e.0 == r.pk {
                        id = e.1;
                        break;
                    }
                    if e.0 == 0 {
                        id = self.pname.len() as u32;
                        self.ptab[s] = (r.pk, id);
                        self.pname.push(r.name);
                        self.pcnt.push(0);
                        if (r.name >> 48) != 0xFFFF {
                            prefetch((r.name & PTR48) as *const u8); // read when the person's rows are written
                        }
                        if self.pname.len() * 2 > self.ptab.len() {
                            self.pgrow();
                        }
                        break;
                    }
                    s = (s + 1) & mask;
                }
                unsafe {
                    *self.pidv.get_unchecked_mut(i) = id;
                    *self.wv.get_unchecked_mut(i) = r.w;
                    *self.hist.get_unchecked_mut((r.w >> BIN_SHIFT) as usize) += 1;
                }
                i += 1;
            }
        }
        let np = self.pname.len();
        // pass 2: counting sort by time bin (hist becomes the fill cursor); events per person
        let mut acc = 0u32;
        for h in self.hist.iter_mut() {
            let c = *h;
            *h = acc;
            acc += c;
        }
        scratch(&mut self.tw, n);
        scratch(&mut self.tp, n);
        for i in 0..n {
            unsafe {
                let w = *self.wv.get_unchecked(i);
                let h = self.hist.get_unchecked_mut((w >> BIN_SHIFT) as usize);
                let at = *h as usize;
                *h += 1;
                let id = *self.pidv.get_unchecked(i);
                *self.pcnt.get_unchecked_mut(id as usize) += 1;
                *self.tw.get_unchecked_mut(at) = w;
                *self.tp.get_unchecked_mut(at) = id;
            }
        }
        // pass 3: stable scatter by person (pcnt becomes the fill cursor)
        self.poff.clear();
        self.poff.reserve(np + 1);
        let mut acc = 0u32;
        for c in self.pcnt.iter_mut() {
            self.poff.push(acc);
            let k = *c;
            *c = acc;
            acc += k;
        }
        self.poff.push(acc);
        scratch(&mut self.ws, n);
        for k in 0..n {
            unsafe {
                let id = *self.tp.get_unchecked(k) as usize;
                let c = self.pcnt.get_unchecked_mut(id);
                let at = *c as usize;
                *c += 1;
                *self.ws.get_unchecked_mut(at) = *self.tw.get_unchecked(k);
            }
        }
        // clicks of persons with events
        self.ccnt.clear();
        self.ccnt.resize(np, 0);
        self.cpid.clear();
        for &(p, len) in cl {
            let recs = unsafe { std::slice::from_raw_parts(p, len) };
            for c in recs {
                let id = self.pfind(c.k);
                if id != u32::MAX {
                    self.ccnt[id as usize] += 1;
                }
                self.cpid.push(id);
            }
        }
        self.coff.clear();
        let mut acc = 0u32;
        for c in self.ccnt.iter_mut() {
            self.coff.push(acc);
            let k = *c;
            *c = acc;
            acc += k;
        }
        self.coff.push(acc);
        scratch(&mut self.cs, acc as usize);
        let mut i = 0usize;
        for &(p, len) in cl {
            let recs = unsafe { std::slice::from_raw_parts(p, len) };
            for c in recs {
                let id = self.cpid[i];
                if id != u32::MAX {
                    let at = self.ccnt[id as usize] as usize;
                    self.cs[at] = *c;
                    self.ccnt[id as usize] += 1;
                }
                i += 1;
            }
        }
        // sessionize each person
        let ws = std::mem::take(&mut self.ws);
        let mut cs = std::mem::take(&mut self.cs);
        for k in 0..np {
            let e = &ws[self.poff[k] as usize..self.poff[k + 1] as usize];
            let c = &mut cs[self.coff[k] as usize..self.coff[k + 1] as usize];
            sort_clicks(c);
            self.reserve_rows(self.pname[k], self.ndays + 1);
            self.person(self.pname[k], e, c);
        }
        self.ws = ws;
        self.cs = cs;
    }
}

fn write_user_daily(path: &Path, c: &Cols) {
    // top_category page: length of the levels, one bit-packed run of all rows, then the values
    let groups = c.lev.len();
    let mut hdr = Vec::new();
    let mut h = ((groups as u64) << 1) | 1;
    while h >= 0x80 {
        hdr.push((h as u8) | 0x80);
        h >>= 7;
    }
    hdr.push(h as u8);
    let mut lead = Vec::with_capacity(4 + hdr.len());
    lead.extend_from_slice(&((hdr.len() + groups) as u32).to_le_bytes());
    lead.extend_from_slice(&hdr);
    fn col<'a>(name: &'static str, ptype: i32, optional: bool, logical: pq::Logical, parts: Vec<&'a [u8]>) -> pq::ColP<'a> {
        pq::ColP { name, ptype, optional, logical, parts }
    }
    let cols = [
        col("user_id", pq::T_BYTES, false, pq::Logical::Str, vec![&c.names]),
        col("day", pq::T_INT32, false, pq::Logical::Date, vec![&c.day]),
        col("sessions", pq::T_INT64, false, pq::Logical::None, vec![&c.sess]),
        col("events", pq::T_INT64, false, pq::Logical::None, vec![&c.ev]),
        col("revenue_cents", pq::T_INT64, false, pq::Logical::None, vec![&c.rev]),
        col("top_category", pq::T_BYTES, true, pq::Logical::Str, vec![&lead, &c.lev, &c.top]),
    ];
    pq::write_file_parts(path, c.n, &cols).unwrap();
}

// ------------------------------------------------------------------ micro-benchmarks (FAST_BENCH=1)
/// Development aid: times the splitter and the event parser on up to 48 MB of events held in
/// memory (no page faults). FAST_BENCH=<substring> runs only the matching entries, longer.
fn bench(data: &Path, ctx: &Ctx) {
    let mut v: Vec<u8> = Vec::new();
    for p in list_csv(&data.join("events")) {
        if v.len() > (48 << 20) {
            break;
        }
        let f = std::fs::read(&p).unwrap();
        v.extend_from_slice(skip_header(Box::leak(f.into_boxed_slice())));
    }
    v.insert(0, b'\n');
    let buf: B = &Box::leak(v.into_boxed_slice())[1..];
    let lines = buf.iter().filter(|&&c| c == b'\n').count();
    eprintln!("bench: {} bytes, {} lines", buf.len(), lines);
    let only = std::env::var("FAST_BENCH").unwrap_or_default();
    let reps = if only.len() > 1 { 30 } else { 5 };
    let run = |name: &str, f: &mut dyn FnMut() -> u64| {
        if only.len() > 1 && !name.contains(only.as_str()) {
            return;
        }
        let mut best = f64::MAX;
        let mut r = 0;
        for _ in 0..reps {
            let t = std::time::Instant::now();
            r = f();
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("  {:<28} {:6.2} ns/line  {:6.2} GB/s   (check {})", name, best * 1e9 / lines as f64, buf.len() as f64 / best / 1e9, r);
    };
    run("split ref", &mut || {
        let mut acc = 0u64;
        for_each_line_ref::<7, _>(buf, |f| acc += (f.get(0).len() + f.get(6).len() + f.get(3).len()) as u64);
        acc
    });
    let mut sp = Split::new();
    run("split new", &mut || {
        let mut acc = 0u64;
        for_each_line::<7, _>(buf, &mut sp, |f| acc += (f.get(0).len() + f.get(6).len() + f.get(3).len()) as u64);
        acc
    });
    run("split new, N=8 (no emit)", &mut || {
        let mut acc = 0u64;
        for_each_line::<8, _>(buf, &mut sp, |f| acc += (f.get(0).len() + f.get(6).len() + f.get(3).len()) as u64);
        acc
    });
    for pbits in [8u32, 10] {
        let np = 1usize << pbits;
        let mut ar = Arena::new();
        let mut st = ParseState::new();
        let mut bk: Vec<Bucket<ERec>> = (0..np).map(|_| Bucket::new()).collect();
        run(if pbits == 8 { "parse_events 256 buckets" } else { "parse_events 1024 buckets" }, &mut || {
            // recycle the chunks of the previous repetition
            let mut sl = Vec::new();
            for b in bk.iter_mut() {
                sl.clear();
                b.slices(&mut sl);
                for &(p, _) in &sl {
                    ar.free.push(p as usize - HDR);
                }
                *b = Bucket::new();
            }
            parse_events(buf, ctx, &mut bk, &mut ar, 64 - pbits, &mut sp, &mut st);
            bk.iter().map(|b| b.wp as u64 & 0xFFFF).sum()
        });
    }
}

// ------------------------------------------------------------------ main
fn run_par<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n)
            .map(|t| {
                let f = &f;
                s.spawn(move || f(t))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

struct AOut {
    ebk: Vec<Bucket<ERec>>,
    cbk: Vec<Bucket<CRec>>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut data = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let mut outd = std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    if args.len() >= 3 {
        data = args[1].clone();
        outd = args[2].clone();
    }
    let nthreads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8));
    let verbose = std::env::var("FAST_VERBOSE").is_ok();
    let t0 = std::time::Instant::now();
    let last = std::cell::Cell::new((0u64, 0u64));
    let lap = |what: &str| {
        if verbose {
            let (i, c) = insn_cycles();
            let (li, lc) = last.replace((i, c));
            eprintln!("[{:7.3}s] {:<34} (+{:.3} G instructions, +{:.3} G cycles)", t0.elapsed().as_secs_f64(), what, (i - li) as f64 / 1e9, (c - lc) as f64 / 1e9);
        }
    };
    let data = PathBuf::from(data);

    // manifest
    let mtxt = std::fs::read_to_string(data.join("manifest.json")).unwrap_or_else(|e| die(&format!("manifest: {}", e)));
    let w0 = legacy::parse_ts(extract_json_str(&mtxt, "window_start").unwrap_or_else(|| die("window_start")).as_bytes())
        .unwrap_or_else(|| die("bad window_start"));
    let w1 = legacy::parse_ts(extract_json_str(&mtxt, "window_end").unwrap_or_else(|| die("window_end")).as_bytes())
        .unwrap_or_else(|| die("bad window_end"));

    lap("manifest");

    // products, campaigns and the mapping of every input file run concurrently
    let ev_paths = list_csv(&data.join("events"));
    let cl_paths = list_csv(&data.join("clicks"));
    let lk_paths = list_csv(&data.join("links"));
    let paths: Vec<&PathBuf> = ev_paths.iter().chain(cl_paths.iter()).chain(lk_paths.iter()).collect();
    let maps: Vec<OnceLock<B>> = (0..paths.len()).map(|_| OnceLock::new()).collect();
    let nextf = AtomicUsize::new(0);
    let first_link = ev_paths.len() + cl_paths.len();
    let link_lines = AtomicUsize::new(0);
    let map_some = || loop {
        // links first: their lines are counted here to size the union-find
        let j = nextf.fetch_add(1, Ordering::Relaxed);
        if j >= paths.len() {
            break;
        }
        let j = paths.len() - 1 - j;
        let m = map_file(paths[j]);
        if j >= first_link {
            link_lines.fetch_add(m.iter().filter(|&&c| c == b'\n').count() + 1, Ordering::Relaxed);
        }
        let _ = maps[j].set(m);
    };
    let (prod, camp) = std::thread::scope(|sc| {
        let hp = sc.spawn(|| {
            // (category names sorted, products with their sorted category index if there are <= 256)
            let pfile = map_file(&data.join("products.csv"));
            let mut catset: BMap<u32> = BMap::default();
            let mut raw: Vec<(B, u32)> = Vec::new();
            for_each_line::<2, _>(skip_header(pfile), &mut Split::new(), |f| {
                if !f.get(0).is_empty() {
                    let n = catset.len() as u32;
                    let t = *catset.entry(f.get(1)).or_insert(n);
                    raw.push((f.get(0), t));
                }
            });
            let mut cat_names: Vec<B> = catset.keys().copied().collect();
            cat_names.push(b"unknown");
            cat_names.sort();
            cat_names.dedup();
            let mut products: StrTable<u8> = StrTable::with_capacity(raw.len());
            if cat_names.len() <= 256 {
                let mut remap = vec![0u8; catset.len()];
                for (name, &t) in catset.iter() {
                    remap[t as usize] = cat_names.binary_search(name).unwrap() as u8;
                }
                for &(pid, t) in &raw {
                    products.insert(pid, remap[t as usize]);
                }
            }
            (cat_names, products)
        });
        let hc = sc.spawn(|| {
            let cfile = map_file(&data.join("campaigns.csv"));
            let mut campaigns: StrTable<u32> = StrTable::with_capacity(4096);
            let mut camp_names: Vec<(B, B)> = Vec::new();
            for_each_line::<2, _>(skip_header(cfile), &mut Split::new(), |f| {
                if f.get(0).is_empty() {
                    return;
                }
                match campaigns.get(f.get(0)) {
                    Some(i) => camp_names[i as usize].1 = f.get(1),
                    None => {
                        campaigns.insert(f.get(0), camp_names.len() as u32);
                        camp_names.push((f.get(0), f.get(1)));
                    }
                }
            });
            (campaigns, camp_names)
        });
        for _ in 2..nthreads.max(3) {
            sc.spawn(map_some);
        }
        map_some();
        (hp.join().unwrap(), hc.join().unwrap())
    });
    let (cat_names, products) = prod;
    let (campaigns, camp_names) = camp;
    let ncamps = camp_names.len();
    lap("dims, files mapped");

    // The fast path packs categories in 8 bits and timestamps relative to the window in 32 bits.
    let fast_ok = cat_names.len() <= 256
        && w1 > w0
        && w0 > -(1i64 << 46)
        && w1 < (1i64 << 46)
        && (w1 - w0) + SEVEN < (1i64 << 32)
        && std::env::var("FAST_LEGACY").is_err();
    if !fast_ok {
        legacy::run();
        std::process::exit(0);
    }
    let unknown_cat = cat_names.binary_search(&(b"unknown" as B)).unwrap() as u8;
    let cat_names: &'static [B] = Box::leak(cat_names.into_boxed_slice());

    // cut ~equal chunks
    let mapped: Vec<B> = maps.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let (ev_files, rest) = mapped.split_at(ev_paths.len());
    let (cl_files, lk_files) = rest.split_at(cl_paths.len());
    let ev_bytes: usize = ev_files.iter().map(|f| f.len()).sum();
    let all_bytes: usize = ev_bytes + cl_files.iter().map(|f| f.len()).sum::<usize>() + lk_files.iter().map(|f| f.len()).sum::<usize>();
    let cdiv: usize = std::env::var("FAST_CHUNK_DIV").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
    let target = (all_bytes / (nthreads * cdiv)).clamp(128 << 10, 4 << 20);
    let lchunks = make_chunks(lk_files, target.min(256 << 10));
    let cchunks = make_chunks(cl_files, target);
    let echunks = make_chunks(ev_files, target);
    // largest chunks first, so that the threads finish together
    let mut jobs: Vec<(u8, B)> = cchunks.iter().map(|&b| (1u8, b)).chain(echunks.iter().map(|&b| (2u8, b))).collect();
    jobs.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    // buckets: ~25k events each (fewer means bigger dedup bitmaps, more means more write streams)
    let est_events = ev_bytes / 74 + 1;
    let pbits = std::env::var("FAST_PBITS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(((est_events / 25_000).max(1).next_power_of_two().trailing_zeros()).clamp(6, 10))
        .clamp(1, 12);
    let np = 1usize << pbits;
    let shift = 64 - pbits;
    lap("chunks cut");

    // links: every thread parses link chunks into a shared lock-free union-find, then every
    // linked id gets its person's key and name
    let mut links = LinkTable::with_lines(if lchunks.is_empty() { 0 } else { link_lines.load(Ordering::Relaxed) });
    if !lchunks.is_empty() {
        let next = AtomicUsize::new(0);
        let lt = &links;
        let mines: Vec<Vec<u32>> = run_par(nthreads, |_| {
            let mut sp = Split::new();
            let mut mine: Vec<u32> = Vec::new();
            let c0 = if verbose { thread_cpu() } else { 0.0 };
            loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                if j >= lchunks.len() {
                    break;
                }
                link_chunk(lchunks[j], lt, &mut sp, &mut mine);
            }
            if verbose {
                acc("L parse+union", thread_cpu() - c0, 0.0);
            }
            mine
        });
        let ids: usize = mines.iter().map(|m| m.len()).sum();
        links.alloc_filter(ids);
        let lt = &links;
        let nonroots: usize = run_par(nthreads, |t| {
            let c0 = if verbose { thread_cpu() } else { 0.0 };
            let n = link_finish(lt, &mines[t]);
            if verbose {
                acc("L finish", thread_cpu() - c0, 0.0);
            }
            n
        })
        .iter()
        .sum();
        if verbose {
            eprintln!("[{:7.3}s] links: {} ids, {} below their person's name, {} slots", t0.elapsed().as_secs_f64(), ids, nonroots, links.mask + 1);
        }
    }

    let ctx = Ctx { w0, w1, products, unknown_cat, campaigns, links };
    if std::env::var("FAST_BENCH").is_ok() {
        bench(&data, &ctx);
        return;
    }

    // phase A: parse clicks and events; events to event-id buckets, clicks to person buckets
    let next = AtomicUsize::new(0);
    let ares: Vec<(AOut, Arena)> = run_par(nthreads, |_| {
        let mut ar = Arena::new();
        let mut out = AOut { ebk: (0..np).map(|_| Bucket::new()).collect(), cbk: (0..np).map(|_| Bucket::new()).collect() };
        let mut sp = Split::new();
        let mut st = ParseState::new();
        let mut cpu = [0f64; 3];
        let wt = std::time::Instant::now();
        loop {
            let j = next.fetch_add(1, Ordering::Relaxed);
            if j >= jobs.len() {
                break;
            }
            let (kind, buf) = jobs[j];
            let c0 = if verbose { thread_cpu() } else { 0.0 };
            match kind {
                1 => parse_clicks(buf, &ctx, &mut out.cbk, &mut ar, shift, &mut sp, &mut st),
                _ => parse_events(buf, &ctx, &mut out.ebk, &mut ar, shift, &mut sp, &mut st),
            }
            if verbose {
                cpu[kind as usize] += thread_cpu() - c0;
            }
        }
        if verbose {
            acc("A clicks", cpu[1], 0.0);
            acc("A events", cpu[2], 0.0);
            acc("A thread", cpu[1] + cpu[2], wt.elapsed().as_secs_f64());
        }
        (out, ar)
    });
    let (aouts, mut arenas): (Vec<AOut>, Vec<Arena>) = ares.into_iter().unzip();
    lap("parsed");

    // phase B: dedup events and route them to person buckets
    let arena_cells: Vec<Mutex<Arena>> = arenas.drain(..).map(Mutex::new).collect();
    let next = AtomicUsize::new(0);
    let bres: Vec<Vec<Bucket<PRec>>> = run_par(nthreads, |t| {
        let mut ar = std::mem::replace(&mut *arena_cells[t].lock().unwrap(), Arena::new());
        let mut out: Vec<Bucket<PRec>> = (0..np).map(|_| Bucket::new()).collect();
        let mut dd = Dedup::new();
        let mut sl: Vec<(*mut ERec, usize)> = Vec::new();
        let wt = std::time::Instant::now();
        let c0 = if verbose { thread_cpu() } else { 0.0 };
        loop {
            let j = next.fetch_add(1, Ordering::Relaxed);
            if j >= np {
                break;
            }
            sl.clear();
            for a in &aouts {
                a.ebk[j].slices(&mut sl);
            }
            dedup_part(&sl, &mut dd, &mut out, &mut ar, shift);
        }
        if verbose {
            acc("B dedup", thread_cpu() - c0, wt.elapsed().as_secs_f64());
        }
        *arena_cells[t].lock().unwrap() = ar;
        out
    });
    lap("deduped");

    // phase C: sessionize, largest buckets first
    std::fs::create_dir_all(Path::new(&outd).join("user_daily")).unwrap();
    std::fs::create_dir_all(Path::new(&outd).join("campaign_daily")).unwrap();
    let outp = PathBuf::from(&outd);
    let w0d = w0.div_euclid(MS_DAY);
    let ndays = ((w1 - 1).div_euclid(MS_DAY) - w0d + 1).max(1) as usize;
    let size = (ncamps + 1) * ndays;
    let mut order: Vec<(usize, usize)> = (0..np).map(|q| (bres.iter().map(|b| b[q].len()).sum::<usize>(), q)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let total_events: usize = order.iter().map(|o| o.0).sum();
    let next = AtomicUsize::new(0);
    let aggs: Vec<Agg> = run_par(nthreads, |t| {
        let mut w = Worker::new(w0, w1, ncamps, cat_names, total_events / nthreads / 6 + 1024);
        let mut es: Vec<(*mut PRec, usize)> = Vec::new();
        let mut cs: Vec<(*mut CRec, usize)> = Vec::new();
        let wt = std::time::Instant::now();
        let c0 = if verbose { thread_cpu() } else { 0.0 };
        loop {
            let j = next.fetch_add(1, Ordering::Relaxed);
            if j >= np {
                break;
            }
            let q = order[j].1;
            es.clear();
            cs.clear();
            for b in &bres {
                b[q].slices(&mut es);
            }
            for a in &aouts {
                a.cbk[q].slices(&mut cs);
            }
            w.partition(&es, &cs);
        }
        let c1 = if verbose { thread_cpu() } else { 0.0 };
        let wt1 = wt.elapsed().as_secs_f64();
        if w.cols.n > 0 || t == 0 {
            write_user_daily(&outp.join("user_daily").join(format!("part-{}.parquet", t)), &w.cols);
        }
        if verbose {
            acc("C sessionize", c1 - c0, wt1);
            acc("C write", thread_cpu() - c1, wt.elapsed().as_secs_f64() - wt1);
        }
        w.agg
    });
    lap("sessionized + user_daily written");

    // campaign_daily
    let mut tot = Agg::new(size);
    for a in &aggs {
        for i in 0..size {
            tot.v[i].sessions += a.v[i].sessions;
            tot.v[i].purch += a.v[i].purch;
            tot.v[i].rev += a.v[i].rev;
            tot.v[i].assisted += a.v[i].assisted;
        }
    }
    let mut rows: Vec<(usize, usize)> = Vec::new();
    for c in 0..=ncamps {
        for d in 0..ndays {
            let i = c * ndays + d;
            if tot.v[i].sessions > 0 || tot.v[i].assisted > 0 {
                rows.push((c, d));
            }
        }
    }
    let n = rows.len();
    let cid = |c: usize| -> &'static [u8] { if c == ncamps { b"organic" } else { camp_names[c].0 } };
    let chn = |c: usize| -> &'static [u8] { if c == ncamps { b"organic" } else { camp_names[c].1 } };
    let cols = vec![
        pq::Col { name: "campaign_id", ptype: pq::T_BYTES, optional: false, logical: pq::Logical::Str, body: pq::str_page(rows.iter().map(|&(c, _)| cid(c)), n * 8) },
        pq::Col { name: "channel", ptype: pq::T_BYTES, optional: false, logical: pq::Logical::Str, body: pq::str_page(rows.iter().map(|&(c, _)| chn(c)), n * 8) },
        pq::Col { name: "day", ptype: pq::T_INT32, optional: false, logical: pq::Logical::Date, body: pq::i32_page(rows.iter().map(|&(_, d)| (w0d + d as i64) as i32), n) },
        pq::Col { name: "sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None, body: pq::i64_page(rows.iter().map(|&(c, d)| tot.v[c * ndays + d].sessions as i64), n) },
        pq::Col { name: "purchasing_sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None, body: pq::i64_page(rows.iter().map(|&(c, d)| tot.v[c * ndays + d].purch as i64), n) },
        pq::Col { name: "revenue_cents", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None, body: pq::i64_page(rows.iter().map(|&(c, d)| tot.v[c * ndays + d].rev as i64), n) },
        pq::Col { name: "assisted_sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None, body: pq::i64_page(rows.iter().map(|&(c, d)| tot.v[c * ndays + d].assisted as i64), n) },
    ];
    pq::write_file(&outp.join("campaign_daily").join("part-0.parquet"), n, cols).unwrap();
    if !verbose {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        unsafe {
            std::arch::asm!("mov eax, 231", "xor edi, edi", "syscall", options(noreturn));
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        std::process::exit(0);
    }
    lap("done");
    if std::env::var("FAST_PAR_UNMAP").map_or(cfg!(target_os = "linux"), |v| v == "1") {
        // Outputs are written: release the big mappings from all threads, so that the page-table
        // teardown is not done by one thread at exit (Linux zaps under the read lock in munmap).
        // On by default on Linux; FAST_PAR_UNMAP=0 turns it off.
        let mut regions: Vec<(usize, usize)> = Vec::new();
        for c in &arena_cells {
            let a = c.lock().unwrap();
            for &b in &a.blocks {
                regions.push((b, BLOCK + CHUNK));
            }
        }
        for f in &mapped {
            if !f.is_empty() {
                regions.push((f.as_ptr() as usize, f.len()));
            }
        }
        if ctx.links.bytes > 0 {
            regions.push((ctx.links.ents as usize, ctx.links.bytes));
        }
        regions.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        let next = AtomicUsize::new(0);
        run_par(nthreads, |_| loop {
            let j = next.fetch_add(1, Ordering::Relaxed);
            if j >= regions.len() {
                break;
            }
            unsafe { munmap(regions[j].0 as *mut u8, regions[j].1) };
        });
        lap("unmapped");
    }
    if verbose {
        let ns = |i: usize| pq::WSTAT[i].load(Ordering::Relaxed) as f64 * 1e-9;
        eprintln!("  output files: slowest create {:.3}s, write {:.3}s, flush+close {:.3}s", ns(0), ns(1), ns(2));
        if let Ok(st) = std::fs::read_to_string("/proc/self/status") {
            let keep: Vec<&str> = st.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("RssAnon") || l.starts_with("RssFile") || l.starts_with("VmPeak") || l.starts_with("AnonHugePages")).collect();
            eprintln!("  memory: {}", keep.join(" | ").replace('\t', " "));
        }
        if let Ok(st) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            let keep: Vec<&str> = st.lines().filter(|l| l.starts_with("AnonHugePages") || l.starts_with("Anonymous")).collect();
            eprintln!("  smaps: {}", keep.join(" | "));
        }
        let v = CPU_ACC.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        for x in v.iter() {
            let nm = x.0;
            if !seen.insert(nm) {
                continue;
            }
            let (mut sc, mut mc, mut mw, mut k) = (0f64, 0f64, 0f64, 0);
            for x in v.iter().filter(|x| x.0 == nm) {
                sc += x.1;
                mc = mc.max(x.1);
                mw = mw.max(x.2);
                k += 1;
            }
            eprintln!("  {:<16} cpu sum {:7.3}s  max/thread {:6.3}s  wall max {:6.3}s  (n={})", nm, sc, mc, mw, k);
        }
    }
    // skip destructors: the kernel reclaims everything at exit
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn leak(v: Vec<u8>) -> B {
        Box::leak(v.into_boxed_slice())
    }

    #[test]
    fn ts_matches_legacy() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let seeds: [&[u8]; 14] = [
            b"2026-10-03T23:58:10Z",
            b"2026-10-03T19:58:10-04:00",
            b"2028-02-29T00:00:00+12:00",
            b"2027-02-29T00:00:00Z",
            b"1791071890000",
            b"0001791071890000",
            b"86400000",
            b"9223372036854775807",
            b"9223372036854775808",
            b"00000000000000000000001791071890000",
            b"2026-12-31T23:59:59-00:00",
            b"1900-02-29T12:00:00Z",
            b"2000-02-29T12:00:00Z",
            b"0000-01-01T00:00:00Z",
        ];
        let alphabet = b"0123456789-+:TZtz .eE\r\"";
        let mut checked = 0u64;
        let mut dc = DateCache::new();
        for s in seeds.iter() {
            assert_eq!(parse_ts(s, &mut dc), legacy::parse_ts(s), "{:?}", std::str::from_utf8(s));
        }
        for _ in 0..3_000_000 {
            let base = seeds[rng.below(seeds.len() as u64) as usize];
            let mut v = base.to_vec();
            let muts = rng.below(4);
            for _ in 0..muts {
                match rng.below(5) {
                    0 if !v.is_empty() => {
                        let i = rng.below(v.len() as u64) as usize;
                        v[i] = alphabet[rng.below(alphabet.len() as u64) as usize];
                    }
                    1 if !v.is_empty() => {
                        let i = rng.below(v.len() as u64) as usize;
                        v[i] = b'0' + rng.below(10) as u8;
                    }
                    2 if !v.is_empty() => {
                        let i = rng.below(v.len() as u64) as usize;
                        v.remove(i);
                    }
                    3 => {
                        let i = rng.below(v.len() as u64 + 1) as usize;
                        v.insert(i, b'0' + rng.below(10) as u8);
                    }
                    _ => {
                        let i = rng.below(v.len() as u64 + 1) as usize;
                        v.insert(i, rng.below(256) as u8);
                    }
                }
            }
            // random digit strings of every length
            if rng.below(4) == 0 {
                let n = rng.below(24) as usize;
                v = (0..n).map(|_| b'0' + rng.below(10) as u8).collect();
                if rng.below(3) == 0 && n > 0 {
                    let i = rng.below(n as u64) as usize;
                    v[i] = rng.below(256) as u8;
                }
            }
            assert_eq!(parse_ts(&v, &mut dc), legacy::parse_ts(&v), "{:?}", v);
            assert_eq!(parse_ts_f1(&v, &mut dc), legacy::parse_ts(&v), "{:?}", v);
            if rng.below(64) == 0 {
                dc = DateCache::new();
            }
            checked += 1;
        }
        // every 20-byte F1 with digits varied at each position
        for y in [0i64, 1969, 1970, 2000, 2024, 2025, 2028, 2100, 9999] {
            for mo in 0..=13 {
                for d in [0, 1, 28, 29, 30, 31, 32] {
                    let s = format!("{:04}-{:02}-{:02}T23:59:59Z", y, mo, d);
                    assert_eq!(parse_ts(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                    let s = format!("{:04}-{:02}-{:02}T00:00:00+23:59", y, mo, d);
                    assert_eq!(parse_ts(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                    let s = format!("{:04}-{:02}-{:02}T24:00:00Z", y, mo, d);
                    assert_eq!(parse_ts(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                    for hms in ["00:00:00", "23:59:59", "23:59:60", "23:60:59", "24:00:00", "19:07:4x", "1:907:45", "12:34:56", "09-08:07", "09:08-07"] {
                        for z in ["Z", "z", "+"] {
                            let s = format!("{:04}-{:02}-{:02}T{}{}", y, mo, d, hms, z);
                            assert_eq!(parse_ts_f1(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                            let s = format!("{:04}-{:02}-{:02}t{}{}", y, mo, d, hms, z);
                            assert_eq!(parse_ts_f1(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                            let s = format!("{:04}/{:02}-{:02}T{}{}", y, mo, d, hms, z);
                            assert_eq!(parse_ts_f1(s.as_bytes(), &mut dc), legacy::parse_ts(s.as_bytes()), "{}", s);
                        }
                    }
                }
            }
        }
        assert!(checked > 0);
    }

    fn collect_new<const N: usize>(buf: B) -> Vec<Vec<Vec<u8>>> {
        // the splitter may read the byte before the buffer: try a quote and a newline there
        let mut all = Vec::new();
        for lead in [b'"', b'\n'] {
            let mut v = vec![lead];
            v.extend_from_slice(buf);
            let b: B = &leak(v)[1..];
            let mut out = Vec::new();
            for_each_line::<N, _>(b, &mut Split::new(), |f| out.push((0..N).map(|j| f.get(j).to_vec()).collect()));
            all.push(out);
        }
        assert_eq!(all[0], all[1]);
        all.pop().unwrap()
    }
    /// Mirrors the oracle's read_rows/split_line: split on '\n' (a trailing empty piece is not a
    /// line), N comma-separated parts, a part containing '"' must be wrapped in quotes with none inside.
    fn collect_old<const N: usize>(buf: B) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut lines: Vec<&[u8]> = buf.split(|&c| c == b'\n').collect();
        if lines.last().map_or(false, |l| l.is_empty()) {
            lines.pop();
        }
        'line: for line in lines {
            let parts: Vec<&[u8]> = line.split(|&c| c == b',').collect();
            if parts.len() != N {
                continue;
            }
            let mut row = Vec::new();
            for p in parts {
                let mut p = p;
                if p.contains(&b'"') {
                    if p.len() < 2 || p[0] != b'"' || p[p.len() - 1] != b'"' || p[1..p.len() - 1].contains(&b'"') {
                        continue 'line;
                    }
                    p = &p[1..p.len() - 1];
                }
                row.push(p.to_vec());
            }
            out.push(row);
        }
        out
    }

    fn collect_ref<const N: usize>(buf: B) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for_each_line_ref::<N, _>(buf, |f| out.push((0..N).map(|j| f.get(j).to_vec()).collect()));
        out
    }

    #[test]
    fn lines_match_legacy() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let alphabet = b"ab,,\"\"\n\n\r x";
        for iter in 0..200_000 {
            let n = if iter % 10 == 0 { rng.below(600) } else { rng.below(160) } as usize;
            let v: Vec<u8> = (0..n).map(|_| alphabet[rng.below(alphabet.len() as u64) as usize]).collect();
            let b = leak(v);
            assert_eq!(collect_new::<2>(b), collect_old::<2>(b));
            assert_eq!(collect_new::<4>(b), collect_old::<4>(b));
            assert_eq!(collect_new::<7>(b), collect_old::<7>(b));
            assert_eq!(collect_ref::<4>(b), collect_old::<4>(b));
            assert_eq!(collect_ref::<7>(b), collect_old::<7>(b));
            if iter % 8 == 0 {
                assert_eq!(collect_new::<3>(b), collect_old::<3>(b));
                assert_eq!(collect_new::<8>(b), collect_old::<8>(b));
                assert_eq!(collect_new::<9>(b), collect_old::<9>(b));
            }
            // also at every start offset of a sub-slice (block alignment)
            let off = rng.below(n as u64 + 1) as usize;
            let sb: B = &b[off..];
            assert_eq!(collect_new::<4>(sb), collect_old::<4>(sb));
        }
        // realistic lines
        for _ in 0..20_000 {
            let mut v = Vec::new();
            let lines = rng.below(20);
            for _ in 0..lines {
                let nf = 5 + rng.below(4);
                for j in 0..nf {
                    if j > 0 {
                        v.push(b',');
                    }
                    let q = rng.below(5) == 0;
                    if q {
                        v.push(b'"');
                    }
                    for _ in 0..rng.below(14) {
                        v.push(b'a' + rng.below(26) as u8);
                    }
                    if q && rng.below(20) != 0 {
                        v.push(b'"');
                    }
                }
                if rng.below(30) != 0 {
                    v.push(b'\n');
                }
            }
            let b = leak(v);
            assert_eq!(collect_new::<7>(b), collect_old::<7>(b));
            assert_eq!(collect_new::<6>(b), collect_old::<6>(b));
        }
        // buffers longer than several index batches, mostly well-formed 7-field lines
        for round in 0..40 {
            let mut v = Vec::new();
            while v.len() < 70_000 + round * 1000 {
                let nf = if rng.below(50) == 0 { 6 + rng.below(3) } else { 7 };
                for j in 0..nf {
                    if j > 0 {
                        v.push(b',');
                    }
                    let q = rng.below(3) == 0;
                    if q {
                        v.push(b'"');
                    }
                    for _ in 0..rng.below(24) {
                        v.push(b'a' + rng.below(26) as u8);
                    }
                    if rng.below(200) == 0 {
                        v.push(b'"');
                    }
                    if q && rng.below(100) != 0 {
                        v.push(b'"');
                    }
                }
                if rng.below(3000) == 0 {
                    // a very long junk line
                    for _ in 0..rng.below(40_000) {
                        v.push(b"ab,\"x"[rng.below(5) as usize]);
                    }
                }
                v.push(b'\n');
            }
            if round % 2 == 0 {
                v.pop();
            }
            let b = leak(v);
            assert_eq!(collect_new::<7>(b), collect_old::<7>(b));
            assert_eq!(collect_ref::<7>(b), collect_old::<7>(b));
        }
    }

    #[test]
    fn fhash_distinct_and_eq() {
        // distinct short strings never share (hash, extra bits); eq_bytes agrees with ==
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        let mut seen = std::collections::HashMap::new();
        let mut strs: Vec<Vec<u8>> = Vec::new();
        for _ in 0..400_000 {
            let n = rng.below(20) as usize;
            let v: Vec<u8> = (0..n).map(|_| b"abu0123456789_\",\n"[rng.below(16) as usize]).collect();
            strs.push(v);
        }
        for v in &strs {
            let h = fhash(v);
            if let Some(o) = seen.insert(h, v.clone()) {
                assert_eq!(&o, v, "hash collision");
            }
        }
        for i in 1..strs.len() {
            assert_eq!(eq_bytes(&strs[i - 1], &strs[i]), strs[i - 1] == strs[i]);
            assert!(eq_bytes(&strs[i], &strs[i].clone()));
            // same length, last byte differs
            if !strs[i].is_empty() {
                let mut w = strs[i].clone();
                let l = w.len() - 1;
                w[l] ^= 1;
                assert!(!eq_bytes(&strs[i], &w));
                let mut w = strs[i].clone();
                w[0] ^= 1;
                assert!(!eq_bytes(&strs[i], &w));
            }
        }
    }

    /// Concurrent union-find on link chunks against a sequential reference: for every id, the
    /// person is the byte-wise smallest id of its component.
    #[test]
    fn links_match_reference() {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        for round in 0..60 {
            let nids = 2 + rng.below(if round % 3 == 0 { 40 } else if round % 10 == 1 { 150_000 } else { 3000 }) as usize;
            let npairs = rng.below(2 * nids as u64 + 2) as usize;
            let ids: Vec<Vec<u8>> = (0..nids)
                .map(|i| {
                    let mut v = format!("u{}", rng.below(1 << 40)).into_bytes();
                    if i % 7 == 0 {
                        v = format!("uverylongprefix-shared-{:05}-{}", i, rng.below(50)).into_bytes();
                    }
                    v
                })
                .collect();
            // pairs: random, plus chains and a star, as text in several chunks
            let mut pairs: Vec<(usize, usize)> = (0..npairs).map(|_| (rng.below(nids as u64) as usize, rng.below(nids as u64) as usize)).collect();
            let chain = rng.below(nids as u64 / 2 + 1) as usize;
            for i in 0..chain {
                pairs.push((i, i + 1));
            }
            for i in 0..rng.below(nids as u64 / 4 + 1) as usize {
                pairs.push((nids - 1, nids - 1 - i));
            }
            let nchunks = 1 + rng.below(if nids > 3000 { 200 } else { 12 }) as usize;
            let mut chunks: Vec<Vec<u8>> = vec![Vec::new(); nchunks];
            for (k, &(a, b)) in pairs.iter().enumerate() {
                let c = &mut chunks[k % nchunks];
                let q = k % 5 == 0;
                if q {
                    c.push(b'"');
                }
                c.extend_from_slice(&ids[a]);
                if q {
                    c.push(b'"');
                }
                c.push(b',');
                c.extend_from_slice(&ids[b]);
                c.push(b'\n');
            }
            let lines: usize = chunks.iter().map(|c| c.iter().filter(|&&x| x == b'\n').count() + 1).sum();
            let chunks: Vec<B> = chunks
                .into_iter()
                .map(|mut c| {
                    c.insert(0, b'\n');
                    &leak(c)[1..]
                })
                .collect();
            // reference
            let mut parent: Vec<usize> = (0..nids).collect();
            fn find(p: &mut Vec<usize>, mut x: usize) -> usize {
                while p[x] != x {
                    p[x] = p[p[x]];
                    x = p[x];
                }
                x
            }
            let mut in_links = vec![false; nids];
            // ids may repeat as strings: unify equal strings first
            let mut first: std::collections::HashMap<&[u8], usize> = std::collections::HashMap::new();
            for (i, id) in ids.iter().enumerate() {
                let f = *first.entry(&id[..]).or_insert(i);
                let (a, b) = (find(&mut parent, f), find(&mut parent, i));
                parent[a] = b;
            }
            for &(a, b) in &pairs {
                in_links[a] = true;
                in_links[b] = true;
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent[ra] = rb;
            }
            let mut best: std::collections::HashMap<usize, &[u8]> = std::collections::HashMap::new();
            for i in 0..nids {
                let r = find(&mut parent, i);
                let e = best.entry(r).or_insert(&ids[i]);
                if &ids[i][..] < *e {
                    *e = &ids[i];
                }
            }
            let linked_strs: std::collections::HashSet<&[u8]> = (0..nids).filter(|&j| in_links[j]).map(|j| &ids[j][..]).collect();
            // concurrent build
            let nthreads = 1 + (round % 8);
            let mut lt = LinkTable::with_lines(lines);
            let next = AtomicUsize::new(0);
            let mines: Vec<Vec<u32>> = run_par(nthreads, |_| {
                let mut sp = Split::new();
                let mut mine = Vec::new();
                loop {
                    let j = next.fetch_add(1, Ordering::Relaxed);
                    if j >= chunks.len() {
                        break;
                    }
                    link_chunk(chunks[j], &lt, &mut sp, &mut mine);
                }
                mine
            });
            lt.alloc_filter(mines.iter().map(|m| m.len()).sum());
            run_par(nthreads, |t| link_finish(&lt, &mines[t]));
            for i in 0..nids {
                let h = uhash(&ids[i]);
                let want = best[&find(&mut parent, i)];
                // ids that share a string with a linked id are linked too
                let linked = linked_strs.contains(&ids[i][..]);
                let got = if lt.maybe(h) { lt.lookup(h) } else { None };
                match got {
                    Some((pk, name)) => {
                        assert!(linked);
                        assert_eq!(unpack_str(name), want, "round {} id {:?}", round, std::str::from_utf8(&ids[i]));
                        assert_eq!(pk, uhash(want));
                        assert_ne!(want, &ids[i][..]);
                    }
                    None => assert_eq!(want, &ids[i][..], "round {} id {:?} should map to {:?}", round, std::str::from_utf8(&ids[i]), std::str::from_utf8(want)),
                }
            }
        }
    }

    /// Bitmap dedup against a map: per event id the copy with the smallest ingest decides.
    #[test]
    fn dedup_matches_reference() {
        let mut rng = Rng(0xfeed_beef_1234_5678);
        for round in 0..200 {
            let n = 1 + rng.below(if round % 4 == 0 { 30 } else { 5000 }) as usize;
            let dup_every = 1 + rng.below(20);
            let mut ar = Arena::new();
            let nb = 1 + rng.below(3) as usize;
            let mut bks: Vec<Bucket<ERec>> = (0..nb).map(|_| Bucket::new()).collect();
            let mut want: std::collections::HashMap<(u64, u64), ERec> = std::collections::HashMap::new();
            let mut ids: Vec<(u64, u64)> = Vec::new();
            for i in 0..n {
                let (eh, h2) = if !ids.is_empty() && rng.below(dup_every) == 0 {
                    ids[rng.below(ids.len() as u64) as usize]
                } else if rng.below(50) == 0 && !ids.is_empty() {
                    // same 64-bit hash, different extra bits: a different event id
                    (ids[rng.below(ids.len() as u64) as usize].0, rng.next() & 0x7FFF)
                } else {
                    // few distinct low bits, so that bitmap collisions happen
                    ((rng.next() << 20) | rng.below(64), rng.next() & 0x7FFF)
                };
                ids.push((eh, h2));
                let ing = rng.below(6);
                let valid = rng.below(4) != 0;
                // copies with the same ingest are identical
                let r = ERec { eh, ik: (ing << 16) | (h2 << 1) | valid as u64, pk: (eh ^ ing).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, name: i as u64 & 0, w: ing * 1000 + (eh >> 60) };
                let r = match want.get(&(eh, h2)) {
                    Some(o) if (o.ik >> 16) == ing => *o,
                    _ => r,
                };
                bks[i % nb].push(r, &mut ar);
                let keep = match want.get(&(eh, h2)) {
                    Some(o) => (r.ik >> 16) < (o.ik >> 16),
                    None => true,
                };
                if keep {
                    want.insert((eh, h2), r);
                }
            }
            let mut sl = Vec::new();
            for b in &bks {
                b.slices(&mut sl);
            }
            let mut out: Vec<Bucket<PRec>> = (0..4).map(|_| Bucket::new()).collect();
            let mut dd = Dedup::new();
            dedup_part(&sl, &mut dd, &mut out, &mut ar, 62);
            let mut got: Vec<(u64, u64)> = Vec::new();
            for (q, b) in out.iter().enumerate() {
                let mut s2 = Vec::new();
                b.slices(&mut s2);
                for &(p, len) in &s2 {
                    for r in unsafe { std::slice::from_raw_parts(p, len) } {
                        assert_eq!((r.pk >> 62) as usize, q);
                        got.push((r.pk, r.w));
                    }
                }
            }
            let mut exp: Vec<(u64, u64)> = want.values().filter(|r| r.ik & 1 != 0).map(|r| (r.pk, r.w)).collect();
            got.sort_unstable();
            exp.sort_unstable();
            assert_eq!(got, exp, "round {}", round);
        }
    }

    /// The sessionizer only needs a person's events grouped by time bin: any order inside a bin
    /// gives the same rows and campaign aggregates as the fully sorted order.
    #[test]
    fn bins_match_sorted() {
        let mut rng = Rng(0x0bad_cafe_dead_beef);
        let w0 = 1_800_000_000_000i64 + 3_600_000;
        let w1 = w0 + 5 * MS_DAY + 1234;
        let cats: &'static [B] = Box::leak(vec![b"a" as B, b"bb", b"unknown"].into_boxed_slice());
        let name = pack_str(b"u_person");
        for round in 0..3000 {
            let n = 1 + rng.below(if round % 5 == 0 { 400 } else { 40 }) as usize;
            let span = [(w1 - w0) as u64, 3 * GAP as u64, (1 << 21) + 5][rng.below(3) as usize];
            let base = rng.below((w1 - w0) as u64 - span + 1);
            let mut ev: Vec<u64> = (0..n)
                .map(|_| {
                    // many events near multiples of the gap and of the bin size
                    let mut t = base + rng.below(span);
                    if rng.below(3) == 0 {
                        t = (t >> 20 << 20) + [0, 1, (1 << 20) - 1, GAP as u64 % (1 << 20)][rng.below(4) as usize];
                    }
                    let t = t.min((w1 - w0 - 1) as u64);
                    let pc = if rng.below(4) == 0 { ((1 + rng.below(5000)) << 8) | rng.below(3) } else { 0 };
                    (t << 32) | pc
                })
                .collect();
            let ncl = rng.below(8) as usize;
            let mut cl: Vec<CRec> = (0..ncl)
                .map(|i| CRec { k: 1, tc: (rng.below((w1 - w0 + SEVEN) as u64) << 32) | rng.below(3), cid: pack_str(leak(format!("k{}", (i * 7) % 5).into_bytes())) })
                .collect();
            sort_clicks(&mut cl);
            let run = |ev: &[u64]| {
                let mut w = Worker::new(w0, w1, 3, cats, 16);
                w.reserve_rows(name, ev.len() + 1);
                w.person(name, ev, &cl);
                (w.cols.names, w.cols.day, w.cols.sess, w.cols.ev, w.cols.rev, w.cols.lev, w.cols.top, w.cols.n, w.agg.v)
            };
            ev.sort_unstable();
            let a = run(&ev);
            // shuffle inside bins
            let mut i = 0;
            while i < n {
                let mut j = i;
                while j < n && (ev[j] >> BIN_SHIFT) == (ev[i] >> BIN_SHIFT) {
                    j += 1;
                }
                for k in (i + 1..j).rev() {
                    let r = i + rng.below((k - i + 1) as u64) as usize;
                    ev.swap(k, r);
                }
                i = j;
            }
            let b = run(&ev);
            assert!(a == b, "round {}", round);
            // and the sorted order agrees with a direct reading of the rule (gap > 30 min splits)
            ev.sort_unstable();
            let mut sessions = 0u64;
            let mut prev = None;
            for &e in &ev {
                let t = (e >> 32) as i64;
                if prev.map_or(true, |p| t - p > GAP) {
                    sessions += 1;
                }
                prev = Some(t);
            }
            assert_eq!(a.8.iter().map(|g| g.sessions).sum::<u64>(), sessions, "round {}", round);
        }
    }

    #[test]
    fn packed_strings() {
        let long: B = leak(vec![b'x'; 70_000]);
        let short: B = b"hello";
        assert_eq!(unpack_str(pack_str(short)), short);
        assert_eq!(unpack_str(pack_str(long)), long);
        let edge: B = leak(vec![b'y'; 0xFFFE]);
        assert_eq!(unpack_str(pack_str(edge)), edge);
        let edge2: B = leak(vec![b'z'; 0xFFFF]);
        assert_eq!(unpack_str(pack_str(edge2)), edge2);
    }
}
