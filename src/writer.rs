use crate::pqw::*;
use crate::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn die(what: &str) -> ! {
    eprintln!("[engine] write error: {what}");
    unsafe { sys::_exit(1) }
}

#[derive(Clone, Copy)]
struct View {
    keys: *const u32,
    day: *const i32,
    ses: *const i32,
    evs: *const i32,
    rev: *const i32,
    cat: *const u8,
    names: *const u8,
    nnames: usize,
    noff: *const u32,
    nnoff: usize,
    rows: usize,
}
unsafe impl Send for View {}
unsafe impl Sync for View {}
fn view(c: &crate::core::Core) -> View {
    View {
        keys: c.keys.as_ptr(),
        day: c.day.as_ptr(),
        ses: c.ses.as_ptr(),
        evs: c.evs.as_ptr(),
        rev: c.rev.as_ptr(),
        cat: c.cat.as_ptr(),
        names: c.names.as_ptr(),
        nnames: c.names.len(),
        noff: c.names_off.as_ptr(),
        nnoff: c.names_off.len(),
        rows: c.day.len(),
    }
}

fn write_piece(g: &G, t: usize, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], kbuf: &mut Vec<u32>) {
    write_rows(&view(&g.t(t).core), a, b, path, cats_off, cats_data, kbuf)
}

#[derive(Default)]
struct Split {
    k: Vec<u32>,
    d: Vec<i32>,
    s: Vec<i32>,
    e: Vec<i32>,
    r: Vec<i32>,
    c: Vec<u8>,
    b: Vec<u8>,
}
thread_local! {
    static SPLIT: std::cell::RefCell<Split> = std::cell::RefCell::new(Split::default());
    static UFILE: std::cell::RefCell<Option<UserFile>> = const { std::cell::RefCell::new(None) };
}

fn end_user_file() {
    if let Some(f) = UFILE.with(|f| f.borrow_mut().take()) {
        if f.finish().is_err() {
            die("user_daily");
        }
    }
}

const SPLIT_ROWS: bool = crate::env_u32(option_env!("XL_SPLIT_DEF"), 1) != 0;

#[allow(clippy::too_many_arguments)]
unsafe fn split_rows(keys: *mut u32, day: *mut i32, ses: *mut i32, evs: *mut i32, rev: *mut i32, cat: *mut u8, n: usize, tk: *mut u32, td: *mut i32, ts: *mut i32, te: *mut i32, tr: *mut i32, tc: *mut u8) -> (usize, usize) {
    use std::arch::x86_64::*;
    let (mut iz, mut ip, mut i) = (0usize, 0usize, 0usize);
    let zero = _mm256_setzero_si256();
    let ff = _mm_set1_epi8(-1);
    type V = __m256i;
    while i + 8 <= n {
        let k = _mm256_loadu_si256(keys.add(i) as *const V);
        let d = _mm256_loadu_si256(day.add(i) as *const V);
        let s = _mm256_loadu_si256(ses.add(i) as *const V);
        let e = _mm256_loadu_si256(evs.add(i) as *const V);
        let r = _mm256_loadu_si256(rev.add(i) as *const V);
        let c = _mm_loadl_epi64(cat.add(i) as *const __m128i);
        let z: u8 = _mm256_cmpeq_epi32_mask(r, zero) & (_mm_cmpeq_epi8_mask(c, ff) as u8);
        let p: u8 = !z;
        _mm256_storeu_si256(keys.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, k));
        _mm256_storeu_si256(day.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, d));
        _mm256_storeu_si256(ses.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, s));
        _mm256_storeu_si256(evs.add(iz) as *mut V, _mm256_maskz_compress_epi32(z, e));
        _mm256_storeu_si256(tk.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, k));
        _mm256_storeu_si256(td.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, d));
        _mm256_storeu_si256(ts.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, s));
        _mm256_storeu_si256(te.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, e));
        _mm256_storeu_si256(tr.add(ip) as *mut V, _mm256_maskz_compress_epi32(p, r));
        let c32 = _mm256_maskz_compress_epi32(p, _mm256_cvtepu8_epi32(c));
        _mm_storel_epi64(tc.add(ip) as *mut __m128i, _mm256_cvtepi32_epi8(c32));
        iz += z.count_ones() as usize;
        ip += p.count_ones() as usize;
        i += 8;
    }
    while i < n {
        let (k, d, se, e, r, c) = (*keys.add(i), *day.add(i), *ses.add(i), *evs.add(i), *rev.add(i), *cat.add(i));
        let z = (r == 0 && c == u8::MAX) as usize;
        *keys.add(iz) = k;
        *day.add(iz) = d;
        *ses.add(iz) = se;
        *evs.add(iz) = e;
        *tk.add(ip) = k;
        *td.add(ip) = d;
        *ts.add(ip) = se;
        *te.add(ip) = e;
        *tr.add(ip) = r;
        *tc.add(ip) = c;
        iz += z;
        ip += 1 - z;
        i += 1;
    }
    (iz, ip)
}

