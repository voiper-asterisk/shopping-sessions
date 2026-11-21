use crate::keyrec::scramble;
use crate::sys::*;
use crate::*;

#[derive(Clone, Copy)]
struct Sess {
    start: u64,
    day: u32,
    rev: u64,
}

#[derive(Clone, Copy)]
pub struct St {
    prev: u64,
    s_start: u64,
    s_day: u32,
    s_rev: u64,
    d_s: u32,
    d_e: u32,
    d_rev: u64,
    catmask: u32,
    mark: usize,
    pidx: u32,
}

pub enum Name<'a> {
    Simple(u32, u32),
    Bytes(&'a [u8]),
}

pub struct Tab<T> {
    p: *mut T,
    n: usize,
}
unsafe impl<T> Send for Tab<T> {}
impl<T: Copy> Tab<T> {
    #[cfg_attr(feature = "tl", track_caller)]
    pub fn new(n: usize) -> Tab<T> {
        Tab { p: anon(n.max(1) * std::mem::size_of::<T>()) as *mut T, n }
    }
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.n
    }
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.p
    }
    pub unsafe fn set_ptr(&mut self, p: *mut T) {
        self.p = p;
    }
}
impl<T> std::ops::Index<usize> for Tab<T> {
    type Output = T;
    #[inline(always)]
    fn index(&self, i: usize) -> &T {
        assert!(i < self.n);
        unsafe { &*self.p.add(i) }
    }
}
impl<T> std::ops::IndexMut<usize> for Tab<T> {
    #[inline(always)]
    fn index_mut(&mut self, i: usize) -> &mut T {
        assert!(i < self.n);
        unsafe { &mut *self.p.add(i) }
    }
}

pub struct Col<T> {
    p: *mut T,
    len: usize,
    cap: usize,
    home: *mut T,
    home_cap: usize,
    zlen: usize,
}
unsafe impl<T> Send for Col<T> {}
pub static COLS_PINNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" {
    fn syscall(n: i64, ...) -> i64;
}
impl<T: Copy> Col<T> {
    #[cfg_attr(feature = "tl", track_caller)]
    pub fn new(cap: usize) -> Col<T> {
        let cap = cap.max(16);
        let p = anon(cap * std::mem::size_of::<T>()) as *mut T;
        Col { p, len: 0, cap, home: p, home_cap: cap, zlen: 0 }
    }
    #[inline(always)]
    pub fn push(&mut self, v: T) {
        if self.len >= self.cap {
            self.grow(self.len + 1);
        }
        unsafe { self.p.add(self.len).write(v) };
        self.len += 1;
    }
    #[inline(always)]
    pub fn extend(&mut self, s: &[T]) {
        if self.len + s.len() > self.cap {
            self.grow(self.len + s.len());
        }
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), self.p.add(self.len), s.len()) };
        self.len += s.len();
    }
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    pub fn cap(&self) -> usize {
        self.cap
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
    pub fn truncate(&mut self, n: usize) {
        self.len = n;
    }
    pub fn zeros_fresh(&mut self, n: usize) {
        if self.len != 0 || self.p != self.home || n > self.cap {
            fallback("zeros_fresh");
        }
        self.len = n;
        self.zlen = n;
    }
    #[inline(always)]
    pub fn reserve_total(&mut self, n: usize) {
        if n > self.cap {
            self.grow(n);
        }
    }
    pub fn in_slab(&self) -> bool {
        self.p != self.home
    }
    pub fn slab_full(&mut self) {
        if self.p != self.home {
            self.cap = self.len;
        }
    }
    pub unsafe fn set_slab(&mut self, p: *mut T, cap: usize) {
        if self.p != self.home || self.len > cap || cap == 0 || p as usize & 4095 != 0 {
            return;
        }
        let z = self.zlen.min(self.len);
        std::ptr::write_bytes(p, 0, z);
        std::ptr::copy_nonoverlapping(self.p.add(z), p.add(z), self.len - z);
        self.zlen = 0;
        self.p = p;
        self.cap = cap;
    }
    #[cold]
    #[inline(never)]
    pub fn grow(&mut self, n: usize) {
        if self.p == self.home || n > self.home_cap || self.len > self.home_cap {
            fallback("avec full");
        }
        let bytes = self.len * std::mem::size_of::<T>();
        let pages = (bytes + 4095) & !4095;
        unsafe {
            let moved = pages != 0 && !COLS_PINNED.load(std::sync::atomic::Ordering::Relaxed) && syscall(25, self.p as *mut u8, pages, pages, 3i32, self.home as *mut u8) == self.home as i64;
            if !moved {
                std::ptr::copy_nonoverlapping(self.p as *const u8, self.home as *mut u8, bytes);
            }
        }
        self.p = self.home;
        self.cap = self.home_cap;
    }
}

