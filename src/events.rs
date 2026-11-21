use crate::core::Name;
use crate::sys::*;
use crate::*;
use std::sync::atomic::Ordering;

pub const UF_TAIL_IN_STAGE3: () = ();

#[inline(always)]
fn user_of(r: &Rec) -> u32 {
    ((r.w1 >> 37) & (ODDU - 1)) as u32
}

#[inline(always)]
pub fn cpush(t: &mut TState, pkey: u64, w0: u64) {
    t.cx_ev[shard(pkey)].push(Rec { w0, w1: pkey });
}

pub fn cid_cmp(g: &G, a: u64, b: u64) -> std::cmp::Ordering {
    if (a | b) & RAW == 0 {
        return enc36(a).0.cmp(&enc36(b).0);
    }
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    g.id_str(a, b'k', &mut sa);
    g.id_str(b, b'k', &mut sb);
    sa.cmp(&sb)
}

pub unsafe fn process_run(g: &G, t: &mut TState, u: u32, recs: &mut [Rec]) {
    t.n_runs += 1;
    let tv = *g.utab.add(u as usize);
    if tv & (COMPLEX | EXTRA) == EXTRA {
        let mut buf = recs.to_vec();
        buf.extend(g.extra[&u].iter().map(|&w0| Rec { w0, w1: 0 }));
        t.n_odd += (buf.len() - recs.len()) as u64;
        return simple_run(g, t, u, &mut buf, tv);
    }
    simple_run(g, t, u, recs, tv)
}

unsafe fn simple_run(g: &G, t: &mut TState, u: u32, recs: &mut [Rec], tv: u16) {
    if tv & COMPLEX != 0 {
        t.cx_raw.extend(recs.iter().map(|r| Rec { w0: r.w0, w1: u as u64 }));
        t.n_complex += recs.len() as u64;
        return;
    }
    if recs.is_empty() {
        return;
    }
    claim(g, u);
    let mask = (tv & MASK) as u32;
    let mut bits = 0u32;
    let mut sorted = true;
    let mut prev = 0u64;
    for r in recs.iter() {
        bits |= 1 << (r.w0 & 15);
        sorted &= (r.w0 >> 33) >= prev;
        prev = r.w0 >> 33;
    }
    let class = if bits & !mask == 0 {
        mask
    } else if bits & (bits - 1) == 0 {
        bits
    } else {
        t.mixed_users.push(u);
        t.cx_raw.extend(recs.iter().map(|r| Rec { w0: r.w0, w1: u as u64 }));
        t.n_complex += recs.len() as u64;
        return;
    };
    if !sorted {
        recs.sort_unstable_by_key(|r| r.w0);
    }
    t.n_simple += recs.len() as u64;
    let mut st = t.core.begin(recs[0].w0);
    for r in recs.iter() {
        t.core.feed(&mut st, r.w0);
    }
    finish_simple(g, t, u, class, &st);
}

#[inline(always)]
fn class_of(tv: u16, bits: u32) -> u32 {
    let mask = (tv & MASK) as u32;
    if bits & !mask == 0 {
        mask
    } else if bits & (bits - 1) == 0 {
        bits
    } else {
        0
    }
}

#[inline(always)]
unsafe fn finish_simple(g: &G, t: &mut TState, u: u32, class: u32, st: &crate::core::St) {
    let csr = if crate::parse::SKIP & 4 != 0 { &[][..] } else { g.csr_range(u) };
    let mut allin = true;
    for c in csr {
        allin &= (class >> (c[0] & 15)) & 1 != 0;
    }
    if !allin {
        t.ck.clear();
        for c in csr {
            if (class >> (c[0] & 15)) & 1 != 0 {
                t.ck.push(*c);
            }
        }
    }
    let cks: &[[u64; 2]] = if allin { csr } else { &t.ck };
    t.core.finish(g, st, cks, Name::Simple(u, class));
}

#[inline(always)]
unsafe fn claim(g: &G, u: u32) {
    let word = &*g.claim.add(u as usize >> 6);
    let bit = 1u64 << (u & 63);
    if word.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        fallback("a user has two runs");
    }
}

#[inline(always)]
unsafe fn claim_word(g: &G, word: usize, bits: u64) {
    if bits != 0 && (*g.claim.add(word)).fetch_or(bits, Ordering::Relaxed) & bits != 0 {
        fallback("a user has two runs");
    }
}

struct Pool {
    next: AtomicUsize,
    end: usize,
}
use std::sync::atomic::AtomicUsize;
impl Pool {
    fn new(g: &G) -> Pool {
        let used = g.us_arena.used() & !((2usize << 20) - 1);
        let on = !std::env::var("XL_RECYCLE").is_ok_and(|v| v == "0");
        let base = g.us_arena.alloc(0) as usize - g.us_arena.used();
        Pool { next: AtomicUsize::new(base), end: if on { base + used } else { base } }
    }
    fn take(&self, len: usize, align: usize) -> *mut u8 {
        let mut a = self.next.load(Ordering::Relaxed);
        loop {
            let s = (a + align - 1) & !(align - 1);
            if s + len > self.end {
                return std::ptr::null_mut();
            }
            match self.next.compare_exchange_weak(a, s + len, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return s as *mut u8,
                Err(x) => a = x,
            }
        }
    }
    fn left(&self) -> usize {
        self.end.saturating_sub(self.next.load(Ordering::Relaxed))
    }
}
#[inline(always)]
fn r4k(x: usize) -> usize {
    (x + 4095) & !4095
}
#[derive(Clone, Copy)]
struct Share {
    dense: *mut u8,
    hot: *mut u8,
    agg: *mut u8,
    aggp: *mut u8,
    slab: *mut u8,
    slab_len: usize,
}
unsafe impl Sync for Share {}
fn fast_sizes(nc: usize, nd: usize) -> (usize, usize) {
    let n = BSIZE + GUARD + 64;
    let r64 = |x: usize| (x + 63) & !63;
    let dense = r64(16 * (BSIZE + 16)) + r64(8 * n) + 8 * r64(4 * n);
    let hot = r64(2 * (BSIZE / 8 + 2)) + 2 * r64(BSIZE / 8 + 8) + r64(4 * (BSIZE / 16 + 2)) + r64(2 * (BSIZE / 16 + 2)) + r64(4 * (BSIZE / 16 + 2)) + r64(2 * (BSIZE / 16 + 2)) + r64(8 * (CBOUND + 2)) + r64(64 * (nc + 2)) + r64(4 * (nc + 2) * nd) + r64(256 * RC_N) + 2 * r64(4 * (WL_CAP + WL_RUN + 64)) + r64(4 * (AL_CAP + 64));
    (dense, hot)
}
fn slab_rows(len: usize) -> (usize, usize) {
    let r = len.saturating_sub(8 * 4096 + 4096) * 5 / 131;
    (r, r * 2 / 5)
}
unsafe fn slab_cols(core: &mut crate::core::Core, slab: *mut u8, len: usize) {
    let (r, p) = slab_rows(len);
    if slab.is_null() || r < 4096 {
        return;
    }
    let mut at = slab;
    let mut part = |bytes: usize| -> *mut u8 {
        let q = at;
        at = at.add(r4k(bytes));
        q
    };
    core.keys.set_slab(part(4 * r) as *mut u32, r);
    core.day.set_slab(part(4 * r) as *mut i32, r);
    core.ses.set_slab(part(4 * r) as *mut i32, r);
    core.evs.set_slab(part(4 * r) as *mut i32, r);
    core.rev.set_slab(part(4 * r) as *mut i32, r);
    core.cat.set_slab(part(r), r);
    core.names.set_slab(part(9 * p + 64), 9 * p + 64);
    core.names_off.set_slab(part(4 * (p + 1)) as *mut u32, p + 1);
    if at as usize > slab as usize + len {
        fallback("slab layout");
    }
}
fn slab_full(core: &mut crate::core::Core) {
    core.keys.slab_full();
    core.day.slab_full();
    core.ses.slab_full();
    core.evs.slab_full();
    core.rev.slab_full();
    core.cat.slab_full();
    core.names.slab_full();
    core.names_off.slab_full();
}
fn slab_settle(core: &mut crate::core::Core) {
    let tight = |len: usize, cap: usize| cap - len.min(cap) < len / 64 + 4096;
    if core.day.in_slab() && tight(core.day.len(), core.day.cap()) {
        let n = core.day.len();
        core.keys.grow(n);
        core.day.grow(n);
        core.ses.grow(n);
        core.evs.grow(n);
        core.rev.grow(n);
        core.cat.grow(n);
    }
    if core.names.in_slab() && tight(core.names.len(), core.names.cap()) {
        core.names.grow(core.names.len());
    }
    if core.names_off.in_slab() && tight(core.names_off.len(), core.names_off.cap()) {
        core.names_off.grow(core.names_off.len());
    }
}

#[inline(always)]
unsafe fn scatter_chunk(p: *const Rec, n: usize, slots: *mut Rec, gen: u64, nextc: *const i8) {
    let gv = _mm_set_epi64x(gen as i64, 0);
    let put = |r: *const Rec| {
        let w1 = (*r).w1;
        let s = slots.add((w1 & (BSIZE as u64 - 1)) as usize);
        let old = (*s).w1;
        if old as u16 != w1.wrapping_add(gen) as u16 || ((w1 >> 16) & ING_MAX) < ((old >> 16) & ING_MAX) {
            _mm_storeu_si128(s as *mut __m128i, _mm_add_epi64(_mm_loadu_si128(r as *const __m128i), gv));
        }
    };
    if PF_CHUNK {
        _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(crate::parse::LINK_OFF));
    }
    let mut i = 0usize;
    while i + 4 <= n {
        if PF_CHUNK {
            _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(i * 16));
        }
        put(p.add(i));
        put(p.add(i + 1));
        put(p.add(i + 2));
        put(p.add(i + 3));
        i += 4;
    }
    while i < n {
        put(p.add(i));
        i += 1;
    }
}

#[inline(always)]
unsafe fn for_kept(slots: *const Rec, blkk: *const u16, keepm: *const u8, a: usize, n: usize, mut f: impl FnMut(u64)) {
    if n == 0 {
        return;
    }
    let (mut lo, mut hi) = (0usize, BSIZE / 8);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if *blkk.add(mid) as usize <= a {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mut blk = lo;
    let mut m = *keepm.add(blk) as u32;
    for _ in 0..a - *blkk.add(blk) as usize {
        m &= m.wrapping_sub(1);
    }
    let mut left = n;
    loop {
        while m != 0 {
            f((*slots.add(8 * blk + m.trailing_zeros() as usize)).w0);
            m &= m - 1;
            left -= 1;
            if left == 0 {
                return;
            }
        }
        blk += 1;
        if blk >= BSIZE / 8 {
            fallback("kept records");
        }
        m = *keepm.add(blk) as u32;
    }
}

#[inline(never)]
unsafe fn edge_copy_k(tid: usize, u: u32, fx: &mut Fast, a: usize, n: usize) -> Seg {
    let ed = &mut *fx.edge;
    let (dst, seg) = ed.seg(tid, u, n);
    if n == 0 {
        return seg;
    }
    let (slots, blkk, keepm) = (fx.slots as *const Rec, fx.blkk as *const u16, fx.keepm as *const u8);
    let ev_idx = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
    let end = a + n;
    let (mut lo, mut hi) = (0usize, BSIZE / 8);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if *blkk.add(mid) as usize <= a {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mut blk = lo;
    let mut at = *blkk.add(blk) as usize;
    let mut p = dst;
    while at < end {
        if blk >= BSIZE / 8 {
            fallback("kept records");
        }
        let m = *keepm.add(blk);
        let cnt = m.count_ones() as usize;
        if at >= a && at + cnt <= end {
            let r0 = _mm512_loadu_si512(slots.add(8 * blk) as *const _);
            let r1 = _mm512_loadu_si512(slots.add(8 * blk + 4) as *const _);
            _mm512_storeu_si512(p as *mut _, _mm512_maskz_compress_epi64(m, _mm512_permutex2var_epi64(r0, ev_idx, r1)));
            p = p.add(cnt);
        } else {
            let mut mm = m as u32;
            let mut i = at;
            while mm != 0 {
                if i >= a && i < end {
                    *p = (*slots.add(8 * blk + mm.trailing_zeros() as usize)).w0;
                    p = p.add(1);
                }
                mm &= mm - 1;
                i += 1;
            }
        }
        at += cnt;
        blk += 1;
    }
    if p != dst.add(n) {
        fallback("kept records");
    }
    seg
}

#[cold]
#[inline(never)]
unsafe fn odd_records(t: &mut TState, slots: *const Rec, blk: usize, odd: u8) {
    let mut m = odd as u32;
    while m != 0 {
        let r = *slots.add(8 * blk + m.trailing_zeros() as usize);
        m &= m - 1;
        t.cx_raw.push(Rec { w0: r.w0, w1: ODDU | user_of(&r) as u64 });
        t.n_odd += 1;
    }
}

const DIG: u32 = 1 << 31;
static DIG_MIN: AtomicUsize = AtomicUsize::new(4096);
#[derive(Clone, Copy)]
struct DigRun {
    first: bool,
    user: u32,
    a: usize,
    e: usize,
    s0: usize,
    s1: usize,
}

struct Edge {
    pieces: Vec<*mut u64>,
    used: usize,
    spare: usize,
    pool: *const Pool,
}
const EP_BYTES: usize = 8 * EP_WORDS + 4096;
const EP_WORDS: usize = 1 << 16;
const _: () = assert!(BSIZE <= EP_WORDS);
const EDGE0: Edge = Edge { pieces: Vec::new(), used: EP_WORDS, spare: 0, pool: std::ptr::null() };
static mut EDGE: [Edge; 64] = [EDGE0; 64];
impl Edge {
    unsafe fn start(tid: usize, pool: *const Pool) -> *mut Edge {
        if tid >= 64 {
            fallback("more than 64 threads");
        }
        let e = std::ptr::addr_of_mut!(EDGE[tid]);
        (*e).pool = pool;
        e
    }
    #[inline(always)]
    unsafe fn seg(&mut self, tid: usize, u: u32, n: usize) -> (*mut u64, Seg) {
        if self.used + n > EP_WORDS {
            self.piece(n);
        }
        let pi = self.pieces.len() - 1;
        let seg = Seg { tid: tid as u32, off: (pi * EP_WORDS + self.used) as u32, len: n as u32, user: u };
        let p = (*self.pieces.get_unchecked(pi)).add(self.used);
        self.used += n;
        (p, seg)
    }
    #[cold]
    #[inline(never)]
    unsafe fn piece(&mut self, n: usize) {
        if n > EP_WORDS {
            fallback("edge run over a piece");
        }
        if (self.pieces.len() + 1) * EP_WORDS > u32::MAX as usize {
            fallback("edge store over 4G records");
        }
        if self.spare != 0 {
            self.spare -= 1;
            let p = (*self.pieces.last().unwrap() as *mut u8).add(8 * EP_WORDS);
            self.pieces.push(p as *mut u64);
        } else {
            let mut p = if self.pool.is_null() { std::ptr::null_mut() } else { (*self.pool).take(EP_BYTES, 4096) };
            if p.is_null() {
                p = anon(2 << 20);
                self.spare = ((2 << 20) - 4096) / (8 * EP_WORDS) - 1;
            }
            self.pieces.push(p as *mut u64);
        }
        self.used = 0;
    }
}
#[inline(always)]
unsafe fn seg_words(s: &Seg, n: usize) -> &'static [u64] {
    if s.tid >= 64 {
        fallback("edge segment");
    }
    let e = &*std::ptr::addr_of!(EDGE[s.tid as usize]);
    let (pi, at) = (s.off as usize / EP_WORDS, s.off as usize % EP_WORDS);
    if pi >= e.pieces.len() || at + n > EP_WORDS || (pi + 1 == e.pieces.len() && at + n > e.used) {
        fallback("edge segment");
    }
    std::slice::from_raw_parts(e.pieces[pi].add(at), n)
}
#[inline(always)]
unsafe fn seg_w0(s: &Seg) -> &'static [u64] {
    if s.len & DIG != 0 {
        fallback("edge segment");
    }
    seg_words(s, s.len as usize)
}