fn write_split(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8]) {
    let mut x = a;
    while x < b {
        let y = if b - x > SUB_ROWS + SUB_ROWS / 2 { x + SUB_ROWS } else { b };
        write_split_group(v, x, y, path, cats_off, cats_data);
        x = y;
    }
}

const SUB_ROWS: usize = crate::env_u32(option_env!("XL_SUB_DEF"), 4096) as usize;

fn write_split_group(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8]) {
    use std::slice::from_raw_parts as sl;
    let n = b - a;
    SPLIT.with(|s| {
        let s = &mut *s.borrow_mut();
        macro_rules! fit {
            ($($f:ident)*) => { $( s.$f.clear(); s.$f.reserve(n + 16); )* };
        }
        fit!(k d s e r c);
        let head = unsafe {
            let (keys, day, ses, evs, rev, cat) = (v.keys.add(a) as *mut u32, v.day.add(a) as *mut i32, v.ses.add(a) as *mut i32, v.evs.add(a) as *mut i32, v.rev.add(a) as *mut i32, v.cat.add(a) as *mut u8);
            let (iz, ip) = split_rows(keys, day, ses, evs, rev, cat, n, s.k.as_mut_ptr(), s.d.as_mut_ptr(), s.s.as_mut_ptr(), s.e.as_mut_ptr(), s.r.as_mut_ptr(), s.c.as_mut_ptr());
            debug_assert_eq!(iz + ip, n);
            std::ptr::write_bytes(rev, 0, iz);
            std::ptr::write_bytes(cat, u8::MAX, iz);
            std::ptr::copy_nonoverlapping(s.k.as_ptr(), keys.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.d.as_ptr(), day.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.s.as_ptr(), ses.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.e.as_ptr(), evs.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.r.as_ptr(), rev.add(iz), ip);
            std::ptr::copy_nonoverlapping(s.c.as_ptr(), cat.add(iz), ip);
            iz
        };
    let u = unsafe {
        UserCols {
            names: StrCol { offsets: sl(v.noff, v.nnoff), data: sl(v.names, v.nnames) },
            keys: sl(v.keys.add(a), n),
            day: sl(v.day.add(a), n),
            ses: sl(v.ses.add(a), n),
            evs: sl(v.evs.add(a), n),
            rev: sl(v.rev.add(a), n),
            cat: sl(v.cat.add(a), n),
            cats: StrCol { offsets: cats_off, data: cats_data },
            head,
        }
    };
    UFILE.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_none() {
            match UserFile::create(path, 1 << 20) {
                Ok(x) => *f = Some(x),
                Err(_) => die("user_daily"),
            }
        }
        if f.as_mut().unwrap().add(&u, &mut s.b).is_err() {
            die("user_daily");
        }
    });
    });
}

