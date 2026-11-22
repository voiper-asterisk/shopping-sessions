//! Structural scanning: 64 bytes at a time, bitmasks of `\n`, `,` and `"`.

#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
#[inline(always)]
pub unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let v = _mm512_loadu_si512(p as *const __m512i);
    let nl = _mm512_set1_epi8(b"\n"[0] as i8);
    let cm = _mm512_set1_epi8(b","[0] as i8);
    let qt = _mm512_set1_epi8(b"\""[0] as i8);
    (
        _mm512_cmpeq_epi8_mask(v, nl),
        _mm512_cmpeq_epi8_mask(v, cm),
        _mm512_cmpeq_epi8_mask(v, qt),
    )
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(target_feature = "avx512bw")))]
#[inline(always)]
pub unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let nl = _mm256_set1_epi8(b'\n' as i8);
    let cm = _mm256_set1_epi8(b',' as i8);
    let qt = _mm256_set1_epi8(b'"' as i8);
    let m = |x: __m256i, c: __m256i| _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, c)) as u32 as u64;
    (m(a, nl) | (m(b, nl) << 32), m(a, cm) | (m(b, cm) << 32), m(a, qt) | (m(b, qt) << 32))
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline(always)]
pub unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::aarch64::*;
    // movemask emulation: AND with bit weights, then pairwise adds.
    let w: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let wv = vld1q_u8(w.as_ptr());
    let mm = |v: uint8x16_t| -> u64 {
        let x = vandq_u8(v, wv);
        let lo = vaddv_u8(vget_low_u8(x)) as u64;
        let hi = vaddv_u8(vget_high_u8(x)) as u64;
        lo | (hi << 8)
    };
    let mut nl = 0u64;
    let mut cm = 0u64;
    let mut qt = 0u64;
    for k in 0..4 {
        let v = vld1q_u8(p.add(16 * k));
        nl |= mm(vceqq_u8(v, vdupq_n_u8(b'\n'))) << (16 * k);
        cm |= mm(vceqq_u8(v, vdupq_n_u8(b','))) << (16 * k);
        qt |= mm(vceqq_u8(v, vdupq_n_u8(b'"'))) << (16 * k);
    }
    (nl, cm, qt)
}

#[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx2"), all(target_arch = "aarch64", target_feature = "neon"))))]
#[inline(always)]
pub unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    let (mut nl, mut cm, mut qt) = (0u64, 0u64, 0u64);
    for i in 0..64 {
        let c = *p.add(i);
        nl |= ((c == b'\n') as u64) << i;
        cm |= ((c == b',') as u64) << i;
        qt |= ((c == b'"') as u64) << i;
    }
    (nl, cm, qt)
}

/// Calls `f(line_start, line_end, commas, n_commas, n_quotes)` for every line
/// in `buf[start..end]` (`end` is a line boundary or the end of `buf`).
/// `commas` holds the first 8 comma positions of the line.
#[inline(always)]
pub fn scan_lines(buf: &[u8], start: usize, end: usize, mut f: impl FnMut(usize, usize, &[usize; 8], usize, u32)) {
    let len = buf.len();
    let base = buf.as_ptr();
    let mut commas = [0usize; 8];
    let mut nc = 0usize;
    let mut qc = 0u32;
    let mut ls = start;
    let mut p = start;
    let mut tail = [0u8; 64];
    while p < end {
        let (nl, cm, qm) = unsafe {
            if p + 64 <= len {
                masks(base.add(p))
            } else {
                tail = [0u8; 64];
                tail[..len - p].copy_from_slice(&buf[p..]);
                masks(tail.as_ptr())
            }
        };
        let valid = if end - p >= 64 { !0u64 } else { (1u64 << (end - p)) - 1 };
        let cm = cm & valid;
        let mut qrem = qm & valid;
        let mut seps = (nl | cm) & valid;
        while seps != 0 {
            let i = seps.trailing_zeros() as usize;
            let bit = 1u64 << i;
            seps &= seps - 1;
            if cm & bit != 0 {
                if nc < 8 {
                    commas[nc] = p + i;
                }
                nc += 1;
            } else {
                let below = bit - 1;
                qc += (qrem & below).count_ones();
                qrem &= !below;
                f(ls, p + i, &commas, nc, qc);
                ls = p + i + 1;
                nc = 0;
                qc = 0;
            }
        }
        qc += qrem.count_ones();
        p += 64;
    }
    if ls < end {
        f(ls, end, &commas, nc, qc);
    }
}

/// Field boundaries of a line with exactly N-1 commas, quotes removed.
/// Returns false if the quoting is malformed.
#[inline(always)]
pub fn fields<const N: usize>(buf: &[u8], ls: usize, le: usize, commas: &[usize; 8], qc: u32, out: &mut [(usize, usize); N]) -> bool {
    out[0].0 = ls;
    for k in 0..N - 1 {
        out[k].1 = commas[k];
        out[k + 1].0 = commas[k] + 1;
    }
    out[N - 1].1 = le;
    if qc == 0 {
        return true;
    }
    if ls == 0 || le >= buf.len() {
        return fields_slow(buf, qc, out);
    }
    // Branch-free: a field is quoted iff its first byte is a quote; then its
    // last byte must be one too (and not the same byte), and the line may
    // hold no other quotes. buf[s] and buf[e - 1] are always readable here.
    let mut nq = 0usize;
    let mut bad = 0usize;
    let p = buf.as_ptr();
    for f in out.iter_mut() {
        let (s, e) = *f;
        let qs = unsafe { *p.add(s) == b'"' } as usize;
        let qe = unsafe { *p.add(e - 1) == b'"' } as usize;
        bad |= qs ^ qe;
        bad |= ((e - s) < 2 * qs) as usize;
        nq += qs;
        *f = (s + qs, e - qs);
    }
    bad == 0 && qc as usize == 2 * nq
}