#[inline(never)]
unsafe fn digest_put(tid: usize, fx: &mut Fast, d: &DigRun, q: *const u64, sidx: *const u32, sts: *const u32, np: usize) -> Seg {
    let ns = d.s1 - d.s0;
    let (ps, pw) = (fx.p_sid as *const u32, fx.p_w as *const u32);
    let mut pa = 0usize;
    while pa < np && (*ps.add(pa) as usize) < d.s0 {
        pa += 1;
    }
    let mut pb = pa;
    while pb < np && (*ps.add(pb) as usize) < d.s1 {
        pb += 1;
    }
    let words = 3 + 2 * ns + (pb - pa);
    if words > EP_WORDS || ns == 0 {
        fallback("digest size");
    }
    let ed = &mut *fx.edge;
    let (p, mut seg) = ed.seg(tid, d.user, words);
    seg.len = words as u32 | DIG;
    *p = ns as u64 | ((pb - pa) as u64) << 32;
    *p.add(1) = (d.e - d.a) as u64 | (1u64 << ((*q.add(d.a) >> 32) & 15)) << 32;
    *p.add(2) = (*q.add(d.e - 1) & 0xFFFF_FFFF) | (*q.add(d.a) & 0xFFFF_FFFF) << 32;
    let mut total = 0usize;
    for i in 0..ns {
        let s = d.s0 + i;
        let (ra, rb) = ((*sidx.add(s) & M31) as usize, (*sidx.add(s + 1) & M31) as usize);
        if rb <= ra || rb > d.e {
            fallback("digest sessions");
        }
        total += rb - ra;
        *p.add(3 + 2 * i) = *sts.add(s) as u64 | ((rb - ra) as u64) << 32;
        *p.add(4 + 2 * i) = *q.add(rb - 1) & 0xFFFF_FFFF;
    }
    if total != d.e - d.a || (*sidx.add(d.s0) & M31) as usize != d.a {
        fallback("digest sessions");
    }
    for j in pa..pb {
        *p.add(3 + 2 * ns + (j - pa)) = ((*ps.add(j) as usize - d.s0) as u64) << 32 | *pw.add(j) as u64;
    }
    seg
}
#[inline(always)]
unsafe fn digest_get(s: &Seg) -> (&'static [u64], &'static [u64], u64, u32, u64, u64) {
    let w = seg_words(s, (s.len & !DIG) as usize);
    if w.len() < 3 {
        fallback("edge segment");
    }
    let (ns, np) = ((w[0] & 0xFFFF_FFFF) as usize, (w[0] >> 32) as usize);
    if 3 + 2 * ns + np != w.len() || ns == 0 {
        fallback("edge segment");
    }
    (&w[3..3 + 2 * ns], &w[3 + 2 * ns..], w[1] & 0xFFFF_FFFF, (w[1] >> 32) as u32, w[2] >> 32, w[2] & 0xFFFF_FFFF)
}

#[inline(always)]
unsafe fn edge_copy(e: *mut Edge, tid: usize, u: u32, recs: &[Rec]) -> Seg {
    let (p, seg) = (*e).seg(tid, u, recs.len());
    for (i, r) in recs.iter().enumerate() {
        *p.add(i) = r.w0;
    }
    seg
}

fn stage3a_old(g: &G, tid: usize) {
    let t = g.t(tid);
    t.slots = vec![Rec { w0: u64::MAX, w1: u64::MAX }; BSIZE];
    let ed = unsafe { Edge::start(tid, std::ptr::null()) };
    let slots = t.slots.as_mut_ptr();
    loop {
        let b = g.next.fetch_add(1, Ordering::Relaxed);
        if b >= g.nb {
            break;
        }
        unsafe {
            let ta = std::time::Instant::now();
            std::ptr::write_bytes(slots as *mut u8, 0xFF, BSIZE * 16);
            let mut any = false;
            for th in 0..g.nt {
                g.t(th).ev.for_each(b, |recs| {
                    any = true;
                    for r in recs {
                        let s = slots.add((r.w1 & (BSIZE as u64 - 1)) as usize);
                        if ((r.w1 >> 16) & ING_MAX) < (((*s).w1 >> 16) & ING_MAX) {
                            *s = *r;
                        }
                    }
                });
            }
            let info = &mut *g.edges.add(b);
            info.n = 0;
            if !any {
                continue;
            }
            let tb = std::time::Instant::now();
            let mut k = 0usize;
            for s in 0..BSIZE {
                let r = *slots.add(s);
                if r.w0 >> 33 != INVALID_TS {
                    if r.w1 >> 63 != 0 {
                        t.cx_raw.push(Rec { w0: r.w0, w1: ODDU | user_of(&r) as u64 });
                        t.n_odd += 1;
                    } else {
                        *slots.add(k) = r;
                        k += 1;
                    }
                }
            }
            let tc = std::time::Instant::now();
            let recs = std::slice::from_raw_parts_mut(slots, k);
            let mut i = 0usize;
            if k > 0 {
                let u = user_of(&recs[0]);
                let mut j = 1;
                while j < k && user_of(&recs[j]) == u {
                    j += 1;
                }
                info.first = edge_copy(ed, tid, u, &recs[..j]);
                info.n = 1;
                i = j;
            }
            while i < k {
                let u = user_of(&recs[i]);
                let tv = *g.utab.add(u as usize);
                let mut j = i;
                if tv & (COMPLEX | EXTRA) != 0 {
                    while j < k && user_of(&recs[j]) == u {
                        j += 1;
                    }
                    if j == k {
                        info.last = edge_copy(ed, tid, u, &recs[i..j]);
                        info.n = 2;
                    } else {
                        process_run(g, t, u, &mut recs[i..j]);
                    }
                    i = j;
                    continue;
                }
                let mut st = t.core.begin(recs[i].w0);
                let (mut bits, mut prev, mut sorted) = (0u32, 0u64, true);
                while j < k {
                    let r = *recs.get_unchecked(j);
                    if user_of(&r) != u {
                        break;
                    }
                    bits |= 1 << (r.w0 & 15);
                    sorted &= r.w0 >> 33 >= prev;
                    prev = r.w0 >> 33;
                    t.core.feed(&mut st, r.w0);
                    j += 1;
                }
                if j == k {
                    t.core.abort(&st);
                    info.last = edge_copy(ed, tid, u, &recs[i..j]);
                    info.n = 2;
                    break;
                }
                let class = class_of(tv, bits);
                if class == 0 || !sorted {
                    t.core.abort(&st);
                    process_run(g, t, u, &mut recs[i..j]);
                } else {
                    t.n_runs += 1;
                    t.n_simple += (j - i) as u64;
                    claim(g, u);
                    finish_simple(g, t, u, class, &st);
                }
                i = j;
            }
            let td = std::time::Instant::now();
            t.prof[0] += (tb - ta).as_nanos() as u64;
            t.prof[1] += (tc - tb).as_nanos() as u64;
            t.prof[2] += (td - tc).as_nanos() as u64;
        }
    }
}


use std::arch::x86_64::*;

const GEN_MAX: u64 = {
    let v = crate::env_u32(option_env!("XL_GENMAX"), BSIZE as u32) as u64;
    if v >= 1 && v <= BSIZE as u64 {
        v
    } else {
        BSIZE as u64
    }
};
const PF_CHUNK: bool = true;
const RC_NOALL: bool = option_env!("XL_RCNOALL").is_some();
const GB_MAX: usize = crate::env_u32(option_env!("XL_GBMAX"), 1 << 31) as usize;
const XRC: usize = crate::env_u32(option_env!("XL_XRC"), 4) as usize;
const M31: u32 = 0x7FFF_FFFF;
const FLAG: u32 = 0x8000_0000;
const NONE: u32 = u32::MAX;
const GUARD: usize = 16;
const DIV_M: u64 = 6_515_625;


