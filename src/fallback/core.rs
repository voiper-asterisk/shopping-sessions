use super::parse::{day_of, first, first_two, for_lines, count_lines, parse_price, parse_ts, split, B, DAY_MS};
use super::util::{hash, Map, Tab};


pub struct LinkMap<'a> {
    tab: Tab,
    ids: Vec<B<'a>>,
    hashes: Vec<u64>,
    name_of: Vec<u32>,
}

impl<'a> LinkMap<'a> {
    #[inline]
    pub fn name(&self, user: B<'a>, h: u64) -> (B<'a>, u64) {
        match self.tab.find(h, |i| self.ids[i as usize] == user) {
            Ok((_, i)) => {
                let n = self.name_of[i as usize] as usize;
                (self.ids[n], self.hashes[n])
            }
            Err(_) => (user, h),
        }
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
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

pub type LinkPart<'a> = Vec<(B<'a>, u64, B<'a>, u64)>;

pub fn parse_links(c: B<'_>) -> LinkPart<'_> {
    let mut v = Vec::with_capacity(count_lines(c) + 1);
    for_lines(c, |line| {
        if let Some([a, b]) = split::<2>(line) {
            if !a.is_empty() && !b.is_empty() {
                v.push((a, hash(a), b, hash(b)));
            }
        }
    });
    v
}

pub fn build_links<'a>(parts: &[LinkPart<'a>]) -> LinkMap<'a> {
    let n: usize = parts.iter().map(|p| p.len()).sum();
    let mut tab = Tab::with_capacity(2 * n + 16);
    let mut ids: Vec<B<'a>> = Vec::with_capacity(n);
    let mut hashes: Vec<u64> = Vec::with_capacity(n);
    let mut parent: Vec<u32> = Vec::with_capacity(n);
    let mut intern = |s: B<'a>, h: u64, ids: &mut Vec<B<'a>>, hashes: &mut Vec<u64>, parent: &mut Vec<u32>| -> u32 {
        match tab.find(h, |i| ids[i as usize] == s) {
            Ok((_, i)) => i,
            Err(slot) => {
                let i = ids.len() as u32;
                ids.push(s);
                hashes.push(h);
                parent.push(i);
                tab.set(slot, h, i);
                i
            }
        }
    };
    for p in parts {
        for &(a, ha, b, hb) in p {
            let x = intern(a, ha, &mut ids, &mut hashes, &mut parent);
            let y = intern(b, hb, &mut ids, &mut hashes, &mut parent);
            let rx = find(&mut parent, x);
            let ry = find(&mut parent, y);
            if rx != ry {
                if rx < ry {
                    parent[ry as usize] = rx
                } else {
                    parent[rx as usize] = ry
                }
            }
        }
    }
    let m = ids.len();
    let mut min_of: Vec<u32> = (0..m as u32).collect();
    let mut roots = vec![0u32; m];
    for i in 0..m as u32 {
        let r = find(&mut parent, i);
        roots[i as usize] = r;
        if ids[i as usize] < ids[min_of[r as usize] as usize] {
            min_of[r as usize] = i;
        }
    }
    let name_of: Vec<u32> = roots.iter().map(|&r| min_of[r as usize]).collect();
    LinkMap { tab, ids, hashes, name_of }
}


pub const T_VIEW: u32 = 0;
pub const T_CART: u32 = 1;
pub const T_PURCHASE: u32 = 2;
pub const T_INVALID: u32 = 255;

#[derive(Clone, Copy)]
pub struct Ev<'a> {
    pub line: &'a u8,
    pub ingest: i64,
    pub ts: i64,
    pub pt: u32,
    pub cat: u32,
}
impl<'a> Ev<'a> {
    #[inline]
    fn eid(&self) -> B<'a> {
        unsafe { first(self.line) }
    }
    #[inline]
    fn user(&self) -> B<'a> {
        unsafe { first_two(self.line).1 }
    }
}

#[derive(Clone, Copy)]
pub struct Rec<'a> {
    pub name: B<'a>,
    pub nh: u64,
    pub ts: i64,
    pub pt: u32,
    pub cat: u32,
}

