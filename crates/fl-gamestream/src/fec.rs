//! Reed-Solomon parity over GF(2^8), bit-compatible with the `nanors` library that
//! moonlight-common-c decodes with.
//!
//! nanors uses polynomial 0x11d (285) and a Cauchy coding matrix whose row `j`,
//! column `i` is `inv((parity_shards + i) ^ j)`. Parity shard `j` is the GF sum of
//! `M[j][i] * data[i]` over the data shards. Only encoding runs on the host.

use std::sync::OnceLock;

/// Data + parity shards per block are limited by GF(2^8).
pub const MAX_SHARDS: usize = 255;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    /// `mul[a][b] = a * b`; 64 KiB, built once.
    mul: Box<[[u8; 256]; 256]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().enumerate().take(255) {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut mul = Box::new([[0u8; 256]; 256]);
        for a in 1..256usize {
            for b in 1..256usize {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Tables { exp, log, mul }
    })
}

fn gf_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// An encoder for one `(data, parity)` geometry.
pub struct ReedSolomon {
    data: usize,
    parity: usize,
    /// Row-major `parity x data` coding matrix.
    matrix: Vec<u8>,
}

impl ReedSolomon {
    pub fn new(data: usize, parity: usize) -> Option<Self> {
        if data == 0 || parity == 0 || data + parity > MAX_SHARDS {
            return None;
        }
        let mut matrix = Vec::with_capacity(data * parity);
        for j in 0..parity {
            for i in 0..data {
                matrix.push(gf_inv(((parity + i) ^ j) as u8));
            }
        }
        Some(Self { data, parity, matrix })
    }

    /// Fill `parity` (`parity_shards * shard_len` bytes) from `data`
    /// (`data_shards * shard_len` bytes); shards are laid out back to back.
    pub fn encode(&self, data: &[u8], parity: &mut [u8], shard_len: usize) {
        assert_eq!(data.len(), self.data * shard_len);
        assert_eq!(parity.len(), self.parity * shard_len);
        if shard_len == 0 {
            return;
        }
        parity.fill(0);
        for (j, out) in parity.chunks_exact_mut(shard_len).enumerate() {
            let row = &self.matrix[j * self.data..(j + 1) * self.data];
            for (&c, shard) in row.iter().zip(data.chunks_exact(shard_len)) {
                mul_xor(c, shard, out);
            }
        }
    }
}

/// `dst ^= c * src` over GF(2^8). This is the only per-byte work of the video path, so
/// on the headset (aarch64) it runs 16 bytes at a time.
fn mul_xor(c: u8, src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    match c {
        0 => {}
        1 => dst.iter_mut().zip(src).for_each(|(o, d)| *o ^= d),
        _ => {
            #[cfg(target_arch = "aarch64")]
            let done = mul_xor_neon(c, src, dst);
            #[cfg(not(target_arch = "aarch64"))]
            let done = 0;
            mul_xor_scalar(c, &src[done..], &mut dst[done..]);
        }
    }
}

fn mul_xor_scalar(c: u8, src: &[u8], dst: &mut [u8]) {
    let row = &tables().mul[c as usize];
    dst.iter_mut().zip(src).for_each(|(o, d)| *o ^= row[*d as usize]);
}