const XP: bool = option_env!("XL_XPROF").is_some();
const NPH: usize = 16;
const XD: bool = option_env!("XL_XD").is_some();
const XR: bool = option_env!("XL_XRU").is_some();
const NEVT: usize = 6;
const PH_NAMES: [&str; NPH] = ["scatter", "passA", "passB", "passC", "runs", "lflush", "cred", "purch", "rows", "other", "r_head", "r_name", "r_light", "r_heavy", "cal", "x"];
static XFLT: [std::sync::atomic::AtomicU64; NPH] = unsafe { std::mem::transmute([0u64; NPH]) };
static XRU: [std::sync::atomic::AtomicU64; 4] = unsafe { std::mem::transmute([0u64; 4]) };
fn thread_ru() -> [u64; 4] {
    let mut ru = [0i64; 18];
    unsafe { getrusage(1, &mut ru) };
    [(ru[0] * 1_000_000 + ru[1]) as u64, (ru[2] * 1_000_000 + ru[3]) as u64, ru[8] as u64, ru[9] as u64]
}
static XH: [std::sync::atomic::AtomicU64; 32] = unsafe { std::mem::transmute([0u64; 32]) };
static XCNT: [std::sync::atomic::AtomicU64; 32] = unsafe { std::mem::transmute([0u64; 32]) };
#[inline(always)]
fn xc(i: usize, v: u64) {
    if XD {
        XCNT[i].fetch_add(v, Ordering::Relaxed);
    }
}
static XACC: [[std::sync::atomic::AtomicU64; NEVT + 1]; NPH] = unsafe { std::mem::transmute([[0u64; NEVT + 1]; NPH]) };
extern "C" {
    fn syscall(n: i64, ...) -> i64;
    fn read(fd: i32, buf: *mut std::ffi::c_void, n: usize) -> isize;
    fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
}
struct Xp {
    fd: i32,
    nev: usize,
    ru0: [u64; 4],
    flt: u64,
    facc: [u64; NPH],
    t: std::time::Instant,
    last: [u64; NEVT],
    acc: [[u64; NEVT + 1]; NPH],
}
impl Xp {
    fn new() -> Xp {
        let mut x = Xp { fd: -1, nev: 0, ru0: if XP || XR { thread_ru() } else { [0; 4] }, flt: if XP { thread_ru()[2] } else { 0 }, facc: [0; NPH], t: std::time::Instant::now(), last: [0; NEVT], acc: [[0; NEVT + 1]; NPH] };
        if !XP {
            return x;
        }
        let evs: [(u32, u64); NEVT] = [(0, 1), (0, 0), (4, 0xC1), (3, 0x10000), (0, 3), (0, 5)];
        for (ty, cfg) in evs {
            let mut attr = [0u64; 8];
            attr[0] = ty as u64 | 64u64 << 32;
            attr[1] = cfg;
            attr[4] = 8;
            attr[5] = 0x60;
            let fd = unsafe { syscall(298, attr.as_ptr(), 0i32, -1i32, x.fd, 0u64) } as i32;
            if fd < 0 {
                eprintln!("XPROF: perf_event_open failed for ({ty}, {cfg:#x})");
                continue;
            }
            if x.fd < 0 {
                x.fd = fd;
            }
            x.nev += 1;
        }
        x.read_now();
        x.t = std::time::Instant::now();
        x
    }
    fn read_now(&mut self) -> [u64; NEVT] {
        let mut buf = [0u64; NEVT + 1];
        unsafe { read(self.fd, buf.as_mut_ptr() as *mut std::ffi::c_void, 8 * (NEVT + 1)) };
        let mut d = [0u64; NEVT];
        for i in 0..NEVT {
            d[i] = buf[i + 1].wrapping_sub(self.last[i]);
            self.last[i] = buf[i + 1];
        }
        d
    }
    #[inline(always)]
    fn ph(&mut self, ph: usize) {
        if XP {
            self.ph_slow(ph);
        }
    }
    #[inline(never)]
    fn ph_slow(&mut self, ph: usize) {
        let now = std::time::Instant::now();
        let d = self.read_now();
        let f = thread_ru()[2];
        self.facc[ph] += f - self.flt;
        self.flt = f;
        self.acc[ph][0] += (now - self.t).as_nanos() as u64;
        for i in 0..NEVT {
            self.acc[ph][i + 1] += d[i];
        }
        self.read_now();
        self.t = std::time::Instant::now();
    }
    fn done(&self) {
        if XP || XR {
            let ru = thread_ru();
            for i in 0..4 {
                XRU[i].fetch_add(ru[i] - self.ru0[i], Ordering::Relaxed);
            }
        }
        if XP {
            for p in 0..NPH {
                XFLT[p].fetch_add(self.facc[p], Ordering::Relaxed);
                for i in 0..=NEVT {
                    XACC[p][i].fetch_add(self.acc[p][i], Ordering::Relaxed);
                }
            }
        }
    }
}
fn xp_report(nev: u64) {
    if XR {
        eprintln!("XRU stage 3a threads: user {:.1} ms, system {:.1} ms, minor faults {}", XRU[0].load(Ordering::Relaxed) as f64 / 1e3, XRU[1].load(Ordering::Relaxed) as f64 / 1e3, XRU[2].load(Ordering::Relaxed));
    }
    if !XP {
        return;
    }
    let mut tot = [0u64; NEVT + 1];
    eprintln!("XPROF rusage of stage 3a threads: user {:.1} ms, system {:.1} ms, minor faults {}, major {}", XRU[0].load(Ordering::Relaxed) as f64 / 1e3, XRU[1].load(Ordering::Relaxed) as f64 / 1e3, XRU[2].load(Ordering::Relaxed), XRU[3].load(Ordering::Relaxed));
    eprintln!("XCNT runs light0/light/heavy {} {} {} | clicks light/heavy {} {} | sessions light0/light/heavy {} {} {} | scalar runs {} recs {} | n-hist {:?}", XCNT[0].load(Ordering::Relaxed), XCNT[1].load(Ordering::Relaxed), XCNT[2].load(Ordering::Relaxed), XCNT[3].load(Ordering::Relaxed), XCNT[4].load(Ordering::Relaxed), XCNT[5].load(Ordering::Relaxed), XCNT[6].load(Ordering::Relaxed), XCNT[7].load(Ordering::Relaxed), XCNT[25].load(Ordering::Relaxed), XCNT[26].load(Ordering::Relaxed), (8..25).map(|i| XCNT[i].load(Ordering::Relaxed)).collect::<Vec<_>>());
    eprintln!("XH heavy bins n<=16(not all in class),<=32,<=64,<=128,<=512,more: runs {:?} clicks {:?} sessions {:?} n*m {:?}", (0..6).map(|i| XH[i].load(Ordering::Relaxed)).collect::<Vec<_>>(), (6..12).map(|i| XH[i].load(Ordering::Relaxed)).collect::<Vec<_>>(), (12..18).map(|i| XH[i].load(Ordering::Relaxed)).collect::<Vec<_>>(), (18..24).map(|i| XH[i].load(Ordering::Relaxed)).collect::<Vec<_>>());
    eprintln!("XPROF per kept event (n={nev}): phase thread-ms | instr cycles uops L1Dmiss L2miss brmiss");
    for p in 0..=NPH {
        let v: Vec<u64> = if p < NPH { (0..=NEVT).map(|i| XACC[p][i].load(Ordering::Relaxed)).collect() } else { tot.to_vec() };
        if p < NPH {
            for i in 0..=NEVT {
                tot[i] += v[i];
            }
        }
        let f = |i: usize| v[i] as f64 / nev as f64;
        eprintln!("XPROF {:8} {:7.1} | {:7.2} {:7.2} {:7.2} {:7.4} {:7.4} {:7.4} | faults {}", if p < NPH { PH_NAMES[p] } else { "TOTAL" }, v[0] as f64 / 1e6, f(1), f(2), f(3), f(4), f(5), f(6), if p < NPH { XFLT[p].load(Ordering::Relaxed) } else { 0 });
    }
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct V16([u32; 16]);

static ONEHOT: [V16; 16] = {
    let mut t = [V16([0; 16]); 16];
    let mut i = 0;
    while i < 16 {
        t[i].0[i] = 1;
        i += 1;
    }
    t
};

const POW36: [u64; 9] = [1, 36, 1296, 46_656, 1_679_616, 60_466_176, 2_176_782_336, 78_364_164_096, 2_821_109_907_456];

#[inline(always)]
pub fn fast_name(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    let best = name_key_scalar(ukey, u, class);
    if best == u64::MAX {
        return (u64::MAX, 0);
    }
    let (norm, len) = (best >> 4, (best & 15) as usize);
    let hi = (norm / 1_679_616) as u32;
    let lo = (norm % 1_679_616) as u32;
    let p = |v: u32| crate::PAIR36[v as usize] as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    (le.swap_bytes() & (u64::MAX << (8 * (8 - len))), len)
}

#[inline(always)]
fn name_key_scalar(ukey: u64, u: u32, class: u32) -> u64 {
    let mut m = class;
    let mut best = u64::MAX;
    while m != 0 {
        let a = m.trailing_zeros() as u64;
        m &= m - 1;
        let y = crate::keyrec::scramble((u as u64) << 9 | a, ukey);
        let len = 1 + (y >= POW36[1]) as usize + (y >= POW36[2]) as usize + (y >= POW36[3]) as usize + (y >= POW36[4]) as usize + (y >= POW36[5]) as usize + (y >= POW36[6]) as usize + (y >= POW36[7]) as usize;
        let key = (y * POW36[8 - len]) << 4 | len as u64;
        best = best.min(key);
    }
    best
}

#[inline(always)]
fn key_chars(best: u64) -> (u64, usize) {
    let (norm, len) = (best >> 4, (best & 15) as usize);
    let hi = (norm / 1_679_616) as u32;
    let lo = (norm % 1_679_616) as u32;
    let p = |v: u32| unsafe { *crate::PAIR36.get_unchecked(v as usize) } as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    (le.swap_bytes() & (u64::MAX << (8 * (8 - len))), len)
}

const NAME_C: f64 = (1.0 / 1_679_616.0) * (1.0 - 1.0 / 1_099_511_627_776.0);

#[inline(always)]
unsafe fn key_chars4(best: __m256i) -> __m256i {
    let norm = _mm256_srli_epi64::<4>(best);
    let len = _mm256_and_si256(best, _mm256_set1_epi64x(15));
    let d = _mm256_set1_epi64x(1_679_616);
    let mut hi = _mm256_cvttpd_epu64(_mm256_mul_pd(_mm256_cvtepu64_pd(norm), _mm256_set1_pd(NAME_C)));
    let mut lo = _mm256_sub_epi64(norm, mul32_256(hi, d));
    let fix = _mm256_cmpge_epu64_mask(lo, d);
    hi = _mm256_mask_add_epi64(hi, fix, hi, _mm256_set1_epi64x(1));
    lo = _mm256_mask_sub_epi64(lo, fix, lo, d);
    let m = _mm256_set1_epi64x(207_127);
    let c1296 = _mm256_set1_epi64x(1296);
    let h1 = _mm256_srli_epi64::<24>(mul32_256(_mm256_srli_epi64::<4>(hi), m));
    let h0 = _mm256_sub_epi64(hi, mul32_256(h1, c1296));
    let l1 = _mm256_srli_epi64::<24>(mul32_256(_mm256_srli_epi64::<4>(lo), m));
    let l0 = _mm256_sub_epi64(lo, mul32_256(l1, c1296));
    let p = _mm256_or_si256(_mm256_or_si256(h1, _mm256_slli_epi64::<16>(h0)), _mm256_or_si256(_mm256_slli_epi64::<32>(l1), _mm256_slli_epi64::<48>(l0)));
    let d1 = _mm256_mulhi_epu16(p, _mm256_set1_epi16(1821));
    let d0 = _mm256_sub_epi16(p, _mm256_mullo_epi16(d1, _mm256_set1_epi16(36)));
    let dg = _mm256_or_si256(d1, _mm256_slli_epi16::<8>(d0));
    let ch = _mm256_add_epi8(_mm256_add_epi8(dg, _mm256_set1_epi8(48)), _mm256_and_si256(_mm256_cmpgt_epi8(dg, _mm256_set1_epi8(9)), _mm256_set1_epi8(39)));
    let sh = _mm256_sub_epi64(_mm256_set1_epi64x(64), _mm256_slli_epi64::<3>(len));
    _mm256_and_si256(ch, _mm256_srlv_epi64(_mm256_set1_epi64x(-1), sh))
}

#[inline(always)]
unsafe fn name_key4(ukey: u64, u: u32, al: __m256i) -> __m256i {
    name_key4v(ukey, _mm256_or_si256(_mm256_set1_epi64x(((u as u64) << 9) as i64), al))
}

#[inline(always)]
unsafe fn name_key4v(ukey: u64, v: __m256i) -> __m256i {
    use crate::keyrec::{A, B, MASK as M40};
    let one = _mm256_set1_epi64x(1);
    let m40 = _mm256_set1_epi64x(M40 as i64);
    let mut m = _mm256_and_si256(_mm256_xor_si256(_mm256_mullo_epi64(v, _mm256_set1_epi64x(A as i64)), _mm256_set1_epi64x(ukey as i64)), m40);
    m = _mm256_xor_si256(m, _mm256_srli_epi64(m, 17));
    let y = _mm256_and_si256(_mm256_mullo_epi64(m, _mm256_set1_epi64x(B as i64)), m40);
    let bits = _mm256_sub_epi64(_mm256_set1_epi64x(64), _mm256_lzcnt_epi64(_mm256_or_si256(y, one)));
    let g = _mm256_add_epi64(_mm256_srli_epi64(_mm256_mul_epu32(_mm256_sub_epi64(bits, one), _mm256_set1_epi64x(99)), 9), one);
    let pow_a = _mm512_loadu_si512(POW36.as_ptr().add(1) as *const _);
    let pow_b = _mm512_loadu_si512(POW36.as_ptr() as *const _);
    let p = _mm512_castsi512_si256(_mm512_permutexvar_epi64(_mm512_zextsi256_si512(_mm256_sub_epi64(g, one)), pow_a));
    let len = _mm256_mask_add_epi64(g, _mm256_cmpge_epu64_mask(y, p), g, one);
    let fac = _mm512_castsi512_si256(_mm512_permutexvar_epi64(_mm512_zextsi256_si512(_mm256_sub_epi64(_mm256_set1_epi64x(8), len)), pow_b));
    _mm256_or_si256(_mm256_slli_epi64(_mm256_mullo_epi64(y, fac), 4), len)
}

#[inline(always)]
unsafe fn min4(k: __m256i, n: u32) -> u64 {
    let k = _mm256_mask_mov_epi64(_mm256_set1_epi64x(-1), ((1u32 << n) - 1) as u8, k);
    let a = _mm256_min_epu64(k, _mm256_permute4x64_epi64::<0b01_00_11_10>(k));
    let b = _mm256_min_epu64(a, _mm256_permute4x64_epi64::<0b00_00_00_01>(a));
    _mm256_extract_epi64::<0>(b) as u64
}

#[inline(always)]
pub unsafe fn fast_name_y(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    let n = class.count_ones();
    if n > 8 || n == 0 {
        return fast_name(ukey, u, class);
    }
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let al = _mm512_maskz_compress_epi32(class as u16, iota);
    let mut best = min4(name_key4(ukey, u, _mm256_cvtepu32_epi64(_mm512_castsi512_si128(al))), n.min(4));
    if n > 4 {
        best = best.min(min4(name_key4(ukey, u, _mm256_cvtepu32_epi64(_mm512_extracti32x4_epi32::<1>(al))), n - 4));
    }
    key_chars(best)
}

#[inline(always)]
pub unsafe fn fast_name_v(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    use crate::keyrec::{A, B, MASK as M40};
    let n = class.count_ones();
    if n > 8 || n == 0 {
        return fast_name(ukey, u, class);
    }
    let one = _mm512_set1_epi64(1);
    let m40 = _mm512_set1_epi64(M40 as i64);
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let al = _mm512_cvtepu32_epi64(_mm512_castsi512_si256(_mm512_maskz_compress_epi32(class as u16, iota)));
    let v = _mm512_or_si512(_mm512_set1_epi64(((u as u64) << 9) as i64), al);
    let mut m = _mm512_and_si512(_mm512_xor_si512(_mm512_mullo_epi64(v, _mm512_set1_epi64(A as i64)), _mm512_set1_epi64(ukey as i64)), m40);
    m = _mm512_xor_si512(m, _mm512_srli_epi64(m, 17));
    let y = _mm512_and_si512(_mm512_mullo_epi64(m, _mm512_set1_epi64(B as i64)), m40);
    let bits = _mm512_sub_epi64(_mm512_set1_epi64(64), _mm512_lzcnt_epi64(_mm512_or_si512(y, one)));
    let g = _mm512_add_epi64(_mm512_srli_epi64(_mm512_mul_epu32(_mm512_sub_epi64(bits, one), _mm512_set1_epi64(99)), 9), one);
    let pow_a = _mm512_loadu_si512(POW36.as_ptr().add(1) as *const _);
    let pow_b = _mm512_loadu_si512(POW36.as_ptr() as *const _);
    let p = _mm512_permutexvar_epi64(_mm512_sub_epi64(g, one), pow_a);
    let len = _mm512_mask_add_epi64(g, _mm512_cmpge_epu64_mask(y, p), g, one);
    let fac = _mm512_permutexvar_epi64(_mm512_sub_epi64(_mm512_set1_epi64(8), len), pow_b);
    let key = _mm512_or_si512(_mm512_slli_epi64(_mm512_mullo_epi64(y, fac), 4), len);
    let best = _mm512_mask_reduce_min_epu64(((1u32 << n) - 1) as u8, key);
    let (norm, len) = (best >> 4, (best & 15) as usize);
    let hi = (norm / 1_679_616) as u32;
    let lo = (norm % 1_679_616) as u32;
    let p = |v: u32| crate::PAIR36[v as usize] as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    (le.swap_bytes() & (u64::MAX << (8 * (8 - len))), len)
}

fn name_selftest(g: &G) {
    use crate::keyrec::{unshuffle, A, MASK as M40};
    let mut x = 0x1234_5678_9ABC_DEFu64;
    let bad = |u: u32, class: u32| -> ! {
        eprintln!("[v2b] CHECK FAILED: fast_name({u}, {class:#x})");
        unsafe { crate::sys::_exit(5) }
    };
    for i in 0..600_000u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u = ((x >> 24) as u32) & (ODDU as u32 - 1);
        let class = if i % 3 == 0 { 1 << ((x >> 8) % NALIAS) } else { ((x >> 3) as u32 & MASK as u32).max(1) };
        let mut key = if i % 2 == 0 { g.ukey } else { (x >> 7) & M40 };
        if i >= 200_000 {
            let j = i - 200_000;
            let y = if j < 100_000 {
                j
            } else if j < 200_000 {
                (POW36[1 + (j % 7) as usize] + (j / 7) % 41).wrapping_sub(20) & M40
            } else {
                (x >> 13) & M40 >> (j % 40)
            };
            let v = (u as u64) << 9 | class.trailing_zeros() as u64;
            key = unshuffle(y) ^ (v.wrapping_mul(A) & M40);
            if crate::keyrec::scramble(v, key) != y {
                bad(u, class);
            }
        }
        let want = crate::core::simple_name_key(key, u, class);
        if want != fast_name(key, u, class) || want != unsafe { fast_name_v(key, u, class) } || want != unsafe { fast_name_y(key, u, class) } {
            bad(u, class);
        }
        let b = name_key_scalar(key, u, class);
        let mut ch = [0u64; 4];
        unsafe { _mm256_storeu_si256(ch.as_mut_ptr() as *mut __m256i, key_chars4(_mm256_set_epi64x(x as i64, (x >> 9) as i64, 0x10, b as i64))) };
        if ch[0].to_le_bytes() != want.0.to_be_bytes() || (b & 15) as usize != want.1 {
            bad(u, class);
        }
    }
    for i in 0..3_000_000u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let lim = POW36[8];
        let norm = match i % 4 {
            0 => (x >> 20) % lim,
            1 => (((x >> 20) % 1_679_616) * 1_679_616 + (i / 4) % 3).wrapping_sub(1) % lim,
            2 => (((x >> 20) % (lim / 1296)) * 1296 + (i / 4) % 3).wrapping_sub(1) % lim,
            _ => (lim - 1 - (i / 4) % 5000) >> ((x >> 50) % 42),
        };
        let mut ks = [0u64; 4];
        for (j, k) in ks.iter_mut().enumerate() {
            *k = (norm + j as u64 * 7) % lim << 4 | (1 + (i + j as u64) % 8);
        }
        let mut ch = [0u64; 4];
        unsafe { _mm256_storeu_si256(ch.as_mut_ptr() as *mut __m256i, key_chars4(_mm256_loadu_si256(ks.as_ptr() as *const __m256i))) };
        for j in 0..4 {
            let (v, _) = key_chars(ks[j]);
            if ch[j].to_le_bytes() != v.to_be_bytes() {
                eprintln!("[v2b] CHECK FAILED: key_chars4({:#x})", ks[j]);
                unsafe { crate::sys::_exit(5) }
            }
        }
    }
    for xx in 0..1_679_616u64 {
        if ((xx >> 4) * 207_127) >> 24 != xx / 1296 || (xx < 1296 && (xx * 1821) >> 16 != xx / 36) {
            eprintln!("[v2b] CHECK FAILED: digit division at {xx}");
            unsafe { crate::sys::_exit(5) }
        }
    }
    for y in 0..(1u64 << 22) {
        if (y * DIV_M) >> 39 != y / 84_375 {
            eprintln!("[v2b] CHECK FAILED: day division at {y}");
            unsafe { crate::sys::_exit(5) }
        }
    }
}