pub struct Parts<T> {
    pub items: Vec<T>,
    pub off: Vec<u32>,
}
impl<T> Default for Parts<T> {
    fn default() -> Self {
        Parts { items: Vec::new(), off: Vec::new() }
    }
}
impl<T: Copy> Parts<T> {
    fn group(tmp: &[(T, u32)], nshard: usize) -> Parts<T> {
        let mut off = vec![0u32; nshard + 1];
        for &(_, s) in tmp {
            off[s as usize + 1] += 1;
        }
        for i in 0..nshard {
            off[i + 1] += off[i];
        }
        let n = tmp.len();
        let mut items: Vec<T> = Vec::with_capacity(n);
        let mut pos: Vec<u32> = off[..nshard].to_vec();
        let p = items.as_mut_ptr();
        for &(x, s) in tmp {
            let q = &mut pos[s as usize];
            unsafe { p.add(*q as usize).write(x) };
            *q += 1;
        }
        unsafe { items.set_len(n) };
        Parts { items, off }
    }
    #[inline]
    pub fn shard(&self, s: usize) -> &[T] {
        if self.off.is_empty() {
            return &[];
        }
        &self.items[self.off[s] as usize..self.off[s + 1] as usize]
    }
}

pub struct EvCtx<'m, 'a> {
    pub ws: i64,
    pub we: i64,
    pub products: &'m Map<B<'a>, u32>,
    pub unknown_cat: u32,
}

#[inline]
pub fn shard_of(h: u64, nshard: usize) -> usize {
    ((h >> 32) as usize) & (nshard - 1)
}

pub fn parse_events<'a>(c: B<'a>, ctx: &EvCtx<'_, 'a>, nshard: usize, tmp: &mut Vec<(Ev<'a>, u32)>) -> Parts<Ev<'a>> {
    tmp.clear();
    for_lines(c, |line| {
        let Some([eid, user, ts, ingest, typ, product, price]) = split::<7>(line) else { return };
        if eid.is_empty() {
            return;
        }
        let Some(ingest) = parse_ts(ingest) else { return };
        let h = hash(eid);
        let mut e = Ev { line: &line[0], ingest, ts: 0, pt: T_INVALID << 24, cat: 0 };
        'v: {
            if user.is_empty() || product.is_empty() {
                break 'v;
            }
            let Some(t) = parse_ts(ts) else { break 'v };
            if t < ctx.ws || t >= ctx.we {
                break 'v;
            }
            let ty = match typ {
                b"view" => T_VIEW,
                b"cart" => T_CART,
                b"purchase" => T_PURCHASE,
                _ => break 'v,
            };
            let mut pr = 0u32;
            if ty == T_PURCHASE {
                let Some(p) = parse_price(price) else { break 'v };
                pr = p;
                e.cat = *ctx.products.get(product).unwrap_or(&ctx.unknown_cat);
            }
            if ingest - t > DAY_MS {
                break 'v;
            }
            e.ts = t;
            e.pt = pr | (ty << 24);
        }
        tmp.push((e, shard_of(h, nshard) as u32));
    });
    Parts::group(tmp, nshard)
}

#[derive(Default)]
pub struct DedupBufs<'a> {
    tab: Tab,
    base: Vec<u32>,
    win: Vec<bool>,
    tmp: Vec<(Rec<'a>, u32)>,
}

pub fn dedup_route<'a>(parts: &[Parts<Ev<'a>>], s: usize, links: &LinkMap<'a>, npshard: usize, b: &mut DedupBufs<'a>) -> Parts<Rec<'a>> {
    b.base.clear();
    let mut total = 0usize;
    for p in parts {
        b.base.push(total as u32);
        total += p.shard(s).len();
    }
    b.base.push(total as u32);
    let base = &b.base;
    let locate = |idx: u32| -> (usize, usize) {
        let c = base.partition_point(|&x| x <= idx) - 1;
        (c, (idx - base[c]) as usize)
    };
    b.tab.reset(total);
    b.win.clear();
    b.win.resize(total, true);
    for (ci, p) in parts.iter().enumerate() {
        let v = p.shard(s);
        for (i, e) in v.iter().enumerate() {
            let eid = e.eid();
            let eh = hash(eid);
            let flat = base[ci] + i as u32;
            let r = b.tab.find(eh, |idx| {
                let (c, j) = locate(idx);
                parts[c].shard(s)[j].eid() == eid
            });
            match r {
                Ok((slot, idx)) => {
                    let (c, j) = locate(idx);
                    if e.ingest < parts[c].shard(s)[j].ingest {
                        b.win[idx as usize] = false;
                        b.tab.set(slot, eh, flat);
                    } else {
                        b.win[flat as usize] = false;
                    }
                }
                Err(slot) => b.tab.set(slot, eh, flat),
            }
        }
    }
    b.tmp.clear();
    let mut k = 0usize;
    for p in parts {
        for e in p.shard(s) {
            let w = b.win[k];
            k += 1;
            if !w || (e.pt >> 24) > T_PURCHASE {
                continue;
            }
            let user = e.user();
            let (name, nh) = links.name(user, hash(user));
            b.tmp.push((Rec { name, nh, ts: e.ts, pt: e.pt, cat: e.cat }, shard_of(nh, npshard) as u32));
        }
    }
    Parts::group(&b.tmp, npshard)
}


