// Legacy pipeline (the validated v1 implementation), kept verbatim as a fallback for inputs outside the
// fast path's ranges (more than 256 categories, a window too wide for 32-bit relative timestamps).
use crate::pq;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const MS_DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const SEVEN: i64 = 7 * MS_DAY;
const NP: usize = 256; // partitions (eid, then person)
const MAX_PRICE: u64 = 10_000_000;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

pub(crate) type B = &'static [u8];

// ------------------------------------------------------------------ io
pub(crate) fn map_file(path: &Path) -> B {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(path).unwrap_or_else(|e| die(&format!("open {:?}: {}", path, e)));
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    unsafe {
        let p = mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0);
        if p as isize != -1 {
            madvise(p, len, 2);
            return std::slice::from_raw_parts(p, len);
        }
    }
    Box::leak(std::fs::read(path).unwrap().into_boxed_slice())
}

pub(crate) fn die(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    std::process::exit(1);
}

pub(crate) fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "csv") && p.is_file() {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn nl_pos(b: &[u8]) -> Option<usize> {
    b.iter().position(|&x| x == b'\n')
}

pub(crate) fn make_chunks(files: &[B], target: usize) -> Vec<B> {
    let mut out = Vec::new();
    for &f in files {
        let hdr = nl_pos(f).map(|i| i + 1).unwrap_or(f.len());
        let mut b = hdr;
        while b < f.len() {
            let mut e = b + target;
            if e >= f.len() {
                e = f.len();
            } else {
                e = match nl_pos(&f[e - 1..]) {
                    Some(i) => e - 1 + i + 1,
                    None => f.len(),
                };
            }
            out.push(&f[b..e]);
            b = e;
        }
    }
    out
}

pub(crate) fn skip_header(b: B) -> B {
    match nl_pos(b) {
        Some(i) => &b[i + 1..],
        None => &[],
    }
}

// ------------------------------------------------------------------ line splitting
#[inline(always)]
pub(crate) fn for_each_line<const N: usize, F: FnMut(&[B; N])>(buf: B, mut f: F) {
    let len = buf.len();
    let mut fields: [B; N] = [&[]; N];
    let mut k = 0usize;
    let mut start = 0usize;
    let mut q = 0u32;
    let mut i = 0usize;
    macro_rules! finish {
        ($end:expr) => {{
            if k == N - 1 {
                fields[N - 1] = &buf[start..$end];
                let mut wrapped = 0u32;
                for fld in fields.iter_mut() {
                    let l = fld.len();
                    if l >= 2 && fld[0] == b'"' && fld[l - 1] == b'"' {
                        *fld = &fld[1..l - 1];
                        wrapped += 2;
                    }
                }
                if wrapped == q {
                    f(&fields);
                }
            }
        }};
    }
    while i < len {
        let b = buf[i];
        if b == b',' {
            if k < N - 1 {
                fields[k] = &buf[start..i];
            }
            k += 1;
            start = i + 1;
        } else if b == b'\n' {
            finish!(i);
            k = 0;
            start = i + 1;
            q = 0;
        } else if b == b'"' {
            q += 1;
        }
        i += 1;
    }
    if start < len {
        finish!(len);
    }
}

// ------------------------------------------------------------------ hashing
#[inline(always)]
fn mix(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline(always)]
fn rd8(s: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(s[i..i + 8].try_into().unwrap())
}

#[inline]
pub(crate) fn hash128(s: &[u8]) -> (u64, u64) {
    const K1: u64 = 0xa076_1d64_78bd_642f;
    const K2: u64 = 0xe703_7ed1_a0b4_28db;
    const K3: u64 = 0x8ebc_6af0_9c88_c6e3;
    const K4: u64 = 0x5899_65cc_7537_4cc3;
    let n = s.len();
    let mut a = K1 ^ (n as u64);
    let mut b = K3 ^ (n as u64);
    let mut i = 0;
    while i + 8 < n {
        let w = rd8(s, i);
        a = mix(a ^ w, K2);
        b = mix(b ^ w, K4);
        i += 8;
    }
    // tail: last (up to 8) bytes
    let tail = if n >= 8 {
        rd8(s, n - 8)
    } else if n >= 4 {
        let lo = u32::from_le_bytes(s[0..4].try_into().unwrap()) as u64;
        let hi = u32::from_le_bytes(s[n - 4..n].try_into().unwrap()) as u64;
        lo | (hi << 32)
    } else if n > 0 {
        (s[0] as u64) | ((s[n >> 1] as u64) << 8) | ((s[n - 1] as u64) << 16)
    } else {
        0
    };
    let x = mix(a ^ tail, K2 ^ (n as u64));
    let y = mix(b ^ tail.rotate_left(29), K4 ^ x);
    (x, y)
}

#[derive(Default)]
pub(crate) struct BytesHasher(u64);
impl Hasher for BytesHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = mix(self.0 ^ hash128(bytes).0, 0x9E37_79B9_7F4A_7C15);
    }
    fn write_usize(&mut self, i: usize) {
        self.0 ^= i as u64;
    }
    fn write_u8(&mut self, i: u8) {
        self.0 ^= i as u64;
    }
}
pub(crate) type BMap<V> = HashMap<B, V, BuildHasherDefault<BytesHasher>>;