const FN_: usize = BSIZE + GUARD + 64;
const fn r64c(x: usize) -> usize {
    (x + 63) & !63
}
const OQ_SIDX: usize = r64c(8 * FN_);
const OQ_STS: usize = OQ_SIDX + r64c(4 * FN_);
const OQ_SDAY: usize = OQ_STS + r64c(4 * FN_) + 4 * GUARD;
const OQ_SCAMP: usize = OQ_STS + 2 * r64c(4 * FN_);
const OQ_SLOTS: usize = r64c(16 * (BSIZE + 16)) - 16 * 8;
const OQ_ROWDAY: usize = OQ_STS + 3 * r64c(4 * FN_);
const OQ_ROWS0: usize = OQ_STS + 4 * r64c(4 * FN_);
const OQ_ROWR0: usize = OQ_STS + 5 * r64c(4 * FN_);
const OQ_ROWREV: usize = OQ_STS + 6 * r64c(4 * FN_);
const OP_RUNS0: usize = 3 * r64c(4 * FN_);
const OP_RUNROW0: usize = 2 * r64c(4 * FN_);
const OP_RUNPIDX: usize = r64c(4 * FN_);
const OP_NMV: usize = OP_W + r64c(4 * BSIZE);
const OP_NMK: usize = OP_NMV + r64c(8 * (8 * FN_ + 16));
const OP_NMR: usize = OP_NMK + r64c(8 * (8 * FN_ + 16));
const OP_NMBEST: usize = OP_NMR + r64c(4 * (8 * FN_ + 16));
const OR_WLS: usize = r64c(256 * RC_N);
const OR_WLR: usize = OR_WLS + r64c(4 * (WL_CAP + WL_RUN + 64));
const OP_ROWCAT: usize = r64c(FN_) + 3 * r64c(4 * FN_);
const OB_KEEPM: usize = r64c(2 * (BSIZE / 8 + 2));
const OB_PKM: usize = OB_KEEPM + r64c(BSIZE / 8 + 8);
const OB_BLKNS: usize = OB_PKM + r64c(BSIZE / 8 + 8);
const OB_SSB: usize = OB_BLKNS + r64c(4 * (BSIZE / 16 + 2));
const OB_BLKNROW: usize = OB_SSB + r64c(2 * (BSIZE / 16 + 2));
const OB_RWB: usize = OB_BLKNROW + r64c(4 * (BSIZE / 16 + 2));
const OP_CELL: usize = r64c(2 * (BSIZE + 64));
const OP_SID: usize = OP_CELL + r64c(4 * BSIZE);
const OP_ROW: usize = OP_SID + r64c(4 * BSIZE);
const OP_W: usize = OP_ROW + r64c(4 * BSIZE);
#[inline(always)]
unsafe fn at<T>(base: *const u8, off: usize) -> *mut T {
    base.add(off) as *mut T
}
#[inline(always)]
fn opaque(p: *const u8) -> *const u8 {
    let mut p = p;
    unsafe { std::arch::asm!("/* {0} */", inout(reg) p, options(pure, nomem, nostack, preserves_flags)) };
    p
}