fn write_rows(v: &View, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], kbuf: &mut Vec<u32>) {
    use std::slice::from_raw_parts as sl;
    let n = b - a;
    if SPLIT_ROWS && n > 0 && RPG == 0 {
        return write_split(v, a, b, path, cats_off, cats_data);
    }
    let keys = unsafe { sl(v.keys.add(a), n) };
    let _ = &kbuf;
    let (names, noff) = unsafe { (sl(v.names, v.nnames), sl(v.noff, v.nnoff)) };
    let (no, lo): (&[u32], u32) = if a == b { (&[], 0) } else { (noff, 0) };
    let cols = unsafe {
        [
            Column { name: "user_id", data: Data::Dict { dict: StrCol { offsets: no, data: names }, keys: Keys::U32Off(keys, lo), nullable: false, plain: false } },
            Column { name: "day", data: Data::Date(sl(v.day.add(a), n), IntEnc::Dense) },
            Column { name: "sessions", data: Data::I32(sl(v.ses.add(a), n), IntEnc::Dense) },
            Column { name: "events", data: Data::I32(sl(v.evs.add(a), n), IntEnc::Auto) },
            Column { name: "revenue_cents", data: Data::I32(sl(v.rev.add(a), n), IntEnc::Plain) },
            Column { name: "top_category", data: Data::Dict { dict: StrCol { offsets: cats_off, data: cats_data }, keys: Keys::U8(sl(v.cat.add(a), n)), nullable: true, plain: false } },
        ]
    };
    let opts = if RPG == 0 { Opts::default() } else { Opts { rows_per_group: RPG } };
    if write_file(path, &cols, &opts, Method::Stream(1 << 20)).is_err() {
        die("user_daily");
    }
}

fn write_campaign(g: &G, dir: &Path) {
    let nd = g.nd;
    let n = (g.nc + 1) * nd;
    let mut agg = vec![[0u64; 4]; n];
    for tid in 0..g.nt {
        let c = &g.t(tid).core;
        for i in 0..n {
            agg[i][0] += c.agg[i][0] as u64;
            agg[i][3] += c.agg[i][1] as u64;
            agg[i][1] += c.aggp[i][0];
            agg[i][2] += c.aggp[i][1];
        }
    }
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][4] = crate::sys::tl::now();
    }
    let (mut id_off, mut id_data, mut ch_off, mut ch_data) = (vec![0u32], Vec::new(), vec![0u32], Vec::new());
    let mut day = Vec::new();
    let mut v: [Vec<i64>; 4] = Default::default();
    for cn in 0..=g.nc {
        for d in 0..nd {
            let a = agg[cn * nd + d];
            if a[0] == 0 && a[3] == 0 {
                continue;
            }
            if cn == g.nc {
                id_data.extend_from_slice(b"organic");
                ch_data.extend_from_slice(b"organic");
            } else {
                let mut d = [0u8; 24];
                let (mut i, mut x) = (d.len(), cn);
                loop {
                    i -= 1;
                    d[i] = b'0' + (x % 10) as u8;
                    x /= 10;
                    if x == 0 {
                        break;
                    }
                }
                i -= 2;
                d[i] = b'c';
                d[i + 1] = b'_';
                id_data.extend_from_slice(&d[i..]);
                ch_data.extend_from_slice(&g.channels[g.camp[cn] as usize]);
            }
            id_off.push(id_data.len() as u32);
            ch_off.push(ch_data.len() as u32);
            day.push(g.day0 + d as i32);
            for k in 0..4 {
                v[k].push(a[k] as i64);
            }
        }
    }
    let empty: &[u32] = &[];
    let rows = day.len();
    let cols = [
        Column { name: "campaign_id", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &id_off }, data: &id_data }) },
        Column { name: "channel", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &ch_off }, data: &ch_data }) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&v[0], IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&v[1], IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&v[2], IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&v[3], IntEnc::Plain) },
    ];
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][5] = crate::sys::tl::now();
    }
    if write_file(&dir.join("part-0.parquet"), &cols, &Opts::default(), Method::Whole).is_err() {
        die("campaign_daily");
    }
    #[cfg(feature = "tl")]
    unsafe {
        crate::sys::tl::MK[15][6] = crate::sys::tl::now();
    }
    if g.check {
        let (mut us, mut ur) = (0i64, 0i64);
        for tid in 0..g.nt {
            let c = &g.t(tid).core;
            us += c.ses.as_slice().iter().map(|&x| x as i64).sum::<i64>();
            ur += c.rev.as_slice().iter().map(|&x| x as i64).sum::<i64>();
        }
        let cs: i64 = v[0].iter().sum();
        let cr: i64 = v[2].iter().sum();
        let ca: i64 = v[3].iter().sum();
        let organic: i64 = (0..nd).map(|d| agg[g.nc * nd + d][0] as i64).sum();
        eprintln!("[engine] check: sessions user={us} campaign={cs} revenue user={ur} campaign={cr} assisted={ca} credited_non_organic={}", cs - organic);
        if us != cs || ur != cr || ca < cs - organic {
            eprintln!("[engine] CHECK FAILED");
            unsafe { sys::_exit(5) }
        }
    }
}

