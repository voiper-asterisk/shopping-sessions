use crate::keyrec;
use crate::k_ref;
use crate::sys::*;
use crate::*;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

fn field(raw: &[u8]) -> Option<&[u8]> {
    let q = raw.iter().filter(|&&b| b == b'"').count();
    if q == 0 {
        Some(raw)
    } else if q == 2 && raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        Some(&raw[1..raw.len() - 1])
    } else {
        None
    }
}
pub fn split2(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let c = line.iter().position(|&b| b == b',')?;
    let rest = &line[c + 1..];
    if rest.contains(&b',') {
        return None;
    }
    Some((field(&line[..c])?, field(rest)?))
}

fn manifest_ts(txt: &[u8], key: &[u8]) -> Option<i64> {
    let at = txt.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &txt[at..];
    let colon = rest.iter().position(|&b| b == b':')?;
    let rest = &rest[colon + 1..];
    let q1 = rest.iter().position(|&b| b == b'"')?;
    let rest = &rest[q1 + 1..];
    let q2 = rest.iter().position(|&b| b == b'"')?;
    let v = k_ref::parse_f1(&rest[..q2]);
    (v != crate::iface::TS_BAD).then_some(v)
}

#[inline]
fn find3(b: &[u8], s: usize) -> (usize, u8) {
    use std::arch::x86_64::*;
    let mut i = s;
    unsafe {
        let (nl, cm, qt) = (_mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'"' as i8));
        while i + 32 <= b.len() {
            let v = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let m = _mm256_movemask_epi8(_mm256_or_si256(_mm256_or_si256(_mm256_cmpeq_epi8(v, nl), _mm256_cmpeq_epi8(v, cm)), _mm256_cmpeq_epi8(v, qt))) as u32;
            if m != 0 {
                let p = i + m.trailing_zeros() as usize;
                return (p, b[p]);
            }
            i += 32;
        }
    }
    while i < b.len() {
        let c = b[i];
        if c == b'\n' || c == b',' || c == b'"' {
            return (i, c);
        }
        i += 1;
    }
    (b.len(), b'\n')
}

fn data_lines<'a>(txt: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let Some(h) = txt.iter().position(|&b| b == b'\n') else { return };
    let body = &txt[h + 1..];
    k_ref::for_each_line(body, |s, e| f(&body[s..e]));
}

fn list(dir: &Path) -> Vec<(std::path::PathBuf, usize)> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { fallback("cannot list an input folder") };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if !name.as_encoded_bytes().ends_with(b".csv") {
            continue;
        }
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_file() {
            v.push((p, md.len() as usize));
        }
    }
    v
}

struct Lookups {
    prod: Vec<u8>,
    cats: Vec<Vec<u8>>,
    camp: Vec<u8>,
    channels: Vec<Vec<u8>>,
    nc: usize,
}