#[derive(Clone, Copy)]
pub struct Ck0<'a> {
    pub line: &'a u8,
    pub ts: i64,
    pub camp: u32,
}

#[derive(Clone, Copy)]
pub struct Ck<'a> {
    pub name: B<'a>,
    pub nh: u64,
    pub line: &'a u8,
    pub ts: i64,
    pub camp: u32,
}

pub fn parse_clicks<'a>(c: B<'a>, ws: i64, we: i64, campaigns: &Map<B<'a>, u32>) -> Vec<Ck0<'a>> {
    let lo = ws - 7 * DAY_MS;
    let mut v = Vec::with_capacity(count_lines(c) + 1);
    for_lines(c, |line| {
        let Some([cid, user, camp, ts]) = split::<4>(line) else { return };
        if cid.is_empty() || user.is_empty() {
            return;
        }
        let Some(t) = parse_ts(ts) else { return };
        if t < lo || t >= we {
            return;
        }
        let Some(&ci) = campaigns.get(camp) else { return };
        v.push(Ck0 { line: &line[0], ts: t, camp: ci });
    });
    v
}

pub fn route_clicks<'a>(v: &[Ck0<'a>], links: &LinkMap<'a>, npshard: usize, tmp: &mut Vec<(Ck<'a>, u32)>) -> Parts<Ck<'a>> {
    tmp.clear();
    for c in v {
        let user = unsafe { first_two(c.line).1 };
        let (name, nh) = links.name(user, hash(user));
        tmp.push((Ck { name, nh, line: c.line, ts: c.ts, camp: c.camp }, shard_of(nh, npshard) as u32));
    }
    Parts::group(tmp, npshard)
}


pub const GAP_MS: i64 = 1_800_000;
pub const WEEK_MS: i64 = 7 * 86_400_000;

pub type Acc = Map<u64, [u64; 4]>;

#[inline]
pub fn key(camp: u32, day: i32) -> u64 {
    ((camp as u64) << 32) | (day as u32 as u64)
}
pub fn unkey(k: u64) -> (u32, i32) {
    ((k >> 32) as u32, k as u32 as i32)
}

#[derive(Clone, Copy)]
pub struct PE {
    pub ts: i64,
    pub pt: u32,
    pub cat: u32,
}

#[derive(Clone, Copy)]
pub struct PC<'a> {
    pub ts: i64,
    pub cid: B<'a>,
    pub camp: u32,
}

pub struct Sess {
    pub start: i64,
    pub day: i32,
    pub rev: u64,
    pub purch: bool,
}

pub struct UserRow<'a> {
    pub name: B<'a>,
    pub day: i32,
    pub sessions: u64,
    pub events: u64,
    pub revenue: u64,
    pub top: Option<u32>,
}

pub struct Scratch {
    sess: Vec<Sess>,
    starts: Vec<i64>,
    days: Vec<i32>,
    day_end: Vec<u32>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    ct: Vec<(u32, i64)>,
    rs: Vec<u32>,
}

impl Scratch {
    pub fn new(ncat: usize) -> Self {
        Scratch { sess: vec![], starts: vec![], days: vec![], day_end: vec![], catsum: vec![0; ncat], touched: vec![], ct: vec![], rs: vec![] }
    }
}

struct DayAcc {
    day: i32,
    sessions: u64,
    events: u64,
    revenue: u64,
}

