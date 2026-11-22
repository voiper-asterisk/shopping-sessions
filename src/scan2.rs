//! Two-stage CSV scanning.
//!
//! Stage 1 (`index`) walks a chunk 64 bytes at a time and writes, without
//! data-dependent branches, the positions of every separator (`,` or `\n`) and
//! of every `\n`. The quoting rules are checked on the bitmasks: a quote must
//! be the first or last byte of a field, the two must pair up, and a field
//! cannot be a single quote. Offending positions go to a (normally empty) list.
//!
//! Stage 2 (`rows`) hands out every well-formed row with exactly N fields as
//! its N separator positions. Quotes, where present, are then known to wrap
//! whole fields, so the caller strips them by looking at one byte.

use crate::scan::masks;

pub struct Index {
    pub sep: Vec<u32>,
    pub nl: Vec<u32>,
    pub bad: Vec<u32>,
    nsep: usize,
    nnl: usize,
}

impl Index {
    pub fn new() -> Self {
        Index { sep: Vec::new(), nl: Vec::new(), bad: Vec::new(), nsep: 0, nnl: 0 }
    }
}

/// Indexes `buf[start..end]`, where `start` is a line start and `end` is just
/// past a `\n` or the end of `buf`. Positions are relative to `start`.
pub fn index(buf: &[u8], start: usize, end: usize, ix: &mut Index) {
    let len = buf.len();
    let base = buf.as_ptr();
    let n = end - start;
    assert!(n < (1 << 31));
    // a last line without `\n` gets a virtual one at `end`
    let virt = n > 0 && buf[end - 1] != b'\n';
    let vend = end + virt as usize;
    let cap = n + 160;
    if ix.sep.len() < cap {
        ix.sep.resize(cap, 0);
        ix.nl.resize(cap, 0);
    }
    ix.bad.clear();
    let sep = ix.sep.as_mut_ptr();
    let nlp = ix.nl.as_mut_ptr();
    let (mut nsep, mut nnl) = (0usize, 0usize);

    let load = |p: usize| -> (u64, u64, u64) {
        if p >= vend {
            return (0, 0, 0);
        }
        let (mut a, b, c) = unsafe {
            if p + 64 <= len {
                masks(base.add(p))
            } else if p < len {
                let mut tail = [0u8; 64];
                tail[..len - p].copy_from_slice(&buf[p..]);
                masks(tail.as_ptr())
            } else {
                (0, 0, 0)
            }
        };
        let (mut b, mut c) = (b, c);
        if end < p + 64 {
            let v = if end > p { (1u64 << (end - p)) - 1 } else { 0 };
            a &= v;
            b &= v;
            c &= v;
            if virt {
                a |= 1u64 << (end - p);
            }
        }
        (a, b, c)
    };

    let mut p = start;
    let mut prev_s63 = 1u64;
    let mut close_carry = 0u64;
    let mut cin = false;
    // One 64-byte block: its masks, and whether the byte after it is a separator.
    macro_rules! step {
        ($nl:expr, $cm:expr, $q:expr, $next_s0:expr) => {{
            let (nl, cm, q): (u64, u64, u64) = ($nl, $cm, $q);
            let s = nl | cm;
            let starts = (s << 1) | prev_s63;
            let ends = (s >> 1) | ($next_s0 << 63);
            let open = q & starts;
            let close = q & ends;
            let (x, c1) = (!s).overflowing_add(open);
            let (x, c2) = x.overflowing_add(cin as u64);
            let opened = x & s;
            let closed = ((close << 1) | close_carry) & s;
            let bad = (q & !(open | close)) | (open & close) | (opened ^ closed);
            cin = c1 | c2;
            close_carry = close >> 63;
            if bad != 0 {
                let mut m = bad;
                while m != 0 {
                    ix.bad.push((p - start) as u32 + m.trailing_zeros());
                    m &= m - 1;
                }
            }
            prev_s63 = s >> 63;
            let rel = (p - start) as u32;
            unsafe {
                let cnt = s.count_ones() as usize;
                let out = sep.add(nsep);
                let mut m = s;
                for k in 0..8 {
                    *out.add(k) = rel + m.trailing_zeros();
                    m &= m.wrapping_sub(1);
                }
                if cnt > 8 {
                    for k in 8..cnt {
                        *out.add(k) = rel + m.trailing_zeros();
                        m &= m.wrapping_sub(1);
                    }
                }
                nsep += cnt;
                let ncnt = nl.count_ones() as usize;
                let out = nlp.add(nnl);
                let mut m = nl;
                *out = rel + m.trailing_zeros();
                m &= m.wrapping_sub(1);
                *out.add(1) = rel + m.trailing_zeros();
                if ncnt > 2 {
                    m &= m.wrapping_sub(1);
                    for k in 2..ncnt {
                        *out.add(k) = rel + m.trailing_zeros();
                        m &= m.wrapping_sub(1);
                    }
                }
                nnl += ncnt;
            }
        }};
    }
    // Whole blocks strictly inside the chunk: no edge handling at all.
    while p + 64 < end {
        let (nl, cm, q) = unsafe { masks(base.add(p)) };
        let nb = unsafe { *base.add(p + 64) };
        step!(nl, cm, q, (nb == b',' || nb == b'\n') as u64);
        p += 64;
    }
    // The last block or two, with the general loader.
    let mut cur = load(p);
    while p < vend {
        let next = load(p + 64);
        step!(cur.0, cur.1, cur.2, (next.0 | next.1) & 1);
        cur = next;
        p += 64;
    }
    // sentinels
    for k in 0..16 {
        ix.sep[nsep + k] = u32::MAX;
    }
    ix.bad.push(u32::MAX);
    ix.nsep = nsep;
    ix.nnl = nnl;
}

