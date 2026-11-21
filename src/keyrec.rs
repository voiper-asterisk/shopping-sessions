
pub const MASK: u64 = (1 << 40) - 1;
pub const A: u64 = 0x9E37_79B9_7F4B;
pub const B: u64 = 0xD6E8_FEB8_6659;
pub const ALIAS_SLOTS: u64 = 512;
pub const KIOSK_BASE: u64 = 1 << 39;

const fn inverse(a: u64) -> u64 {
    let mut x = a;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(x)));
        i += 1;
    }
    x & MASK
}
pub const AINV: u64 = inverse(A);
pub const BINV: u64 = inverse(B);

#[inline(always)]
pub fn scramble(value: u64, key: u64) -> u64 {
    let mut m = value & MASK;
    m = (m.wrapping_mul(A) ^ key) & MASK;
    m ^= m >> 17;
    m.wrapping_mul(B) & MASK
}

#[inline(always)]
pub fn unshuffle(y: u64) -> u64 {
    let x = y.wrapping_mul(BINV) & MASK;
    x ^ (x >> 17) ^ (x >> 34)
}

#[inline(always)]
pub fn unkey(x: u64, key: u64) -> u64 {
    ((x ^ key) & MASK).wrapping_mul(AINV) & MASK
}

#[inline(always)]
pub fn unscramble(y: u64, key: u64) -> u64 {
    unkey(unshuffle(y), key)
}

pub fn base36_encode(value: u64, out: &mut Vec<u8>) {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buffer = [0u8; 13];
    let mut at = buffer.len();
    let mut rest = value;
    loop {
        at -= 1;
        buffer[at] = DIGITS[(rest % 36) as usize];
        rest /= 36;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&buffer[at..]);
}

#[inline]
pub fn base36_decode(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 8 || (s.len() > 1 && s[0] == b'0') {
        return None;
    }
    let mut v = 0u64;
    for &c in s {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            _ => return None,
        };
        v = v * 36 + d as u64;
    }
    (v <= MASK).then_some(v)
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HI: u64 = ONES * 0x80;

#[inline(always)]
pub fn base36_word(w: u64, n: u32) -> (u64, bool) {
    let sh = 8 * (8 - n);
    let w2 = (w << sh) | (((ONES * 0x30) >> 1) >> (63 - sh));
    let ge0 = w2.wrapping_add(ONES * 0x50) & HI;
    let gt9 = w2.wrapping_add(ONES * 0x46) & HI;
    let gea = w2.wrapping_add(ONES * 0x1F) & HI;
    let gtz = w2.wrapping_add(ONES * 0x05) & HI;
    let good = !w2 & HI & ge0 & (!gt9 | (gea & !gtz));
    let d = w2
        .wrapping_sub(ONES * 0x30)
        .wrapping_sub((gea >> 7).wrapping_mul(39));
    let p = (d & 0x00FF_00FF_00FF_00FF)
        .wrapping_mul(36)
        .wrapping_add((d >> 8) & 0x00FF_00FF_00FF_00FF);
    let q = (p & 0x0000_FFFF_0000_FFFF)
        .wrapping_mul(1296)
        .wrapping_add((p >> 16) & 0x0000_FFFF_0000_FFFF);
    let r = (q & 0xFFFF_FFFF)
        .wrapping_mul(1_679_616)
        .wrapping_add(q >> 32);
    let canonical = n == 1 || (w & 0xFF) != 0x30;
    (r, good == HI && canonical && r <= MASK)
}

#[inline(always)]
pub fn base36_decode_at(buf: &[u8], start: usize, len: usize) -> Option<u64> {
    if len == 0 || len > 8 {
        return None;
    }
    let w = if start + 8 <= buf.len() {
        u64::from_le_bytes(buf[start..start + 8].try_into().unwrap())
    } else {
        let mut b = [0u8; 8];
        b[..len].copy_from_slice(&buf[start..start + len]);
        u64::from_le_bytes(b)
    };
    let (v, ok) = base36_word(w, len as u32);
    ok.then_some(v)
}

pub fn base36_decode_swar(s: &[u8]) -> Option<u64> {
    base36_decode_at(s, 0, s.len())
}