fn flush_row<'a>(name: B<'a>, d: &DayAcc, sc: &mut Scratch, rows: &mut Vec<UserRow<'a>>) {
    let mut top: Option<u32> = None;
    let mut best = 0u64;
    for &c in &sc.touched {
        let v = sc.catsum[c as usize];
        if top.is_none() || v > best || (v == best && c < top.unwrap()) {
            top = Some(c);
            best = v;
        }
        sc.catsum[c as usize] = 0;
    }
    sc.touched.clear();
    rows.push(UserRow { name, day: d.day, sessions: d.sessions, events: d.events, revenue: d.revenue, top });
}

fn person<'a>(name: B<'a>, evs: &mut [PE], cks: &mut [PC<'a>], sc: &mut Scratch, rows: &mut Vec<UserRow<'a>>, organic: u32, acc: &mut Acc) {
    if evs.is_empty() {
        return;
    }
    evs.sort_unstable_by_key(|e| e.ts);
    sc.sess.clear();
    let mut d = DayAcc { day: day_of(evs[0].ts), sessions: 0, events: 0, revenue: 0 };
    let mut prev = evs[0].ts;
    for (i, e) in evs.iter().enumerate() {
        if i == 0 || e.ts - prev > GAP_MS {
            let day = day_of(e.ts);
            if day != d.day {
                flush_row(name, &d, sc, rows);
                d = DayAcc { day, sessions: 0, events: 0, revenue: 0 };
            }
            d.sessions += 1;
            sc.sess.push(Sess { start: e.ts, day, rev: 0, purch: false });
        }
        prev = e.ts;
        d.events += 1;
        if (e.pt >> 24) == T_PURCHASE {
            let price = (e.pt & 0x00ff_ffff) as u64;
            let s = sc.sess.last_mut().unwrap();
            s.rev += price;
            s.purch = true;
            d.revenue += price;
            let c = e.cat as usize;
            if sc.catsum[c] == 0 {
                sc.touched.push(e.cat);
            }
            sc.catsum[c] += price;
        }
    }
    flush_row(name, &d, sc, rows);

    if !cks.is_empty() {
        cks.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.cid.cmp(b.cid)));
    }
    credit(&sc.sess, cks, &mut sc.rs, organic, acc);
    if !cks.is_empty() {
        sc.starts.clear();
        sc.days.clear();
        sc.starts.extend(sc.sess.iter().map(|s| s.start));
        sc.days.extend(sc.sess.iter().map(|s| s.day));
        let n = sc.sess.len();
        sc.day_end.clear();
        sc.day_end.resize(n, 0);
        let mut end = n as u32;
        for i in (0..n).rev() {
            if i + 1 < n && sc.days[i + 1] != sc.days[i] {
                end = (i + 1) as u32;
            }
            sc.day_end[i] = end;
        }
        assists(&sc.starts, &sc.days, &sc.day_end, cks, &mut sc.ct, acc);
    }
}

#[derive(Default)]
pub struct ShardBufs<'a> {
    tab: Tab,
    names: Vec<B<'a>>,
    pid_ev: Vec<u32>,
    off: Vec<u32>,
    pos: Vec<u32>,
    pe: Vec<PE>,
    pid_ck: Vec<u32>,
    coff: Vec<u32>,
    pc: Vec<PC<'a>>,
}