pub struct Core {
    ws_mod: u64,
    day0: i32,
    pub nd: usize,
    pub organic: usize,
    pub agg: Tab<[u32; 2]>,
    pub aggp: Tab<[u64; 2]>,
    pub names_off: Col<u32>,
    pub names: Col<u8>,
    pub keys: Col<u32>,
    pub day: Col<i32>,
    pub ses: Col<i32>,
    pub evs: Col<i32>,
    pub rev: Col<i32>,
    pub cat: Col<u8>,
    pub n_sess: u64,
    sess: Vec<Sess>,
    stamp: Vec<u32>,
    serial: u32,
    catsum: [u64; 32],
    cts: Vec<u64>,
    iv: Vec<[u64; 2]>,
    dstart: Vec<u32>,
}

#[inline]
pub fn simple_name(g: &G, u: u32, class: u32) -> (u64, usize) {
    simple_name_key(g.ukey, u, class)
}

#[inline]
pub fn simple_name_key(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    let mut m = class;
    let mut best = (u64::MAX, 0usize);
    while m != 0 {
        let a = m.trailing_zeros() as u64;
        m &= m - 1;
        let e = enc36(scramble((u as u64) << 9 | a, ukey));
        if e.0 < best.0 {
            best = e;
        }
    }
    best
}

impl Core {
    pub fn new(nc: usize, nd: usize, row_cap: usize, ws_mod: u64, day0: i32) -> Core {
        let mut names_off = Col::new(row_cap + 1);
        if crate::sys::CORE_NOTOUCH.load(std::sync::atomic::Ordering::Relaxed) {
            names_off.zeros_fresh(1);
        } else {
            names_off.push(0u32);
        }
        Core {
            ws_mod,
            day0,
            nd,
            organic: nc,
            agg: Tab::new((nc + 2) * nd),
            aggp: Tab::new((nc + 2) * nd),
            names_off,
            names: Col::new(row_cap * 10),
            keys: Col::new(row_cap),
            day: Col::new(row_cap),
            ses: Col::new(row_cap),
            evs: Col::new(row_cap),
            rev: Col::new(row_cap),
            cat: Col::new(row_cap),
            n_sess: 0,
            sess: Vec::new(),
            stamp: vec![0; nc + 1],
            serial: 0,
            catsum: [0; 32],
            cts: Vec::new(),
            iv: Vec::new(),
            dstart: vec![0; nd + 2],
        }
    }

    #[inline(always)]
    fn flush_row(&mut self, pidx: u32, day: u32, s: u32, e: u32, rev: u64, catmask: u32) {
        let mut top = 255u8;
        if catmask != 0 {
            let mut m = catmask;
            let mut best = 0u64;
            while m != 0 {
                let c = m.trailing_zeros() as usize;
                m &= m - 1;
                let v = self.catsum[c];
                self.catsum[c] = 0;
                if v > best {
                    best = v;
                    top = c as u8;
                }
            }
        }
        if rev > i32::MAX as u64 || e > i32::MAX as u32 {
            fallback("revenue_cents or events over i32");
        }
        self.keys.push(pidx);
        self.day.push(self.day0 + day as i32);
        self.ses.push(s as i32);
        self.evs.push(e as i32);
        self.rev.push(rev as i32);
        self.cat.push(top);
    }