pub fn base36_batch(words: &[u64], lens: &[u8], out: &mut [u64]) -> usize {
    let mut bad = 0usize;
    for ((w, &n), o) in words.iter().zip(lens).zip(out.iter_mut()) {
        let n = (n as u32).clamp(1, 8);
        let (v, ok) = base36_word(*w, n);
        *o = v;
        bad += (!ok) as usize;
    }
    bad
}

#[inline]
pub fn id_x(field: &[u8], prefix: u8) -> Option<u64> {
    if field.len() < 2 || field[0] != prefix {
        return None;
    }
    base36_decode_at(field, 1, field.len() - 1).map(unshuffle)
}

pub fn split_line<'a>(line: &'a [u8], out: &mut [&'a [u8]]) -> bool {
    if line.is_empty() {
        return false;
    }
    let mut k = 0;
    for f in line.split(|&c| c == b',') {
        if k == out.len() {
            return false;
        }
        let f = if f.first() == Some(&b'"') {
            if f.len() < 2 || f[f.len() - 1] != b'"' {
                return false;
            }
            &f[1..f.len() - 1]
        } else {
            f
        };
        if f.contains(&b'"') {
            return false;
        }
        out[k] = f;
        k += 1;
    }
    k == out.len()
}

fn digits(s: &[u8]) -> Option<i64> {
    let mut v = 0i64;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

pub fn parse_ts(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    if s.iter().all(|c| c.is_ascii_digit()) {
        let mut v: i64 = 0;
        for &c in s {
            v = match v.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as i64)) {
                Some(v) => v,
                None => return Some(i64::MAX),
            };
        }
        return Some(v);
    }
    if s.len() != 20 && s.len() != 25 {
        return None;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = digits(&s[0..4])?;
    let mo = digits(&s[5..7])?;
    let d = digits(&s[8..10])?;
    let h = digits(&s[11..13])?;
    let mi = digits(&s[14..16])?;
    let se = digits(&s[17..19])?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut t = ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60 + se;
    if s.len() == 20 {
        if s[19] != b'Z' {
            return None;
        }
    } else {
        let sign = match s[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if s[22] != b':' {
            return None;
        }
        let oh = digits(&s[20..22])?;
        let om = digits(&s[23..25])?;
        if oh > 23 || om > 59 {
            return None;
        }
        t -= sign * (oh * 3600 + om * 60);
    }
    Some(t * 1000)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    pub user: u64,
    pub event: u64,
    pub click: u64,
}

pub struct EventRow {
    pub id_x: u64,
    pub user_x: Option<u64>,
    pub ts: Option<i64>,
}
pub struct ClickRow {
    pub id_x: u64,
    pub user_x: Option<u64>,
}

#[derive(Default)]
pub struct Sample {
    pub events: Vec<EventRow>,
    pub clicks: Vec<ClickRow>,
}

impl Sample {
    pub fn add_event_lines(&mut self, bytes: &[u8], max: usize) {
        let mut f: [&[u8]; 7] = [&[]; 7];
        for line in bytes.split(|&c| c == b'\n') {
            if self.events.len() >= max {
                return;
            }
            if !split_line(line, &mut f) {
                continue;
            }
            if let Some(id) = id_x(f[0], b'e') {
                self.events.push(EventRow { id_x: id, user_x: id_x(f[1], b'u'), ts: parse_ts(f[2]) });
            }
        }
    }
    pub fn add_click_lines(&mut self, bytes: &[u8], max: usize) {
        let mut f: [&[u8]; 4] = [&[]; 4];
        for line in bytes.split(|&c| c == b'\n') {
            if self.clicks.len() >= max {
                return;
            }
            if !split_line(line, &mut f) {
                continue;
            }
            if let Some(id) = id_x(f[0], b'k') {
                self.clicks.push(ClickRow { id_x: id, user_x: id_x(f[1], b'u') });
            }
        }
    }
}

pub const USER_INDEX_LIMIT: u64 = 1 << 23;
pub const ROW_INDEX_LIMIT: u64 = 1 << 28;
const MIN_DISTINCT: usize = 20;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub user_candidates_tried: u64,
    pub event_steps: u64,
    pub click_steps: u64,
}

fn sorted_unique(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v.dedup();
    v
}

#[inline(always)]
fn user_ok(v: u64) -> bool {
    (v >> 9) < USER_INDEX_LIMIT || (v >= KIOSK_BASE && v < KIOSK_BASE + 256)
}