pub fn run_shard<'a>(recs: &[Parts<Rec<'a>>], cks: &[Parts<Ck<'a>>], p: usize, organic: u32, sc: &mut Scratch, bf: &mut ShardBufs<'a>, acc: &mut Acc) -> Vec<UserRow<'a>> {
    let n_ev: usize = recs.iter().map(|r| r.shard(p).len()).sum();
    bf.tab.reset(n_ev);
    let names = &mut bf.names;
    names.clear();
    bf.pid_ev.clear();
    for r in recs {
        for e in r.shard(p) {
            let h = e.nh;
            let pid = match bf.tab.find(h, |i| names[i as usize] == e.name) {
                Ok((_, i)) => i,
                Err(slot) => {
                    let i = names.len() as u32;
                    names.push(e.name);
                    bf.tab.set(slot, h, i);
                    i
                }
            };
            bf.pid_ev.push(pid);
        }
    }
    let np = names.len();
    let off = &mut bf.off;
    off.clear();
    off.resize(np + 1, 0);
    for &pid in &bf.pid_ev {
        off[pid as usize + 1] += 1;
    }
    for i in 0..np {
        off[i + 1] += off[i];
    }
    bf.pos.clear();
    bf.pos.extend_from_slice(&off[..np]);
    bf.pe.clear();
    bf.pe.resize(n_ev, PE { ts: 0, pt: 0, cat: 0 });
    let mut k = 0usize;
    for r in recs {
        for e in r.shard(p) {
            let pid = bf.pid_ev[k] as usize;
            k += 1;
            bf.pe[bf.pos[pid] as usize] = PE { ts: e.ts, pt: e.pt, cat: e.cat };
            bf.pos[pid] += 1;
        }
    }
    bf.pid_ck.clear();
    let coff = &mut bf.coff;
    coff.clear();
    coff.resize(np + 1, 0);
    for c in cks {
        for x in c.shard(p) {
            let h = x.nh;
            let pid = match bf.tab.find(h, |i| names[i as usize] == x.name) {
                Ok((_, i)) => {
                    coff[i as usize + 1] += 1;
                    i
                }
                Err(_) => u32::MAX,
            };
            bf.pid_ck.push(pid);
        }
    }
    for i in 0..np {
        coff[i + 1] += coff[i];
    }
    bf.pos.clear();
    bf.pos.extend_from_slice(&coff[..np]);
    let nck_kept = coff[np] as usize;
    bf.pc.clear();
    bf.pc.resize(nck_kept, PC { ts: 0, cid: &[], camp: 0 });
    let mut k = 0usize;
    for c in cks {
        for x in c.shard(p) {
            let pid = bf.pid_ck[k];
            k += 1;
            if pid != u32::MAX {
                let q = &mut bf.pos[pid as usize];
                bf.pc[*q as usize] = PC { ts: x.ts, cid: unsafe { first(x.line) }, camp: x.camp };
                *q += 1;
            }
        }
    }
    let mut rows = Vec::with_capacity(np * 2);
    for i in 0..np {
        let evs = &mut bf.pe[off[i] as usize..off[i + 1] as usize];
        let cs = &mut bf.pc[coff[i] as usize..coff[i + 1] as usize];
        person(names[i], evs, cs, sc, &mut rows, organic, acc);
    }
    rows
}


fn credit(sess: &[Sess], cks: &[PC], rs: &mut Vec<u32>, organic: u32, acc: &mut Acc) {
    rs.clear();
    for i in 0..cks.len() {
        if i > 0 && cks[i].ts == cks[i - 1].ts {
            let r = rs[i - 1];
            rs.push(r);
        } else {
            rs.push(i as u32);
        }
    }
    let n = cks.len();
    let (mut lo, mut hi) = (0usize, 0usize);
    for s in sess {
        while hi < n && cks[hi].ts <= s.start {
            hi += 1;
        }
        while lo < hi && cks[lo].ts < s.start - WEEK_MS {
            lo += 1;
        }
        let camp = if hi > lo { cks[rs[hi - 1] as usize].camp } else { organic };
        let a = acc.entry(key(camp, s.day)).or_insert([0; 4]);
        a[0] += 1;
        a[1] += s.purch as u64;
        a[2] += s.rev;
    }
}

fn assists(starts: &[i64], days: &[i32], day_end: &[u32], cks: &[PC], ct: &mut Vec<(u32, i64)>, acc: &mut Acc) {
    ct.clear();
    ct.extend(cks.iter().map(|c| (c.camp, c.ts)));
    ct.sort_unstable();
    let n = ct.len();
    let mut flush = |camp: u32, a: i64, b: i64| {
        let lo = starts.partition_point(|&s| s < a);
        let hi = starts.partition_point(|&s| s <= b);
        let mut k = lo;
        while k < hi {
            let e = (day_end[k] as usize).min(hi);
            acc.entry(key(camp, days[k])).or_insert([0; 4])[3] += (e - k) as u64;
            k = e;
        }
    };
    let mut i = 0;
    while i < n {
        let camp = ct[i].0;
        let a = ct[i].1;
        let mut b = a + WEEK_MS;
        i += 1;
        while i < n && ct[i].0 == camp && ct[i].1 <= b {
            b = b.max(ct[i].1 + WEEK_MS);
            i += 1;
        }
        flush(camp, a, b);
    }
}