// ------------------------------------------------------------------ timestamps
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[inline(always)]
fn dig2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a < 10 && b < 10 {
        Some((a * 10 + b) as i64)
    } else {
        None
    }
}

/// Epoch ms, or None if bad (F1/F2/F3).
#[inline]
pub(crate) fn parse_ts(s: &[u8]) -> Option<i64> {
    let n = s.len();
    if n == 0 {
        return None;
    }
    if s[0].wrapping_sub(b'0') < 10 {
        // F3 candidate: all ASCII digits (stops at the first non-digit)
        let mut v: u64 = 0;
        let mut all = true;
        for &c in s {
            let d = c.wrapping_sub(b'0');
            if d >= 10 {
                all = false;
                break;
            }
            // overflow means a value too large for any window: bad either way
            v = v.checked_mul(10)?.checked_add(d as u64)?;
        }
        if all {
            return if v <= i64::MAX as u64 { Some(v as i64) } else { None };
        }
    }
    if n != 20 && n != 25 {
        return None;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = (dig2(s, 0)? * 100) + dig2(s, 2)?;
    let mo = dig2(s, 5)?;
    let d = dig2(s, 8)?;
    let h = dig2(s, 11)?;
    let mi = dig2(s, 14)?;
    let se = dig2(s, 17)?;
    let off: i64;
    if n == 20 {
        if s[19] != b'Z' {
            return None;
        }
        off = 0;
    } else {
        let sign = s[19];
        if (sign != b'+' && sign != b'-') || s[22] != b':' {
            return None;
        }
        let oh = dig2(s, 20)?;
        let om = dig2(s, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let o = (oh * 60 + om) * 60_000;
        off = if sign == b'-' { -o } else { o };
    }
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 {
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
    Some((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se) * 1000 - off)
}

#[inline]
pub(crate) fn parse_digits(s: &[u8]) -> Option<u64> {
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
    Some(v)
}

// ------------------------------------------------------------------ links
#[derive(Clone, Copy)]
struct Slot {
    h1: u64,
    h2: u64,
    pk: u64,
    nm: u64, // 0 = empty
}

struct LinkTable {
    slots: Vec<Slot>,
    mask: usize,
}

#[inline(always)]
fn pack_name(s: &[u8]) -> u64 {
    (s.as_ptr() as u64) | ((s.len() as u64) << 48)
}
#[inline(always)]
fn unpack_name(nm: u64) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts((nm & 0xFFFF_FFFF_FFFF) as *const u8, (nm >> 48) as usize) }
}

impl LinkTable {
    #[inline(always)]
    fn lookup(&self, h1: u64, h2: u64) -> Option<(u64, u64)> {
        let mut s = (h1 as usize) & self.mask;
        loop {
            let sl = unsafe { self.slots.get_unchecked(s) };
            if sl.nm == 0 {
                return None;
            }
            if sl.h1 == h1 && sl.h2 == h2 {
                return Some((sl.pk, sl.nm));
            }
            s = (s + 1) & self.mask;
        }
    }
}

fn build_links(pairs: &[(B, B)]) -> LinkTable {
    struct Id {
        h1: u64,
        h2: u64,
        s: B,
    }
    let mut ids: Vec<Id> = Vec::new();
    let cap = ((pairs.len() * 4).max(16)).next_power_of_two();
    let mask = cap - 1;
    let mut tab: Vec<u32> = vec![u32::MAX; cap];
    let mut parent: Vec<u32> = Vec::new();
    let mut intern = |s: B, ids: &mut Vec<Id>, parent: &mut Vec<u32>| -> u32 {
        let (h1, h2) = hash128(s);
        let mut p = (h1 as usize) & mask;
        loop {
            let t = tab[p];
            if t == u32::MAX {
                let id = ids.len() as u32;
                ids.push(Id { h1, h2, s });
                parent.push(id);
                tab[p] = id;
                return id;
            }
            let e = &ids[t as usize];
            if e.h1 == h1 && e.h2 == h2 {
                return t;
            }
            p = (p + 1) & mask;
        }
    };
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        let mut r = x;
        while parent[r as usize] != r {
            r = parent[r as usize];
        }
        while parent[x as usize] != r {
            let nx = parent[x as usize];
            parent[x as usize] = r;
            x = nx;
        }
        r
    }
    for &(a, b) in pairs {
        let ia = intern(a, &mut ids, &mut parent);
        let ib = intern(b, &mut ids, &mut parent);
        let ra = find(&mut parent, ia);
        let rb = find(&mut parent, ib);
        if ra != rb {
            let (lo, hi) = if ids[rb as usize].s < ids[ra as usize].s { (rb, ra) } else { (ra, rb) };
            parent[hi as usize] = lo;
        }
    }
    drop(tab);
    // entries for non-root ids only (roots and singletons map to themselves anyway)
    let mut n_ent = 0usize;
    let mut roots = vec![0u32; ids.len()];
    for i in 0..ids.len() {
        let r = find(&mut parent, i as u32);
        roots[i] = r;
        if r as usize != i {
            n_ent += 1;
        }
    }
    let cap2 = (n_ent * 2).max(16).next_power_of_two();
    let mask2 = cap2 - 1;
    let mut slots = vec![Slot { h1: 0, h2: 0, pk: 0, nm: 0 }; cap2];
    for i in 0..ids.len() {
        let r = roots[i] as usize;
        if r == i {
            continue;
        }
        let id = &ids[i];
        let root = &ids[r];
        let mut p = (id.h1 as usize) & mask2;
        while slots[p].nm != 0 {
            p = (p + 1) & mask2;
        }
        slots[p] = Slot { h1: id.h1, h2: id.h2, pk: root.h1, nm: pack_name(root.s) };
    }
    LinkTable { slots, mask: mask2 }
}