fn verify(xs: &[u64], key: u64, ok: impl Fn(u64) -> bool, slack: usize) -> bool {
    let mut miss = 0;
    for &x in xs {
        if !ok(unkey(x, key)) {
            miss += 1;
            if miss > slack {
                return false;
            }
        }
    }
    true
}

pub fn recover_user_key(s: &Sample, st: &mut Stats) -> Option<u64> {
    let mut all: Vec<u64> = s.events.iter().filter_map(|r| r.user_x).chain(s.clicks.iter().filter_map(|r| r.user_x)).collect();
    all.sort_unstable();
    let mut runs: Vec<(u32, u64)> = Vec::new();
    for &x in &all {
        match runs.last_mut() {
            Some((c, y)) if *y == x => *c += 1,
            _ => runs.push((1, x)),
        }
    }
    if runs.len() < MIN_DISTINCT {
        return None;
    }
    let distinct: Vec<u64> = runs.iter().map(|r| r.1).collect();
    let top = runs.len().min(16);
    runs.select_nth_unstable_by(top - 1, |a, b| b.0.cmp(&a.0));
    runs[..top].sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let probe = [distinct[0], distinct[distinct.len() / 2], distinct[distinct.len() - 1]];
    let mut found: Option<u64> = None;
    for &(_, x) in &runs[..top] {
        for i in 0..64u64 {
            st.user_candidates_tried += 1;
            let key = (x ^ (i * ALIAS_SLOTS).wrapping_mul(A)) & MASK;
            if !probe.iter().all(|&p| user_ok(unkey(p, key))) {
                continue;
            }
            if verify(&distinct, key, user_ok, 0) {
                if found.is_some_and(|f| f != key) {
                    return None;
                }
                found = Some(key);
            }
        }
    }
    found
}

fn search_key(x1: u64, limit: u64, x2: u64, x3: u64, distinct: &[u64], steps: &mut u64) -> Option<u64> {
    let slack = distinct.len() / 1000;
    let mut base = 0u64;
    while base < limit {
        let mut hit = 0u32;
        for j in 0..16u64 {
            let key = x1 ^ ((base + j).wrapping_mul(A) & MASK);
            let a = ((x2 ^ key).wrapping_mul(AINV)) & MASK;
            let b = ((x3 ^ key).wrapping_mul(AINV)) & MASK;
            hit |= (((a < ROW_INDEX_LIMIT) & (b < ROW_INDEX_LIMIT)) as u32) << j;
        }
        if hit != 0 {
            for j in 0..16u64 {
                if hit >> j & 1 == 1 {
                    let key = x1 ^ ((base + j).wrapping_mul(A) & MASK);
                    if verify(distinct, key, |v| v < ROW_INDEX_LIMIT, slack) {
                        *steps += base + j + 1;
                        return Some(key);
                    }
                }
            }
        }
        base += 16;
    }
    *steps += limit;
    None
}

fn user_zero_rows(xs: impl Iterator<Item = (u64, Option<u64>)>, user_key: u64) -> Vec<u64> {
    let mut v: Vec<u64> = xs
        .filter(|(_, u)| u.is_some_and(|u| unkey(u, user_key) >> 9 == 0))
        .map(|(id, _)| id)
        .collect();
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(*x));
    v
}

fn probes(distinct: &[u64], not: u64) -> Option<(u64, u64)> {
    let mut it = distinct.iter().copied().filter(|&x| x != not);
    Some((it.next()?, it.last()?))
}

pub fn recover_event_key(s: &Sample, user_key: u64, window_start: Option<i64>, st: &mut Stats) -> Option<u64> {
    let distinct = sorted_unique(s.events.iter().map(|r| r.id_x).collect());
    if distinct.len() < MIN_DISTINCT {
        return None;
    }
    let mut rows: Vec<(i64, u64)> = s
        .events
        .iter()
        .filter(|r| r.user_x.is_some_and(|u| unkey(u, user_key) >> 9 == 0))
        .map(|r| {
            let t = match (r.ts, window_start) {
                (Some(t), Some(w)) if t >= w => t,
                (Some(t), None) => t,
                _ => i64::MAX,
            };
            (t, r.id_x)
        })
        .collect();
    rows.sort_unstable();
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| seen.insert(r.1));
    if rows.is_empty() {
        return None;
    }
    let tries: Vec<(u64, u64)> = rows.iter().take(4).map(|r| (r.1, 1 << 16)).chain(rows.iter().take(2).map(|r| (r.1, 1 << 27))).collect();
    for (x1, limit) in tries {
        let (x2, x3) = probes(&distinct, x1)?;
        if let Some(k) = search_key(x1, limit, x2, x3, &distinct, &mut st.event_steps) {
            return Some(k);
        }
    }
    None
}