    #[inline]
    pub fn person(&mut self, g: &G, evs: &[Rec], cks: &[u64], name: Name) {
        if evs.is_empty() {
            return;
        }
        let mut st = self.begin(evs[0].w0);
        for r in evs {
            self.feed(&mut st, r.w0);
        }
        self.finish(g, &st, cks, name);
    }

    #[inline(always)]
    pub fn begin(&mut self, w0: u64) -> St {
        self.sess.clear();
        let t = w0 >> 33;
        St { prev: t, s_start: t, s_day: self.dayof(t), s_rev: 0, d_s: 1, d_e: 0, d_rev: 0, catmask: 0, mark: self.day.len(), pidx: (self.names_off.len() - 1) as u32 }
    }

    #[inline(always)]
    fn dayof(&self, t: u64) -> u32 {
        ((t + self.ws_mod) / DAY_MS as u64) as u32
    }

    #[inline(always)]
    pub fn feed(&mut self, st: &mut St, w: u64) {
        let t = w >> 33;
        if t.wrapping_sub(st.prev) > GAP_MS {
            self.sess.push(Sess { start: st.s_start + WEEK_MS, day: st.s_day, rev: st.s_rev });
            let nd = self.dayof(t);
            if nd != st.s_day {
                self.flush_row(st.pidx, st.s_day, st.d_s, st.d_e, st.d_rev, st.catmask);
                st.d_s = 0;
                st.d_e = 0;
                st.d_rev = 0;
                st.catmask = 0;
                st.s_day = nd;
            }
            st.d_s += 1;
            st.s_start = t;
            st.s_rev = 0;
        }
        st.prev = t;
        st.d_e += 1;
        let price = (w >> 9) & 0xFF_FFFF;
        if price != 0 {
            let c = ((w >> 4) & 31) as usize;
            st.s_rev += price;
            st.d_rev += price;
            self.catsum[c] += price;
            st.catmask |= 1 << c;
        }
    }

    pub fn feed_digest(&mut self, st: &mut St, sess: &[u64], purch: &[u64], extra: &[u64], xi: &mut usize) -> u32 {
        let mut pi = 0usize;
        let mut bits = 0u32;
        for i in 0..sess.len() / 2 {
            let (w, last) = (sess[2 * i], sess[2 * i + 1]);
            let t = w & 0xFFFF_FFFF;
            if last < t {
                fallback("digest session");
            }
            while *xi < extra.len() && (extra[*xi] >> 33) < t {
                bits |= 1 << (extra[*xi] & 15);
                self.feed(st, extra[*xi]);
                *xi += 1;
            }
            if t.wrapping_sub(st.prev) > GAP_MS {
                self.sess.push(Sess { start: st.s_start + WEEK_MS, day: st.s_day, rev: st.s_rev });
                let nd = self.dayof(t);
                if nd != st.s_day {
                    self.flush_row(st.pidx, st.s_day, st.d_s, st.d_e, st.d_rev, st.catmask);
                    st.d_s = 0;
                    st.d_e = 0;
                    st.d_rev = 0;
                    st.catmask = 0;
                    st.s_day = nd;
                }
                st.d_s += 1;
                st.s_start = t;
                st.s_rev = 0;
            }
            let cnt = (w >> 32) as u32;
            if cnt > i32::MAX as u32 - st.d_e.min(i32::MAX as u32) {
                fallback("revenue_cents or events over i32");
            }
            st.d_e += cnt;
            while pi < purch.len() && (purch[pi] >> 32) as usize == i {
                let price = (purch[pi] >> 5) & 0xFF_FFFF;
                let c = (purch[pi] & 31) as usize;
                st.s_rev += price;
                st.d_rev += price;
                self.catsum[c] += price;
                st.catmask |= 1 << c;
                pi += 1;
            }
            while *xi < extra.len() && (extra[*xi] >> 33) <= last {
                let e = extra[*xi];
                bits |= 1 << (e & 15);
                st.d_e += 1;
                let price = (e >> 9) & 0xFF_FFFF;
                if price != 0 {
                    let c = ((e >> 4) & 31) as usize;
                    st.s_rev += price;
                    st.d_rev += price;
                    self.catsum[c] += price;
                    st.catmask |= 1 << c;
                }
                *xi += 1;
            }
            st.prev = last;
        }
        if pi != purch.len() {
            fallback("digest purchases");
        }
        bits
    }

