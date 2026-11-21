mod core;
mod parse;
use crate::pqw;
mod util;

use self::core::*;
use self::parse::{chunk_target, chunks, for_lines, split, B};
use self::util::{map_dir, new_map, par_run, Map, Mapped};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const ES: usize = 256;
const PS: usize = 256;

fn take<T: Default>(m: &Mutex<T>) -> T {
    std::mem::take(&mut *m.lock().unwrap_or_else(|e| e.into_inner()))
}

enum P1<'a> {
    None,
    E(Parts<Ev<'a>>),
    C(Vec<Ck0<'a>>),
}
enum P2<'a> {
    R(Parts<Rec<'a>>),
    C(Parts<Ck<'a>>),
}

#[derive(Default)]
struct P1Bufs<'a> {
    ev: Vec<(Ev<'a>, u32)>,
}
#[derive(Default)]
struct P2Bufs<'a> {
    d: DedupBufs<'a>,
    ck: Vec<(Ck<'a>, u32)>,
}

pub fn run(data_dir: &Path, out_dir: &Path, ncpu: usize) -> io::Result<()> {
    let ncpu = ncpu.max(1);
    let log = std::env::var_os("V1ND_LOG").is_some();
    let t0 = std::time::Instant::now();
    let mut last = t0;
    let mut lap = |what: &str| {
        if log {
            let now = std::time::Instant::now();
            eprintln!("[v1nd] {what:<12} {:>8.1} ms (total {:>8.1} ms)", (now - last).as_secs_f64() * 1e3, (now - t0).as_secs_f64() * 1e3);
            last = now;
        }
    };
    let man = Mapped::open(&data_dir.join("manifest.json"))?;
    let prodm = Mapped::open(&data_dir.join("products.csv"))?;
    let campm = Mapped::open(&data_dir.join("campaigns.csv"))?;
    let evm = map_dir(&data_dir.join("events"))?;
    let ckm = map_dir(&data_dir.join("clicks"))?;
    let lkm = map_dir(&data_dir.join("links"))?;
    let (ws, we) = parse::manifest(man.bytes())?;

    let mut prod_rows: Vec<(B, B)> = Vec::new();
    for c in chunks(&[prodm.bytes()], usize::MAX / 2) {
        for_lines(c, |l| {
            if let Some([p, cat]) = split::<2>(l) {
                prod_rows.push((p, cat));
            }
        });
    }
    let mut cat_names: Vec<B> = prod_rows.iter().map(|r| r.1).collect();
    cat_names.push(b"unknown");
    cat_names.sort_unstable();
    cat_names.dedup();
    let mut cat_rank: Map<B, u32> = new_map(cat_names.len());
    for (i, &c) in cat_names.iter().enumerate() {
        cat_rank.insert(c, i as u32);
    }
    let mut products: Map<B, u32> = new_map(prod_rows.len());
    for &(p, c) in &prod_rows {
        products.entry(p).or_insert(cat_rank[c]);
    }
    let unknown_cat = cat_rank[&b"unknown"[..]];
    drop(prod_rows);
    let mut camp_ids: Vec<B> = Vec::new();
    let mut channels: Vec<B> = Vec::new();
    let mut campaigns: Map<B, u32> = new_map(1024);
    for c in chunks(&[campm.bytes()], usize::MAX / 2) {
        for_lines(c, |l| {
            if let Some([id, ch]) = split::<2>(l) {
                if !campaigns.contains_key(id) {
                    campaigns.insert(id, camp_ids.len() as u32);
                    camp_ids.push(id);
                    channels.push(ch);
                }
            }
        });
    }
    let organic = camp_ids.len() as u32;
    camp_ids.push(b"organic");
    channels.push(b"organic");

    let ev_files: Vec<B> = evm.iter().map(|m| m.bytes()).collect();
    let ck_files: Vec<B> = ckm.iter().map(|m| m.bytes()).collect();
    let lk_files: Vec<B> = lkm.iter().map(|m| m.bytes()).collect();
    let ev_chunks = chunks(&ev_files, chunk_target(&ev_files, ncpu));
    let ck_chunks = chunks(&ck_files, chunk_target(&ck_files, ncpu));
    let lk_chunks = chunks(&lk_files, chunk_target(&lk_files, ncpu));
    lap("setup");

    let ectx = EvCtx { ws, we, products: &products, unknown_cat };
    let (nl, ne, nc) = (lk_chunks.len(), ev_chunks.len(), ck_chunks.len());
    let lk_parts: Vec<Mutex<LinkPart>> = (0..nl).map(|_| Mutex::new(Vec::new())).collect();
    let lk_done = AtomicUsize::new(0);
    let links_slot: Mutex<Option<LinkMap>> = Mutex::new(None);
    let (res, _) = par_run(nl + ne + nc, ncpu, P1Bufs::default, |bf, i| {
        if i < nl {
            let v = parse_links(lk_chunks[i]);
            *lk_parts[i].lock().unwrap() = v;
            if lk_done.fetch_add(1, Ordering::AcqRel) + 1 == nl {
                let parts: Vec<LinkPart> = lk_parts.iter().map(take).collect();
                let lm = build_links(&parts);
                *links_slot.lock().unwrap() = Some(lm);
            }
            P1::None
        } else if i < nl + ne {
            P1::E(parse_events(ev_chunks[i - nl], &ectx, ES, &mut bf.ev))
        } else {
            P1::C(parse_clicks(ck_chunks[i - nl - ne], ws, we, &campaigns))
        }
    });
    let links = links_slot.into_inner().unwrap().unwrap_or_else(|| build_links(&[]));
    drop(lk_parts);
    let mut ev_parts: Vec<Parts<Ev>> = Vec::with_capacity(ne);
    let mut ck_parts: Vec<Vec<Ck0>> = Vec::with_capacity(nc);
    for r in res {
        match r {
            P1::E(v) => ev_parts.push(v),
            P1::C(c) => ck_parts.push(c),
            P1::None => {}
        }
    }
    lap("parse+links");

    let nck = ck_parts.len();
    let (res, _) = par_run(ES + nck, ncpu, P2Bufs::default, |bf, i| {
        if i < ES {
            P2::R(dedup_route(&ev_parts, i, &links, PS, &mut bf.d))
        } else {
            P2::C(route_clicks(&ck_parts[i - ES], &links, PS, &mut bf.ck))
        }
    });
    drop(ev_parts);
    drop(ck_parts);
    drop(links);
    let mut rec_parts: Vec<Parts<Rec>> = Vec::with_capacity(ES);
    let mut ckr_parts: Vec<Parts<Ck>> = Vec::with_capacity(nck);
    for r in res {
        match r {
            P2::R(v) => rec_parts.push(v),
            P2::C(v) => ckr_parts.push(v),
        }
    }
    lap("dedup+route");

    let ncat = cat_names.len();
    let (rows, states) = par_run(
        PS,
        ncpu,
        || (Scratch::new(ncat), ShardBufs::default(), Acc::default()),
        |st, p| run_shard(&rec_parts, &ckr_parts, p, organic, &mut st.0, &mut st.1, &mut st.2),
    );
    drop(rec_parts);
    drop(ckr_parts);
    let mut acc: Acc = Acc::default();
    for (_, _, a) in states {
        if acc.is_empty() {
            acc = a;
            continue;
        }
        for (k, v) in a {
            let e = acc.entry(k).or_insert([0; 4]);
            for i in 0..4 {
                e[i] += v[i];
            }
        }
    }
    lap("sessions");
    let ud = out_dir.join("user_daily");
    let cd = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud)?;
    std::fs::create_dir_all(&cd)?;
    clear_parquet(&ud)?;
    clear_parquet(&cd)?;
    let nparts = ncpu.min(PS).max(1);
    let per = PS.div_ceil(nparts);
    let (res, _) = par_run(nparts + 1, ncpu, || (), |_, k| {
        if k == nparts {
            write_campaigns(&cd.join("part-0.parquet"), &acc, &camp_ids, &channels)
        } else {
            let group = &rows[(k * per).min(PS)..((k + 1) * per).min(PS)];
            write_users(&ud.join(format!("part-{k}.parquet")), group, &cat_names)
        }
    });
    for r in res {
        r?;
    }
    lap("write");
    drop(rows);
    drop(acc);
    lap("free");
    Ok(())
}

fn clear_parquet(dir: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_file() && p.extension().map_or(false, |x| x == "parquet") {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

struct StrBuf {
    off: Vec<u32>,
    data: Vec<u8>,
}
impl StrBuf {
    fn new(n: usize, bytes: usize) -> StrBuf {
        let mut off = Vec::with_capacity(n + 1);
        off.push(0);
        StrBuf { off, data: Vec::with_capacity(bytes) }
    }
    fn push(&mut self, s: &[u8]) {
        self.data.extend_from_slice(s);
        self.off.push(self.data.len() as u32);
    }
    fn col(&self) -> pqw::StrCol<'_> {
        pqw::StrCol { offsets: &self.off, data: &self.data }
    }
}

const STREAM: pqw::Method = pqw::Method::Stream(1 << 20);

fn write_users(path: &Path, group: &[Vec<UserRow>], cat_names: &[B]) -> io::Result<()> {
    use pqw::*;
    let n: usize = group.iter().map(|g| g.len()).sum();
    let mut names = StrBuf::new(n / 2, n * 12);
    let mut ukeys: Vec<u32> = Vec::with_capacity(n);
    let mut day = Vec::with_capacity(n);
    let mut s = Vec::with_capacity(n);
    let mut ev = Vec::with_capacity(n);
    let mut rev = Vec::with_capacity(n);
    let mut top: Vec<u32> = Vec::with_capacity(n);
    let mut prev: Option<&[u8]> = None;
    for g in group {
        for r in g {
            if prev != Some(r.name) {
                names.push(r.name);
                prev = Some(r.name);
            }
            ukeys.push((names.off.len() - 2) as u32);
            day.push(r.day);
            s.push(r.sessions as i64);
            ev.push(r.events as i64);
            rev.push(r.revenue as i64);
            top.push(r.top.unwrap_or(u32::MAX));
        }
    }
    let mut cats = StrBuf::new(cat_names.len(), 0);
    for c in cat_names {
        cats.push(c);
    }
    let ncat = cat_names.len();
    let top8: Vec<u8>;
    let top16: Vec<u16>;
    let tkeys = if ncat < u8::MAX as usize {
        top8 = top.iter().map(|&t| if t == u32::MAX { u8::MAX } else { t as u8 }).collect();
        Keys::U8(&top8)
    } else if ncat < u16::MAX as usize {
        top16 = top.iter().map(|&t| if t == u32::MAX { u16::MAX } else { t as u16 }).collect();
        Keys::U16(&top16)
    } else {
        Keys::U32(&top)
    };
    let cols = [
        Column { name: "user_id", data: Data::Dict { dict: names.col(), keys: Keys::U32(&ukeys), nullable: false, plain: false } },
        Column { name: "day", data: Data::Date(&day, IntEnc::Dense) },
        Column { name: "sessions", data: Data::I64(&s, IntEnc::Auto) },
        Column { name: "events", data: Data::I64(&ev, IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I64(&rev, IntEnc::Plain) },
        Column { name: "top_category", data: Data::Dict { dict: cats.col(), keys: tkeys, nullable: true, plain: false } },
    ];
    write_file(path, &cols, &Opts::default(), STREAM)?;
    Ok(())
}

fn write_campaigns(path: &Path, acc: &Acc, camp_ids: &[B], channels: &[B]) -> io::Result<()> {
    use pqw::*;
    let n = acc.len();
    let mut cid = StrBuf::new(n, n * 8);
    let mut ch = StrBuf::new(n, n * 8);
    let mut day = Vec::with_capacity(n);
    let mut s = Vec::with_capacity(n);
    let mut ps = Vec::with_capacity(n);
    let mut rev = Vec::with_capacity(n);
    let mut asst = Vec::with_capacity(n);
    for (&key, v) in acc {
        let (c, d) = unkey(key);
        cid.push(camp_ids[c as usize]);
        ch.push(channels[c as usize]);
        day.push(d);
        s.push(v[0] as i64);
        ps.push(v[1] as i64);
        rev.push(v[2] as i64);
        asst.push(v[3] as i64);
    }
    let cols = [
        Column { name: "campaign_id", data: Data::Str(cid.col()) },
        Column { name: "channel", data: Data::Str(ch.col()) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Dense) },
        Column { name: "sessions", data: Data::I64(&s, IntEnc::Auto) },
        Column { name: "purchasing_sessions", data: Data::I64(&ps, IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I64(&rev, IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&asst, IntEnc::Auto) },
    ];
    write_file(path, &cols, &Opts::default(), STREAM)?;
    Ok(())
}
