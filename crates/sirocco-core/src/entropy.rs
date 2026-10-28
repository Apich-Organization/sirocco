//! Data characterisation: is this payload worth compressing?
//!
//! Entropy alone is a poor predictor (a JPEG and a compiled binary both look
//! "high entropy", but the binary compresses 2:1 with LZ). So we measure the real
//! thing: run the actual LZ4 compressor over a sample and look at the resulting
//! ratio. Entropy is kept as a cheap secondary signal and for reporting.

use crate::lz::Compressor;
use crate::lz::max_compressed_size;

/// What a sample of the payload looks like, and how well it actually compresses.
#[derive(Clone, Copy, Debug)]
pub struct DataProfile {
    /// Order-0 Shannon entropy in bits/byte (0..=8).
    pub entropy_bits: f64,
    /// Number of distinct byte values in the sample.
    pub distinct: u32,
    /// Fraction of zero bytes.
    pub zero_ratio: f64,
    /// Measured LZ4 ratio (`compressed / original`) on the sample.
    pub lz4_ratio: f64,
    /// Bytes of input the profile was measured on.
    pub sample_len: usize,
    /// Time spent compressing the sample, for throughput estimation.
    pub compress_ns: u64,
}

impl DataProfile {
    /// Effective compression throughput measured on the sample (bytes/s).
    #[must_use]
    pub fn compress_bps(&self) -> f64 {
        if self.compress_ns == 0 {
            return 0.0;
        }
        self.sample_len as f64 / (self.compress_ns as f64 / 1e9)
    }

    /// True when the payload looks like it has no exploitable structure.
    #[must_use]
    pub fn is_incompressible(&self) -> bool {
        self.lz4_ratio > 0.98
    }
}

/// Measure a sample. `scratch` must be at least `max_compressed_size(sample.len())`.
pub fn assess(
    sample: &[u8],
    comp: &mut Compressor,
    scratch: &mut [u8],
) -> DataProfile {
    if sample.is_empty() {
        return DataProfile {
            entropy_bits: 0.0,
            distinct: 0,
            zero_ratio: 0.0,
            lz4_ratio: 1.0,
            sample_len: 0,
            compress_ns: 0,
        };
    }
    let mut hist = [0u32; 256];
    let mut zeros = 0usize;
    for &b in sample {
        hist[b as usize] += 1;
        if b == 0 {
            zeros += 1;
        }
    }
    let n = sample.len() as f64;
    let mut entropy = 0.0;
    let mut distinct = 0;
    for &c in &hist {
        if c > 0 {
            distinct += 1;
            let p = f64::from(c) / n;
            entropy = p.mul_add(-p.log2(), entropy);
        }
    }

    let t0 = std::time::Instant::now();
    let wrote = if scratch.len() >= max_compressed_size(sample.len()) {
        comp.compress(sample, scratch).unwrap_or(sample.len())
    } else {
        sample.len()
    };
    let compress_ns = t0.elapsed().as_nanos() as u64;

    DataProfile {
        entropy_bits: entropy,
        distinct,
        zero_ratio: zeros as f64 / n,
        lz4_ratio: wrote as f64 / n,
        sample_len: sample.len(),
        compress_ns,
    }
}

/// Decide whether compressing this payload pays for itself.
///
/// Compressing costs `S / compress_bps` CPU time and turns `S` bytes of output
/// into `ratio * S`. It only wins when the write path is the bottleneck:
///
/// ```text
/// ratio * S / write_bps + S / compress_bps  <  S / write_bps
///   <=>  ratio < 1 - write_bps / compress_bps
/// ```
///
/// On a 3 GB/s `NVMe` and a 600 MB/s LZ4 this is never true, which is why
/// `--compress=auto` correctly stays off there. On a 150 MB/s HDD anything below
/// 0.75 wins immediately.
#[must_use]
pub fn compress_pays(
    ratio: f64,
    write_bps: f64,
    compress_bps: f64,
    safety: f64,
) -> bool {
    if compress_bps <= 0.0 || write_bps <= 0.0 {
        return false;
    }
    let threshold = 1.0 - write_bps / compress_bps;
    threshold > 0.0 && ratio < threshold * (1.0 - safety)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(data: &[u8]) -> DataProfile {
        let mut c = Compressor::default();
        let mut s = vec![0u8; max_compressed_size(data.len())];
        assess(data, &mut c, &mut s)
    }

    #[test]
    fn zeros_are_highly_compressible() {
        let p = prof(&vec![0u8; 65536]);
        assert!(p.entropy_bits < 0.01);
        assert!(p.lz4_ratio < 0.01);
        assert!(p.zero_ratio > 0.99);
    }

    #[test]
    fn noise_is_not() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..65536)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let p = prof(&data);
        assert!(p.entropy_bits > 7.9);
        assert!(p.is_incompressible());
    }

    #[test]
    fn cost_model() {
        // Fast NVMe: never worth compressing.
        assert!(!compress_pays(0.5, 3e9, 6e8, 0.1));
        // Slow HDD: a 50% ratio is a clear win.
        assert!(compress_pays(0.5, 1.5e8, 6e8, 0.1));
    }
}