struct Fast {
    slots: *mut Rec,
    q: *mut u64,
    blkk: *mut u16,
    keepm: *mut u8,
    pkm: *mut u8,
    sidx: *mut u32,
    sts: *mut u32,
    sday: *mut u32,
    scamp: *mut u32,
    blk_ns: *mut u32,
    ssb: *mut u16,
    row_day: *mut u32,
    row_s0: *mut u32,
    row_r0: *mut u32,
    row_rev: *mut u32,
    row_cat: *mut u8,
    blk_nrow: *mut u32,
    rwb: *mut u16,
    run_s0: *mut u32,
    run_row0: *mut u32,
    run_pidx: *mut u32,
    plist: *mut u16,
    p_cell: *mut u32,
    p_sid: *mut u32,
    p_row: *mut u32,
    p_w: *mut u32,
    nm_v: *mut u64,
    nm_k: *mut u64,
    nm_r: *mut u32,
    nm_best: *mut u64,
    mix: Vec<u32>,
    f: Vec<V16>,
    last: *mut u64,
    gbase: u32,
    asst: *mut V16,
    cred: *mut u32,
    rc: *mut Rc,
    wl_s: *mut u32,
    wl_r: *mut u32,
    alist: *mut u32,
    edge: *mut Edge,
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Rc {
    ts: [u32; 16],
    camp: [u32; 16],
    conf: [u32; 16],
    vm: u32,
    pad: [u32; 15],
}
const RC_N: usize = 128;
const WL_CAP: usize = 4096;
const WL_RUN: usize = 1024;
const AL_CAP: usize = 4096;


#[inline(never)]
unsafe fn light_flush(fx: &mut Fast, nw: usize, org: u32) {
    let (wl_s, wl_r, list) = (fx.wl_s, fx.wl_r, fx.alist);
    let qb = fx.q as *const u8;
    let rcb = fx.rc as *const u8;
    let af = fx.asst as *mut u32;
    let weekv = _mm512_set1_epi32(WEEK_MS as i32);
    let mut nl = 0usize;
    for i in 0..nw {
        let x = *wl_s.add(i) as usize;
        let rcp = opaque(rcb.add(*wl_r.add(i) as usize)) as *const Rc;
        let xq = opaque(qb.add(4 * x));
        let vm = (*rcp).vm;
        let sv = _mm512_set1_epi32(*(xq.add(OQ_STS) as *const u32) as i32);
        let dv = _mm512_set1_epi32(*(xq.add(OQ_SDAY) as *const u32) as i32);
        let ts = _mm512_load_si512((*rcp).ts.as_ptr() as *const _);
        let win = cmp_mask::<2>(_mm512_sub_epi32(ts, sv), weekv) & vm;
        let cv = *(*rcp).camp.as_ptr().add((win | 1).ilog2() as usize) >> 4;
        *(xq.add(OQ_SCAMP) as *mut u32) = std::hint::select_unpredictable(win != 0, cv, org);
        let lo = win & win.wrapping_neg();
        let am = win & _mm512_cmplt_epu32_mask(_mm512_load_si512((*rcp).conf.as_ptr() as *const _), _mm512_set1_epi32(lo as i32)) as u32;
        let cells = _mm512_add_epi32(_mm512_load_si512((*rcp).camp.as_ptr() as *const _), dv);
        _mm512_storeu_si512(list.add(nl) as *mut _, _mm512_maskz_compress_epi32(am as u16, cells));
        nl += am.count_ones() as usize;
        if nl > AL_CAP {
            for j in 0..nl {
                *af.add(*list.add(j) as usize) += 1;
            }
            nl = 0;
        }
    }
    for i in 0..nl {
        *af.add(*list.add(i) as usize) += 1;
    }
}
impl Fast {
    fn new(nc: usize, nd: usize, sh: &Share) -> Fast {
        let n = BSIZE + GUARD + 64;
        let r64 = |x: usize| (x + 63) & !63;
        let (dense, hot) = fast_sizes(nc, nd);
        let cold = r64(n) + 3 * r64(4 * n) + r64(2 * (BSIZE + 64)) + 4 * r64(4 * BSIZE) + 2 * r64(8 * (8 * n + 16)) + r64(4 * (8 * n + 16)) + r64(8 * n);
        unsafe {
            let hp = if sh.dense.is_null() {
                anon(dense)
            } else {
                std::ptr::write_bytes(sh.dense, 0, r64(16 * (BSIZE + 16)));
                sh.dense
            };
            let sp = anon(cold + if sh.hot.is_null() { r4k(hot) } else { 0 });
            madvise(sp, cold + if sh.hot.is_null() { r4k(hot) } else { 0 }, MADV_NOHUGEPAGE);
            let (ht, cd) = if sh.hot.is_null() { (sp, sp.add(r4k(hot))) } else { (sh.hot, sp) };
            let (mut h, mut q, mut c) = (0usize, 0usize, 0usize);
            let take = |base: *mut u8, cur: &mut usize, bytes: usize| -> *mut u8 {
                let p = base.add(*cur);
                *cur += r64(bytes);
                p
            };
            let fx = Fast {
                slots: (take(hp, &mut h, 16 * (BSIZE + 16)) as *mut Rec).add(8),
                q: take(hp, &mut h, 8 * n) as *mut u64,
                sidx: take(hp, &mut h, 4 * n) as *mut u32,
                sts: take(hp, &mut h, 4 * n) as *mut u32,
                sday: (take(hp, &mut h, 4 * n) as *mut u32).add(GUARD),
                scamp: take(hp, &mut h, 4 * n) as *mut u32,
                row_day: take(hp, &mut h, 4 * n) as *mut u32,
                row_s0: take(hp, &mut h, 4 * n) as *mut u32,
                row_r0: take(hp, &mut h, 4 * n) as *mut u32,
                row_rev: take(hp, &mut h, 4 * n) as *mut u32,
                blkk: take(ht, &mut q, 2 * (BSIZE / 8 + 2)) as *mut u16,
                keepm: take(ht, &mut q, BSIZE / 8 + 8),
                pkm: take(ht, &mut q, BSIZE / 8 + 8),
                blk_ns: take(ht, &mut q, 4 * (BSIZE / 16 + 2)) as *mut u32,
                ssb: take(ht, &mut q, 2 * (BSIZE / 16 + 2)) as *mut u16,
                blk_nrow: take(ht, &mut q, 4 * (BSIZE / 16 + 2)) as *mut u32,
                rwb: take(ht, &mut q, 2 * (BSIZE / 16 + 2)) as *mut u16,
                mix: Vec::new(),
                f: vec![V16([0; 16]); 64],
                last: take(ht, &mut q, 8 * (CBOUND + 2)) as *mut u64,
                gbase: 0,
                asst: take(ht, &mut q, 64 * (nc + 2)) as *mut V16,
                cred: take(ht, &mut q, 4 * (nc + 2) * nd) as *mut u32,
                rc: take(ht, &mut q, 256 * RC_N) as *mut Rc,
                wl_s: take(ht, &mut q, 4 * (WL_CAP + WL_RUN + 64)) as *mut u32,
                wl_r: take(ht, &mut q, 4 * (WL_CAP + WL_RUN + 64)) as *mut u32,
                alist: take(ht, &mut q, 4 * (AL_CAP + 64)) as *mut u32,
                row_cat: take(cd, &mut c, n),
                run_s0: take(cd, &mut c, 4 * n) as *mut u32,
                run_row0: take(cd, &mut c, 4 * n) as *mut u32,
                run_pidx: take(cd, &mut c, 4 * n) as *mut u32,
                plist: take(cd, &mut c, 2 * (BSIZE + 64)) as *mut u16,
                p_cell: take(cd, &mut c, 4 * BSIZE) as *mut u32,
                p_sid: take(cd, &mut c, 4 * BSIZE) as *mut u32,
                p_row: take(cd, &mut c, 4 * BSIZE) as *mut u32,
                p_w: take(cd, &mut c, 4 * BSIZE) as *mut u32,
                nm_v: take(cd, &mut c, 8 * (8 * n + 16)) as *mut u64,
                nm_k: take(cd, &mut c, 8 * (8 * n + 16)) as *mut u64,
                nm_r: take(cd, &mut c, 4 * (8 * n + 16)) as *mut u32,
                nm_best: take(cd, &mut c, 8 * n) as *mut u64,
                edge: std::ptr::null_mut(),
            };
            if h > dense || q > hot || c > cold {
                fallback("scratch layout");
            }
            {
                let (bq, bb, bp) = (fx.q as *const u8, fx.blkk as *const u8, fx.plist as *const u8);
                let ok = fx.sidx == at(bq, OQ_SIDX) && fx.sts == at(bq, OQ_STS) && fx.sday == at(bq, OQ_SDAY) && fx.scamp == at(bq, OQ_SCAMP) && fx.slots as *const u8 == bq.wrapping_sub(OQ_SLOTS)
                    && fx.keepm == at(bb, OB_KEEPM) && fx.pkm == at(bb, OB_PKM) && fx.blk_ns == at(bb, OB_BLKNS) && fx.ssb == at(bb, OB_SSB) && fx.blk_nrow == at(bb, OB_BLKNROW) && fx.rwb == at(bb, OB_RWB)
                    && fx.row_rev == at(bq, OQ_ROWREV) && fx.row_day == at(bq, OQ_ROWDAY) && fx.row_s0 == at(bq, OQ_ROWS0) && fx.row_r0 == at(bq, OQ_ROWR0)
                    && fx.run_s0 as *const u8 == bp.wrapping_sub(OP_RUNS0) && fx.run_row0 as *const u8 == bp.wrapping_sub(OP_RUNROW0) && fx.run_pidx as *const u8 == bp.wrapping_sub(OP_RUNPIDX)
                    && fx.nm_v == at(bp, OP_NMV) && fx.nm_k == at(bp, OP_NMK) && fx.nm_r == at(bp, OP_NMR) && fx.nm_best == at(bp, OP_NMBEST)
                    && fx.wl_s == at(fx.rc as *const u8, OR_WLS) && fx.wl_r == at(fx.rc as *const u8, OR_WLR) && fx.row_cat as *const u8 == bp.wrapping_sub(OP_ROWCAT) && fx.p_cell == at(bp, OP_CELL) && fx.p_sid == at(bp, OP_SID) && fx.p_row == at(bp, OP_ROW) && fx.p_w == at(bp, OP_W);
                if !ok {
                    fallback("scratch layout");
                }
            }
            if !sh.hot.is_null() {
                std::ptr::write_bytes(fx.last as *mut u8, 0, r64(8 * (CBOUND + 2)));
                std::ptr::write_bytes(fx.asst as *mut u8, 0, r64(64 * (nc + 2)));
                std::ptr::write_bytes(fx.cred as *mut u8, 0, r64(4 * (nc + 2) * nd));
            }
            fx
        }
    }
}

#[inline(never)]
unsafe fn flush_names(fx: &Fast, core: &mut crate::core::Core, ukey: u64, row_cap: usize, np: &mut usize, na: &mut usize) {
    let (pv, pk, pr, best) = (fx.nm_v, fx.nm_k, fx.nm_r, fx.nm_best);
    let mut p = 0usize;
    while p < *np {
        _mm256_storeu_si256(pk.add(p) as *mut __m256i, name_key4v(ukey, _mm256_loadu_si256(pv.add(p) as *const __m256i)));
        p += 4;
    }
    for p in 0..*np {
        let b = best.add(*pr.add(p) as usize);
        *b = (*b).min(*pk.add(p));
    }
    let mut a = 0usize;
    while a < *na {
        _mm256_storeu_si256(pk.add(a) as *mut __m256i, key_chars4(_mm256_loadu_si256(best.add(a) as *const __m256i)));
        a += 4;
    }
    let mut nl = core.names.len();
    core.names.reserve_total(nl + 9 * *na + 32);
    core.names_off.reserve_total(core.names_off.len() + *na + 1);
    let ncap = core.names.cap().min(row_cap * 10).min(u32::MAX as usize);
    let names = core.names.as_mut_ptr();
    let no0 = core.names_off.len();
    let offs = core.names_off.as_mut_ptr().add(no0);
    for a in 0..*na {
        if nl + 16 > ncap {
            fallback("avec full");
        }
        let b = *best.add(a);
        let (ch, len) = if b == u64::MAX { (u64::MAX, 0) } else { (*pk.add(a), (b & 15) as usize) };
        let q = names.add(nl);
        *q = b'u';
        (q.add(1) as *mut u64).write_unaligned(ch);
        nl += 1 + len;
        *offs.add(a) = nl as u32;
    }
    core.names_off.truncate(no0 + *na);
    core.names.truncate(nl);
    *np = 0;
    *na = 0;
}

#[inline(always)]
unsafe fn bcast_hi(p: *const u64) -> __m512i {
    let v: __m512i;
    std::arch::asm!("vpbroadcastd {v}, dword ptr [{p} + 4]", v = lateout(zmm_reg) v, p = in(reg) p, options(pure, readonly, nostack, preserves_flags));
    v
}

#[inline(always)]
unsafe fn cmp_mask<const P: i32>(a: __m512i, b: __m512i) -> u32 {
    let r: u32;
    std::arch::asm!("vpcmpud {k}, {a}, {b}, {p}", "kmovw {r:e}, {k}", k = out(kreg) _, a = in(zmm_reg) a, b = in(zmm_reg) b, p = const P, r = lateout(reg) r, options(pure, nomem, nostack, preserves_flags));
    r
}

#[inline(never)]
unsafe fn run_clicks(fx: &mut Fast, cks: &[[u64; 2]], class: u32, s0: usize, nsr: usize, org: u32) {
    let n = cks.len();
    let p = cks.as_ptr() as *const u64;
    let classv = _mm512_setr_epi64(class as i64, -1, class as i64, -1, class as i64, -1, class as i64, -1);
    let am = _mm512_setr_epi64(15, 0, 15, 0, 15, 0, 15, 0);
    let one = _mm512_set1_epi64(1);
    let mut out = _mm512_setzero_si512();
    let mut i = 0usize;
    while i + 4 <= n {
        let a = _mm512_and_si512(_mm512_loadu_si512(p.add(2 * i) as *const _), am);
        out = _mm512_ternarylogic_epi64(out, _mm512_sllv_epi64(one, a), classv, 0xF4);
        i += 4;
    }
    if i < n {
        let lm = ((1u32 << (2 * (n - i))) - 1) as u8;
        let a = _mm512_and_si512(_mm512_maskz_loadu_epi64(lm, p.add(2 * i) as *const _), am);
        out = _mm512_ternarylogic_epi64(out, _mm512_maskz_mov_epi64(lm, _mm512_sllv_epi64(one, a)), classv, 0xF4);
    }
    if _mm512_test_epi64_mask(out, out) == 0 && !RC_NOALL {
        run_clicks_k::<true>(fx, cks, class, s0, nsr, org)
    } else {
        run_clicks_k::<false>(fx, cks, class, s0, nsr, org)
    }
}

#[inline(always)]
unsafe fn run_clicks_k<const ALL: bool>(fx: &mut Fast, cks: &[[u64; 2]], class: u32, s0: usize, nsr: usize, org: u32) {
    let last = fx.last as *mut u32;
    if fx.gbase as usize + nsr + 1 >= GB_MAX {
        std::ptr::write_bytes(last, 0, CBOUND + 2);
        fx.gbase = 0;
    }
    let g0 = fx.gbase as usize;
    fx.gbase = (g0 + nsr + 1) as u32;
    let sts0 = fx.sts.add(s0);
    let sday = fx.sday.add(s0);
    if fx.f.len() < nsr + 1 {
        fx.f.resize(nsr + 1, V16([0; 16]));
    }
    let f0 = fx.f.as_mut_ptr();
    let mut acc = _mm512_setzero_si512();
    _mm512_store_si512(f0 as *mut _, acc);
    for x in 0..nsr {
        acc = _mm512_add_epi32(acc, _mm512_load_si512(ONEHOT.as_ptr().add(*sday.add(x) as usize) as *const _));
        _mm512_store_si512(f0.add(x + 1) as *mut _, acc);
    }
    let sts = sts0.wrapping_sub(g0);
    let sc = fx.scamp.add(s0).wrapping_sub(g0);
    let f = f0.wrapping_sub(g0);
    let asst = fx.asst;
    let saved = _mm512_loadu_si512(sts0.add(nsr) as *const _);
    _mm512_storeu_si512(sts0.add(nsr) as *mut _, _mm512_set1_epi32(-1));
    let orgv = _mm512_set1_epi32(org as i32);
    let weekv = _mm512_set1_epi32(WEEK_MS as i32);
    let lo_load = |v: __m512i| _mm512_max_epu32(v, _mm512_add_epi32(v, weekv));
    let (mut lo_p, mut hi_p) = (g0, g0);
    let mut cpv = orgv;
    let (mut lob, mut hib) = (g0, g0);
    let mut hiv = _mm512_loadu_si512(sts0 as *const _);
    let mut lov = lo_load(hiv);
    let class2 = class | class << 16;
    let mut kp = cks.as_ptr();
    let kend = kp.add(cks.len());
    while kp != kend {
        let k0 = (*kp)[0];
        let tp = kp as *const u64;
        kp = kp.add(1);
        if !ALL && (class2 >> (k0 as u32 & 31)) & 1 == 0 {
            continue;
        }
        let c = ((k0 >> 4) & (CBOUND as u64 - 1)) as usize;
        if XRC != 0 {
            let cn = (((*kp.add(XRC - 1))[0] >> 4) & (CBOUND as u64 - 1)) as usize;
            _mm_prefetch::<_MM_HINT_T0>(asst.add(cn) as *const i8);
        }
        let tv = bcast_hi(tp);
        let mut m = cmp_mask::<2>(hiv, tv);
        while m == 0xFFFF {
            std::hint::cold_path();
            hib += 16;
            hiv = _mm512_loadu_si512(sts.wrapping_add(hib) as *const _);
            m = cmp_mask::<2>(hiv, tv);
        }
        let hi = hib + m.count_ones() as usize;
        let mut m = cmp_mask::<1>(lov, tv);
        while m == 0xFFFF {
            std::hint::cold_path();
            lob += 16;
            lov = lo_load(_mm512_loadu_si512(sts.wrapping_add(lob) as *const _));
            m = cmp_mask::<1>(lov, tv);
        }
        let lo = lob + m.count_ones() as usize;
        let to = lo.min(hi_p);
        _mm512_storeu_si512(sc.wrapping_add(lo_p) as *mut _, cpv);
        if to > lo_p + 16 {
            let mut x = lo_p + 16;
            loop {
                _mm512_storeu_si512(sc.wrapping_add(x) as *mut _, cpv);
                x += 16;
                if x >= to {
                    break;
                }
            }
        }
        _mm512_storeu_si512(sc.wrapping_add(to) as *mut _, orgv);
        lo_p = lo;
        hi_p = hi;
        cpv = _mm512_set1_epi32(c as i32);
        let from = lo.max(*last.add(c) as usize);
        *last.add(c) = hi as u32;
        let row = asst.add(c) as *mut __m512i;
        let d = _mm512_sub_epi32(_mm512_load_si512(f.wrapping_add(hi) as *const _), _mm512_load_si512(f.wrapping_add(from) as *const _));
        _mm512_store_si512(row, _mm512_add_epi32(_mm512_load_si512(row), d));
    }
    let to = (g0 + nsr).min(hi_p);
    let mut x = lo_p;
    loop {
        _mm512_storeu_si512(sc.wrapping_add(x) as *mut _, cpv);
        x += 16;
        if x >= to {
            break;
        }
    }
    _mm512_storeu_si512(sc.wrapping_add(to) as *mut _, orgv);
    _mm512_storeu_si512(sts0.add(nsr) as *mut _, saved);
}

#[inline(always)]
unsafe fn mul32_256(a: __m256i, b: __m256i) -> __m256i {
    let o: __m256i;
    std::arch::asm!("vpmuludq {o}, {a}, {b}", o = lateout(ymm_reg) o, a = in(ymm_reg) a, b = in(ymm_reg) b, options(pure, nomem, nostack, preserves_flags));
    o
}

#[inline(never)]
unsafe fn pass_b(q: *const u64, k: usize, utab: *const u16, blkk: *const u16, mix: &mut Vec<u32>) -> usize {
    let mut st = PassB { tlast: _mm512_setzero_si512(), ulast: _mm512_set1_epi32(0x4000_0000), carry: 0, cur: 0, ns: 0, nmix: 0 };
    mix.clear();
    mix.reserve(k + 1);
    let mp = mix.as_mut_ptr();
    let mut i = 0usize;
    while i + 16 <= k {
        pass_b_step(&mut st, q, i, 0xFFFF, utab, blkk as *const u8, mp);
        i += 16;
    }
    if i < k {
        pass_b_step(&mut st, q, i, (1u32 << (k - i)) - 1, utab, blkk as *const u8, mp);
    }
    *mp.add(st.nmix) = u32::MAX;
    mix.set_len(st.nmix + 1);
    st.ns
}

struct PassB {
    tlast: __m512i,
    ulast: __m512i,
    carry: u32,
    cur: u32,
    ns: usize,
    nmix: usize,
}

#[inline(always)]
unsafe fn pass_b_step(st: &mut PassB, q: *const u64, i: usize, tail: u32, utab: *const u16, bb: *const u8, mix: *mut u32) {
    let iota16 = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let even32 = _mm512_setr_epi32(0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30);
    let odd32 = _mm512_setr_epi32(1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31);
    let gapv = _mm512_set1_epi32(GAP_MS as i32);
    let c15 = _mm512_set1_epi32(15);
    let userm = _mm512_set1_epi32(0x7FFF_FFF0);
    let one = _mm512_set1_epi32(1);
    let maskc = _mm512_set1_epi32(MASK as i32);
    let flagv = _mm512_set1_epi32(FLAG as i32);
    let qb = q as *const u8;
    let sidx: *mut u32 = at(qb, OQ_SIDX);
    let sts: *mut u32 = at(qb, OQ_STS);
    let blk_ns: *mut u32 = at(bb, OB_BLKNS);
    let ssb: *mut u16 = at(bb, OB_SSB);
    let ns = st.ns;
    let q0 = _mm512_loadu_si512(q.add(i) as *const _);
    let q1 = _mm512_loadu_si512(q.add(i + 8) as *const _);
    let tv = _mm512_permutex2var_epi32(q0, even32, q1);
    let uv = _mm512_permutex2var_epi32(q0, odd32, q1);
    let tpv = _mm512_alignr_epi32::<15>(tv, st.tlast);
    let upv = _mm512_alignr_epi32::<15>(uv, st.ulast);
    st.tlast = tv;
    st.ulast = uv;
    let x = _mm512_xor_si512(uv, upv);
    let rs = _mm512_test_epi32_mask(x, userm) as u32 & tail;
    let acm = _mm512_test_epi32_mask(x, c15) as u32;
    let gap = _mm512_cmpgt_epu32_mask(_mm512_sub_epi32(tv, tpv), gapv) as u32;
    let ss = (rs | gap) & tail;
    let un = _mm512_cmplt_epu32_mask(tv, tpv) as u32 & !rs;
    let mut ut = _mm512_set1_epi32(st.cur as i32);
    let mut m = rs;
    while m != 0 {
        let p = m.trailing_zeros();
        m &= m - 1;
        st.cur = *utab.add((*q.add(i + p as usize) >> 36) as usize) as u32;
        ut = _mm512_mask_mov_epi32(ut, (0xFFFFu32 << p) as u16, _mm512_set1_epi32(st.cur as i32));
    }
    let abit = _mm512_sllv_epi32(one, _mm512_and_si512(uv, c15));
    let bad = _mm512_test_epi32_mask(abit, _mm512_andnot_si512(ut, maskc)) as u32;
    let ac = acm & !rs;
    let badp = ((bad << 1) | st.carry) & 0xFFFF;
    st.carry = (bad >> 15) & 1;
    let special = (un | (ac & (bad | badp))) & tail;
    if special != 0 {
        let mut m = special;
        while m != 0 {
            *mix.add(st.nmix) = (i + m.trailing_zeros() as usize) as u32;
            st.nmix += 1;
            m &= m - 1;
        }
    }
    let base = _mm512_add_epi32(_mm512_set1_epi32(i as i32), iota16);
    let idxf = _mm512_mask_or_epi32(base, rs as u16, base, flagv);
    _mm512_storeu_si512(sidx.add(ns) as *mut _, _mm512_maskz_compress_epi32(ss as u16, idxf));
    _mm512_storeu_si512(sts.add(ns) as *mut _, _mm512_maskz_compress_epi32(ss as u16, tv));
    *blk_ns.add(i >> 4) = ns as u32;
    *ssb.add(i >> 4) = ss as u16;
    st.ns = ns + ss.count_ones() as usize;
}

#[inline(never)]
unsafe fn purch_scan(q: *const u64, blkk: *const u16, pl: *const u16, aggp: *const [u64; 2], nd: usize, np: usize) {
    let (qb, bb, pb) = (q as *const u8, blkk as *const u8, pl as *const u8);
    let slots = qb.wrapping_sub(OQ_SLOTS) as *const Rec;
    let (scamp, sday): (*const u32, *const u32) = (at(qb, OQ_SCAMP), at(qb, OQ_SDAY));
    let keepm: *const u8 = at(bb, OB_KEEPM);
    let (blk_ns, ssb, blk_nrow, rwb): (*const u32, *const u16, *const u32, *const u16) = (at(bb, OB_BLKNS), at(bb, OB_SSB), at(bb, OB_BLKNROW), at(bb, OB_RWB));
    let (pc, ps, pr, pw): (*mut u32, *mut u32, *mut u32, *mut u32) = (at(pb, OP_CELL), at(pb, OP_SID), at(pb, OP_ROW), at(pb, OP_W));
    for j in 0..np {
        let sl = *pl.add(j) as usize;
        let wv = (*slots.add(sl)).w0;
        let i = *blkk.add(sl >> 3) as usize + _bzhi_u32(*keepm.add(sl >> 3) as u32, sl as u32 & 7).count_ones() as usize;
        let sid = (*blk_ns.add(i >> 4) + _bzhi_u32(*ssb.add(i >> 4) as u32, (i as u32 & 15) + 1).count_ones() - 1) as usize;
        let cell = *scamp.add(sid) as usize * nd + *sday.add(sid) as usize;
        let row = *blk_nrow.add(sid >> 4) + _bzhi_u32(*rwb.add(sid >> 4) as u32, (sid as u32 & 15) + 1).count_ones() - 1;
        _mm_prefetch::<_MM_HINT_T0>(aggp.add(cell) as *const i8);
        *pc.add(j) = cell as u32;
        *ps.add(j) = sid as u32;
        *pr.add(j) = row;
        *pw.add(j) = ((wv >> 4) & 0x1FFF_FFFF) as u32;
    }
}

#[inline(never)]
unsafe fn purch_add(q: *const u64, pl: *const u16, aggp: *mut [u64; 2], np: usize) {
    let (qb, pb) = (q as *const u8, pl as *const u8);
    let row_rev: *mut u32 = at(qb, OQ_ROWREV);
    let row_cat = pb.wrapping_sub(OP_ROWCAT) as *mut u8;
    let (pc, ps, pr, pw): (*const u32, *const u32, *const u32, *const u32) = (at(pb, OP_CELL), at(pb, OP_SID), at(pb, OP_ROW), at(pb, OP_W));
    let mut csum = [0u64; 32];
    let mut crow = [u32::MAX; 32];
    let (mut prev_sid, mut cur_row, mut best) = (u32::MAX, u32::MAX, 0u64);
    for j in 0..np {
        let w = *pw.add(j);
        let price = (w >> 5) as u64;
        let cat = w & 31;
        let sid = *ps.add(j);
        let cell = *pc.add(j) as usize;
        let row = *pr.add(j);
        (*aggp.add(cell))[0] += (sid != prev_sid) as u64;
        (*aggp.add(cell))[1] += price;
        prev_sid = sid;
        let v = std::hint::select_unpredictable(crow[cat as usize] == row, csum[cat as usize], 0) + price;
        csum[cat as usize] = v;
        crow[cat as usize] = row;
        best = std::hint::select_unpredictable(row != cur_row, 0, best);
        cur_row = row;
        best = best.max(v << 5 | (31 - cat) as u64);
        let rv = *row_rev.add(row as usize) as u64 + price;
        *row_rev.add(row as usize) = rv.min(0x8000_0000) as u32;
        *row_cat.add(row as usize) = (31 - (best & 31)) as u8;
    }
}

struct Walk {
    cw: usize,
    cb: u64,
    nw: usize,
    nrc: usize,
    nm_np: usize,
    nm_na: usize,
    mixp: usize,
    c_runs: u64,
    c_simple: u64,
    c_sess: u64,
}

#[inline(never)]
unsafe fn walk_runs(fx: &mut Fast, g: &G, wk: &mut Walk, xp: &mut Xp, r0: usize, nrun: usize, pidx0: u32, org: u32) -> usize {
    let (qb, pb, rcb) = (fx.q as *const u8, fx.plist as *const u8, fx.rc as *const u8);
    let q = fx.q as *const u64;
    let (sidx, scamp): (*const u32, *mut u32) = (at(qb, OQ_SIDX), at(qb, OQ_SCAMP));
    let (run_s0, run_pidx) = (pb.wrapping_sub(OP_RUNS0) as *const u32, pb.wrapping_sub(OP_RUNPIDX) as *mut u32);
    let (nm_v, nm_r, nm_best): (*mut u64, *mut u32, *mut u64) = (at(pb, OP_NMV), at(pb, OP_NMR), at(pb, OP_NMBEST));
    let (wl_s0, wl_r0): (*mut u32, *mut u32) = (at(rcb, OR_WLS), at(rcb, OR_WLR));
    let mixv = fx.mix.as_ptr();
    let ev_idx = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
    let iota16 = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let orgv = _mm512_set1_epi32(org as i32);
    let (mut cw, mut cb) = (wk.cw, wk.cb);
    let (mut nw, mut nrc) = (wk.nw, wk.nrc);
    let (mut nm_np, mut nm_na) = (wk.nm_np, wk.nm_na);
    let mut mixp = wk.mixp;
    let (mut c_runs, mut c_simple, mut c_sess) = (0u64, 0u64, 0u64);
    let mut r = r0;
    while r < nrun {
        let s0 = *run_s0.add(r) as usize;
        let s1 = *run_s0.add(r + 1) as usize;
        let a = (*sidx.add(s0) & M31) as usize;
        let e = (*sidx.add(s1) & M31) as usize;
        let qa = *q.add(a);
        let u = (qa >> 36) as u32;
        let tv = *g.utab.add(u as usize);
        while (*mixv.add(mixp) as usize) < a {
            mixp += 1;
        }
        let special = (*mixv.add(mixp) as usize) < e;
        let edge = r == 0 || r + 1 == nrun;
        if edge || special || tv & (COMPLEX | EXTRA) != 0 {
            break;
        }
        if XD { xp.ph(4); xp.ph(14); }
        c_runs += 1;
        c_simple += (e - a) as u64;
        if u as usize >> 6 != cw {
            claim_word(g, cw, cb);
            cw = u as usize >> 6;
            cb = 0;
        }
        if cb >> (u & 63) & 1 != 0 {
            fallback("a user has two runs");
        }
        cb |= 1u64 << (u & 63);
        let mask = (tv & MASK) as u32;
        let bit = 1u32 << ((qa >> 32) & 15);
        let class = if bit & mask != 0 { mask } else { bit };
        if XD { xp.ph(10); }
        let ncl = class.count_ones() as usize;
        *nm_best.add(nm_na) = u64::MAX;
        *run_pidx.add(r) = pidx0 + nm_na as u32;
        if ncl.wrapping_sub(1) >= 8 {
            *nm_best.add(nm_na) = name_key_scalar(g.ukey, u, class);
        } else {
            let al = _mm512_cvtepu32_epi64(_mm512_castsi512_si256(_mm512_maskz_compress_epi32(class as u16, iota16)));
            _mm512_storeu_si512(nm_v.add(nm_np) as *mut _, _mm512_or_si512(_mm512_set1_epi64(((u as u64) << 9) as i64), al));
            _mm256_storeu_si256(nm_r.add(nm_np) as *mut _, _mm256_set1_epi32(nm_na as i32));
            nm_np += ncl;
        }
        nm_na += 1;
        c_sess += (s1 - s0) as u64;
        if XD { xp.ph(11); }
        let mut x = s0;
        while x < s1 {
            _mm512_storeu_si512(scamp.add(x) as *mut _, orgv);
            x += 16;
        }
        let cks = g.csr_range(u);
        let n = cks.len();
        let nsr = s1 - s0;
        let mut light = n == 0;
        if n != 0 && n <= 16 && nsr <= WL_RUN {
            if nrc == RC_N || nw + nsr > WL_CAP {
                xp.ph(4);
                light_flush(fx, nw, org);
                xp.ph(5);
                nw = 0;
                nrc = 0;
            }
            let p = cks.as_ptr() as *const __m512i;
            let ka = _mm512_permutex2var_epi64(_mm512_loadu_si512(p), ev_idx, _mm512_loadu_si512(p.add(1)));
            let kb = _mm512_permutex2var_epi64(_mm512_loadu_si512(p.add(2)), ev_idx, _mm512_loadu_si512(p.add(3)));
            let lo32 = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(ka)), _mm512_cvtepi64_epi32(kb));
            let ts = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(_mm512_srli_epi64(ka, 32))), _mm512_cvtepi64_epi32(_mm512_srli_epi64(kb, 32)));
            let vm = (1u32 << n) - 1;
            let inc = _mm512_test_epi32_mask(_mm512_sllv_epi32(_mm512_set1_epi32(1), _mm512_and_si512(lo32, _mm512_set1_epi32(15))), _mm512_set1_epi32(class as i32)) as u32;
            if inc & vm == vm {
                light = true;
                let camp = _mm512_and_si512(lo32, _mm512_set1_epi32((CBOUND as i32 - 1) << 4));
                let ri = nrc;
                nrc += 1;
                let rcp = (rcb as *mut Rc).add(ri);
                _mm512_store_si512((*rcp).ts.as_mut_ptr() as *mut _, ts);
                _mm512_store_si512((*rcp).camp.as_mut_ptr() as *mut _, camp);
                _mm512_store_si512((*rcp).conf.as_mut_ptr() as *mut _, _mm512_conflict_epi32(camp));
                (*rcp).vm = vm;
                let wl_s = wl_s0.add(nw);
                let wl_r = wl_r0.add(nw);
                let rv = _mm512_set1_epi32((ri * std::mem::size_of::<Rc>()) as i32);
                let mut x = 0usize;
                loop {
                    _mm512_storeu_si512(wl_s.add(x) as *mut _, _mm512_add_epi32(_mm512_set1_epi32((s0 + x) as i32), iota16));
                    _mm512_storeu_si512(wl_r.add(x) as *mut _, rv);
                    x += 16;
                    if x >= nsr {
                        break;
                    }
                }
                nw += nsr;
            }
        }
        if XD { xp.ph(12); }
        xc(if light { if n == 0 { 0 } else { 1 } } else { 2 }, 1);
        xc(if light { 3 } else { 4 }, n as u64);
        xc(if light { if n == 0 { 5 } else { 6 } } else { 7 }, nsr as u64);
        if XD && n <= 16 { xc(8 + n, 1); }
        if XD && !light { let bin = if n <= 16 { 0 } else if n <= 32 { 1 } else if n <= 64 { 2 } else if n <= 128 { 3 } else if n <= 512 { 4 } else { 5 }; XH[bin].fetch_add(1, Ordering::Relaxed); XH[6 + bin].fetch_add(n as u64, Ordering::Relaxed); XH[12 + bin].fetch_add(nsr as u64, Ordering::Relaxed); XH[18 + bin].fetch_add((n * nsr) as u64, Ordering::Relaxed); }
        if !light {
            run_clicks(fx, cks, class, s0, nsr, org);
            if XD { xp.ph(13); }
        }
        r += 1;
    }
    wk.cw = cw;
    wk.cb = cb;
    wk.nw = nw;
    wk.nrc = nrc;
    wk.nm_np = nm_np;
    wk.nm_na = nm_na;
    wk.mixp = mixp;
    wk.c_runs += c_runs;
    wk.c_simple += c_simple;
    wk.c_sess += c_sess;
    r
}