// ------------------------------------------------------------------ records
#[derive(Clone, Copy)]
struct EvRec {
    h1: u64,
    ing: i64,
    pkey: u64,
    ts: i64,
    name: u64,
    aux: u64, // bit0 valid, bit1 purchase, 2..26 price, 26..38 cat, 38..64 h2
}

#[derive(Clone, Copy)]
struct GRec {
    pkey: u64,
    ts: i64,
    name: u64,
    aux: u64, // bit1 purchase, price, cat (same layout as EvRec)
}

#[derive(Clone, Copy)]
struct ClRec {
    pkey: u64,
    ts: i64,
    idptr: u64,
    camp: u32,
    idlen: u32,
}

struct Ctx {
    w0: i64,
    w1: i64,
    links: LinkTable,
    products: BMap<u16>,
    campaigns: BMap<u32>,
    unknown_cat: u16,
}

#[inline(always)]
fn person_of(ctx: &Ctx, user: &'static [u8]) -> Option<(u64, u64)> {
    if user.len() >= 65536 {
        return None;
    }
    let (h1, h2) = hash128(user);
    Some(match ctx.links.lookup(h1, h2) {
        Some(x) => x,
        None => (h1, pack_name(user)),
    })
}

fn parse_clicks(chunks: &[B], next: &AtomicUsize, ctx: &Ctx) -> Vec<Vec<ClRec>> {
    let mut out: Vec<Vec<ClRec>> = (0..NP).map(|_| Vec::new()).collect();
    loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= chunks.len() {
            break;
        }
        for_each_line::<4, _>(chunks[i], |f| {
            let cid = f[0];
            if cid.is_empty() || f[1].is_empty() {
                return;
            }
            let camp = match ctx.campaigns.get(f[2]) {
                Some(&c) => c,
                None => return,
            };
            let ts = match parse_ts(f[3]) {
                Some(t) if t >= ctx.w0 - SEVEN && t < ctx.w1 => t,
                _ => return,
            };
            let (pk, _) = match person_of(ctx, f[1]) {
                Some(x) => x,
                None => return,
            };
            out[(pk >> 56) as usize].push(ClRec {
                pkey: pk,
                ts,
                idptr: cid.as_ptr() as u64,
                camp,
                idlen: cid.len() as u32,
            });
        });
    }
    out
}

