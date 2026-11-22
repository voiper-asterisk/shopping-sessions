//! IDs as 64-bit keys.
//!
//! An ID of at most 9 bytes, all in 0x01..=0x7F, packs exactly into 63 bits,
//! 7 bits a byte, first byte highest, zero padded. That packing is injective,
//! invertible, and its numeric order is the byte-wise order of the IDs.
//!
//! Every other ID (longer, non-ASCII, or holding a NUL) gets the top bit and a
//! 63-bit hash of its bytes: equal IDs have equal keys, and two different IDs
//! share a key only with probability about 2^-63 a pair. Where the bytes of
//! such an ID are needed (a person's name in the output, byte-wise order), they
//! are read back through a locator: a pointer into the mapped input file plus
//! a length. Key 0 is never produced.

use std::cmp::Ordering;

pub const SLOW_BIT: u64 = 1 << 63;

#[inline(always)]
unsafe fn rd(p: *const u8) -> u64 {
    std::ptr::read_unaligned(p as *const u64)
}

#[inline(always)]
fn mum(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

const S0: u64 = 0xA076_1D64_78BD_642F;
const S1: u64 = 0xE703_7ED1_A0B4_28DB;
const S2: u64 = 0x8EBC_6AF0_9C88_C6E3;
const S3: u64 = 0x5899_65CC_7537_4CC3;

/// A 64-bit hash of `len` bytes at `p`; reads nothing outside them.
#[inline(always)]
pub unsafe fn hash_id(p: *const u8, len: usize) -> u64 {
    let seed = S0 ^ (len as u64).wrapping_mul(S3);
    if len >= 16 {
        let mut h = seed;
        let mut i = 0;
        while i + 16 <= len {
            h = mum(rd(p.add(i)) ^ S1, rd(p.add(i + 8)) ^ h);
            i += 16;
        }
        // the last 16 bytes, overlapping what was read
        h = mum(rd(p.add(len - 16)) ^ S2, rd(p.add(len - 8)) ^ h);
        mum(h ^ S0, S1)
    } else if len >= 8 {
        mum(mum(rd(p) ^ S1, rd(p.add(len - 8)) ^ seed) ^ S2, S1)
    } else {
        let mut a = 0u64;
        for i in 0..len {
            a |= (*p.add(i) as u64) << (8 * i);
        }
        mum(mum(a ^ S1, seed) ^ S2, S1)
    }
}

/// Drops the top bit of each byte: 8 bytes become 56 bits.
#[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
#[inline(always)]
fn compress7(x: u64) -> u64 {
    unsafe { std::arch::x86_64::_pext_u64(x, 0x7F7F_7F7F_7F7F_7F7F) }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
#[inline(always)]
fn compress7(mut x: u64) -> u64 {
    x = ((x & 0x7F00_7F00_7F00_7F00) >> 1) | (x & 0x007F_007F_007F_007F);
    x = ((x & 0x3FFF_0000_3FFF_0000) >> 2) | (x & 0x0000_3FFF_0000_3FFF);
    x = ((x & 0x0FFF_FFFF_0000_0000) >> 4) | (x & 0x0000_0000_0FFF_FFFF);
    x
}

/// A mask of the low `len` bytes of a u64 (all of them for `len >= 8`).
#[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
#[inline(always)]
fn low_bytes(len: usize) -> u64 {
    unsafe { std::arch::x86_64::_bzhi_u64(!0u64, (len as u32) * 8) }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
#[inline(always)]
fn low_bytes(len: usize) -> u64 {
    if len >= 8 {
        !0u64
    } else {
        (1u64 << (len * 8)) - 1
    }
}

/// The key of a non-empty ID. Reads 9 bytes from `p` whatever `len` is: the
/// caller guarantees they are readable.
#[inline(always)]
pub unsafe fn id_key(p: *const u8, len: usize) -> u64 {
    debug_assert!(len > 0);
    if len <= 9 {
        let raw = std::ptr::read_unaligned(p as *const u64);
        let m = low_bytes(len);
        let lo = raw & m;
        let is9 = (len == 9) as u64;
        let ninth = (*p.add(8) as u64) & 0u64.wrapping_sub(is9);
        // every byte must be 0x01..=0x7F: no high bit, and no zero among the first `len`
        let x = lo | (!m & 0x0101_0101_0101_0101);
        let zero = x.wrapping_sub(0x0101_0101_0101_0101) & !x;
        let ninth_bad = (ninth.wrapping_sub(is9) | ninth) & 0x80; // ninth == 0 when present, or >= 0x80
        if ((lo | zero) & 0x8080_8080_8080_8080) | ninth_bad == 0 {
            return (compress7(lo.swap_bytes()) << 7) | ninth;
        }
    }
    SLOW_BIT | hash_id(p, len)
}

/// Safe variant for slices of any length.
pub fn id_key_slice(b: &[u8]) -> u64 {
    if b.len() <= 9 {
        let mut buf = [0u8; 16];
        buf[..b.len()].copy_from_slice(b);
        unsafe { id_key(buf.as_ptr(), b.len()) }
    } else {
        SLOW_BIT | unsafe { hash_id(b.as_ptr(), b.len()) }
    }
}

/// The bytes of a packed (non-hashed) key, first byte lowest, and their count.
#[inline(always)]
pub fn unpack_key(key: u64) -> (u64, u8, usize) {
    let len = 9 - (key.trailing_zeros() / 7).min(8) as usize;
    let mut x = key >> 7;
    x = ((x & 0x00FF_FFFF_F000_0000) << 4) | (x & 0x0000_0000_0FFF_FFFF);
    x = ((x & 0x0FFF_C000_0FFF_C000) << 2) | (x & 0x0000_3FFF_0000_3FFF);
    x = ((x & 0x3F80_3F80_3F80_3F80) << 1) | (x & 0x007F_007F_007F_007F);
    (x.swap_bytes(), (key & 0x7F) as u8, len)
}

/// Where an ID's bytes live: pointer (48 bits) and length (15 bits).
#[inline(always)]
pub fn locator(p: *const u8, len: usize) -> u64 {
    assert!(len < (1 << 15), "ID longer than 32767 bytes");
    ((len as u64) << 48) | (p as u64 & 0xFFFF_FFFF_FFFF)
}

/// The bytes behind a locator. They must still be mapped.
#[inline(always)]
pub unsafe fn loc_bytes<'a>(loc: u64) -> &'a [u8] {
    std::slice::from_raw_parts((loc & 0xFFFF_FFFF_FFFF) as *const u8, ((loc >> 48) & 0x7FFF) as usize)
}

/// Appends an ID's bytes: from the key if it is packed, else from its locator.
#[inline(always)]
pub fn id_bytes(key: u64, loc: u64, out: &mut Vec<u8>) {
    if key & SLOW_BIT == 0 {
        let (lo, ninth, len) = unpack_key(key);
        let mut tmp = [0u8; 9];
        tmp[..8].copy_from_slice(&lo.to_le_bytes());
        tmp[8] = ninth;
        out.extend_from_slice(&tmp[..len]);
    } else {
        out.extend_from_slice(unsafe { loc_bytes(loc) });
    }
}

/// An ID's bytes, in `buf` if the key is packed.
#[inline(always)]
fn bytes_of<'a>(key: u64, loc: u64, buf: &'a mut [u8; 9]) -> &'a [u8] {
    if key & SLOW_BIT == 0 {
        let (lo, ninth, len) = unpack_key(key);
        buf[..8].copy_from_slice(&lo.to_le_bytes());
        buf[8] = ninth;
        &buf[..len]
    } else {
        unsafe { loc_bytes(loc) }
    }
}

#[inline(never)]
fn cmp_slow(ka: u64, la: u64, kb: u64, lb: u64) -> Ordering {
    let (mut x, mut y) = ([0u8; 9], [0u8; 9]);
    bytes_of(ka, la, &mut x).cmp(bytes_of(kb, lb, &mut y))
}

/// Byte-wise order of two IDs given their keys and locators (a locator is only
/// looked at for a hashed key).
#[inline(always)]
pub fn cmp_ids(ka: u64, la: u64, kb: u64, lb: u64) -> Ordering {
    if (ka | kb) & SLOW_BIT == 0 {
        ka.cmp(&kb)
    } else {
        cmp_slow(ka, la, kb, lb)
    }
}

/// A click ID as one word: its packed key, or the top bit plus its locator.
#[inline(always)]
pub unsafe fn click_word(p: *const u8, len: usize, orig: *const u8) -> u64 {
    let k = id_key(p, len);
    if k & SLOW_BIT == 0 {
        k
    } else {
        SLOW_BIT | locator(orig, len)
    }
}

/// Byte-wise order of two click words.
#[inline(always)]
pub fn cmp_click(a: u64, b: u64) -> Ordering {
    cmp_ids(a, a & !SLOW_BIT, b, b & !SLOW_BIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_keep_order() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabets: [&[u8]; 3] = [b"0123456789abcdefghijklmnopqrstuvwxyz", b"abAB01 \t\x01\x7f", b"ab\x00\x80\xff\xc3\xa9z"];
        let mut all: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut seen: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
        for i in 0..300_000 {
            let al = alphabets[(i % 3) as usize];
            let len = 1 + (rnd() % 40) as usize;
            let s: Vec<u8> = (0..len).map(|_| al[(rnd() % al.len() as u64) as usize]).collect();
            let k = id_key_slice(&s);
            let mut padded = s.clone();
            padded.extend_from_slice(&[0xAA; 16]);
            let k2 = unsafe { id_key(padded.as_ptr(), s.len()) };
            assert_eq!(k, k2);
            // equal keys only for equal IDs
            if let Some(prev) = seen.insert(k, s.clone()) {
                assert_eq!(prev, s);
            }
            if k & SLOW_BIT == 0 {
                let mut back = Vec::new();
                id_bytes(k, 0, &mut back);
                assert_eq!(back, s);
            }
            all.push((s, k));
        }
        for w in all.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let (la, lb) = (locator(a.0.as_ptr(), a.0.len()), locator(b.0.as_ptr(), b.0.len()));
            assert_eq!(cmp_ids(a.1, la, b.1, lb), a.0.cmp(&b.0), "{:?} {:?}", a.0, b.0);
            let (ca, cb) = unsafe {
                let mut pa = a.0.clone();
                pa.extend_from_slice(&[0; 16]);
                let mut pb = b.0.clone();
                pb.extend_from_slice(&[0; 16]);
                (click_word(pa.as_ptr(), a.0.len(), a.0.as_ptr()), click_word(pb.as_ptr(), b.0.len(), b.0.as_ptr()))
            };
            assert_eq!(cmp_click(ca, cb), a.0.cmp(&b.0));
        }
    }
}
