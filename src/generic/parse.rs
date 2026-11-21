use super::util::{count_byte, eq_mask64, find_byte, find_sub};
use std::io;

pub type B<'a> = &'a [u8];

pub const DAY_MS: i64 = 86_400_000;

pub fn chunks<'a>(files: &[B<'a>], target: usize) -> Vec<B<'a>> {
    let target = target.max(1);
    let mut out = Vec::new();
    for &f in files {
        let body = match find_byte(f, b'\n') {
            Some(p) => &f[p + 1..],
            None => continue,
        };
        let mut s = 0;
        while s < body.len() {
            let mut e = s.saturating_add(target).min(body.len());
            if e < body.len() {
                e = match find_byte(&body[e..], b'\n') {
                    Some(p) => e + p + 1,
                    None => body.len(),
                };
            }
            out.push(&body[s..e]);
            s = e;
        }
    }
    out
}

pub fn chunk_target(files: &[B], ncpu: usize) -> usize {
    let total: usize = files.iter().map(|f| f.len()).sum();
    (total / (ncpu * 16).max(1)).clamp(1 << 20, 16 << 20)
}

#[inline]
pub fn count_lines(c: B) -> usize {
    count_byte(c, b'\n')
}

#[inline]
pub fn for_lines<'a>(chunk: B<'a>, mut f: impl FnMut(B<'a>)) {
    let n = chunk.len();
    let mut s = 0usize;
    let mut base = 0usize;
    while base + 64 <= n {
        let mut m = eq_mask64(&chunk[base..], b'\n');
        while m != 0 {
            let p = base + m.trailing_zeros() as usize;
            if p > s {
                f(&chunk[s..p]);
            }
            s = p + 1;
            m &= m - 1;
        }
        base += 64;
    }
    for p in base..n {
        if chunk[p] == b'\n' {
            if p > s {
                f(&chunk[s..p]);
            }
            s = p + 1;
        }
    }
    if s < n {
        f(&chunk[s..]);
    }
}

#[inline]
fn unquote(f: B, has_quote: bool) -> Option<B> {
    if !has_quote {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && !f[1..n - 1].contains(&b'"') {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

#[inline]
pub fn split<'a, const N: usize>(line: B<'a>) -> Option<[B<'a>; N]> {
    let mut out: [B<'a>; N] = [&[]; N];
    let mut k = 0usize;
    let mut start = 0usize;
    let mut q = false;
    for (i, &b) in line.iter().enumerate() {
        if b == b',' {
            if k + 1 >= N {
                return None;
            }
            out[k] = unquote(&line[start..i], q)?;
            k += 1;
            start = i + 1;
            q = false;
        } else if b == b'"' {
            q = true;
        }
    }
    if k + 1 != N {
        return None;
    }
    out[k] = unquote(&line[start..], q)?;
    Some(out)
}

#[inline]
pub unsafe fn first_two<'a>(p: &'a u8) -> (B<'a>, B<'a>) {
    let p0 = p as *const u8;
    let mut q = p0;
    while *q != b',' {
        q = q.add(1);
    }
    let f0 = std::slice::from_raw_parts(p0, q.offset_from(p0) as usize);
    let p1 = q.add(1);
    let mut q = p1;
    while *q != b',' {
        q = q.add(1);
    }
    let f1 = std::slice::from_raw_parts(p1, q.offset_from(p1) as usize);
    (strip(f0), strip(f1))
}

#[inline]
pub unsafe fn first<'a>(p: &'a u8) -> B<'a> {
    let p0 = p as *const u8;
    let mut q = p0;
    while *q != b',' {
        q = q.add(1);
    }
    strip(std::slice::from_raw_parts(p0, q.offset_from(p0) as usize))
}

#[inline]
fn strip(f: B) -> B {
    if !f.is_empty() && f[0] == b'"' {
        &f[1..f.len() - 1]
    } else {
        f
    }
}

#[inline]
fn d2(a: u8, b: u8) -> Option<i64> {
    if a.is_ascii_digit() && b.is_ascii_digit() {
        Some(((a - b'0') * 10 + (b - b'0')) as i64)
    } else {
        None
    }
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

#[inline]
fn civil(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s[0], s[1])? * 100 + d2(s[2], s[3])?;
    let mo = d2(s[5], s[6])?;
    let d = d2(s[8], s[9])?;
    let h = d2(s[11], s[12])?;
    let mi = d2(s[14], s[15])?;
    let se = d2(s[17], s[18])?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 || d < 1 {
        return None;
    }
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    };
    if d > dim {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) * 1000)
}