fn parse_events(chunks: &[B], next: &AtomicUsize, ctx: &Ctx) -> Vec<Vec<EvRec>> {
    let mut out: Vec<Vec<EvRec>> = (0..NP).map(|_| Vec::new()).collect();
    loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= chunks.len() {
            break;
        }
        for_each_line::<7, _>(chunks[i], |f| {
            let eid = f[0];
            if eid.is_empty() {
                return;
            }
            let (h1, h2) = hash128(eid);
            let ing = parse_ts(f[3]).unwrap_or(i64::MAX);
            let mut rec = EvRec { h1, ing, pkey: 0, ts: 0, name: 0, aux: (h2 & 0x3FF_FFFF) << 38 };
            'v: {
                if f[1].is_empty() || f[5].is_empty() {
                    break 'v;
                }
                let purchase = match f[4] {
                    b"view" | b"cart" => false,
                    b"purchase" => true,
                    _ => break 'v,
                };
                let ts = match parse_ts(f[2]) {
                    Some(t) if t >= ctx.w0 && t < ctx.w1 => t,
                    _ => break 'v,
                };
                let mut bits = 1u64;
                if purchase {
                    match parse_digits(f[6]) {
                        Some(p) if (1..=MAX_PRICE).contains(&p) => {
                            let cat = ctx.products.get(f[5]).copied().unwrap_or(ctx.unknown_cat);
                            bits |= 2 | (p << 2) | ((cat as u64) << 26);
                        }
                        _ => break 'v,
                    }
                }
                if ing == i64::MAX || ing - ts > MS_DAY {
                    break 'v;
                }
                let (pk, nm) = match person_of(ctx, f[1]) {
                    Some(x) => x,
                    None => break 'v,
                };
                rec.pkey = pk;
                rec.ts = ts;
                rec.name = nm;
                rec.aux |= bits;
            }
            out[(h1 >> 56) as usize].push(rec);
        });
    }
    out
}

fn dedup_partition(vecs: Vec<Vec<EvRec>>, out: &mut [Vec<GRec>]) {
    let total: usize = vecs.iter().map(|v| v.len()).sum();
    if total == 0 {
        return;
    }
    let recs: Vec<EvRec> = if vecs.len() == 1 {
        vecs.into_iter().next().unwrap()
    } else {
        let mut r = Vec::with_capacity(total);
        for v in vecs {
            r.extend_from_slice(&v);
        }
        r
    };
    let cap = (total * 2).next_power_of_two();
    let mask = cap - 1;
    let mut tab = vec![u32::MAX; cap];
    for (i, r) in recs.iter().enumerate() {
        let mut s = (r.h1 as usize) & mask;
        loop {
            let t = tab[s];
            if t == u32::MAX {
                tab[s] = i as u32;
                break;
            }
            let o = &recs[t as usize];
            if o.h1 == r.h1 && (o.aux >> 38) == (r.aux >> 38) {
                if r.ing < o.ing {
                    tab[s] = i as u32;
                }
                break;
            }
            s = (s + 1) & mask;
        }
    }
    for &t in tab.iter() {
        if t == u32::MAX {
            continue;
        }
        let r = &recs[t as usize];
        if r.aux & 1 == 0 {
            continue;
        }
        out[(r.pkey >> 56) as usize].push(GRec { pkey: r.pkey, ts: r.ts, name: r.name, aux: r.aux & ((1 << 38) - 1) });
    }
}

// ------------------------------------------------------------------ sessionize
#[derive(Clone, Copy)]
struct UserRow {
    name: u64,
    day: i32,
    sessions: u32,
    events: u32,
    top: u16, // 0xFFFF = null
    rev: u64,
}