#[cold]
#[inline(never)]
fn fields_slow<const N: usize>(buf: &[u8], qc: u32, out: &mut [(usize, usize); N]) -> bool {
    let mut nq = 0u32;
    for f in out.iter_mut() {
        let (s, e) = *f;
        if s < e && buf[s] == b'"' {
            if e - s < 2 || buf[e - 1] != b'"' {
                return false;
            }
            *f = (s + 1, e - 1);
            nq += 1;
        }
    }
    qc == 2 * nq
}

#[inline(always)]
fn below(x: usize) -> u128 {
    if x >= 128 {
        !0u128
    } else {
        (1u128 << x) - 1
    }
}

/// Calls `f(fields)` for every well-formed line with exactly N fields in
/// `buf[start..end]`, with quotes already stripped. Malformed lines are skipped.
#[inline(always)]
pub fn scan_records<const N: usize>(buf: &[u8], start: usize, end: usize, mut f: impl FnMut(&[(usize, usize); N])) {
    let len = buf.len();
    let base = buf.as_ptr();
    let load = |p: usize| -> (u64, u64, u64) {
        if p >= end {
            return (0, 0, 0);
        }
        let (a, b, c) = unsafe {
            if p + 64 <= len {
                masks(base.add(p))
            } else {
                let mut tail = [0u8; 64];
                tail[..len - p].copy_from_slice(&buf[p..]);
                masks(tail.as_ptr())
            }
        };
        let v = if end - p >= 64 { !0u64 } else { (1u64 << (end - p)) - 1 };
        (a & v, b & v, c & v)
    };
    let mut p = start;
    let mut b0 = load(p);
    let mut b1 = load(p + 64);
    let mut b2 = load(p + 128);
    let mut cur = 0usize;
    let mut out = [(0usize, 0usize); N];
    loop {
        while cur >= 64 {
            p += 64;
            cur -= 64;
            b0 = b1;
            b1 = b2;
            b2 = load(p + 128);
        }
        let ls = p + cur;
        if ls >= end {
            break;
        }
        let c0 = cur;
        let win = move |x0: u64, x1: u64, x2: u64| -> u128 {
            let w = ((x0 as u128) | ((x1 as u128) << 64)) >> c0;
            if c0 == 0 {
                w
            } else {
                w | ((x2 as u128) << (128 - c0))
            }
        };
        let wn = win(b0.0, b1.0, b2.0);
        let le = if wn != 0 {
            wn.trailing_zeros() as usize
        } else if end - ls <= 128 {
            end - ls
        } else {
            // A line longer than the window: scalar path.
            let mut e = ls;
            while e < end && buf[e] != b'\n' {
                e += 1;
            }
            slow_line::<N>(buf, ls, e, &mut out, &mut f);
            let np = e + 1;
            p = np;
            cur = 0;
            b0 = load(p);
            b1 = load(p + 64);
            b2 = load(p + 128);
            continue;
        };
        let lm = below(le);
        let cb = win(b0.1, b1.1, b2.1) & lm;
        cur += le + 1;
        if cb.count_ones() as usize != N - 1 {
            continue;
        }
        let mut bits = cb;
        out[0].0 = 0;
        for k in 0..N - 1 {
            let c = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            out[k].1 = c;
            out[k + 1].0 = c + 1;
        }
        out[N - 1].1 = le;
        let qb = win(b0.2, b1.2, b2.2) & lm;
        if qb != 0 {
            let mut nq = 0u32;
            let mut ok = true;
            for fl in out.iter_mut() {
                let (s, e) = *fl;
                let qs = ((qb >> s) & 1) as usize;
                let ei = if e > s { e - 1 } else { e };
                let qe = ((qb >> ei) & 1) as usize;
                ok &= qs == qe && (qs == 0 || e - s >= 2);
                nq += qs as u32;
                *fl = (s + qs, e - qs);
            }
            if !ok || qb.count_ones() != 2 * nq {
                continue;
            }
        }
        for fl in out.iter_mut() {
            fl.0 += ls;
            fl.1 += ls;
        }
        f(&out);
    }
}

#[cold]
#[inline(never)]
fn slow_line<const N: usize>(buf: &[u8], ls: usize, le: usize, out: &mut [(usize, usize); N], f: &mut impl FnMut(&[(usize, usize); N])) {
    let mut commas = [0usize; 8];
    let mut nc = 0;
    let mut qc = 0u32;
    for i in ls..le {
        if buf[i] == b',' {
            if nc < 8 {
                commas[nc] = i;
            }
            nc += 1;
        } else if buf[i] == b'"' {
            qc += 1;
        }
    }
    if nc == N - 1 && fields::<N>(buf, ls, le, &commas, qc, out) {
        f(out);
    }
}
