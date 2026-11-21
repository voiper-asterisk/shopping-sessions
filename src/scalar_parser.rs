
use crate::iface::*;

pub struct Ref;

impl Kernel for Ref {
    const NAME: &'static str = "ref";
    fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
        events_with(buf, |row, _, _| sink.event(row));
    }
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
        clicks_with(buf, |row, _, _| sink.click(row));
    }
}

pub fn for_each_line(buf: &[u8], mut f: impl FnMut(usize, usize)) {
    let mut start = 0;
    while start < buf.len() {
        let end = match buf[start..].iter().position(|&b| b == b'\n') {
            Some(p) => start + p,
            None => buf.len(),
        };
        f(start, end);
        start = end + 1;
    }
}

fn field_value(buf: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
    let raw = &buf[s..e];
    let quotes = raw.iter().filter(|&&b| b == b'"').count();
    if quotes == 0 {
        return Some((s, e));
    }
    if quotes == 2 && raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        return Some((s + 1, e - 1));
    }
    None
}

pub fn split_line<const N: usize>(buf: &[u8], start: usize, end: usize) -> Option<[(usize, usize); N]> {
    if start == end {
        return None;
    }
    let mut out = [(0usize, 0usize); N];
    let mut count = 0;
    let mut field_start = start;
    let mut i = start;
    loop {
        if i == end || buf[i] == b',' {
            if count == N {
                return None;
            }
            out[count] = field_value(buf, field_start, i)?;
            count += 1;
            if i == end {
                break;
            }
            field_start = i + 1;
        }
        i += 1;
    }
    if count != N {
        return None;
    }
    Some(out)
}

pub fn parse_id(buf: &[u8], span: (usize, usize), prefix: u8) -> u64 {
    let (s, e) = span;
    let v = &buf[s..e];
    if v.is_empty() {
        return ID_EMPTY;
    }
    let digits = &v[1..];
    let canonical = v[0] == prefix
        && (1..=8).contains(&digits.len())
        && digits.iter().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && !(digits[0] == b'0' && digits.len() > 1);
    if !canonical {
        return odd_id(s, v.len());
    }
    let mut value: u64 = 0;
    for &b in digits {
        let d = if b.is_ascii_digit() { b - b'0' } else { b - b'a' + 10 };
        value = value * 36 + d as u64;
    }
    value
}

fn canonical_decimal(d: &[u8]) -> Option<u32> {
    if !(1..=8).contains(&d.len()) || !d.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if d[0] == b'0' && d.len() > 1 {
        return None;
    }
    Some(d.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32))
}

pub fn parse_product(v: &[u8]) -> u32 {
    if v.is_empty() {
        return PROD_EMPTY;
    }
    if v[0] != b'p' {
        return PROD_ODD;
    }
    canonical_decimal(&v[1..]).unwrap_or(PROD_ODD)
}

pub fn parse_campaign(v: &[u8]) -> u32 {
    if v.is_empty() {
        return CAMP_EMPTY;
    }
    if !v.starts_with(b"c_") {
        return CAMP_ODD;
    }
    canonical_decimal(&v[2..]).unwrap_or(CAMP_ODD)
}

pub fn parse_kind(v: &[u8]) -> u8 {
    match v {
        b"view" => KIND_VIEW,
        b"cart" => KIND_CART,
        b"purchase" => KIND_PURCHASE,
        _ => KIND_OTHER,
    }
}

pub fn parse_price(v: &[u8]) -> u32 {
    if v.is_empty() || !v.iter().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let first_nonzero = v.iter().position(|&b| b != b'0').unwrap_or(v.len());
    let significant = &v[first_nonzero..];
    if significant.len() > 8 {
        return 0;
    }
    let value = significant.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32);
    if (1..=10_000_000).contains(&value) { value } else { 0 }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 0,
    }
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn digits_value(v: &[u8]) -> Option<i64> {
    if v.iter().all(|b| b.is_ascii_digit()) {
        Some(v.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64))
    } else {
        None
    }
}

fn civil_ms(v: &[u8]) -> Option<i64> {
    if v.len() != 19 || v[4] != b'-' || v[7] != b'-' || v[10] != b'T' || v[13] != b':' || v[16] != b':' {
        return None;
    }
    let y = digits_value(&v[0..4])?;
    let mo = digits_value(&v[5..7])?;
    let d = digits_value(&v[8..10])?;
    let h = digits_value(&v[11..13])?;
    let mi = digits_value(&v[14..16])?;
    let s = digits_value(&v[17..19])?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400_000 + ((h * 60 + mi) * 60 + s) * 1000)
}

pub fn parse_f1(v: &[u8]) -> i64 {
    if v.len() == 20 && v[19] == b'Z' {
        if let Some(ms) = civil_ms(&v[..19]) {
            return ms;
        }
    }
    TS_BAD
}

pub fn parse_ts(v: &[u8]) -> i64 {
    if !v.is_empty() && v.iter().all(|b| b.is_ascii_digit()) {
        let mut value: u128 = 0;
        for &b in v {
            value = (value * 10 + (b - b'0') as u128).min(u64::MAX as u128);
        }
        return value.min(i64::MAX as u128) as i64;
    }
    if v.len() == 20 {
        return parse_f1(v);
    }
    if v.len() == 25 && (v[19] == b'+' || v[19] == b'-') && v[22] == b':' {
        let Some(local) = civil_ms(&v[..19]) else { return TS_BAD };
        let (Some(oh), Some(om)) = (digits_value(&v[20..22]), digits_value(&v[23..25])) else {
            return TS_BAD;
        };
        if oh > 23 || om > 59 {
            return TS_BAD;
        }
        let offset = (oh * 60 + om) * 60_000;
        return if v[19] == b'+' { local - offset } else { local + offset };
    }
    TS_BAD
}

pub fn events_with(buf: &[u8], mut f: impl FnMut(&EvRow, usize, usize)) {
    for_each_line(buf, |start, end| {
        let Some(v) = split_line::<7>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let kind = parse_kind(val(4));
        let row = EvRow {
            eid: parse_id(buf, v[0], b'e'),
            uid: parse_id(buf, v[1], b'u'),
            ts: parse_ts(val(2)),
            ingest: parse_f1(val(3)),
            kind,
            product: parse_product(val(5)),
            price: if kind == KIND_PURCHASE { parse_price(val(6)) } else { 0 },
        };
        f(&row, start, end);
    });
}

pub fn clicks_with(buf: &[u8], mut f: impl FnMut(&ClRow, usize, usize)) {
    for_each_line(buf, |start, end| {
        let Some(v) = split_line::<4>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let row = ClRow {
            cid: parse_id(buf, v[0], b'k'),
            uid: parse_id(buf, v[1], b'u'),
            campaign: parse_campaign(val(2)),
            ts: parse_ts(val(3)),
        };
        f(&row, start, end);
    });
}

pub fn event_lines(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    events_with(buf, |_, s, e| out.push((s, e)));
    out
}
pub fn click_lines(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    clicks_with(buf, |_, s, e| out.push((s, e)));
    out
}