fn stage3a(g: &G, tid: usize, pool: &Pool, sh: &Share) {
    let t = g.t(tid);
    let mut fx = Fast::new(g.nc, g.nd, sh);
    unsafe {
        fx.edge = Edge::start(tid, pool);
    }
    unsafe {
        let cells = t.core.agg.len();
        if t.n_runs == 0 && t.n_complex == 0 && t.core.n_sess == 0 && !sh.agg.is_null() && !sh.aggp.is_null() {
            std::ptr::write_bytes(sh.agg, 0, cells * 8);
            std::ptr::write_bytes(sh.aggp, 0, cells * 16);
            t.core.agg.set_ptr(sh.agg as *mut [u32; 2]);
            t.core.aggp.set_ptr(sh.aggp as *mut [u64; 2]);
        }
        t.cx_raw.reserve(g.ev_bytes / 64 / g.nt / 16);
        slab_cols(&mut t.core, sh.slab, sh.slab_len);
    }
    let mut xp = Xp::new();
    let nd = g.nd;
    let org = g.nc as u32;
    let trash = g.nc as u32 + 1;
    let row_cap = g.row_cap;
    unsafe {
        let (q, blkk, pl) = (fx.q, fx.blkk, fx.plist);
        let (qb, bb, pb) = (q as *const u8, blkk as *const u8, pl as *const u8);
        let slots = qb.wrapping_sub(OQ_SLOTS) as *mut Rec;
        let (keepm, pkm): (*mut u8, *mut u8) = (at(bb, OB_KEEPM), at(bb, OB_PKM));
        let (sidx, sts, sday, scamp): (*mut u32, *mut u32, *mut u32, *mut u32) = (at(qb, OQ_SIDX), at(qb, OQ_STS), at(qb, OQ_SDAY), at(qb, OQ_SCAMP));
        let (row_day, row_s0, row_r0, row_rev): (*mut u32, *mut u32, *mut u32, *mut u32) = (at(qb, OQ_ROWDAY), at(qb, OQ_ROWS0), at(qb, OQ_ROWR0), at(qb, OQ_ROWREV));
        let row_cat = pb.wrapping_sub(OP_ROWCAT) as *mut u8;
        let (blk_nrow, rwb): (*mut u32, *mut u16) = (at(bb, OB_BLKNROW), at(bb, OB_RWB));
        let (run_s0, run_row0, run_pidx) = (pb.wrapping_sub(OP_RUNS0) as *mut u32, pb.wrapping_sub(OP_RUNROW0) as *mut u32, pb.wrapping_sub(OP_RUNPIDX) as *mut u32);
        let mut gen = 0u64;
        let dig_min = DIG_MIN.load(Ordering::Relaxed);
        let ev_idx = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
        let od_idx = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
        let iota8q = _mm512_setr_epi64(0, 1, 2, 3, 4, 5, 6, 7);
        let iota16 = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        loop {
            let b = g.next.fetch_add(1, Ordering::Relaxed);
            if b >= g.nb {
                break;
            }
            let ta = std::time::Instant::now();
            xp.ph(9);
            gen += 1;
            if gen > GEN_MAX {
                std::ptr::write_bytes(slots as *mut u8, 0, BSIZE * 16);
                gen = 1;
            }
            let mut any = false;
            for th in 0..g.nt {
                g.t(th).ev.for_each(b, |recs| {
                    any = true;
                    let n = recs.len();
                    let p = recs.as_ptr();
                    let nextc = if PF_CHUNK && n == crate::parse::LINK_OFF / 16 { *((p as usize + crate::parse::LINK_OFF) as *const usize) as *const i8 } else { std::ptr::null() };
                    scatter_chunk(p, n, slots, gen, nextc);
                });
            }
            let info = &mut *g.edges.add(b);
            info.n = 0;
            if !any {
                continue;
            }
            let tb = std::time::Instant::now();
            xp.ph(0);
            let mut k = 0usize;
            {
                let inv = _mm512_set1_epi64(INVALID_TS as i64);
                let m16 = _mm512_set1_epi64(0xFFFF);
                let m15 = _mm512_set1_epi64(15);
                let pmask = _mm512_set1_epi64((0xFF_FFFFu64 << 9) as i64);
                let eight = _mm512_set1_epi64(8);
                let mut tagv = _mm512_add_epi64(iota8q, _mm512_set1_epi64(gen as i64));
                let mut step = |blk: usize, k: &mut usize, tagv: &mut __m512i| {
                    let r0 = _mm512_loadu_si512(slots.add(8 * blk) as *const _);
                    let r1 = _mm512_loadu_si512(slots.add(8 * blk + 4) as *const _);
                    let w0 = _mm512_permutex2var_epi64(r0, ev_idx, r1);
                    let w1 = _mm512_permutex2var_epi64(r0, od_idx, r1);
                    let occ = _mm512_testn_epi64_mask(_mm512_sub_epi64(w1, *tagv), m16);
                    *tagv = _mm512_add_epi64(*tagv, eight);
                    let tsq = _mm512_srli_epi64(w0, 33);
                    let valid = _mm512_mask_cmpneq_epu64_mask(occ, tsq, inv);
                    let odd = valid & _mm512_movepi64_mask(w1);
                    if odd != 0 {
                        odd_records(t, slots, blk, odd);
                    }
                    let keep = valid & !odd;
                    let ua = _mm512_ternarylogic_epi64(_mm512_srli_epi64(w1, 33), w0, m15, 0xD8);
                    let qv = _mm512_or_si512(_mm512_slli_epi64(ua, 32), tsq);
                    _mm512_storeu_si512(q.add(*k) as *mut _, _mm512_maskz_compress_epi64(keep, qv));
                    *blkk.add(blk) = *k as u16;
                    *keepm.add(blk) = keep;
                    *pkm.add(blk) = keep & _mm512_test_epi64_mask(w0, pmask);
                    *k += keep.count_ones() as usize;
                };
                const _: () = assert!((BSIZE / 8) % 2 == 0);
                let mut blk = 0usize;
                while blk < BSIZE / 8 {
                    step(blk, &mut k, &mut tagv);
                    step(blk + 1, &mut k, &mut tagv);
                    blk += 2;
                }
                *blkk.add(BSIZE / 8) = k as u16;
            }
            let tc = std::time::Instant::now();
            xp.ph(1);
            if k == 0 {
                t.prof[0] += (tb - ta).as_nanos() as u64;
                t.prof[1] += (tc - tb).as_nanos() as u64;
                continue;
            }
            let ns = pass_b(q, k, g.utab, blkk, &mut fx.mix);
            *sidx.add(ns) = k as u32 | FLAG;
            xp.ph(2);
            let mut nrow = 0usize;
            {
                let wsm = _mm512_set1_epi32(g.ws_mod as i32);
                let mg = _mm256_set1_epi64x(DIV_M as i64);
                let flagv = _mm512_set1_epi32(FLAG as i32);
                let m31v = _mm512_set1_epi32(M31 as i32);
                *sday.sub(1) = u32::MAX;
                let mut s = 0usize;
                while s < ns {
                    let rem = ns - s;
                    let tail: u32 = if rem >= 16 { 0xFFFF } else { (1u32 << rem) - 1 };
                    let x = _mm512_srli_epi32(_mm512_add_epi32(_mm512_loadu_si512(sts.add(s) as *const _), wsm), 10);
                    let div = |v: __m256i| -> __m256i {
                        let pe = _mm256_srli_epi64(mul32_256(v, mg), 39);
                        let po = _mm256_srli_epi64(mul32_256(_mm256_srli_epi64(v, 32), mg), 39);
                        _mm256_or_si256(pe, _mm256_slli_epi64(po, 32))
                    };
                    let day = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(div(_mm512_castsi512_si256(x))), div(_mm512_extracti64x4_epi64::<1>(x)));
                    _mm512_storeu_si512(sday.add(s) as *mut _, day);
                    let dprev = _mm512_loadu_si512(sday.add(s).sub(1) as *const _);
                    let iv = _mm512_loadu_si512(sidx.add(s) as *const _);
                    let rw = ((_mm512_movepi32_mask(iv) | _mm512_cmpneq_epi32_mask(day, dprev)) as u32 & tail) as u16;
                    *blk_nrow.add(s >> 4) = nrow as u32;
                    *rwb.add(s >> 4) = rw;
                    let ord = _mm512_or_si512(_mm512_add_epi32(_mm512_set1_epi32(s as i32), iota16), _mm512_and_si512(iv, flagv));
                    _mm512_storeu_si512(row_day.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, day));
                    _mm512_storeu_si512(row_s0.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, ord));
                    _mm512_storeu_si512(row_r0.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, _mm512_and_si512(iv, m31v)));
                    nrow += rw.count_ones() as usize;
                    s += 16;
                }
                *row_s0.add(nrow) = ns as u32 | FLAG;
                *row_r0.add(nrow) = k as u32;
            }
            let mut nrun = 0usize;
            {
                let m31v = _mm512_set1_epi32(M31 as i32);
                let mut r = 0usize;
                while r < nrow {
                    let rem = nrow - r;
                    let tail: u32 = if rem >= 16 { 0xFFFF } else { (1u32 << rem) - 1 };
                    let v = _mm512_loadu_si512(row_s0.add(r) as *const _);
                    let m = (_mm512_movepi32_mask(v) as u32 & tail) as u16;
                    _mm512_storeu_si512(run_s0.add(nrun) as *mut _, _mm512_maskz_compress_epi32(m, _mm512_and_si512(v, m31v)));
                    _mm512_storeu_si512(run_row0.add(nrun) as *mut _, _mm512_maskz_compress_epi32(m, _mm512_add_epi32(_mm512_set1_epi32(r as i32), iota16)));
                    nrun += m.count_ones() as usize;
                    r += 16;
                }
                *run_s0.add(nrun) = ns as u32;
                *run_row0.add(nrun) = nrow as u32;
            }
            std::ptr::write_bytes(row_rev, 0, nrow);
            std::ptr::write_bytes(row_cat, 255, nrow);
            xp.ph(3);
            let mut wk = Walk { cw: 0, cb: 0, nw: 0, nrc: 0, nm_np: 0, nm_na: 0, mixp: 0, c_runs: 0, c_simple: 0, c_sess: 0 };
            let mut digs = [DigRun { first: false, user: 0, a: 0, e: 0, s0: 0, s1: 0 }; 2];
            let mut ndig = 0usize;
            let mixv = fx.mix.as_ptr();
            let trashv = _mm512_set1_epi32(trash as i32);
            let mut r = 0usize;
            loop {
                xp.ph(4);
                r = walk_runs(&mut fx, g, &mut wk, &mut xp, r, nrun, (t.core.names_off.len() - 1) as u32, org);
                if r >= nrun {
                    break;
                }
                let s0 = *run_s0.add(r) as usize;
                let s1 = *run_s0.add(r + 1) as usize;
                let a = (*sidx.add(s0) & M31) as usize;
                let e = (*sidx.add(s1) & M31) as usize;
                let u = (*q.add(a) >> 36) as u32;
                let tv = *g.utab.add(u as usize);
                let special = (*mixv.add(wk.mixp) as usize) < e;
                let edge = r == 0 || r + 1 == nrun;
                xc(25, 1);
                xc(26, (e - a) as u64);
                xp.ph(4);
                *run_pidx.add(r) = NONE;
                let mut x = s0;
                while x < s1 {
                    _mm512_storeu_si512(scamp.add(x) as *mut _, trashv);
                    x += 16;
                }
                let dig = edge && !special && tv & COMPLEX == 0 && e - a >= dig_min && 3 + 2 * (s1 - s0) + (e - a) <= EP_WORDS;
                if dig {
                    digs[ndig] = DigRun { first: r == 0, user: u, a, e, s0, s1 };
                    ndig += 1;
                    if r == 0 {
                        info.n = 1;
                    } else {
                        info.n = 2;
                    }
                } else if r == 0 {
                    info.first = edge_copy_k(tid, u, &mut fx, a, e - a);
                    info.n = 1;
                } else if r + 1 == nrun {
                    info.last = edge_copy_k(tid, u, &mut fx, a, e - a);
                    info.n = 2;
                } else {
                    flush_names(&fx, &mut t.core, g.ukey, row_cap, &mut wk.nm_np, &mut wk.nm_na);
                    let mut buf = std::mem::take(&mut t.runbuf);
                    buf.clear();
                    for_kept(slots, blkk, keepm, a, e - a, |w0| buf.push(Rec { w0, w1: (u as u64) << 37 }));
                    process_run(g, t, u, &mut buf);
                    t.runbuf = buf;
                }
                xp.ph(15);
                r += 1;
            }
            flush_names(&fx, &mut t.core, g.ukey, row_cap, &mut wk.nm_np, &mut wk.nm_na);
            t.n_runs += wk.c_runs;
            t.n_simple += wk.c_simple;
            t.core.n_sess += wk.c_sess;
            xp.ph(4);
            light_flush(&mut fx, wk.nw, org);
            claim_word(g, wk.cw, wk.cb);
            xp.ph(5);
            {
                let cred = fx.cred;
                for s in 0..ns {
                    let cell = *scamp.add(s) as usize * nd + *sday.add(s) as usize;
                    *cred.add(cell) += 1;
                }
            }
            xp.ph(6);
            {
                let aggp = t.core.aggp.as_mut_ptr();
                let mut np = 0usize;
                for wd in 0..BSIZE / 64 {
                    let mut pm = (pkm.add(8 * wd) as *const u64).read_unaligned();
                    let n = pm.count_ones() as usize;
                    let base = (64 * wd) as u32;
                    *pl.add(np) = (base + pm.trailing_zeros()) as u16;
                    pm &= pm.wrapping_sub(1);
                    *pl.add(np + 1) = (base + pm.trailing_zeros()) as u16;
                    pm &= pm.wrapping_sub(1);
                    *pl.add(np + 2) = (base + pm.trailing_zeros()) as u16;
                    pm &= pm.wrapping_sub(1);
                    *pl.add(np + 3) = (base + pm.trailing_zeros()) as u16;
                    pm &= pm.wrapping_sub(1);
                    if n > 4 {
                        let mut j = np + 4;
                        while pm != 0 {
                            *pl.add(j) = (base + pm.trailing_zeros()) as u16;
                            pm &= pm - 1;
                            j += 1;
                        }
                    }
                    _mm_prefetch::<_MM_HINT_T0>(slots.add(*pl.add(np) as usize) as *const i8);
                    np += n;
                }
                purch_scan(q, blkk, pl, aggp, nd, np);
                for d in &digs[..ndig] {
                    let seg = digest_put(tid, &mut fx, d, q, sidx, sts, np);
                    if d.first {
                        info.first = seg;
                    } else {
                        info.last = seg;
                    }
                }
                purch_add(q, pl, aggp, np);
            }
            xp.ph(7);
            {
                let core = &mut t.core;
                let mut n = core.day.len();
                if core.keys.len() != n || core.ses.len() != n || core.evs.len() != n || core.rev.len() != n || core.cat.len() != n {
                    fallback("row columns differ in length");
                }
                core.keys.reserve_total(n + nrow + 16);
                core.day.reserve_total(n + nrow + 16);
                core.ses.reserve_total(n + nrow + 16);
                core.evs.reserve_total(n + nrow + 16);
                core.rev.reserve_total(n + nrow + 16);
                core.cat.reserve_total(n + nrow + 16);
                let row_cap = row_cap.min(core.keys.cap()).min(core.day.cap()).min(core.ses.cap()).min(core.evs.cap()).min(core.rev.cap()).min(core.cat.cap());
                let (keys, dayc, sesc, evsc, revc, catc) = (core.keys.as_mut_ptr(), core.day.as_mut_ptr(), core.ses.as_mut_ptr(), core.evs.as_mut_ptr(), core.rev.as_mut_ptr(), core.cat.as_mut_ptr());
                let day0 = _mm512_set1_epi32(g.day0);
                let m31v = _mm512_set1_epi32(M31 as i32);
                let maxv = _mm512_set1_epi32(i32::MAX);
                let mut r = 0usize;
                while r < nrun {
                    if *run_pidx.add(r) == NONE {
                        r += 1;
                        continue;
                    }
                    let ra = *run_row0.add(r) as usize;
                    let mut rb = ra;
                    while r < nrun {
                        let pidx = *run_pidx.add(r);
                        if pidx == NONE {
                            break;
                        }
                        let xb = *run_row0.add(r + 1) as usize;
                        if n + (xb - ra) + 16 > row_cap {
                            fallback("avec full");
                        }
                        let pv = _mm512_set1_epi32(pidx as i32);
                        let mut x = rb;
                        loop {
                            _mm512_storeu_si512(keys.add(n + (x - ra)) as *mut _, pv);
                            x += 16;
                            if x >= xb {
                                break;
                            }
                        }
                        rb = xb;
                        r += 1;
                    }
                    let mut x = ra;
                    while x < rb {
                        let o = n + (x - ra);
                        _mm512_storeu_si512(dayc.add(o) as *mut _, _mm512_add_epi32(_mm512_loadu_si512(row_day.add(x) as *const _), day0));
                        let sa = _mm512_and_si512(_mm512_loadu_si512(row_s0.add(x) as *const _), m31v);
                        let sb = _mm512_and_si512(_mm512_loadu_si512(row_s0.add(x + 1) as *const _), m31v);
                        _mm512_storeu_si512(sesc.add(o) as *mut _, _mm512_sub_epi32(sb, sa));
                        _mm512_storeu_si512(evsc.add(o) as *mut _, _mm512_sub_epi32(_mm512_loadu_si512(row_r0.add(x + 1) as *const _), _mm512_loadu_si512(row_r0.add(x) as *const _)));
                        let rv = _mm512_loadu_si512(row_rev.add(x) as *const _);
                        let tail: u32 = if rb - x >= 16 { 0xFFFF } else { (1u32 << (rb - x)) - 1 };
                        if _mm512_cmpgt_epu32_mask(rv, maxv) as u32 & tail != 0 {
                            fallback("revenue_cents or events over i32");
                        }
                        _mm512_storeu_si512(revc.add(o) as *mut _, rv);
                        _mm_storeu_si128(catc.add(o) as *mut _, _mm_loadu_si128(row_cat.add(x) as *const _));
                        x += 16;
                    }
                    n += rb - ra;
                }
                core.keys.truncate(n);
                core.day.truncate(n);
                core.ses.truncate(n);
                core.evs.truncate(n);
                core.rev.truncate(n);
                core.cat.truncate(n);
                if core.day.len() != n {
                    fallback("avec length");
                }
            }
            let td = std::time::Instant::now();
            xp.ph(8);
            t.prof[0] += (tb - ta).as_nanos() as u64;
            t.prof[1] += (tc - tb).as_nanos() as u64;
            t.prof[2] += (td - tc).as_nanos() as u64;
        }
        xp.done();
        slab_settle(&mut t.core);
        if nd <= 16 && t.core.agg.len() >= (g.nc + 1) * nd {
            let agg = t.core.agg.as_mut_ptr() as *mut u32;
            let lo_i = _mm512_setr_epi32(0, 16, 1, 17, 2, 18, 3, 19, 4, 20, 5, 21, 6, 22, 7, 23);
            let hi_i = _mm512_setr_epi32(8, 24, 9, 25, 10, 26, 11, 27, 12, 28, 13, 29, 14, 30, 15, 31);
            let m_lo = ((1u64 << (2 * nd).min(16)) - 1) as u16;
            let m_hi = ((1u64 << (2 * nd).max(16).min(32) - 16) - 1) as u16;
            let m_cr = ((1u32 << nd) - 1) as u16;
            for c in 0..=g.nc {
                let cr = _mm512_maskz_loadu_epi32(m_cr, fx.cred.add(c * nd) as *const _);
                let a = _mm512_load_si512(fx.asst.add(c) as *const _);
                let p = agg.add(2 * c * nd);
                _mm512_mask_storeu_epi32(p as *mut _, m_lo, _mm512_add_epi32(_mm512_maskz_loadu_epi32(m_lo, p as *const _), _mm512_permutex2var_epi32(cr, lo_i, a)));
                if m_hi != 0 {
                    let p = p.add(16);
                    _mm512_mask_storeu_epi32(p as *mut _, m_hi, _mm512_add_epi32(_mm512_maskz_loadu_epi32(m_hi, p as *const _), _mm512_permutex2var_epi32(cr, hi_i, a)));
                }
            }
        } else {
            for c in 0..=g.nc {
                for d in 0..nd {
                    t.core.agg[c * nd + d][0] += *fx.cred.add(c * nd + d);
                    t.core.agg[c * nd + d][1] += (*fx.asst.add(c)).0[d];
                }
            }
        }
    }
}