    #[inline(never)]
    pub fn feed_slice(&mut self, st: &mut St, ws: &[u64], sprev: u64) -> (u32, bool) {
        use std::arch::x86_64::*;
        let n = ws.len();
        let p = ws.as_ptr();
        let mut bits = 0u32;
        let mut uns = n != 0 && (ws[0] >> 33) < sprev;
        let mut i = 0usize;
        unsafe {
            if n >= 8 {
                let gapv = _mm512_set1_epi64(GAP_MS as i64);
                let pm = _mm512_set1_epi64(0xFF_FFFFi64 << 9);
                let one = _mm512_set1_epi64(1);
                let m15 = _mm512_set1_epi64(15);
                let mut bitsv = _mm512_setzero_si512();
                let mut unsm = 0u8;
                let mut last = st.prev;
                let mut prevv = _mm512_set1_epi64(last as i64);
                while i + 8 <= n {
                    let w = _mm512_loadu_si512(p.add(i) as *const _);
                    let ts = _mm512_srli_epi64(w, 33);
                    let pv = _mm512_alignr_epi64(ts, prevv, 7);
                    let gap = _mm512_cmpgt_epu64_mask(_mm512_sub_epi64(ts, pv), gapv);
                    unsm |= _mm512_cmplt_epu64_mask(ts, pv);
                    bitsv = _mm512_or_si512(bitsv, _mm512_sllv_epi64(one, _mm512_and_si512(w, m15)));
                    let f = gap | _mm512_test_epi64_mask(w, pm);
                    if f != 0 {
                        let mut m = f as u32;
                        let mut done = 0u32;
                        while m != 0 {
                            let k = m.trailing_zeros();
                            m &= m - 1;
                            st.d_e += k - done;
                            st.prev = if k == 0 { last } else { *p.add(i + k as usize - 1) >> 33 };
                            self.feed(st, *p.add(i + k as usize));
                            done = k + 1;
                        }
                        st.d_e += 8 - done;
                    } else {
                        st.d_e += 8;
                    }
                    last = *p.add(i + 7) >> 33;
                    prevv = ts;
                    i += 8;
                }
                st.prev = last;
                bits = _mm512_reduce_or_epi64(bitsv) as u32;
                uns |= unsm != 0;
            }
        }
        while i < n {
            let w = ws[i];
            bits |= 1 << (w & 15);
            uns |= (w >> 33) < st.prev;
            self.feed(st, w);
            i += 1;
        }
        (bits, uns)
    }

    pub fn abort(&mut self, st: &St) {
        let mut m = st.catmask;
        while m != 0 {
            self.catsum[m.trailing_zeros() as usize] = 0;
            m &= m - 1;
        }
        self.keys.truncate(st.mark);
        self.day.truncate(st.mark);
        self.ses.truncate(st.mark);
        self.evs.truncate(st.mark);
        self.rev.truncate(st.mark);
        self.cat.truncate(st.mark);
    }