pub fn recover_click_key(s: &Sample, user_key: u64, st: &mut Stats) -> Option<u64> {
    recover_click_key_lim(s, user_key, 1 << 27, st)
}
pub fn recover_click_key_lim(s: &Sample, user_key: u64, limit: u64, st: &mut Stats) -> Option<u64> {
    let distinct = sorted_unique(s.clicks.iter().map(|r| r.id_x).collect());
    if distinct.len() < MIN_DISTINCT {
        return None;
    }
    let rows = user_zero_rows(s.clicks.iter().map(|r| (r.id_x, r.user_x)), user_key);
    for &x1 in rows.iter().take(2) {
        let (x2, x3) = probes(&distinct, x1)?;
        if let Some(k) = search_key(x1, limit.min(1 << 27), x2, x3, &distinct, &mut st.click_steps) {
            return Some(k);
        }
    }
    None
}

pub fn recover(s: &Sample, window_start: Option<i64>, st: &mut Stats) -> Option<Keys> {
    let user = recover_user_key(s, st)?;
    let event = recover_event_key(s, user, window_start, st)?;
    let click = recover_click_key(s, user, st)?;
    Some(Keys { user, event, click })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        assert_eq!(A.wrapping_mul(AINV) & MASK, 1);
        assert_eq!(B.wrapping_mul(BINV) & MASK, 1);
        let mut seed = 12345u64;
        for _ in 0..200_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let v = seed >> 24;
            let key = seed.rotate_left(17);
            let y = scramble(v, key);
            assert_eq!(unscramble(y, key), v & MASK);
            let mut s = Vec::new();
            base36_encode(y, &mut s);
            assert_eq!(base36_decode(&s), Some(y));
            assert_eq!(base36_decode_swar(&s), Some(y));
        }
        for bad in [&b""[..], b"0a", b"A", b"a-", b"zzzzzzzzz", b"zzzzzzzz", b"a b", b"\xff"] {
            assert_eq!(base36_decode(bad), None, "{:?}", bad);
            assert_eq!(base36_decode_swar(bad), None, "{:?}", bad);
        }
        assert_eq!(base36_decode_swar(b"0"), Some(0));
        assert_eq!(base36_decode_swar(b"z"), Some(35));
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                let s = [a, b];
                assert_eq!(base36_decode(&s), base36_decode_swar(&s), "{:?}", s);
                let s3 = [b'a', a, b];
                assert_eq!(base36_decode(&s3), base36_decode_swar(&s3), "{:?}", s3);
                let s8 = [b'1', b'z', b'z', a, b'0', b, b'9', b'a'];
                assert_eq!(base36_decode(&s8), base36_decode_swar(&s8), "{:?}", s8);
                let s1 = [a];
                assert_eq!(base36_decode(&s1), base36_decode_swar(&s1), "{:?}", s1);
            }
        }
    }
    #[test]
    fn no_key_on_random_ids() {
        let mut seed = 99u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 20) & MASK
        };
        for trial in 0..20 {
            let mut s = Sample::default();
            let users: Vec<u64> = (0..2000).map(|_| next()).collect();
            for i in 0..20_000usize {
                let u = users[(i * i + trial) % (1 + i % 2000)];
                s.events.push(EventRow { id_x: next(), user_x: Some(u), ts: Some(i as i64) });
                s.clicks.push(ClickRow { id_x: next(), user_x: Some(u) });
            }
            let mut st = Stats::default();
            assert_eq!(recover(&s, None, &mut st), None);
        }
    }
    #[test]
    fn ts() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2027-02-29T00:00:00Z"), None);
        assert_eq!(parse_ts(b"2028-02-29T00:00:00Z").is_some(), true);
        assert_eq!(parse_ts(b"2026-10-03T23:58:10+0200"), None);
    }
}