fn stage3b(g: &G, tid: usize) {
    let t = g.t(tid);
    let mut released = false;
    loop {
        let i = g.next.fetch_add(1, Ordering::Relaxed);
        if i >= g.joined.len() {
            break;
        }
        if i >= g.nt && !released {
            g.ev_arena.release_part(tid, g.nt);
            released = true;
        }
        let segs = &g.joined[i];
        let u = segs[0].user;
        unsafe {
            let tv = *g.utab.add(u as usize);
            let seg = |s: &Seg| seg_w0(s);
            if tv & COMPLEX == 0 {
                let extra: &[u64] = if tv & EXTRA != 0 { &g.extra[&u] } else { &[] };
                let first = if segs[0].len & DIG != 0 { digest_get(&segs[0]).4 << 33 } else { seg(&segs[0])[0] };
                let mut st = t.core.begin(if !extra.is_empty() && extra[0] < first { extra[0] } else { first });
                let (mut bits, mut prev, mut sorted, mut n, mut xi) = (0u32, 0u64, true, 0u64, 0usize);
                let mut has_dig = false;
                for s in segs {
                    if s.len & DIG != 0 {
                        let (se, pu, cnt, ab, t0, t1) = digest_get(s);
                        has_dig = true;
                        bits |= ab;
                        sorted &= t0 >= prev;
                        bits |= t.core.feed_digest(&mut st, se, pu, extra, &mut xi);
                        prev = t1;
                        n += cnt;
                        continue;
                    }
                    let ws = seg(s);
                    let mut a = 0usize;
                    while xi < extra.len() {
                        let e = extra[xi];
                        let j = match ws[a..].iter().position(|&w0| e < w0) {
                            Some(d) => a + d,
                            None => break,
                        };
                        if j > a {
                            let (b, uns) = t.core.feed_slice(&mut st, &ws[a..j], prev);
                            bits |= b;
                            sorted &= !uns;
                            prev = ws[j - 1] >> 33;
                        }
                        bits |= 1 << (e & 15);
                        t.core.feed(&mut st, e);
                        xi += 1;
                        a = j;
                    }
                    if a < ws.len() {
                        let (b, uns) = t.core.feed_slice(&mut st, &ws[a..], prev);
                        bits |= b;
                        sorted &= !uns;
                        prev = ws[ws.len() - 1] >> 33;
                    }
                    n += s.len as u64;
                }
                while xi < extra.len() {
                    bits |= 1 << (extra[xi] & 15);
                    t.core.feed(&mut st, extra[xi]);
                    xi += 1;
                }
                let class = class_of(tv, bits);
                if class != 0 && sorted {
                    t.n_runs += 1;
                    t.n_simple += n;
                    t.n_odd += extra.len() as u64;
                    claim(g, u);
                    finish_simple(g, t, u, class, &st);
                    continue;
                }
                if has_dig {
                    fallback("digest of a user that needs its records");
                }
                t.core.abort(&st);
            }
            let mut buf = std::mem::take(&mut t.runbuf);
            buf.clear();
            for s in segs {
                buf.extend(seg(s).iter().map(|&w0| Rec { w0, w1: (u as u64) << 37 }));
            }
            process_run(g, t, u, &mut buf);
            t.runbuf = buf;
        }
        if !released {
            g.ev_arena.release_part(tid, g.nt);
            released = true;
        }
    }
    if !released {
        g.ev_arena.release_part(tid, g.nt);
    }
}