struct Agg {
    ndays: usize,
    day0: i64,
    sessions: Vec<u64>,
    purch: Vec<u64>,
    rev: Vec<u64>,
    assisted: Vec<u64>,
}

struct Worker<'a> {
    ncat: usize,
    organic: usize,
    rows: Vec<UserRow>,
    agg: Agg,
    // per-person scratch
    cat_sum: Vec<u64>,
    touched: Vec<u16>,
    cnt: Vec<u32>,
    pos: Vec<u32>,
    present: Vec<u32>,
    _p: std::marker::PhantomData<&'a ()>,
}

fn id_of(c: &ClRec) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts(c.idptr as *const u8, c.idlen as usize) }
}

impl<'a> Worker<'a> {
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
                // reset after reading below
            }
            top = bi;
            for &c in &self.touched {
                self.cat_sum[c as usize] = 0;
            }
            self.touched.clear();
        }
        self.rows.push(UserRow { name, day: day as i32, sessions, events, top, rev });
    }

    fn person(&mut self, ev: &[GRec], cl: &[ClRec]) {
        let name = ev[0].name;
        let mut cur_day = i64::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0u32, 0u32, 0u64);
        // current session
        let mut in_sess = false;
        let (mut s_start, mut s_rev, mut s_npur) = (0i64, 0u64, 0u32);
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut prev = 0i64;
        let day0 = self.agg.day0;
        let ndays = self.agg.ndays;
        macro_rules! end_session {
            () => {{
                let dayidx = (cur_day - day0) as usize;
                let mut cred = self.organic;
                if !cl.is_empty() {
                    while hi < cl.len() && cl[hi].ts <= s_start {
                        let c = cl[hi].camp as usize;
                        if self.cnt[c] == 0 {
                            self.pos[c] = self.present.len() as u32;
                            self.present.push(c as u32);
                        }
                        self.cnt[c] += 1;
                        hi += 1;
                    }
                    while lo < hi && cl[lo].ts < s_start - SEVEN {
                        let c = cl[lo].camp as usize;
                        self.cnt[c] -= 1;
                        if self.cnt[c] == 0 {
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
                        let ts = cl[hi - 1].ts;
                        let mut j = hi - 1;
                        while j > lo && cl[j - 1].ts == ts {
                            j -= 1;
                        }
                        cred = cl[j].camp as usize;
                        for &c in &self.present {
                            self.agg.assisted[c as usize * ndays + dayidx] += 1;
                        }
                    }
                }
                let ix = cred * ndays + dayidx;
                self.agg.sessions[ix] += 1;
                if s_npur > 0 {
                    self.agg.purch[ix] += 1;
                }
                self.agg.rev[ix] += s_rev;
            }};
        }
        for e in ev {
            if !in_sess || e.ts - prev > GAP {
                if in_sess {
                    end_session!();
                }
                in_sess = true;
                s_start = e.ts;
                s_rev = 0;
                s_npur = 0;
                let day = e.ts.div_euclid(MS_DAY);
                if day != cur_day {
                    if cur_day != i64::MIN {
                        self.flush_day(name, cur_day, d_sess, d_ev, d_rev);
                    }
                    cur_day = day;
                    d_sess = 0;
                    d_ev = 0;
                    d_rev = 0;
                }
                d_sess += 1;
            }
            prev = e.ts;
            d_ev += 1;
            if e.aux & 2 != 0 {
                let price = (e.aux >> 2) & 0xFF_FFFF;
                let cat = ((e.aux >> 26) & 0xFFF) as usize;
                s_rev += price;
                s_npur += 1;
                d_rev += price;
                if self.cat_sum[cat] == 0 {
                    self.touched.push(cat as u16);
                }
                self.cat_sum[cat] += price;
            }
        }
        if in_sess {
            end_session!();
            self.flush_day(name, cur_day, d_sess, d_ev, d_rev);
        }
        // reset window state
        for c in &cl[lo..hi] {
            self.cnt[c.camp as usize] = 0;
        }
        self.present.clear();
        let _ = self.ncat;
    }
}