/// Split-nibble table lookup (`c * x = c * (x & 15) ^ c * (x & 0xf0)`) with `TBL`.
/// Handles the leading whole 16-byte blocks and returns how many bytes that was.
#[cfg(target_arch = "aarch64")]
fn mul_xor_neon(c: u8, src: &[u8], dst: &mut [u8]) -> usize {
    use std::arch::aarch64::{vandq_u8, vdupq_n_u8, veorq_u8, vld1q_u8, vqtbl1q_u8, vshrq_n_u8, vst1q_u8};
    let row = &tables().mul[c as usize];
    let (mut lo, mut hi) = ([0u8; 16], [0u8; 16]);
    for i in 0..16 {
        lo[i] = row[i];
        hi[i] = row[i << 4];
    }
    let n = src.len().min(dst.len()) / 16 * 16;
    // SAFETY: NEON is part of the aarch64 baseline; every load and store below covers
    // 16 bytes inside `src[..n]` / `dst[..n]`.
    unsafe {
        let (tlo, thi, mask) = (vld1q_u8(lo.as_ptr()), vld1q_u8(hi.as_ptr()), vdupq_n_u8(0x0f));
        for (s, d) in src[..n].chunks_exact(16).zip(dst[..n].chunks_exact_mut(16)) {
            let x = vld1q_u8(s.as_ptr());
            let p = veorq_u8(vqtbl1q_u8(tlo, vandq_u8(x, mask)), vqtbl1q_u8(thi, vshrq_n_u8::<4>(x)));
            vst1q_u8(d.as_mut_ptr(), veorq_u8(vld1q_u8(d.as_ptr()), p));
        }
    }
    n
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn mul(a: u8, b: u8) -> u8 {
        tables().mul[a as usize][b as usize]
    }

    /// Recover missing data shards the way a decoder would: build the generator rows of
    /// the shards we hold (identity for data, the Cauchy rows for parity), invert, multiply.
    pub(crate) fn decode(
        rs: &ReedSolomon,
        shards: &mut [Option<Vec<u8>>],
        len: usize,
    ) -> Result<(), &'static str> {
        let k = rs.data;
        let present: Vec<usize> = (0..shards.len()).filter(|&i| shards[i].is_some()).take(k).collect();
        if present.len() < k {
            return Err("not enough shards");
        }
        // k x k matrix of the rows we have.
        let mut m = vec![0u8; k * k];
        for (r, &idx) in present.iter().enumerate() {
            if idx < k {
                m[r * k + idx] = 1;
            } else {
                m[r * k..r * k + k].copy_from_slice(&rs.matrix[(idx - k) * k..(idx - k) * k + k]);
            }
        }
        // Invert with Gauss-Jordan.
        let mut inv = vec![0u8; k * k];
        for i in 0..k {
            inv[i * k + i] = 1;
        }
        for col in 0..k {
            let pivot = (col..k).find(|&r| m[r * k + col] != 0).ok_or("singular")?;
            for c in 0..k {
                m.swap(col * k + c, pivot * k + c);
                inv.swap(col * k + c, pivot * k + c);
            }
            let p = gf_inv(m[col * k + col]);
            for c in 0..k {
                m[col * k + c] = mul(m[col * k + c], p);
                inv[col * k + c] = mul(inv[col * k + c], p);
            }
            for r in 0..k {
                if r != col && m[r * k + col] != 0 {
                    let f = m[r * k + col];
                    for c in 0..k {
                        m[r * k + c] ^= mul(f, m[col * k + c]);
                        inv[r * k + c] ^= mul(f, inv[col * k + c]);
                    }
                }
            }
        }
        let have: Vec<Vec<u8>> = present.iter().map(|&i| shards[i].clone().unwrap()).collect();
        for d in 0..k {
            if shards[d].is_none() {
                let mut out = vec![0u8; len];
                for (r, h) in have.iter().enumerate() {
                    let c = inv[d * k + r];
                    for (o, b) in out.iter_mut().zip(h) {
                        *o ^= mul(c, *b);
                    }
                }
                shards[d] = Some(out);
            }
        }
        Ok(())
    }

    #[test]
    fn gf_matches_nanors_tables() {
        // Spot values from nanors' gf2_8_tables.h (polynomial 285).
        assert_eq!(gf_inv(2), 142);
        assert_eq!(gf_inv(3), 244);
        assert_eq!(gf_inv(17), 114);
        assert_eq!(tables().exp[8], 29);
    }

    #[test]
    fn erasures_up_to_parity_count_recover() {
        // 33 bytes: two SIMD blocks plus a scalar tail.
        let (k, p, len) = (10, 4, 33);
        let rs = ReedSolomon::new(k, p).unwrap();
        let data: Vec<u8> = (0..k * len).map(|n| (n / len * 37 + n % len * 11 + 5) as u8).collect();
        let mut parity = vec![0xaau8; p * len];
        rs.encode(&data, &mut parity, len);
        let mut shards: Vec<Option<Vec<u8>>> =
            data.chunks(len).chain(parity.chunks(len)).map(|c| Some(c.to_vec())).collect();
        for drop in [0usize, 3, 7, 9] {
            shards[drop] = None;
        }
        decode(&rs, &mut shards, len).unwrap();
        for i in 0..k {
            assert_eq!(shards[i].as_deref().unwrap(), &data[i * len..(i + 1) * len], "shard {i}");
        }
    }

    #[test]
    fn mul_xor_matches_the_table_for_every_coefficient() {
        let src: Vec<u8> = (0..=255u8).chain(0..77).collect();
        for c in 0..=255u8 {
            let mut dst: Vec<u8> = src.iter().map(|b| b.wrapping_mul(3)).collect();
            mul_xor(c, &src, &mut dst);
            for (i, (&s, &d)) in src.iter().zip(&dst).enumerate() {
                assert_eq!(d, s.wrapping_mul(3) ^ mul(c, s), "c={c} i={i}");
            }
        }
    }

    /// The parity nanors (the library moonlight-common-c decodes with) produces for
    /// data shards `[1,2,3,4]`, `[5,6,7,8]`, `[9,10,11,12]` with two parity shards,
    /// computed here from its definition: row j, column i is `inv((parity + i) ^ j)`.
    #[test]
    fn parity_matches_the_nanors_matrix() {
        let rs = ReedSolomon::new(3, 2).unwrap();
        let data: Vec<u8> = (1..=12).collect();
        let mut parity = vec![0u8; 8];
        rs.encode(&data, &mut parity, 4);
        for j in 0..2usize {
            for b in 0..4 {
                let want = (0..3).fold(0u8, |acc, i| acc ^ mul(gf_inv(((2 + i) ^ j) as u8), data[i * 4 + b]));
                assert_eq!(parity[j * 4 + b], want);
            }
        }
    }
}