fn shares(g: &G, pool: &Pool, old: bool) -> Vec<Share> {
    let none = Share { dense: std::ptr::null_mut(), hot: std::ptr::null_mut(), agg: std::ptr::null_mut(), aggp: std::ptr::null_mut(), slab: std::ptr::null_mut(), slab_len: 0 };
    let mut v = vec![none; g.nt];
    if old {
        return v;
    }
    let (dense, hot) = fast_sizes(g.nc, g.nd);
    let cells = (g.nc + 2) * g.nd;
    for s in v.iter_mut() {
        s.dense = pool.take(r4k(dense), 4096);
        s.hot = pool.take(r4k(hot), 4096);
        s.agg = pool.take(r4k(cells * 8), 4096);
        s.aggp = pool.take(r4k(cells * 16), 4096);
    }
    let n_ev: usize = (0..g.nt).map(|t| g.t(t).n_ev_rows as usize).sum();
    let left = pool.left();
    let want = r4k(n_ev / g.nt * SLAB_ROWS / 100 * 131 / 5) + 16 * 4096;
    let slab = want.min(left.saturating_sub(EP_BYTES * g.nt) / g.nt) & !4095;
    if slab >= 1 << 20 {
        for s in v.iter_mut() {
            s.slab = pool.take(slab, 4096);
            s.slab_len = if s.slab.is_null() { 0 } else { slab };
        }
    }
    v
}
const SLAB_ROWS: usize = crate::env_u32(option_env!("XL_SLABROWS"), 13) as usize;

pub fn stage3(g: &mut G) {
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        let old = gr.nd > 16 || BB > 15 || std::env::var("XL_OLD3").is_ok_and(|v| v == "1");
        if gr.check {
            name_selftest(gr);
        }
        if let Some(v) = std::env::var("XL_DIGMIN").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
            DIG_MIN.store(if v == 0 { usize::MAX } else { v }, Ordering::Relaxed);
        }
        let pool = Pool::new(gr);
        let pool0 = pool.next.load(Ordering::Relaxed);
        let shares = shares(gr, &pool, old);
        run_par(gr.nt, |tid| {
            if tid == gr.nt - 1 {
                crate::links::uf_tail(gr);
            }
            if old {
                stage3a_old(gr, tid)
            } else {
                stage3a(gr, tid, &pool, &shares[tid])
            }
        });
        if XR {
            eprintln!("XRU pool: {} kB of {} kB taken", (pool.next.load(Ordering::Relaxed) - pool0) >> 10, (pool.end.max(pool0) - pool0) >> 10);
            for t in 0..gr.nt {
                let c = &gr.t(t).core;
                eprintln!("XRU core {t}: slab {} kB, rows {} of {} (slab {}), names {} of {} (slab {}), persons {} of {} (slab {}), edge pieces {}", shares[t].slab_len >> 10, c.day.len(), c.day.cap(), c.day.in_slab(), c.names.len(), c.names.cap(), c.names.in_slab(), c.names_off.len(), c.names_off.cap(), c.names_off.in_slab(), unsafe { EDGE[t].pieces.len() });
            }
        }
        xp_report((0..gr.nt).map(|t| gr.t(t).n_simple).sum::<u64>().max(1));
    }
    g.uf = crate::links::uf_take(g);
    let slabfull = std::env::var("XL_SLABFULL").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
    if slabfull == 1 {
        for tid in 0..g.nt {
            slab_full(&mut g.t(tid).core);
        }
    }
    lap("ev3a");
    let mut joined: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    for b in 0..g.nb {
        let info = unsafe { *g.edges.add(b) };
        if info.n == 0 {
            continue;
        }
        if !cur.is_empty() && cur[0].user != info.first.user {
            joined.push(std::mem::take(&mut cur));
        }
        cur.push(info.first);
        if info.n == 2 {
            joined.push(std::mem::take(&mut cur));
            cur.push(info.last);
        }
    }
    if !cur.is_empty() {
        joined.push(cur);
    }
    joined.sort_by_key(|v| std::cmp::Reverse(v.iter().map(|s| s.len as u64).sum::<u64>()));
    g.joined = joined;
    lap("stitch");
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        run_par(gr.nt, |tid| stage3b(gr, tid));
    }
    if slabfull == 2 {
        for tid in 0..g.nt {
            slab_full(&mut g.t(tid).core);
        }
    }
    crate::core::COLS_PINNED.store(true, Ordering::Relaxed);
}