    #[inline(always)]
    pub fn finish(&mut self, g: &G, st: &St, cks: &[u64], name: Name) {
        self.sess.push(Sess { start: st.s_start + WEEK_MS, day: st.s_day, rev: st.s_rev });
        self.flush_row(st.pidx, st.s_day, st.d_s, st.d_e, st.d_rev, st.catmask);
        self.n_sess += self.sess.len() as u64;
        match name {
            Name::Simple(u, class) => {
                let (v, len) = simple_name(g, u, class);
                self.names.push(b'u');
                self.names.extend(&v.to_be_bytes()[..len]);
            }
            Name::Bytes(b) => self.names.extend(b),
        }
        if self.names.len() > u32::MAX as usize {
            fallback("names over 4 GB");
        }
        self.names_off.push(self.names.len() as u32);

        let nd = self.nd;
        let n = cks.len();
        if unsafe { crate::parse::SKIP } & 1 != 0 {
            return;
        }
        if n == 0 {
            let base = self.organic * nd;
            for s in &self.sess {
                let i = base + s.day as usize;
                self.agg[i][0] += 1;
                if s.rev != 0 {
                    self.aggp[i][0] += 1;
                    self.aggp[i][1] += s.rev;
                }
            }
            return;
        }
        let small = self.sess.len() * n <= 4096;
        let (mut lo, mut hi) = (0usize, 0usize);
        for s in &self.sess {
            while hi < n && (cks[hi] >> 32) <= s.start {
                hi += 1;
            }
            while lo < hi && (cks[lo] >> 32) + WEEK_MS < s.start {
                lo += 1;
            }
            let camp = if hi > lo { ((cks[hi - 1] >> 4) & (CBOUND as u64 - 1)) as usize } else { self.organic };
            let i = camp * nd + s.day as usize;
            self.agg[i][0] += 1;
            if s.rev != 0 {
                self.aggp[i][0] += 1;
                self.aggp[i][1] += s.rev;
            }
            if small && hi > lo && unsafe { crate::parse::SKIP } & 2 == 0 {
                self.serial += 1;
                let serial = self.serial;
                for k in &cks[lo..hi] {
                    let c = ((k >> 4) & (CBOUND as u64 - 1)) as usize;
                    if self.stamp[c] != serial {
                        self.stamp[c] = serial;
                        self.agg[c * nd + s.day as usize][1] += 1;
                    }
                }
            }
        }
        if !small {
            self.assists_big(cks);
        }
    }

    #[inline(never)]
    fn assists_big(&mut self, cks: &[u64]) {
        let nd = self.nd;
        let ns = self.sess.len();
        let mut i = 0usize;
        for d in 0..=nd {
            while i < ns && (self.sess[i].day as usize) < d {
                i += 1;
            }
            self.dstart[d] = i as u32;
        }
        self.dstart[nd + 1] = ns as u32;
        const NONE: u64 = u64::MAX;
        if self.iv.len() < self.organic + 2 {
            self.iv.resize(self.organic + 2, [NONE, 0]);
        }
        self.cts.clear();
        for &k in cks {
            let c = ((k >> 4) & (CBOUND as u64 - 1)) as usize;
            let t = k >> 32;
            if c >= self.iv.len() {
                fallback("campaign number");
            }
            let iv = unsafe { self.iv.get_unchecked_mut(c) };
            if iv[0] != NONE && t <= iv[1] {
                iv[1] = t + WEEK_MS;
            } else {
                if iv[0] == NONE {
                    self.cts.push(c as u64);
                } else {
                    let (a, b) = (iv[0], iv[1]);
                    self.assist_interval(c, a, b);
                }
                self.iv[c] = [t, t + WEEK_MS];
            }
        }
        for x in 0..self.cts.len() {
            let c = self.cts[x] as usize;
            let (a, b) = (self.iv[c][0], self.iv[c][1]);
            self.iv[c][0] = NONE;
            self.assist_interval(c, a, b);
        }
    }

    #[inline(always)]
    fn assist_interval(&mut self, c: usize, a: u64, b: u64) {
        let nd = self.nd;
        let lo = self.sess.partition_point(|s| s.start < a);
        let hi = self.sess.partition_point(|s| s.start <= b);
        let mut k = lo;
        while k < hi {
            let d = self.sess[k].day as usize;
            let e = (self.dstart[d + 1] as usize).min(hi);
            self.agg[c * nd + d][1] += (e - k) as u32;
            k = e;
        }
    }
}