fn lookups(data: &Path) -> Lookups {
    #[cfg(feature = "tl")]
    crate::sys::tl::mark(15, 0);
    let rd = |name: &str| std::fs::read(data.join(name)).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let ptxt = rd("products.csv");
    #[cfg(feature = "tl")]
    crate::sys::tl::mark(15, 1);
    let mut prod = vec![255u8; PBOUND + 1];
    let mut ctab = [255u8; 64];
    let mut bad = false;
    let body: &[u8] = match ptxt.iter().position(|&b| b == b'\n') {
        Some(h) => &ptxt[h + 1..],
        None => &[],
    };
    let mut add = |n: usize, c: &[u8], bad: &mut bool, names: &mut Vec<Vec<u8>>| {
        if n >= PBOUND || prod[n] != 255 {
            *bad = true;
            return;
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &x in c {
            h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
        }
        let mut slot = (h >> 58) as usize;
        let ci = loop {
            let k = ctab[slot];
            if k == 255 {
                if names.len() >= 31 {
                    *bad = true;
                    return;
                }
                names.push(c.to_vec());
                ctab[slot] = (names.len() - 1) as u8;
                break names.len() - 1;
            }
            if names[k as usize] == c {
                break k as usize;
            }
            slot = (slot + 1) & 63;
        };
        prod[n] = ci as u8;
    };
    let mut nm: Vec<Vec<u8>> = Vec::new();
    let mut s = 0usize;
    while s < body.len() {
        let (e, stop) = find3(body, s);
        let mut fast = false;
        if stop == b',' && body[s] == b'p' && e - s >= 2 && e - s <= 9 && !(body[s + 1] == b'0' && e - s > 2) && body[s + 1..e].iter().all(|c| c.is_ascii_digit()) {
            let (e2, stop2) = find3(body, e + 1);
            if stop2 == b'\n' && e2 > e + 1 {
                let mut n = 0usize;
                for &c in &body[s + 1..e] {
                    n = n * 10 + (c - b'0') as usize;
                }
                add(n, &body[e + 1..e2], &mut bad, &mut nm);
                s = e2 + 1;
                fast = true;
            }
        }
        if !fast {
            let le = body[s..].iter().position(|&b| b == b'\n').map_or(body.len(), |p| s + p);
            let l = &body[s..le];
            match split2(l) {
                Some((p, c)) if !c.is_empty() => add(k_ref::parse_product(p) as usize, c, &mut bad, &mut nm),
                _ => bad = true,
            }
            s = le + 1;
        }
        if bad {
            break;
        }
    }
    let names: Vec<&[u8]> = nm.iter().map(|v| v.as_slice()).collect();
    if bad {
        fallback("products.csv is not canonical, or has too many categories");
    }
    let mut cats: Vec<Vec<u8>> = names.iter().map(|c| c.to_vec()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort_unstable();
    cats.dedup();
    if cats.len() > 31 {
        fallback("too many categories");
    }
    let unknown = cats.iter().position(|c| c == b"unknown").unwrap() as u8;
    let remap: Vec<u8> = names.iter().map(|c| cats.iter().position(|x| x == c).unwrap() as u8).collect();
    for p in prod.iter_mut() {
        *p = if *p == 255 { unknown } else { remap[*p as usize] };
    }
    #[cfg(feature = "tl")]
    crate::sys::tl::mark(15, 2);
    let ctxt = rd("campaigns.csv");
    let mut camp = vec![255u8; CBOUND];
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut nc = 0usize;
    let mut bad = false;
    data_lines(&ctxt, |l| match split2(l) {
        Some((c, ch)) if !ch.is_empty() => {
            let n = k_ref::parse_campaign(c) as usize;
            if n >= CBOUND || camp[n] != 255 {
                bad = true;
                return;
            }
            let ci = match channels.iter().position(|x| x == ch) {
                Some(i) => i,
                None => {
                    channels.push(ch.to_vec());
                    channels.len() - 1
                }
            };
            if ci >= 250 {
                bad = true;
                return;
            }
            camp[n] = ci as u8;
            nc = nc.max(n + 1);
        }
        _ => bad = true,
    });
    if bad {
        fallback("campaigns.csv is not canonical");
    }

    #[cfg(feature = "tl")]
    crate::sys::tl::mark(15, 3);
    Lookups { prod, cats, camp, channels, nc }
}

pub fn setup(data: &str, nt: usize, check: bool) -> G {
    let psetup = std::env::var("XL_PSETUP").map_or(PSETUP_DEF, |v| v == "1");
    if nt >= 2 && crate::sys::pool_size() + 1 >= nt && psetup {
        return setup_par(data, nt, check);
    }
    let data = Path::new(data);
    let rd = |name: &str| std::fs::read(data.join(name)).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let man = rd("manifest.json");
    let (Some(ws), Some(we)) = (manifest_ts(&man, b"\"window_start\""), manifest_ts(&man, b"\"window_end\"")) else { fallback("manifest") };
    if we <= ws || we - ws > 24 * DAY_MS || ws < 8 * DAY_MS {
        fallback("window");
    }
    crate::sys::WORKERS.store(true, std::sync::atomic::Ordering::SeqCst);
    let lk_handle = {
        let d = data.to_path_buf();
        std::thread::spawn(move || lookups(&d))
    };
    let mut all: Vec<(Kind, std::path::PathBuf, usize)> = Vec::new();
    for (kind, dir) in [(Kind::Ev, "events"), (Kind::Ck, "clicks"), (Kind::Lk, "links")] {
        for (p, len) in list(&data.join(dir)) {
            if len > 0 {
                all.push((kind, p, len));
            }
        }
    }
    all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    lap("s.list");
    let pg = 4096usize;
    let hpg = 2usize << 20;
    let total: usize = all.iter().map(|f| (f.2 + pg + hpg - 1) & !(hpg - 1)).sum::<usize>() + 2 * hpg;
    if total as u64 >= (1u64 << 40) {
        fallback("input too large");
    }
    let region = unsafe { mmap(std::ptr::null_mut(), total + hpg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
    if region as isize == -1 {
        fallback("mmap region");
    }
    let region = ((region as usize + hpg - 1) & !(hpg - 1)) as *mut u8;
    let mut files = Vec::with_capacity(all.len());
    let mut off = hpg;
    let (mut ev_bytes, mut ck_bytes, mut lk_bytes) = (0usize, 0usize, 0usize);
    for (kind, p, len) in &all {
        let Ok(f) = std::fs::File::open(p) else { fallback("open input") };
        let q = unsafe { mmap(region.add(off), *len, PROT_READ, MAP_PRIVATE | MAP_FIXED, f.as_raw_fd(), 0) };
        if q as isize == -1 {
            fallback("mmap input");
        }
        match kind {
            Kind::Ev => ev_bytes += len,
            Kind::Ck => ck_bytes += len,
            Kind::Lk => lk_bytes += len,
        }
        files.push(FileInfo { kind: *kind, off, len: *len, hdr: usize::MAX });
        unsafe { crate::parse::keep_fd(f) };
        off += (len + pg + hpg - 1) & !(hpg - 1);
    }
    crate::parse::fault_around_check(region, &files);
    lap("s.mmap");
    let total_bytes = ev_bytes + ck_bytes + lk_bytes;
    let ch = (total_bytes / (nt * 16)).clamp(64 << 10, 2 << 20);
    let mut queue = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let mut a = 1;
        while a < f.len {
            let b = (a + ch).min(f.len);
            queue.push(Chunk { file: i as u32, a, b });
            a = b;
        }
    }

    let mut sample = keyrec::Sample::default();
    for f in files.iter().filter(|f| f.kind == Kind::Ev) {
        if sample.events.len() >= 5000 {
            break;
        }
        let all = unsafe { std::slice::from_raw_parts(region.add(f.off), f.len) };
        let hdr = all.iter().position(|&b| b == b'\n').map_or(f.len, |i| i + 1);
        let bytes = &all[hdr..];
        let mut cut = bytes.len().min(600_000);
        if cut < bytes.len() {
            while cut > 0 && bytes[cut - 1] != b'\n' {
                cut -= 1;
            }
        }
        sample.add_event_lines(&bytes[..cut], 5000);
    }
    lap("s.sample");
    let mut st = keyrec::Stats::default();
    let envkey = |n: &str| if cfg!(feature = "testkeys") { std::env::var(n).ok().and_then(|v| u64::from_str_radix(&v, 16).ok()) } else { None };
    let (ukey, ekey) = match (envkey("XL_UKEY"), envkey("XL_EKEY")) {
        (Some(u), Some(e)) => (u, e),
        _ => {
            let Some(ukey) = keyrec::recover_user_key(&sample, &mut st) else { fallback("no user key") };
            let Some(ekey) = keyrec::recover_event_key(&sample, ukey, Some(ws), &mut st) else { fallback("no event key") };
            (ukey, ekey)
        }
    };
    if check {
        eprintln!("[v2b] keys: XL_UKEY={ukey:x} XL_EKEY={ekey:x}");
    }
    let mut ccap = click_cap(ck_bytes);
    let mut ckey = 0;
    if ccap != 0 {
        let mut cs = keyrec::Sample::default();
        if let Some(f) = files.iter().find(|f| f.kind == Kind::Ck) {
            cs.add_click_lines(sample_bytes(unsafe { std::slice::from_raw_parts(region.add(f.off), f.len) }), CK_SAMPLE);
        }
        match click_key(cs.clicks, ukey, ccap, &mut st) {
            Some(k) => ckey = k,
            None => ccap = 0,
        }
    }
    if check {
        eprintln!("[v2b] click key: XL_CKEY={ckey:x} ccap={ccap}");
    }

    lap("maps+keys");
    let Lookups { prod, cats, camp, channels, nc } = lk_handle.join().unwrap_or_else(|_| fallback("lookup thread"));
    lap("lookups");
    let ecap = ((ev_bytes / 64 + 4096 + BSIZE - 1) & !(BSIZE - 1)) as u64;
    let ucap = (((ecap as usize / 16 + 4096 + UBSIZE - 1) & !(UBSIZE - 1)) as u64).min(1 << 26);
    let nb = (ecap as usize) >> BB;
    let nub = (ucap as usize) >> UB;
    let blk = 2usize << 20;
    let ev_arena = Arena::new(ev_bytes / 28 * 16 + nt * nb * 4096 + nt * blk + (16 << 20));
    let us_arena = Arena::new((ck_bytes / 10 + lk_bytes / 6) * 16 + nt * nub * 4096 + nt * blk + (16 << 20));
    let row_cap = ev_bytes / 40 / nt * 2 + 65536;
    let nd = ((we - ws + ws.rem_euclid(DAY_MS) + DAY_MS - 1) / DAY_MS) as usize;
    let mut t = Vec::with_capacity(nt);
    crate::sys::ANON_TINY.store(true, std::sync::atomic::Ordering::Relaxed);
    for _ in 0..nt {
        t.push(UnsafeCell::new(TState {
            ev: parse::Part::empty(),
            us: parse::Part::empty(),
            raws: Vec::new(),
            rawbuf: Vec::new(),
            odd_users: Vec::new(),
            odd_events: Vec::new(),
            odd_clicks: Vec::new(),
            gen_edges: Vec::new(),
            cx_edges: Vec::new(),
            cx_users: Vec::new(),
            mixed_users: Vec::new(),
            slots: Vec::new(),
            edge: AVec::empty(),
            cx_raw: Vec::new(),
            cx_ev: (0..NSHARD).map(|_| Vec::new()).collect(),
            cx_ck: (0..NSHARD).map(|_| Vec::new()).collect(),
            ck: Vec::new(),
            runbuf: Vec::new(),
            pend: Vec::new(),
            cnt: Vec::new(),
            core: core::Core::new(0, 1, 16, 0, 0),
            n_ev_rows: 0,
            n_ck_rows: 0,
            n_lk_rows: 0,
            n_simple: 0,
            n_complex: 0,
            n_odd: 0,
            n_runs: 0,
            prof: [0; 8],
        }));
    }
    crate::sys::ANON_TINY.store(false, std::sync::atomic::Ordering::Relaxed);
    G {
        nt,
        check,
        ws,
        wlen: (we - ws) as u64,
        cbase: ws - 7 * DAY_MS,
        clen: (we - ws + 7 * DAY_MS) as u64,
        ing_base: ws - DAY_MS,
        ws_mod: ws.rem_euclid(DAY_MS) as u64,
        day0: ws.div_euclid(DAY_MS) as i32,
        nd,
        ukey,
        ekey,
        ckey,
        ccap,
        cpart: ckix_mode() == 2,
        ecap,
        ucap,
        prod,
        camp,
        nc,
        cats,
        channels,
        region,
        files,
        queue,
        qnext: AtomicUsize::new(0),
        ev_bytes,
        row_cap,
        nb,
        nub,
        ev_arena,
        us_arena,
        t,
        utab: anon(ucap as usize * 2) as *mut u16,
        claim: anon(ucap as usize / 8 + 64) as *const AtomicU64,
        csr_off: anon((ucap as usize + nub + 8) * 4) as *mut u32,
        csr: anon(csr_cap(ccap, ck_bytes, lk_bytes) * 16) as *mut [u64; 2],
        csr_cap: csr_cap(ccap, ck_bytes, lk_bytes),
        csr_base: Vec::new(),
        ncsr: 0,
        uf: links::Uf::new(),
        edges: anon(nb * std::mem::size_of::<EdgeInfo>()) as *mut EdgeInfo,
        joined: Vec::new(),
        cxu: Vec::new(),
        extra: std::collections::HashMap::new(),
        odd_kept: Vec::new(),
        next: AtomicUsize::new(0),
        st2b_done: std::sync::atomic::AtomicBool::new(false),
    }
}


const PSETUP_DEF: bool = crate::env_u32(option_env!("XL_PSETUP_DEF"), 1) == 1;

struct Slot<T> {
    v: UnsafeCell<Option<T>>,
    done: AtomicBool,
}
unsafe impl<T> Sync for Slot<T> {}
impl<T> Slot<T> {
    fn new() -> Slot<T> {
        Slot { v: UnsafeCell::new(None), done: AtomicBool::new(false) }
    }
    fn set(&self, v: T) {
        unsafe { *self.v.get() = Some(v) };
        self.done.store(true, Ordering::Release);
    }
    fn get(&self) -> &T {
        while !self.done.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        unsafe { (*self.v.get()).as_ref().unwrap() }
    }
    fn take(&self) -> T {
        self.get();
        unsafe { (*self.v.get()).take().unwrap() }
    }
}

fn run_tasks(nt: usize, n: usize, f: impl Fn(usize) + Sync) {
    let next = AtomicUsize::new(0);
    run_par(nt, |_| loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= n {
            break;
        }
        f(i);
    });
}

fn campaigns(data: &Path) -> (Vec<u8>, Vec<Vec<u8>>, usize) {
    let ctxt = std::fs::read(data.join("campaigns.csv")).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let mut camp = vec![255u8; CBOUND + 64];
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut nc = 0usize;
    let mut bad = false;
    data_lines(&ctxt, |l| match split2(l) {
        Some((c, ch)) if !ch.is_empty() => {
            let n = k_ref::parse_campaign(c) as usize;
            if n >= CBOUND || camp[n] != 255 {
                bad = true;
                return;
            }
            let ci = match channels.iter().position(|x| x == ch) {
                Some(i) => i,
                None => {
                    channels.push(ch.to_vec());
                    channels.len() - 1
                }
            };
            if ci >= 250 {
                bad = true;
                return;
            }
            camp[n] = ci as u8;
            nc = nc.max(n + 1);
        }
        _ => bad = true,
    });
    if bad {
        fallback("campaigns.csv is not canonical");
    }
    (camp, channels, nc)
}

fn products_part(body: &[u8]) -> Option<(Vec<(u32, u8)>, Vec<Vec<u8>>)> {
    let mut pairs: Vec<(u32, u8)> = Vec::with_capacity(body.len() / 16 + 16);
    let mut nm: Vec<Vec<u8>> = Vec::new();
    let mut ctab = [255u8; 64];
    let mut add = |n: usize, c: &[u8], nm: &mut Vec<Vec<u8>>, pairs: &mut Vec<(u32, u8)>| -> bool {
        if n >= PBOUND {
            return false;
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &x in c {
            h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
        }
        let mut slot = (h >> 58) as usize;
        let ci = loop {
            let k = ctab[slot];
            if k == 255 {
                if nm.len() >= 31 {
                    return false;
                }
                nm.push(c.to_vec());
                ctab[slot] = (nm.len() - 1) as u8;
                break nm.len() - 1;
            }
            if nm[k as usize] == c {
                break k as usize;
            }
            slot = (slot + 1) & 63;
        };
        pairs.push((n as u32, ci as u8));
        true
    };
    let mut s = 0usize;
    while s < body.len() {
        let (e, stop) = find3(body, s);
        let mut fast = false;
        if stop == b',' && body[s] == b'p' && e - s >= 2 && e - s <= 9 && !(body[s + 1] == b'0' && e - s > 2) && body[s + 1..e].iter().all(|c| c.is_ascii_digit()) {
            let (e2, stop2) = find3(body, e + 1);
            if stop2 == b'\n' && e2 > e + 1 {
                let mut n = 0usize;
                for &c in &body[s + 1..e] {
                    n = n * 10 + (c - b'0') as usize;
                }
                if !add(n, &body[e + 1..e2], &mut nm, &mut pairs) {
                    return None;
                }
                s = e2 + 1;
                fast = true;
            }
        }
        if !fast {
            let le = body[s..].iter().position(|&b| b == b'\n').map_or(body.len(), |p| s + p);
            let l = &body[s..le];
            match split2(l) {
                Some((p, c)) if !c.is_empty() => {
                    if !add(k_ref::parse_product(p) as usize, c, &mut nm, &mut pairs) {
                        return None;
                    }
                }
                _ => return None,
            }
            s = le + 1;
        }
    }
    Some((pairs, nm))
}

fn products_merge(parts: &[Option<(Vec<(u32, u8)>, Vec<Vec<u8>>)>]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut maps: Vec<Vec<u8>> = Vec::with_capacity(parts.len());
    for p in parts {
        let Some((_, nm)) = p else { fallback("products.csv is not canonical, or has too many categories") };
        let mut m = Vec::with_capacity(nm.len());
        for c in nm {
            let i = match names.iter().position(|x| x == c) {
                Some(i) => i,
                None => {
                    if names.len() >= 31 {
                        fallback("products.csv is not canonical, or has too many categories");
                    }
                    names.push(c.clone());
                    names.len() - 1
                }
            };
            m.push(i as u8);
        }
        maps.push(m);
    }
    let mut cats: Vec<Vec<u8>> = names.clone();
    cats.push(b"unknown".to_vec());
    cats.sort_unstable();
    cats.dedup();
    if cats.len() > 31 {
        fallback("too many categories");
    }
    let unknown = cats.iter().position(|c| c == b"unknown").unwrap() as u8;
    let remap: Vec<u8> = names.iter().map(|c| cats.iter().position(|x| x == c).unwrap() as u8).collect();
    let mut prod = vec![255u8; PBOUND + 1 + 64];
    for (p, m) in parts.iter().zip(&maps) {
        let (pairs, _) = p.as_ref().unwrap();
        for &(n, ci) in pairs {
            let slot = &mut prod[n as usize];
            if *slot != 255 {
                fallback("products.csv is not canonical, or has too many categories");
            }
            *slot = remap[m[ci as usize] as usize];
        }
    }
    for p in prod[..PBOUND + 1].iter_mut() {
        if *p == 255 {
            *p = unknown;
        }
    }
    for p in prod[PBOUND + 1..].iter_mut() {
        *p = 0;
    }
    (prod, cats)
}

fn map_file(p: &Path) -> (usize, usize) {
    let Ok(f) = std::fs::File::open(p) else { fallback("cannot read a lookup file") };
    let Ok(md) = f.metadata() else { fallback("cannot read a lookup file") };
    let n = md.len() as usize;
    if n == 0 {
        return (8, 0);
    }
    let q = unsafe { mmap(std::ptr::null_mut(), n, PROT_READ, MAP_PRIVATE, f.as_raw_fd(), 0) };
    if q as isize == -1 {
        fallback("cannot read a lookup file");
    }
    (q as usize, n)
}

fn piece(buf: &[u8], j: usize, k: usize) -> (usize, usize) {
    let at = |x: usize| -> usize {
        if x == 0 {
            return 0;
        }
        if x >= buf.len() {
            return buf.len();
        }
        match buf[x - 1..].iter().position(|&b| b == b'\n') {
            Some(i) => x + i,
            None => buf.len(),
        }
    };
    (at(buf.len() * j / k), at(buf.len() * (j + 1) / k))
}

const CK_SAMPLE: usize = 4000;
fn click_cap(ck_bytes: usize) -> u64 {
    let cap = ((ck_bytes / 20 + 4096 + 63) & !63) as u64;
    if cap >= (1 << 31) || ckix_mode() == 0 {
        return 0;
    }
    cap
}
fn ckix_mode() -> u32 {
    std::env::var("XL_CKIX").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(crate::env_u32(option_env!("XL_CKIX_DEF"), 0))
}
fn csr_cap(ccap: u64, ck_bytes: usize, lk_bytes: usize) -> usize {
    if ccap != 0 {
        ccap as usize + 64
    } else {
        ck_bytes / 10 + lk_bytes / 6 + 64
    }
}
fn click_key(rows: Vec<keyrec::ClickRow>, ukey: u64, ccap: u64, st: &mut keyrec::Stats) -> Option<u64> {
    if ccap == 0 {
        return None;
    }
    let envkey = if cfg!(feature = "testkeys") { std::env::var("XL_CKEY").ok().and_then(|v| u64::from_str_radix(&v, 16).ok()) } else { None };
    if envkey.is_some() {
        return envkey;
    }
    let s = keyrec::Sample { events: Vec::new(), clicks: rows };
    keyrec::recover_click_key_lim(&s, ukey, ccap, st)
}

fn sample_bytes(all: &[u8]) -> &[u8] {
    let hdr = all.iter().position(|&b| b == b'\n').map_or(all.len(), |i| i + 1);
    let bytes = &all[hdr..];
    let mut cut = bytes.len().min(600_000);
    if cut < bytes.len() {
        while cut > 0 && bytes[cut - 1] != b'\n' {
            cut -= 1;
        }
    }
    &bytes[..cut]
}

pub fn setup_par(data: &str, nt: usize, check: bool) -> G {
    let data = Path::new(data);
    let rd = |name: &str| std::fs::read(data.join(name)).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let man = rd("manifest.json");
    let (Some(ws), Some(we)) = (manifest_ts(&man, b"\"window_start\""), manifest_ts(&man, b"\"window_end\"")) else { fallback("manifest") };
    if we <= ws || we - ws > 24 * DAY_MS || ws < 8 * DAY_MS {
        fallback("window");
    }
    crate::sys::WORKERS.store(true, Ordering::SeqCst);
    let nd = ((we - ws + ws.rem_euclid(DAY_MS) + DAY_MS - 1) / DAY_MS) as usize;
    let (ws_mod, day0) = (ws.rem_euclid(DAY_MS) as u64, ws.div_euclid(DAY_MS) as i32);
    let kp = match crate::sys::SETUP_KP.load(Ordering::Relaxed) {
        0 => (2 * nt).clamp(2, 16),
        k => k.clamp(1, 64),
    };
    let ks = nt.clamp(2, 8);
    let camps: Slot<(Vec<u8>, Vec<Vec<u8>>, usize)> = Slot::new();
    let ptxt: Slot<(usize, usize)> = Slot::new();
    let pparts: Vec<Slot<Option<(Vec<(u32, u8)>, Vec<Vec<u8>>)>>> = (0..kp).map(|_| Slot::new()).collect();
    let lookups: Slot<(Vec<u8>, Vec<Vec<u8>>)> = Slot::new();
    let rowcap: Slot<usize> = Slot::new();
    let cores: Vec<Slot<core::Core>> = (0..nt).map(|_| Slot::new()).collect();
    let sbytes: Slot<(usize, usize)> = Slot::new();
    let spieces: Vec<Slot<keyrec::Sample>> = (0..ks).map(|_| Slot::new()).collect();
    let snext = AtomicUsize::new(0);
    let cbytes: Slot<(usize, usize)> = Slot::new();
    let crows: Slot<Vec<keyrec::ClickRow>> = Slot::new();
    let ctaken = AtomicBool::new(false);
    let click_sample = || {
        if cbytes.done.load(Ordering::Acquire) && !ctaken.swap(true, Ordering::AcqRel) {
            let (p, n) = *cbytes.get();
            let mut s = keyrec::Sample::default();
            s.add_click_lines(unsafe { std::slice::from_raw_parts(p as *const u8, n) }, CK_SAMPLE);
            crows.set(s.clicks);
        }
    };
    let sample_piece = |j: usize| {
        let (p, n) = *sbytes.get();
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, n) };
        let (a, b) = piece(bytes, j, ks);
        #[cfg(feature = "tl")]
        let t0 = crate::sys::tl::now();
        let mut s = keyrec::Sample::default();
        s.add_event_lines(&bytes[a..b], 5000);
        spieces[j].set(s);
        #[cfg(feature = "tl")]
        unsafe {
            crate::sys::tl::TASK[48 + j.min(15)] = (t0, crate::sys::tl::now(), 0);
        }
    };
    let upart = crate::sys::SETUP_PARTS.load(Ordering::Relaxed);
    let npt = if upart { 1 } else { 0 };
    let (q_p, q_ord, q_merge, q_part) = (2, 2 + kp, 3 + kp, 4 + kp);
    let q_core = q_part + npt;
    let nq = q_core + nt;
    let nbs: Slot<(usize, usize)> = Slot::new();
    let parts: Vec<Slot<(crate::parse::Part, crate::parse::Part)>> = (0..if upart { nt } else { 0 }).map(|_| Slot::new()).collect();
    let qnext = AtomicUsize::new(0);
    let task_inner = |i: usize| {
        if i == 1 {
            camps.set(campaigns(data));
        } else if i == 0 {
            ptxt.set(map_file(&data.join("products.csv")));
        } else if i < q_ord {
            let &(p, n) = ptxt.get();
            let t = unsafe { std::slice::from_raw_parts(p as *const u8, n) };
            let body: &[u8] = match t.iter().position(|&b| b == b'\n') {
                Some(h) => &t[h + 1..],
                None => &[],
            };
            let (a, b) = piece(body, i - q_p, kp);
            pparts[i - q_p].set(products_part(&body[a..b]));
        } else if i == q_ord {
            if crate::parse::pin_mode() != 0 {
                crate::parse::cpu_order();
            }
        } else if i >= q_core {
            let nc = camps.get().2;
            cores[i - q_core].set(core::Core::new(nc, nd, *rowcap.get(), ws_mod, day0));
        } else if i == q_part {
            let &(nb, nub) = nbs.get();
            for p in &parts {
                p.set((crate::parse::Part::new_unbound(nb), crate::parse::Part::new_unbound(nub)));
            }
        } else if i == q_merge {
            let parts: Vec<_> = pparts.iter().map(|p| p.take()).collect();
            let (prod, cats) = products_merge(&parts);
            unsafe { crate::parse::init_tabs_shared(prod.as_ptr(), prod.len(), camps.get().0.as_ptr(), camps.get().0.len()) };
            lookups.set((prod, cats));
        }
    };
    let task = |i: usize| {
        #[cfg(feature = "tl")]
        let t0 = crate::sys::tl::now();
        task_inner(i);
        #[cfg(feature = "tl")]
        unsafe {
            crate::sys::tl::TASK[i.min(47)] = (t0, crate::sys::tl::now(), 0);
            crate::sys::tl::STASK[i.min(63)] = (t0, crate::sys::tl::now());
        }
    };
    let help = || {
        click_sample();
        if sbytes.done.load(Ordering::Acquire) {
            loop {
                let j = snext.fetch_add(1, Ordering::Relaxed);
                if j >= ks {
                    break;
                }
                sample_piece(j);
            }
        }
        let i = qnext.fetch_add(1, Ordering::Relaxed);
        if i < nq {
            task(i);
            true
        } else {
            false
        }
    };
    let chain: Slot<(std::ptr::NonNull<u8>, Vec<FileInfo>, Vec<Chunk>, [usize; 3], u64, u64)> = Slot::new();
    let keys: Slot<(u64, u64, u64, u64)> = Slot::new();
    run_par(nt, |tid| {
        if tid != 0 {
            while help() {}
            while !(sbytes.done.load(Ordering::Acquire) && cbytes.done.load(Ordering::Acquire)) && !keys.done.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            while help() {}
            return;
        }
        let mut all: Vec<(Kind, std::path::PathBuf, usize)> = Vec::new();
        for (kind, dir) in [(Kind::Ev, "events"), (Kind::Ck, "clicks"), (Kind::Lk, "links")] {
            for (p, len) in list(&data.join(dir)) {
                if len > 0 {
                    all.push((kind, p, len));
                }
            }
        }
        all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        let ev_bytes: usize = all.iter().filter(|f| f.0 == Kind::Ev).map(|f| f.2).sum();
        rowcap.set(ev_bytes / 40 / nt * 2 + 65536);
        nbs.set(part_counts(ev_bytes));
        lap("s.list");
        let pg = 4096usize;
        let hpg = 2usize << 20;
        let total: usize = all.iter().map(|f| (f.2 + pg + hpg - 1) & !(hpg - 1)).sum::<usize>() + 2 * hpg;
        if total as u64 >= (1u64 << 40) {
            fallback("input too large");
        }
        let region = unsafe { mmap(std::ptr::null_mut(), total + hpg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
        if region as isize == -1 {
            fallback("mmap region");
        }
        let region = ((region as usize + hpg - 1) & !(hpg - 1)) as *mut u8;
        unsafe { crate::parse::fds_init(all.len()) };
        let ckix = ckix_mode() != 0;
        let mut files = Vec::with_capacity(all.len());
        let mut off = hpg;
        let mut bytes = [0usize; 3];
        for (i, (kind, p, len)) in all.iter().enumerate() {
            let Ok(f) = std::fs::File::open(p) else { fallback("open input") };
            let q = unsafe { mmap(region.add(off), *len, PROT_READ, MAP_PRIVATE | MAP_FIXED, f.as_raw_fd(), 0) };
            if q as isize == -1 {
                fallback("mmap input");
            }
            bytes[*kind as usize] += len;
            files.push(FileInfo { kind: *kind, off, len: *len, hdr: usize::MAX });
            unsafe { crate::parse::set_fd(i, f) };
            off += (len + pg + hpg - 1) & !(hpg - 1);
            if *kind == Kind::Ev && !sbytes.done.load(Ordering::Relaxed) {
                let b = sample_bytes(unsafe { std::slice::from_raw_parts(region.add(files[i].off), *len) });
                sbytes.set((b.as_ptr() as usize, b.len()));
            }
            if *kind == Kind::Ck && !cbytes.done.load(Ordering::Relaxed) {
                if ckix {
                    let b = sample_bytes(unsafe { std::slice::from_raw_parts(region.add(files[i].off), *len) });
                    cbytes.set((b.as_ptr() as usize, b.len()));
                } else {
                    cbytes.set((region as usize, 0));
                }
            }
        }
        crate::parse::fault_around_check(region, &files);
        lap("s.mmap");
        if !sbytes.done.load(Ordering::Relaxed) {
            sbytes.set((region as usize, 0));
        }
        if !cbytes.done.load(Ordering::Relaxed) {
            cbytes.set((region as usize, 0));
        }
        loop {
            let j = snext.fetch_add(1, Ordering::Relaxed);
            if j >= ks {
                break;
            }
            sample_piece(j);
        }
        let mut sample = keyrec::Sample::default();
        for p in &spieces {
            sample.events.extend(p.take().events);
        }
        sample.events.truncate(5000);
        for f in files.iter().filter(|f| f.kind == Kind::Ev).skip(1) {
            if sample.events.len() >= 5000 {
                break;
            }
            sample.add_event_lines(sample_bytes(unsafe { std::slice::from_raw_parts(region.add(f.off), f.len) }), 5000);
        }
        lap("s.sample");
        let mut st = keyrec::Stats::default();
        let envkey = |n: &str| if cfg!(feature = "testkeys") { std::env::var(n).ok().and_then(|v| u64::from_str_radix(&v, 16).ok()) } else { None };
        let k = match (envkey("XL_UKEY"), envkey("XL_EKEY")) {
            (Some(u), Some(e)) => (u, e),
            _ => {
                let Some(ukey) = keyrec::recover_user_key(&sample, &mut st) else { fallback("no user key") };
                let Some(ekey) = keyrec::recover_event_key(&sample, ukey, Some(ws), &mut st) else { fallback("no event key") };
                (ukey, ekey)
            }
        };
        click_sample();
        let mut ccap = click_cap(bytes[Kind::Ck as usize]);
        let mut ckey = 0;
        match click_key(crows.take(), k.0, ccap, &mut st) {
            Some(x) => ckey = x,
            None => ccap = 0,
        }
        keys.set((k.0, k.1, ckey, ccap));
        lap("maps+keys");
        let total_bytes = bytes[0] + bytes[1] + bytes[2];
        let ch = (total_bytes / (nt * 16)).clamp(64 << 10, 2 << 20);
        let mut queue = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let mut a = 1;
            while a < f.len {
                let b = (a + ch).min(f.len);
                queue.push(Chunk { file: i as u32, a, b });
                a = b;
            }
        }
        chain.set((std::ptr::NonNull::new(region).unwrap(), files, queue, bytes, 0, 0));
        #[cfg(feature = "tl")]
        unsafe {
            crate::sys::tl::CHAIN_END = crate::sys::tl::now();
        }
        while help() {}
    });
    lap("lookups");
    let (region, files, queue, bytes, _, _) = chain.take();
    let region = region.as_ptr() as *const u8;
    let [ev_bytes, ck_bytes, lk_bytes] = bytes;
    let (ukey, ekey, ckey, ccap) = keys.take();
    if check {
        eprintln!("[v2b] keys: XL_UKEY={ukey:x} XL_EKEY={ekey:x}");
        eprintln!("[v2b] click key: XL_CKEY={ckey:x} ccap={ccap}");
    }
    let (camp, channels, nc) = camps.take();
    let (prod, cats) = lookups.take();
    let row_cap = *rowcap.get();
    let ecap = ((ev_bytes / 64 + 4096 + BSIZE - 1) & !(BSIZE - 1)) as u64;
    let ucap = (((ecap as usize / 16 + 4096 + UBSIZE - 1) & !(UBSIZE - 1)) as u64).min(1 << 26);
    let nb = (ecap as usize) >> BB;
    let nub = (ucap as usize) >> UB;
    let blk = 2usize << 20;
    let ev_arena = Arena::new(ev_bytes / 28 * 16 + nt * nb * 4096 + nt * blk + (16 << 20));
    let us_arena = Arena::new((ck_bytes / 10 + lk_bytes / 6) * 16 + nt * nub * 4096 + nt * blk + (16 << 20));
    let mut t = Vec::with_capacity(nt);
    for c in &cores {
        t.push(UnsafeCell::new(tstate(c.take())));
    }
    crate::parse::CORE_READY.store(true, Ordering::Relaxed);
    if upart && *nbs.get() == (nb, nub) {
        for (tid, p) in parts.iter().enumerate() {
            let (ev, us) = p.take();
            let ts = t[tid].get_mut();
            ts.ev = ev;
            ts.us = us;
        }
        crate::parse::PARTS_READY.store(true, Ordering::Relaxed);
    }
    G {
        nt,
        check,
        ws,
        wlen: (we - ws) as u64,
        cbase: ws - 7 * DAY_MS,
        clen: (we - ws + 7 * DAY_MS) as u64,
        ing_base: ws - DAY_MS,
        ws_mod,
        day0,
        nd,
        ukey,
        ekey,
        ckey,
        ccap,
        cpart: ckix_mode() == 2,
        ecap,
        ucap,
        prod,
        camp,
        nc,
        cats,
        channels,
        region,
        files,
        queue,
        qnext: AtomicUsize::new(0),
        ev_bytes,
        row_cap,
        nb,
        nub,
        ev_arena,
        us_arena,
        t,
        utab: anon(ucap as usize * 2) as *mut u16,
        claim: anon(ucap as usize / 8 + 64) as *const AtomicU64,
        csr_off: anon((ucap as usize + nub + 8) * 4) as *mut u32,
        csr: anon(csr_cap(ccap, ck_bytes, lk_bytes) * 16) as *mut [u64; 2],
        csr_cap: csr_cap(ccap, ck_bytes, lk_bytes),
        csr_base: Vec::new(),
        ncsr: 0,
        uf: links::Uf::new(),
        edges: anon(nb * std::mem::size_of::<EdgeInfo>()) as *mut EdgeInfo,
        joined: Vec::new(),
        cxu: Vec::new(),
        extra: std::collections::HashMap::new(),
        odd_kept: Vec::new(),
        next: AtomicUsize::new(0),
        st2b_done: std::sync::atomic::AtomicBool::new(false),
    }
}

fn part_counts(ev_bytes: usize) -> (usize, usize) {
    let ecap = (ev_bytes / 64 + 4096 + BSIZE - 1) & !(BSIZE - 1);
    let ucap = ((ecap / 16 + 4096 + UBSIZE - 1) & !(UBSIZE - 1)).min(1 << 26);
    (ecap >> BB, ucap >> UB)
}

fn tstate(core: core::Core) -> TState {
    TState {
        ev: parse::Part::empty(),
        us: parse::Part::empty(),
        raws: Vec::new(),
        rawbuf: Vec::new(),
        odd_users: Vec::new(),
        odd_events: Vec::new(),
        odd_clicks: Vec::new(),
        gen_edges: Vec::new(),
        cx_edges: Vec::new(),
        cx_users: Vec::new(),
        mixed_users: Vec::new(),
        slots: Vec::new(),
        edge: AVec::empty(),
        cx_raw: Vec::new(),
        cx_ev: (0..NSHARD).map(|_| Vec::new()).collect(),
        cx_ck: (0..NSHARD).map(|_| Vec::new()).collect(),
        ck: Vec::new(),
        runbuf: Vec::new(),
        pend: Vec::new(),
        cnt: Vec::new(),
        core,
        n_ev_rows: 0,
        n_ck_rows: 0,
        n_lk_rows: 0,
        n_simple: 0,
        n_complex: 0,
        n_odd: 0,
        n_runs: 0,
        prof: [0; 8],
    }
}
