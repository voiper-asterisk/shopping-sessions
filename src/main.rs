#![allow(dead_code, static_mut_refs, unused_unsafe, unexpected_cfgs)]
mod ckix;
mod complex;
mod core;
mod events;
mod iface;
mod k_ref;
mod keyrec;
mod links;
mod out;
mod parse;
mod pqw;
mod setup;
mod sys;
mod v3b;

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Instant;
use sys::*;

#[global_allocator]
static ALLOC: sys::Alloc = sys::Alloc;

mod generic;
mod k_free;
pub type K = k_free::Free;

fn dirs(args: &[String], skip: usize) -> (String, String) {
    if args.len() >= 3 + skip {
        (args[args.len() - 2].clone(), args[args.len() - 1].clone())
    } else {
        (std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()), std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()))
    }
}

pub fn ncpu_env() -> usize {
    std::env::var("NCPU").ok().and_then(|s| s.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(8)
}

pub fn run_generic(data: &str, out: &str, ncpu: usize) -> ! {
    match generic::run(std::path::Path::new(data), std::path::Path::new(out), ncpu) {
        Ok(()) => unsafe { sys::_exit(0) },
        Err(e) => {
            eprintln!("[generic] error: {e}");
            unsafe { sys::_exit(1) }
        }
    }
}

pub const fn env_u32(s: Option<&str>, d: u32) -> u32 {
    match s {
        None => d,
        Some(s) => {
            let b = s.as_bytes();
            let (mut v, mut i) = (0u32, 0);
            while i < b.len() {
                v = v * 10 + (b[i] - b'0') as u32;
                i += 1;
            }
            v
        }
    }
}
pub const BB: u32 = env_u32(option_env!("XL_BB"), 15);
pub const SPIN_US_DEFAULT: u64 = env_u32(option_env!("XL_SPIN_DEF"), 400) as u64;
pub const BSIZE: usize = 1 << BB;
pub const UB: u32 = env_u32(option_env!("XL_UB"), 12);
pub const UBSIZE: usize = 1 << UB;
pub const DAY_MS: i64 = 86_400_000;
pub const WEEK_MS: u64 = 604_800_000;
pub const GAP_MS: u64 = 1_800_000;
pub const PBOUND: usize = 1 << 17;
pub const CBOUND: usize = 1 << 14;
pub const INVALID_TS: u64 = 0x7FFF_FFFF;
pub const ING_MAX: u64 = (1 << 21) - 1;
pub const COMPLEX: u16 = 0x8000;
pub const EXTRA: u16 = 0x4000;
pub const MASK: u16 = 0x3FFF;
pub const NALIAS: u64 = 14;
pub const RAW: u64 = 1 << 42;
pub const ODDU: u64 = 1 << 26;
pub const NSHARD: usize = 64;
pub const TAG_UF: u64 = 1 << 62;
pub const TAG_ODDC: u64 = 2 << 62;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rec {
    pub w0: u64,
    pub w1: u64,
}

#[derive(Clone, Copy)]
pub struct OddEv {
    pub eid: u64,
    pub ing: i64,
    pub w0: u64,
    pub user: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ev,
    Ck,
    Lk,
}
pub struct FileInfo {
    pub kind: Kind,
    pub off: usize,
    pub len: usize,
    pub hdr: usize,
}
#[derive(Clone, Copy)]
pub struct Chunk {
    pub file: u32,
    pub a: usize,
    pub b: usize,
}

#[derive(Clone, Copy, Default)]
pub struct Seg {
    pub tid: u32,
    pub off: u32,
    pub len: u32,
    pub user: u32,
}
#[derive(Clone, Copy, Default)]
pub struct EdgeInfo {
    pub n: u32,
    pub first: Seg,
    pub last: Seg,
}

pub struct TState {
    pub ev: parse::EvPart,
    pub us: parse::Part,
    pub raws: Vec<(u64, u32)>,
    pub rawbuf: Vec<u8>,
    pub odd_users: Vec<u64>,
    pub odd_events: Vec<OddEv>,
    pub odd_clicks: Vec<[u64; 3]>,
    pub gen_edges: Vec<(u64, u64)>,
    pub cx_edges: Vec<(u32, u8, u8)>,
    pub cx_users: Vec<u32>,
    pub mixed_users: Vec<u32>,
    pub slots: Vec<Rec>,
    pub edge: AVec<Rec>,
    pub cx_raw: Vec<Rec>,
    pub cx_ev: Vec<Vec<Rec>>,
    pub cx_ck: Vec<Vec<[u64; 3]>>,
    pub ck: Vec<u64>,
    pub runbuf: Vec<Rec>,
    pub pend: Vec<(u32, u16)>,
    pub cnt: Vec<u32>,
    pub core: core::Core,
    pub n_ev_rows: u64,
    pub n_ck_rows: u64,
    pub n_lk_rows: u64,
    pub n_simple: u64,
    pub n_complex: u64,
    pub n_odd: u64,
    pub n_runs: u64,
    pub prof: [u64; 8],
}

pub struct G {
    pub nt: usize,
    pub check: bool,
    pub ws: i64,
    pub wlen: u64,
    pub cbase: i64,
    pub clen: u64,
    pub ing_base: i64,
    pub ws_mod: u64,
    pub day0: i32,
    pub nd: usize,
    pub ukey: u64,
    pub ekey: u64,
    pub ckey: u64,
    pub ccap: u64,
    pub cpart: bool,
    pub ecap: u64,
    pub ucap: u64,
    pub prod: Vec<u8>,
    pub camp: Vec<u8>,
    pub nc: usize,
    pub cats: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
    pub region: *const u8,
    pub files: Vec<FileInfo>,
    pub queue: Vec<Chunk>,
    pub qnext: AtomicUsize,
    pub ev_bytes: usize,
    pub row_cap: usize,
    pub nb: usize,
    pub nub: usize,
    pub ev_arena: Arena,
    pub us_arena: Arena,
    pub t: Vec<UnsafeCell<TState>>,
    pub utab: *mut u16,
    pub claim: *const AtomicU64,
    pub csr_off: *mut u32,
    pub csr: *mut [u64; 2],
    pub csr_cap: usize,
    pub csr_base: Vec<usize>,
    pub ncsr: usize,
    pub uf: links::Uf,
    pub edges: *mut EdgeInfo,
    pub joined: Vec<Vec<Seg>>,
    pub cxu: Vec<u32>,
    pub extra: std::collections::HashMap<u32, Vec<u64>>,
    pub odd_kept: Vec<OddEv>,
    pub next: AtomicUsize,
    pub st2b_done: std::sync::atomic::AtomicBool,
}
unsafe impl Sync for G {}
impl G {
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub fn t(&self, tid: usize) -> &mut TState {
        unsafe { &mut *self.t[tid].get() }
    }
    #[inline(always)]
    pub fn normal(&self, y: u64) -> Option<(u32, u32)> {
        if y < (1 << 40) {
            let w = keyrec::unscramble(y, self.ukey);
            let u = w >> 9;
            let a = w & 511;
            if u < self.ucap && a < NALIAS {
                return Some((u as u32, a as u32));
            }
        }
        None
    }
    pub fn raw_bytes(&self, tok: u64) -> &[u8] {
        let tid = ((tok >> 34) & 15) as usize;
        let idx = (tok & ((1 << 34) - 1)) as usize;
        let t = self.t(tid);
        let (off, len) = t.raws[idx];
        &t.rawbuf[off as usize..off as usize + len as usize]
    }
    pub fn id_str(&self, tok: u64, prefix: u8, out: &mut Vec<u8>) {
        out.clear();
        if tok & RAW != 0 {
            out.extend_from_slice(self.raw_bytes(tok));
        } else {
            let (v, len) = enc36(tok);
            out.push(prefix);
            out.extend_from_slice(&v.to_be_bytes()[..len]);
        }
    }
    #[inline(always)]
    pub fn csr_range(&self, u: u32) -> &[u64] {
        unsafe {
            let i = u as usize + (u as usize >> UB);
            let a = *self.csr_off.add(i) as usize;
            let b = *self.csr_off.add(i + 1) as usize;
            std::slice::from_raw_parts((self.csr as *const u64).add(a), b - a)
        }
    }
}

static PAIR36: [u16; 1296] = {
    let mut t = [0u16; 1296];
    let d = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut i = 0;
    while i < 1296 {
        t[i] = d[i / 36] as u16 | (d[i % 36] as u16) << 8;
        i += 1;
    }
    t
};

#[inline(always)]
pub fn enc36(y: u64) -> (u64, usize) {
    let hi = (y / 1_679_616) as u32;
    let lo = (y % 1_679_616) as u32;
    let p = |v: u32| PAIR36[v as usize] as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    let be = le.swap_bytes();
    let z = be ^ 0x3030_3030_3030_3030;
    let lz = if z == 0 { 7 } else { (z.leading_zeros() / 8) as usize };
    (be << (8 * lz), 8 - lz)
}

#[inline(always)]
pub fn shard(pkey: u64) -> usize {
    (pkey.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
}

fn main() {
    sys::xm(1);
    let t0 = Instant::now();
    #[cfg(feature = "sp")]
    let sp_t0 = sys::sp::ns();
    #[cfg(feature = "sp")]
    sys::sp::snap("t0");
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--generic") {
        let (data, outdir) = dirs(&args, 1);
        run_generic(&data, &outdir, ncpu_env());
    }
    let (data, outdir) = dirs(&args, 0);
    sys::exec_init(&args, &data, &outdir, ncpu_env());
    sys::xm_page();
    sys::fault_fallback_init();
    sys::fd_reserve();
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[v2b] panic: {info}");
        sys::fallback("panic")
    }));
    let nt: usize = std::env::var("NCPU").ok().and_then(|s| s.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(8).min(16);
    let timing = std::env::var("XL_TIMING").is_ok_and(|v| v == "1");
    let check = std::env::var("XL_CHECK").is_ok_and(|v| v == "1");
    if std::env::var("XL_NOHUGE").is_ok_and(|v| v == "1") {
        HUGE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    sys::detect_pages();
    unsafe { sys::LAST = Some(t0) };
    #[cfg(feature = "tl")]
    unsafe {
        sys::tl::T0 = sys::tl::now();
        sys::tl::T0_NS = sys::tl::realtime_ns() - t0.elapsed().as_nanos() as u64;
        sys::tl::LAPT.reserve(64);
    }
    #[cfg(feature = "tl")]
    sys::mp::ON.store(std::env::var("XL_MEMPROF").is_ok(), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("XL_ALLOC").map_or(true, |v| v == "1") {
        sys::alloc_enable();
    }
    if std::env::var("XL_PMB").is_ok_and(|v| v == "0") {
        sys::BIG_ON.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = std::env::var("XL_DONATE").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        sys::DONATE_DIV.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var("XL_FIRST4K").is_ok_and(|v| v == "1") {
        sys::FIRST_SMALL.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var("XL_PREFAULT").is_ok_and(|v| v == "1") {
        sys::PREFAULT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var("XL_ANONR").is_ok_and(|v| v == "0") {
        sys::ANON_REGION.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(us) = std::env::var("XL_SPIN").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        sys::SPIN_TICKS.store(us * 2000, std::sync::atomic::Ordering::Relaxed);
    } else {
        sys::SPIN_TICKS.store(SPIN_US_DEFAULT * 2000, std::sync::atomic::Ordering::Relaxed);
    }
    sys::PIN_AT_START.store(parse::pin_mode() == 2 && std::env::var("XL_PINSTART").is_ok_and(|v| v == "1"), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("XL_NOTOUCH").is_ok_and(|v| v == "0") {
        sys::CORE_NOTOUCH.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = std::env::var("XL_KP").ok().and_then(|v| v.trim().parse().ok()) {
        sys::SETUP_KP.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if let Ok(v) = std::env::var("XL_RGREL") {
        out::REGION_UNMAP.store(v == "1", std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(feature = "sp")]
    parse::SPTOUCH.store(std::env::var("XL_SPTOUCH").is_ok(), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("XL_HOLD").is_ok_and(|v| v == "1") {
        parse::HOLD_FREE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var("XL_PPARTS").is_ok_and(|v| v == "0") {
        sys::SETUP_PARTS.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(feature = "sp")]
    sys::sp::snap("init");
    sys::xm(7);
    if std::env::var("XL_POOL").map_or(true, |v| v == "1") {
        sys::pool_start(nt);
    }
    sys::xm(8);
    #[cfg(feature = "sp")]
    sys::sp::snap("pool");
    let knob = |name: &str, default: bool| std::env::var(name).map_or(default, |v| v == "1");
    unsafe {
        parse::NT = knob("XL_NT", true);
        parse::RELEASE = knob("XL_REL", true);
        parse::PREFETCH = knob("XL_PF", false);
        sys::RELEASE.store(knob("XL_FREE", true), std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "knobs")]
        {
            parse::NOSINK = knob("XL_NOSINK", false);
            parse::SKIP = std::env::var("XL_SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        }
    }

    if check {
        let mut x = 0x9E37_79B9u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let y = if i < 100_000 { i } else { (x >> 20) % 2_821_109_907_456 >> (i % 41) };
            let mut s = Vec::new();
            keyrec::base36_encode(y, &mut s);
            let (v, len) = enc36(y);
            if v.to_be_bytes()[..len] != s[..] || (len < 8 && v.to_be_bytes()[len] != 0) {
                eprintln!("[v2b] CHECK FAILED: enc36({y})");
                unsafe { sys::_exit(5) }
            }
        }
    }
    if std::env::var("XL_MEMTEST").is_ok() {
        eprintln!("[memtest] {}", if sys::mem_test() { "OK" } else { "FAILED" });
        unsafe { sys::_exit(0) };
    }
    sys::xm(9);
    let g: &'static mut G = Box::leak(Box::new(setup::setup(&data, nt, check)));
    sys::xm(10);
    lap("setup");
    if v3b::ON.load(std::sync::atomic::Ordering::SeqCst) {
        v3b::run(g, &outdir, timing || check);
        if timing {
            let mut s = String::new();
            for (n, ms) in unsafe { sys::LAPS.iter() } {
                s.push_str(&format!("{n}={ms:.1} "));
            }
            eprintln!("[v3b] setup ms: {s}; exit at {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
        }
        match std::env::var("XL_PMX").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(2) {
            1 => sys::exit_release(g.nt, unsafe { &parse::FDS }),
            2 => sys::exit_release_pieces(g.nt, unsafe { &parse::FDS }),
            _ => {}
        }
        unsafe { sys::_exit(0) }
    }
    if std::env::var("XL_SORTBENCH").is_ok() {
        links::sort_bench(g);
        unsafe { sys::_exit(0) };
    }
    {
        let gr: &G = g;
        run_par(nt, |tid| parse::stage1(gr, tid));
    }
    sys::xm(11);
    lap("parse");
    links::stage2(g);
    sys::xm(12);
    lap("st2sort");
    if std::env::var("XL_OVL").map_or(true, |v| v != "0") {
        out::s3_init(g, &outdir);
    }
    events::stage3(g);
    sys::xm(13);
    lap("ev3b");
    if std::env::var("XL_OVL").map_or(true, |v| v != "0") {
        out::stage45(g, &outdir);
        lap("write");
    } else {
        complex::stage4(g);
        lap("complex");
        out::stage5(g, &outdir);
        lap("write");
    }
    sys::xm(14);
    if timing || check {
        let mut s = String::new();
        for (n, ms) in unsafe { sys::LAPS.iter() } {
            s.push_str(&format!("{n}={ms:.1} "));
        }
        let (mut ev, mut ck, mut lk, mut si, mut cx, mut od, mut runs, mut rows, mut sess) = (0, 0, 0, 0, 0, 0, 0, 0, 0);
        for tid in 0..nt {
            let t = g.t(tid);
            ev += t.n_ev_rows;
            ck += t.n_ck_rows;
            lk += t.n_lk_rows;
            si += t.n_simple;
            cx += t.n_complex;
            od += t.n_odd;
            runs += t.n_runs;
            rows += t.core.day.len();
            sess += t.core.n_sess;
        }
        let hwm = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())).unwrap_or_default();
        eprintln!("[v2b] ms: {s}total={:.1} peak_rss_kb={hwm}", t0.elapsed().as_secs_f64() * 1e3);
        let mut prof = [0u64; 8];
        for tid in 0..nt {
            for k in 0..8 {
                if k == 4 {
                    prof[k] = prof[k].max(g.t(tid).prof[k]);
                } else {
                    prof[k] += g.t(tid).prof[k];
                }
            }
        }
        eprintln!("[v2b] prof (thread-ms): scatter={:.1} compact={:.1} runs={:.1} st2b={:.1} st2b_max_bucket={:.1} st2b_pass1={:.1} st2b_pass2={:.1} st2b_sort={:.1}", prof[0] as f64 / 1e6, prof[1] as f64 / 1e6, prof[2] as f64 / 1e6, prof[3] as f64 / 1e6, prof[4] as f64 / 1e6, prof[5] as f64 / 1e6, (prof[6] - prof[5]) as f64 / 1e6, (prof[3] - prof[6]) as f64 / 1e6);
        eprintln!(
            "[v2b] rows: events={ev} clicks={ck} links={lk} | kept valid: simple={si} complex={cx} odd={od} | runs={runs} sessions={sess} user_rows={rows} uf_nodes={} cx_users={} joined={} nb={} nub={}",
            g.uf.len(),
            g.cxu.len(),
            g.joined.len(),
            g.nb,
            g.nub
        );
    }
    #[cfg(feature = "tl")]
    let rel0 = sys::tl::now();
    let endrel = std::env::var("XL_ENDREL").ok();
    let many_faults = endrel.is_none() && sys::minflt(0) as usize > 2 * (g.files.last().map_or(0, |f| f.off + f.len) >> 16) + 60_000;
    let endrel_ran = ((sys::SMALLPAGES.load(std::sync::atomic::Ordering::Relaxed) || many_faults) && endrel.as_deref().map_or(true, |v| v == "1")) || endrel.as_deref() == Some("2");
    if endrel_ran {
        let nt = g.nt;
        let mut r = [[(0usize, 0usize); 12]; 16];
        for tid in 0..nt {
            let t = g.t(tid);
            let c = &t.core;
            let v = |p: *const u8, n: usize| (p as usize, n);
            r[tid] = [
                v(c.names.as_ptr(), c.names.len()),
                v(c.names_off.as_ptr() as *const u8, c.names_off.len() * 4),
                v(c.keys.as_ptr() as *const u8, c.keys.len() * 4),
                v(c.day.as_ptr() as *const u8, c.day.len() * 4),
                v(c.ses.as_ptr() as *const u8, c.ses.len() * 4),
                v(c.evs.as_ptr() as *const u8, c.evs.len() * 4),
                v(c.rev.as_ptr() as *const u8, c.rev.len() * 4),
                v(c.cat.as_ptr(), c.cat.len()),
                v(t.edge.as_ptr() as *const u8, t.edge.len() * 16),
                if g.csr.is_null() { (0, 0) } else { v((g.csr as *const u8).wrapping_add(g.ncsr * 8 * tid / nt), g.ncsr * 8 * (tid + 1) / nt - g.ncsr * 8 * tid / nt) },
                if endrel.as_deref() == Some("2") { let tot = g.files.last().map_or(0, |f| f.off + f.len); v(g.region.wrapping_add(tot * tid / nt), tot * (tid + 1) / nt - tot * tid / nt) } else { (0, 0) },
                (0, 0),
            ];
        }
        let rr = &r;
        run_par(nt, |tid| unsafe {
            for &(p, n) in &rr[tid] {
                if n > 0 {
                    sys::release(p as *const u8, n);
                }
            }
            sys::release_heap_part(tid, nt);
        });
    }
    if timing {
        eprintln!("[v2b] exit at {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    }
    #[cfg(feature = "sp")]
    {
        sys::sp::snap("endrel");
        sys::sp::report();
        let r = sys::sp::ru(0);
        eprintln!("[sp] at report: user {:.2} sys {:.2} minflt {} t {:.2}", r[0] as f64 / 1e3, r[1] as f64 / 1e3, r[2], t0.elapsed().as_secs_f64() * 1e3);
        eprintln!("[sp] mono {} {}", sp_t0, sys::sp::ns());
    }
    #[cfg(feature = "tl")]
    unsafe {
        sys::tl::REL = sys::tl::now() - rel0;
    }
    #[cfg(feature = "tl")]
    sys::tl::report(t0);
    #[cfg(feature = "tl")]
    if std::env::var("XL_EXITU").is_ok() {
        extern "C" {
            fn munmap(p: *mut u8, n: usize) -> i32;
        }
        let tot = g.files.last().map_or(0, |f| f.off + f.len);
        let (eb, ec) = g.ev_arena.span();
        let (ub, uc) = g.us_arena.span();
        let mut ts = [0u64; 4];
        ts[0] = sys::tl::now();
        unsafe { munmap(g.region as *mut u8, tot) };
        ts[1] = sys::tl::now();
        unsafe { munmap(eb, ec) };
        ts[2] = sys::tl::now();
        unsafe { munmap(ub, uc) };
        ts[3] = sys::tl::now();
        eprintln!("[tl] exitu ticks: files {} ev_arena {} us_arena {}", ts[1] - ts[0], ts[2] - ts[1], ts[3] - ts[2]);
    }
    if std::env::var("XL_DBG").is_ok() {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for l in s.lines().filter(|l| l.starts_with("Rss") || l.starts_with("VmPTE") || l.starts_with("VmRSS")) {
                eprintln!("[dbg] end {l}");
            }
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            for l in s.lines() {
                eprintln!("[dbg] rollup {l}");
            }
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/maps") {
            eprintln!("[dbg] vmas {}", s.lines().count());
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/smaps") {
            let mut v: Vec<(u64, String)> = Vec::new();
            let mut cur = String::new();
            for l in s.lines() {
                if l.starts_with("Rss:") {
                    let kb: u64 = l.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
                    if kb > 1024 { v.push((kb, cur.clone())); }
                } else if !l.contains(':') || l.split_whitespace().next().map_or(false, |w| w.contains('-')) {
                    cur = l.to_string();
                }
            }
            v.sort();
            for (kb, n) in v.iter().rev().take(12) { eprintln!("[dbg] rss {kb} kB {n}"); }
        }
    }
    sys::xm(62);
    if !endrel_ran && std::env::var("XL_XREL").map_or(true, |v| v != "0") {
        sys::exit_release(g.nt, unsafe { &parse::FDS });
    }
    sys::xm_end();
    unsafe { sys::_exit(0) }
}