#[inline]
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    if s.iter().all(|b| b.is_ascii_digit()) {
        let mut i = 0;
        while i < s.len() && s[i] == b'0' {
            i += 1;
        }
        let r = &s[i..];
        if r.len() > 18 {
            return None;
        }
        let mut v: i64 = 0;
        for &b in r {
            v = v * 10 + (b - b'0') as i64;
        }
        return Some(v);
    }
    match s.len() {
        20 => {
            if s[19] != b'Z' {
                return None;
            }
            civil(s)
        }
        25 => {
            let sign = match s[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if s[22] != b':' {
                return None;
            }
            let oh = d2(s[20], s[21])?;
            let om = d2(s[23], s[24])?;
            if oh > 23 || om > 59 {
                return None;
            }
            civil(s).map(|t| t - sign * (oh * 3600 + om * 60) * 1000)
        }
        _ => None,
    }
}

#[inline]
pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut i = 0;
    while i < s.len() && s[i] == b'0' {
        i += 1;
    }
    let r = &s[i..];
    if r.len() > 8 {
        return None;
    }
    let mut v: u32 = 0;
    for &b in r {
        v = v * 10 + (b - b'0') as u32;
    }
    if (1..=10_000_000).contains(&v) {
        Some(v)
    } else {
        None
    }
}

#[inline]
pub fn day_of(ts: i64) -> i32 {
    ts.div_euclid(DAY_MS) as i32
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

pub fn manifest(data: &[u8]) -> io::Result<(i64, i64)> {
    let get = |key: &[u8]| -> io::Result<i64> {
        let k = String::from_utf8_lossy(key).into_owned();
        let pos = find_sub(data, key).ok_or_else(|| bad(format!("manifest: no {k}")))?;
        let rest = &data[pos + key.len()..];
        let colon = find_byte(rest, b':').ok_or_else(|| bad(format!("manifest: no colon after {k}")))?;
        let rest = &rest[colon + 1..];
        let q1 = find_byte(rest, b'"').ok_or_else(|| bad(format!("manifest: no value for {k}")))?;
        let rest = &rest[q1 + 1..];
        let q2 = find_byte(rest, b'"').ok_or_else(|| bad(format!("manifest: unterminated {k}")))?;
        parse_ts(&rest[..q2]).ok_or_else(|| bad(format!("manifest: bad timestamp {}", String::from_utf8_lossy(&rest[..q2]))))
    };
    Ok((get(b"\"window_start\"")?, get(b"\"window_end\"")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10z"), None);
        assert_eq!(parse_ts(b"2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_ts(b"2028-02-29T00:00:00Z").is_some(), true);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10+24:00"), None);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10+0200"), None);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-00:00"), parse_ts(b"2026-10-03T19:58:10Z"));
        assert_eq!(parse_ts(b"2026-10-03T24:00:00Z"), None);
        assert_eq!(parse_ts(b"2026-10-03T23:59:60Z"), None);
        assert_eq!(parse_ts(b"1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        assert_eq!(parse_price(b"10000001"), None);
        assert_eq!(parse_price(b"0"), None);
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], &b""[..]]));
        assert_eq!(split::<2>(b"a,b\""), None);
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a"), None);
        let l = b"\"e1\",u 1,x,y";
        let (a, b) = unsafe { first_two(&l[0]) };
        assert_eq!((a, b), (&b"e1"[..], &b"u 1"[..]));
        let l = b",\"\",x";
        let (a, b) = unsafe { first_two(&l[0]) };
        assert_eq!((a, b), (&b""[..], &b""[..]));
    }
    #[test]
    fn lines() {
        let mut s = Vec::new();
        for i in 0..300 {
            s.extend_from_slice(format!("line{i}\n").as_bytes());
            if i % 7 == 0 {
                s.extend_from_slice(b"\n\n");
            }
        }
        s.extend_from_slice(b"tail");
        let mut got = Vec::new();
        for_lines(&s, |l| got.push(l.to_vec()));
        assert_eq!(got.len(), 301);
        assert_eq!(got[300], b"tail".to_vec());
        assert_eq!(count_lines(&s), 300 + 2 * 43);
        assert_eq!(find_byte(&s[1..], b'\n'), Some(4));
    }
}