fn write_campaign_part(g: &G, path: &Path, lo: usize, hi: usize) -> [i64; 4] {
    let nd = g.nd;
    let n = (hi - lo) * nd;
    let mut agg = vec![[0u64; 4]; n];
    for tid in 0..g.nt {
        let c = &g.t(tid).core;
        let base = lo * nd;
        for i in 0..n {
            agg[i][0] += c.agg[base + i][0] as u64;
            agg[i][3] += c.agg[base + i][1] as u64;
            agg[i][1] += c.aggp[base + i][0];
            agg[i][2] += c.aggp[base + i][1];
        }
    }
    let (mut id_off, mut id_data, mut ch_off, mut ch_data) = (vec![0u32], Vec::new(), vec![0u32], Vec::new());
    let mut day = Vec::new();
    let mut v: [Vec<i64>; 4] = Default::default();
    let mut organic = 0i64;
    for cn in lo..hi {
        for d in 0..nd {
            let a = agg[(cn - lo) * nd + d];
            if a[0] == 0 && a[3] == 0 {
                continue;
            }
            if cn == g.nc {
                id_data.extend_from_slice(b"organic");
                ch_data.extend_from_slice(b"organic");
                organic += a[0] as i64;
            } else {
                let mut d = [0u8; 24];
                let (mut i, mut x) = (d.len(), cn);
                loop {
                    i -= 1;
                    d[i] = b'0' + (x % 10) as u8;
                    x /= 10;
                    if x == 0 {
                        break;
                    }
                }
                i -= 2;
                d[i] = b'c';
                d[i + 1] = b'_';
                id_data.extend_from_slice(&d[i..]);
                ch_data.extend_from_slice(&g.channels[g.camp[cn] as usize]);
            }
            id_off.push(id_data.len() as u32);
            ch_off.push(ch_data.len() as u32);
            day.push(g.day0 + d as i32);
            for k in 0..4 {
                v[k].push(a[k] as i64);
            }
        }
    }
    let empty: &[u32] = &[];
    let rows = day.len();
    let cols = [
        Column { name: "campaign_id", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &id_off }, data: &id_data }) },
        Column { name: "channel", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &ch_off }, data: &ch_data }) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&v[0], IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&v[1], IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&v[2], IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&v[3], IntEnc::Plain) },
    ];
    if (rows > 0 || lo == 0) && write_file(path, &cols, &Opts::default(), Method::Whole).is_err() {
        die("campaign_daily");
    }
    [v[0].iter().sum(), v[2].iter().sum(), v[3].iter().sum(), organic]
}