// ------------------------------------------------------------------ main
fn run_par<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n).map(|t| {
            let f = &f;
            s.spawn(move || f(t))
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

pub(crate) fn extract_json_str(text: &str, key: &str) -> Option<String> {
    let k = text.find(&format!("\"{}\"", key))?;
    let rest = &text[k + key.len() + 2..];
    let c = rest.find(':')?;
    let rest = &rest[c + 1..];
    let q1 = rest.find('"')?;
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"')?;
    Some(rest[..q2].to_string())
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let mut data = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let mut outd = std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    if args.len() >= 3 {
        data = args[1].clone();
        outd = args[2].clone();
    }
    let nthreads: usize = std::env::var("NCPU").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or_else(|| {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
    });
    let verbose = std::env::var("FAST_VERBOSE").is_ok();
    let t0 = std::time::Instant::now();
    let lap = |what: &str| {
        if verbose {
            eprintln!("[{:7.3}s] {}", t0.elapsed().as_secs_f64(), what);
        }
    };
    let data = PathBuf::from(data);

    // manifest
    let mtxt = std::fs::read_to_string(data.join("manifest.json")).unwrap_or_else(|e| die(&format!("manifest: {}", e)));
    let w0 = parse_ts(extract_json_str(&mtxt, "window_start").unwrap_or_else(|| die("window_start")).as_bytes())
        .unwrap_or_else(|| die("bad window_start"));
    let w1 = parse_ts(extract_json_str(&mtxt, "window_end").unwrap_or_else(|| die("window_end")).as_bytes())
        .unwrap_or_else(|| die("bad window_end"));

    // products & campaigns
    let pfile = map_file(&data.join("products.csv"));
    let mut prod_raw: Vec<(B, B)> = Vec::new();
    for_each_line::<2, _>(skip_header(pfile), |f| {
        if !f[0].is_empty() {
            prod_raw.push((f[0], f[1]));
        }
    });
    let mut cat_names: Vec<B> = prod_raw.iter().map(|p| p.1).collect();
    cat_names.push(b"unknown");
    cat_names.sort();
    cat_names.dedup();
    if cat_names.len() > 4000 {
        die("too many categories");
    }
    let mut products: BMap<u16> = BMap::default();
    for (pid, cat) in &prod_raw {
        let ci = cat_names.binary_search(cat).unwrap() as u16;
        products.insert(*pid, ci);
    }
    let unknown_cat = cat_names.binary_search(&(b"unknown" as B)).unwrap() as u16;
    let cfile = map_file(&data.join("campaigns.csv"));
    let mut campaigns: BMap<u32> = BMap::default();
    let mut camp_names: Vec<(B, B)> = Vec::new();
    for_each_line::<2, _>(skip_header(cfile), |f| {
        if f[0].is_empty() {
            return;
        }
        match campaigns.get(f[0]) {
            Some(&i) => camp_names[i as usize].1 = f[1],
            None => {
                campaigns.insert(f[0], camp_names.len() as u32);
                camp_names.push((f[0], f[1]));
            }
        }
    });
    let ncamps = camp_names.len();
    lap("dims");

    // map files
    let map_dir = |sub: &str| -> Vec<B> { list_csv(&data.join(sub)).iter().map(|p| map_file(p)).collect() };
    let ev_files = map_dir("events");
    let cl_files = map_dir("clicks");
    let lk_files = map_dir("links");

    // links
    let lchunks = make_chunks(&lk_files, 4 << 20);
    let next = AtomicUsize::new(0);
    let lparts = run_par(nthreads, |_| {
        let mut v: Vec<(B, B)> = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= lchunks.len() {
                break;
            }
            for_each_line::<2, _>(lchunks[i], |f| {
                if !f[0].is_empty() && !f[1].is_empty() && f[0].len() < 65536 && f[1].len() < 65536 {
                    v.push((f[0], f[1]));
                }
            });
        }
        v
    });
    let pairs: Vec<(B, B)> = lparts.into_iter().flatten().collect();
    let links = build_links(&pairs);
    drop(pairs);
    lap("links");

    let w0d = w0.div_euclid(MS_DAY);
    let ndays = ((w1 - 1).div_euclid(MS_DAY) - w0d + 1).max(1) as usize;
    let ctx = Ctx { w0, w1, links, products, campaigns, unknown_cat };

    // clicks
    let cchunks = make_chunks(&cl_files, 4 << 20);
    let next = AtomicUsize::new(0);
    let cparts = run_par(nthreads, |_| parse_clicks(&cchunks, &next, &ctx));
    let mut click_parts: Vec<Mutex<Vec<Vec<ClRec>>>> = (0..NP).map(|_| Mutex::new(Vec::new())).collect();
    for tp in cparts {
        for (p, v) in tp.into_iter().enumerate() {
            click_parts[p].get_mut().unwrap().push(v);
        }
    }
    lap("clicks parsed");

    // events
    let echunks = make_chunks(&ev_files, 4 << 20);
    let next = AtomicUsize::new(0);
    let eparts = run_par(nthreads, |_| parse_events(&echunks, &next, &ctx));
    let mut ev_parts: Vec<Mutex<Vec<Vec<EvRec>>>> = (0..NP).map(|_| Mutex::new(Vec::new())).collect();
    for tp in eparts {
        for (p, v) in tp.into_iter().enumerate() {
            ev_parts[p].get_mut().unwrap().push(v);
        }
    }
    lap("events parsed");

    // dedup
    let next = AtomicUsize::new(0);
    let gparts = run_par(nthreads, |_| {
        let mut out: Vec<Vec<GRec>> = (0..NP).map(|_| Vec::new()).collect();
        loop {
            let p = next.fetch_add(1, Ordering::Relaxed);
            if p >= NP {
                break;
            }
            let vecs = std::mem::take(&mut *ev_parts[p].lock().unwrap());
            dedup_partition(vecs, &mut out);
        }
        out
    });
    drop(ev_parts);
    let mut g_parts: Vec<Mutex<Vec<Vec<GRec>>>> = (0..NP).map(|_| Mutex::new(Vec::new())).collect();
    for tp in gparts {
        for (p, v) in tp.into_iter().enumerate() {
            g_parts[p].get_mut().unwrap().push(v);
        }
    }
    lap("deduped");

    // sessionize
    std::fs::create_dir_all(Path::new(&outd).join("user_daily")).unwrap();
    std::fs::create_dir_all(Path::new(&outd).join("campaign_daily")).unwrap();
    let outp = PathBuf::from(&outd);
    let ncat = cat_names.len();
    let next = AtomicUsize::new(0);
    let aggs = run_par(nthreads, |t| {
        let mut w = Worker {
            ncat,
            organic: ncamps,
            rows: Vec::new(),
            agg: Agg {
                ndays,
                day0: w0d,
                sessions: vec![0; (ncamps + 1) * ndays],
                purch: vec![0; (ncamps + 1) * ndays],
                rev: vec![0; (ncamps + 1) * ndays],
                assisted: vec![0; (ncamps + 1) * ndays],
            },
            cat_sum: vec![0; ncat],
            touched: Vec::new(),
            cnt: vec![0; ncamps + 1],
            pos: vec![0; ncamps + 1],
            present: Vec::new(),
            _p: std::marker::PhantomData,
        };
        loop {
            let p = next.fetch_add(1, Ordering::Relaxed);
            if p >= NP {
                break;
            }
            let gv = std::mem::take(&mut *g_parts[p].lock().unwrap());
            let cv = std::mem::take(&mut *click_parts[p].lock().unwrap());
            let mut ev: Vec<GRec> = Vec::with_capacity(gv.iter().map(|v| v.len()).sum());
            for v in gv {
                ev.extend_from_slice(&v);
            }
            let mut cl: Vec<ClRec> = Vec::with_capacity(cv.iter().map(|v| v.len()).sum());
            for v in cv {
                cl.extend_from_slice(&v);
            }
            ev.sort_unstable_by_key(|r| (r.pkey, r.ts));
            cl.sort_unstable_by(|a, b| {
                (a.pkey, a.ts).cmp(&(b.pkey, b.ts)).then_with(|| id_of(a).cmp(id_of(b)))
            });
            let mut ci = 0usize;
            let mut i = 0usize;
            while i < ev.len() {
                let pk = ev[i].pkey;
                let mut j = i + 1;
                while j < ev.len() && ev[j].pkey == pk {
                    j += 1;
                }
                while ci < cl.len() && cl[ci].pkey < pk {
                    ci += 1;
                }
                let mut cj = ci;
                while cj < cl.len() && cl[cj].pkey == pk {
                    cj += 1;
                }
                w.person(&ev[i..j], &cl[ci..cj]);
                ci = cj;
                i = j;
            }
        }
        // write this thread's user_daily part
        let rows = std::mem::take(&mut w.rows);
        if !rows.is_empty() || t == 0 {
            let n = rows.len();
            let cols = vec![
                pq::Col { name: "user_id", ptype: pq::T_BYTES, optional: false, logical: pq::Logical::Str,
                    body: pq::str_page(rows.iter().map(|r| unpack_name(r.name)), n * 16) },
                pq::Col { name: "day", ptype: pq::T_INT32, optional: false, logical: pq::Logical::Date,
                    body: pq::i32_page(rows.iter().map(|r| r.day), n) },
                pq::Col { name: "sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
                    body: pq::i64_page(rows.iter().map(|r| r.sessions as i64), n) },
                pq::Col { name: "events", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
                    body: pq::i64_page(rows.iter().map(|r| r.events as i64), n) },
                pq::Col { name: "revenue_cents", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
                    body: pq::i64_page(rows.iter().map(|r| r.rev as i64), n) },
                pq::Col { name: "top_category", ptype: pq::T_BYTES, optional: true, logical: pq::Logical::Str,
                    body: pq::opt_str_page(
                        rows.iter().map(|r| if r.top == 0xFFFF { None } else { Some(cat_names[r.top as usize]) }), n) },
            ];
            pq::write_file(&outp.join("user_daily").join(format!("part-{}.parquet", t)), n, cols).unwrap();
        }
        w.agg
    });
    lap("sessionized + user_daily written");

    // campaign_daily
    let size = (ncamps + 1) * ndays;
    let mut tot = Agg { ndays, day0: w0d, sessions: vec![0; size], purch: vec![0; size], rev: vec![0; size], assisted: vec![0; size] };
    for a in &aggs {
        for i in 0..size {
            tot.sessions[i] += a.sessions[i];
            tot.purch[i] += a.purch[i];
            tot.rev[i] += a.rev[i];
            tot.assisted[i] += a.assisted[i];
        }
    }
    let mut rows: Vec<(usize, usize)> = Vec::new();
    for c in 0..=ncamps {
        for d in 0..ndays {
            let i = c * ndays + d;
            if tot.sessions[i] > 0 || tot.assisted[i] > 0 {
                rows.push((c, d));
            }
        }
    }
    let n = rows.len();
    let cid = |c: usize| -> &'static [u8] { if c == ncamps { b"organic" } else { camp_names[c].0 } };
    let chn = |c: usize| -> &'static [u8] { if c == ncamps { b"organic" } else { camp_names[c].1 } };
    let cols = vec![
        pq::Col { name: "campaign_id", ptype: pq::T_BYTES, optional: false, logical: pq::Logical::Str,
            body: pq::str_page(rows.iter().map(|&(c, _)| cid(c)), n * 8) },
        pq::Col { name: "channel", ptype: pq::T_BYTES, optional: false, logical: pq::Logical::Str,
            body: pq::str_page(rows.iter().map(|&(c, _)| chn(c)), n * 8) },
        pq::Col { name: "day", ptype: pq::T_INT32, optional: false, logical: pq::Logical::Date,
            body: pq::i32_page(rows.iter().map(|&(_, d)| (w0d + d as i64) as i32), n) },
        pq::Col { name: "sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
            body: pq::i64_page(rows.iter().map(|&(c, d)| tot.sessions[c * ndays + d] as i64), n) },
        pq::Col { name: "purchasing_sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
            body: pq::i64_page(rows.iter().map(|&(c, d)| tot.purch[c * ndays + d] as i64), n) },
        pq::Col { name: "revenue_cents", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
            body: pq::i64_page(rows.iter().map(|&(c, d)| tot.rev[c * ndays + d] as i64), n) },
        pq::Col { name: "assisted_sessions", ptype: pq::T_INT64, optional: false, logical: pq::Logical::None,
            body: pq::i64_page(rows.iter().map(|&(c, d)| tot.assisted[c * ndays + d] as i64), n) },
    ];
    pq::write_file(&outp.join("campaign_daily").join("part-0.parquet"), n, cols).unwrap();
    lap("done");
}