/// Calls `f(row_start, seps)` for every well-formed row with exactly N fields:
/// `seps[i]` is the position of the separator ending field `i` (the last one is
/// the row's `\n`), all relative to the chunk start.
#[inline(always)]
pub fn rows<const N: usize>(ix: &Index, mut f: impl FnMut(u32, &[u32; N])) {
    let sep = ix.sep.as_ptr();
    let bad = ix.bad.as_ptr();
    let mut k = 0usize;
    let mut bi = 0usize;
    let mut rs = 0u32;
    for r in 0..ix.nnl {
        unsafe {
            let nl = *ix.nl.get_unchecked(r);
            let good = *sep.add(k + N - 1) == nl;
            let clean = *bad.add(bi) > nl;
            if good & clean {
                f(rs, &*(sep.add(k) as *const [u32; N]));
                k += N;
            } else {
                while *sep.add(k) != nl {
                    k += 1;
                }
                k += 1;
                while *bad.add(bi) <= nl {
                    bi += 1;
                }
            }
            rs = nl + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dialect, straight from the rules: split on commas; a field with a
    /// quote must be exactly `"` + quote-free bytes + `"`.
    fn reference<const N: usize>(buf: &[u8], start: usize, end: usize) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let body = &buf[start..end];
        let mut lines: Vec<&[u8]> = body.split(|&c| c == b'\n').collect();
        if body.is_empty() || body.ends_with(b"\n") {
            lines.pop();
        }
        for line in lines {
            let raw: Vec<&[u8]> = line.split(|&c| c == b',').collect();
            if raw.len() != N {
                continue;
            }
            let mut fields = Vec::new();
            let mut ok = true;
            for f in raw {
                if f.contains(&b'"') {
                    if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' && !f[1..f.len() - 1].contains(&b'"') {
                        fields.push(f[1..f.len() - 1].to_vec());
                    } else {
                        ok = false;
                    }
                } else {
                    fields.push(f.to_vec());
                }
            }
            if ok {
                out.push(fields);
            }
        }
        out
    }

    fn mine<const N: usize>(buf: &[u8], start: usize, end: usize) -> Vec<Vec<Vec<u8>>> {
        let mut ix = Index::new();
        index(buf, start, end, &mut ix);
        let mut out = Vec::new();
        rows::<N>(&ix, |rs, seps| {
            let mut fields = Vec::new();
            let mut s = start + rs as usize;
            for &e in seps.iter() {
                let mut e = start + e as usize;
                let next = e + 1;
                if s < e && buf[s] == b'"' {
                    s += 1;
                    e -= 1;
                }
                fields.push(buf[s..e].to_vec());
                s = next;
            }
            out.push(fields);
        });
        out
    }

    #[test]
    fn fuzz_rows() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: &[u8] = b"ab,,,,\"\"\n\nxyz0 \r";
        for iter in 0..200_000 {
            let len = (rnd() % 400) as usize;
            let dense = rnd() % 3;
            let mut buf: Vec<u8> = (0..len)
                .map(|_| {
                    let r = rnd();
                    match dense {
                        0 => alphabet[(r % alphabet.len() as u64) as usize],
                        1 => {
                            if r % 9 == 0 {
                                b','
                            } else if r % 23 == 0 {
                                b'"'
                            } else if r % 31 == 0 {
                                b'\n'
                            } else {
                                b'a' + (r % 20) as u8
                            }
                        }
                        _ => {
                            if r % 4 == 0 {
                                b','
                            } else if r % 5 == 0 {
                                b'"'
                            } else if r % 11 == 0 {
                                b'\n'
                            } else {
                                b'q'
                            }
                        }
                    }
                })
                .collect();
            // sometimes build well-formed quoted rows
            if iter % 4 == 0 {
                buf.clear();
                for _ in 0..(rnd() % 12) {
                    let nf = 1 + rnd() % 8;
                    for i in 0..nf {
                        if i > 0 {
                            buf.push(b',');
                        }
                        let q = rnd() % 4 == 0;
                        if q {
                            buf.push(b'"');
                        }
                        for _ in 0..(rnd() % 30) {
                            buf.push(b'a' + (rnd() % 26) as u8);
                        }
                        if q && rnd() % 20 != 0 {
                            buf.push(b'"');
                        }
                        if rnd() % 60 == 0 {
                            buf.push(b'"');
                        }
                    }
                    buf.push(b'\n');
                }
                if rnd() % 3 == 0 {
                    buf.pop();
                }
            }
            // start at a line start, end at a line end or the end of the buffer
            let mut starts: Vec<usize> = vec![0];
            starts.extend(buf.iter().enumerate().filter(|(_, &c)| c == b'\n').map(|(i, _)| i + 1));
            let a = starts[(rnd() % starts.len() as u64) as usize];
            let mut ends: Vec<usize> = starts.iter().copied().filter(|&e| e > a).collect();
            ends.push(buf.len());
            let e = ends[(rnd() % ends.len() as u64) as usize].max(a);
            assert_eq!(mine::<7>(&buf, a, e), reference::<7>(&buf, a, e), "N=7 {:?} [{a},{e})", String::from_utf8_lossy(&buf));
            assert_eq!(mine::<4>(&buf, a, e), reference::<4>(&buf, a, e), "N=4 {:?} [{a},{e})", String::from_utf8_lossy(&buf));
            assert_eq!(mine::<2>(&buf, a, e), reference::<2>(&buf, a, e), "N=2 {:?} [{a},{e})", String::from_utf8_lossy(&buf));
        }
    }
}