fn clear_stale(dir: &Path) {
    if let Ok(mut rd) = std::fs::read_dir(dir) {
        if rd.next().is_none() {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().as_encoded_bytes().ends_with(b".parquet") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

pub fn stage5(g: &mut G, outdir: &str) {
    let ud = Path::new(outdir).join("user_daily");
    let cd = Path::new(outdir).join("campaign_daily");
    let _ = std::fs::create_dir(&ud);
    let _ = std::fs::create_dir(&cd);
    if !ud.is_dir() && std::fs::create_dir_all(&ud).is_err() {
        die("cannot create the output folders");
    }
    if !cd.is_dir() && std::fs::create_dir_all(&cd).is_err() {
        die("cannot create the output folders");
    }
    clear_stale(&ud);
    clear_stale(&cd);
    let mut cats_off = vec![0u32];
    let mut cats_data = Vec::new();
    for c in &g.cats {
        cats_data.extend_from_slice(c);
        cats_off.push(cats_data.len() as u32);
    }
    let nt = g.nt;
    let mut base = vec![0usize; nt + 1];
    for t in 0..nt {
        base[t + 1] = base[t] + g.t(t).core.day.len();
    }
    let total = base[nt];
    let np = (2 * nt).min(total).max(1);
    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    for p in 0..np {
        let (ga, gb) = (total * p / np, total * (p + 1) / np);
        for t in 0..nt {
            let (a, b) = (ga.max(base[t]), gb.min(base[t + 1]));
            if a < b {
                pieces.push((t, a - base[t], b - base[t]));
            }
        }
    }
    if pieces.is_empty() {
        pieces.push((0, 0, 0));
    }
    let gr: &G = g;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (pr, nx) = (&pieces, &next);
    sys::run_par(nt, |_| {
        let mut kbuf = Vec::new();
        loop {
            let i = nx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i == 0 {
                write_campaign(gr, &cd);
            } else if i <= pr.len() {
                let (t, a, b) = pr[i - 1];
                write_piece(gr, t, a, b, &ud.join(format!("part-{}.parquet", i - 1)), &cats_off, &cats_data, &mut kbuf);
            } else {
                break;
            }
        }
        end_user_file();
    });
}

fn cut(rows: &[(usize, usize)], np: usize) -> Vec<(usize, usize, usize)> {
    let mut base = vec![0usize; rows.len() + 1];
    for (t, &(a, b)) in rows.iter().enumerate() {
        base[t + 1] = base[t] + (b - a);
    }
    let total = base[rows.len()];
    let np = np.min(total).max(1);
    let mut v = Vec::new();
    for p in 0..np {
        let (ga, gb) = (total * p / np, total * (p + 1) / np);
        for t in 0..rows.len() {
            let (a, b) = (ga.max(base[t]), gb.min(base[t + 1]));
            if a < b {
                v.push((t, rows[t].0 + a - base[t], rows[t].0 + b - base[t]));
            }
        }
    }
    v
}

const RPG: usize = crate::env_u32(option_env!("XL_RPG_DEF"), 0) as usize;

fn cut_guided(rows: &[(usize, usize)], nt: usize) -> Vec<(usize, usize, usize)> {
    let total: usize = rows.iter().map(|&(a, b)| b - a).sum();
    let min = (total / (8 * nt)).max(4096);
    let mut v = Vec::new();
    let mut rem = total;
    let (mut t, mut at) = (0usize, rows.first().map_or(0, |r| r.0));
    while rem > 0 {
        let mut want = (rem / (2 * nt)).max(min).min(rem);
        rem -= want;
        while want > 0 {
            while at >= rows[t].1 {
                t += 1;
                at = rows[t].0;
            }
            let take = want.min(rows[t].1 - at);
            v.push((t, at, at + take));
            at += take;
            want -= take;
        }
    }
    v
}

pub static REGION_UNMAP: AtomicBool = AtomicBool::new(false);

pub fn stage45(g: &mut G, outdir: &str) {
    crate::complex::stage4_pre(g);
    let ud = Path::new(outdir).join("user_daily");
    let cd = Path::new(outdir).join("campaign_daily");
    let _ = std::fs::create_dir(&ud);
    let _ = std::fs::create_dir(&cd);
    if !ud.is_dir() && std::fs::create_dir_all(&ud).is_err() {
        die("cannot create the output folders");
    }
    if !cd.is_dir() && std::fs::create_dir_all(&cd).is_err() {
        die("cannot create the output folders");
    }
    clear_stale(&ud);
    clear_stale(&cd);
    let mut cats_off = vec![0u32];
    let mut cats_data = Vec::new();
    for c in &g.cats {
        cats_data.extend_from_slice(c);
        cats_off.push(cats_data.len() as u32);
    }
    let nt = g.nt;
    let v3: Vec<View> = (0..nt).map(|t| view(&g.t(t).core)).collect();
    let pieces3 = cut_guided(&v3.iter().map(|v| (0, v.rows)).collect::<Vec<_>>(), nt);
    let order = crate::complex::shard_order(g);
    g.next.store(0, Ordering::Relaxed);
    let gr: &G = g;
    let gr_nc_plus1 = |g: &G| g.nc + 1;
    let (ca_done, sh_next, sh_done, pq, fq) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let (ns, np) = (order.len(), pieces3.len());
    let nf = 2 * nt;
    let csum: Vec<std::sync::Mutex<[i64; 4]>> = (0..nt).map(|_| std::sync::Mutex::new([0; 4])).collect();
    let ncp = gr_nc_plus1(g);
    let rg_len = gr.files.last().map_or(0, |f| f.off + f.len);
    let rg_todo = AtomicBool::new(REGION_UNMAP.load(Ordering::Relaxed) && rg_len > 0 && !gr.region.is_null());
    run_par(nt, |tid| {
        crate::complex::clicks_a(gr, tid);
        ca_done.fetch_add(1, Ordering::AcqRel);
        let mut sc = crate::complex::PScratch::default();
        let mut kbuf = Vec::new();
        loop {
            if ca_done.load(Ordering::Acquire) == nt && sh_next.load(Ordering::Relaxed) < ns {
                let i = sh_next.fetch_add(1, Ordering::Relaxed);
                if i < ns {
                    #[cfg(feature = "tl")]
                    let t0 = crate::sys::tl::now();
                    crate::complex::person_shard(gr, tid, order[i], &mut sc);
                    #[cfg(feature = "tl")]
                    unsafe {
                        if i < 8 {
                            crate::sys::tl::TASK[i] = (t0, crate::sys::tl::now(), tid as u64);
                        }
                        crate::sys::tl::TASK[63] = (crate::sys::tl::TASK[63].0.max(crate::sys::tl::now()), 0, 0);
                    }
                    sh_done.fetch_add(1, Ordering::AcqRel);
                    continue;
                }
            }
            let shards_done = ca_done.load(Ordering::Acquire) == nt && sh_done.load(Ordering::Acquire) == ns;
            if !shards_done || fq.load(Ordering::Relaxed) >= nf {
                if rg_todo.load(Ordering::Relaxed) && rg_todo.swap(false, Ordering::AcqRel) {
                    extern "C" {
                        fn munmap(p: *mut u8, n: usize) -> i32;
                    }
                    unsafe { munmap(gr.region as *mut u8, rg_len) };
                    continue;
                }
                if pq.load(Ordering::Relaxed) < np {
                    let i = pq.fetch_add(1, Ordering::Relaxed);
                    if i < np {
                        let (t, a, b) = pieces3[i];
                        #[cfg(feature = "tl")]
                        let t0 = crate::sys::tl::now();
                        write_rows(&v3[t], a, b, &ud.join(format!("part-{i}.parquet")), &cats_off, &cats_data, &mut kbuf);
                        #[cfg(feature = "tl")]
                        unsafe {
                            crate::sys::tl::TASK[(16 + i).min(47)] = (t0, crate::sys::tl::now(), tid as u64);
                        }
                        continue;
                    }
                }
                if !shards_done {
                    std::hint::spin_loop();
                    continue;
                }
            }
            let i = fq.fetch_add(1, Ordering::Relaxed);
            if i >= nf {
                if pq.load(Ordering::Relaxed) < np {
                    continue;
                }
                break;
            }
            #[cfg(feature = "tl")]
            let t0 = crate::sys::tl::now();
            if i < nt {
                let (lo, hi) = (ncp * i / nt, ncp * (i + 1) / nt);
                let s = write_campaign_part(gr, &cd.join(format!("part-{i}.parquet")), lo, hi);
                *csum[i].lock().unwrap() = s;
                #[cfg(feature = "tl")]
                unsafe {
                    crate::sys::tl::TASK[8 + i.min(7)] = (t0, crate::sys::tl::now(), tid as u64);
                }
            } else {
                let t = i - nt;
                let v = view(&gr.t(t).core);
                if v.rows > v3[t].rows || (np == 0 && t == 0) {
                    write_rows(&v, v3[t].rows, v.rows, &ud.join(format!("part-{}.parquet", np + t)), &cats_off, &cats_data, &mut kbuf);
                }
            }
        }
        end_user_file();
    });
    if gr.check {
        let (mut us, mut ur) = (0i64, 0i64);
        for tid in 0..nt {
            let c = &gr.t(tid).core;
            us += c.ses.as_slice().iter().map(|&x| x as i64).sum::<i64>();
            ur += c.rev.as_slice().iter().map(|&x| x as i64).sum::<i64>();
        }
        let mut s = [0i64; 4];
        for m in &csum {
            let x = *m.lock().unwrap();
            for k in 0..4 {
                s[k] += x[k];
            }
        }
        let [cs, cr, ca, organic] = s;
        eprintln!("[engine] check: sessions user={us} campaign={cs} revenue user={ur} campaign={cr} assisted={ca} credited_non_organic={}", cs - organic);
        if us != cs || ur != cr || ca < cs - organic {
            eprintln!("[engine] CHECK FAILED");
            unsafe { sys::_exit(5) }
        }
    }
}
