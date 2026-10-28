//! Fast hashing primitives.
//!
//! * [`xxh64`] — XXH64, used for block fingerprints and content verification.
//!   ~13 GB/s on a modern core for 4 KiB blocks, which is well above `NVMe`
//!   throughput, so hashing never becomes the bottleneck.
//! * [`crc32c`] — Castagnoli CRC with the SSE4.2 instruction when available.
//!   Slower per byte than XXH64 but a well-known interoperable checksum.

// Every `try_into().unwrap()` in this file converts a fixed-size slice into
// an array of the same size. That conversion cannot fail, and a `Result`
// would only add a branch to a loop that runs once per eight bytes.
#![allow(clippy::unwrap_used)]
// --- `unsafe` is allowed here, and only here -------------------------------
// The SSE4.2 path is one `#[target_feature]` function guarded by a
// runtime `is_x86_feature_detected!` check.
#![allow(unsafe_code)]

// ---------------------------------------------------------------- XXH64

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline(always)]
fn rd64(
    b: &[u8],
    i: usize,
) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

#[inline(always)]
fn rd32(
    b: &[u8],
    i: usize,
) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

#[inline(always)]
const fn round(
    acc: u64,
    input: u64,
) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

#[inline(always)]
const fn merge_round(
    acc: u64,
    val: u64,
) -> u64 {
    let v = round(0, val);
    (acc ^ v).wrapping_mul(P1).wrapping_add(P4)
}

#[inline(always)]
const fn avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// XXH64 with a seed.
#[must_use]
pub fn xxh64_seed(
    data: &[u8],
    seed: u64,
) -> u64 {
    let len = data.len();
    let mut h: u64;
    let mut p = 0usize;

    if len >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        let limit = len - 32;
        while p <= limit {
            v1 = round(v1, rd64(data, p));
            v2 = round(v2, rd64(data, p + 8));
            v3 = round(v3, rd64(data, p + 16));
            v4 = round(v4, rd64(data, p + 24));
            p += 32;
        }
        h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        h = merge_round(h, v4);
    } else {
        h = seed.wrapping_add(P5);
    }

    h = h.wrapping_add(len as u64);

    while p + 8 <= len {
        let k1 = round(0, rd64(data, p));
        h ^= k1;
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        p += 8;
    }
    if p + 4 <= len {
        h ^= u64::from(rd32(data, p)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        p += 4;
    }
    while p < len {
        h ^= u64::from(data[p]).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
        p += 1;
    }
    avalanche(h)
}

/// XXH64 with the canonical seed of 0.
#[inline]
#[must_use]
pub fn xxh64(data: &[u8]) -> u64 {
    xxh64_seed(data, 0)
}

// ---------------------------------------------------------------- CRC32C

const POLY: u32 = 0x82F6_3B78;

const fn make_table() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            k += 1;
        }
        t[0][i] = crc;
        i += 1;
    }
    let mut s = 1;
    while s < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            i += 1;
        }
        s += 1;
    }
    t
}

static TABLE: [[u32; 256]; 8] = make_table();

fn crc32c_sw(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    let mut i = 0usize;
    // A manual index loop instead of `chunks_exact`: the tail is handled by the
    // same loop bound, and the slice-of-8 keeps the table lookups branch-free.
    while i + 8 <= data.len() {
        let c = &data[i..i + 8];
        let lo = u32::from_le_bytes(c[0..4].try_into().unwrap()) ^ crc;
        let hi = u32::from_le_bytes(c[4..8].try_into().unwrap());
        crc = TABLE[7][(lo & 0xff) as usize]
            ^ TABLE[6][((lo >> 8) & 0xff) as usize]
            ^ TABLE[5][((lo >> 16) & 0xff) as usize]
            ^ TABLE[4][(lo >> 24) as usize]
            ^ TABLE[3][(hi & 0xff) as usize]
            ^ TABLE[2][((hi >> 8) & 0xff) as usize]
            ^ TABLE[1][((hi >> 16) & 0xff) as usize]
            ^ TABLE[0][(hi >> 24) as usize];
        i += 8;
    }
    for &b in &data[i..] {
        crc = (crc >> 8) ^ TABLE[0][((crc ^ u32::from(b)) & 0xff) as usize];
    }
    !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw(data: &[u8]) -> u32 {
    use core::arch::x86_64::_mm_crc32_u8;
    use core::arch::x86_64::_mm_crc32_u32;
    use core::arch::x86_64::_mm_crc32_u64;
    // The CRC32 instruction has 3-cycle latency and 1/cycle throughput, so a
    // single dependency chain runs at ~2.6 B/cycle. Interleaving several chains
    // would need crc32_combine (pclmulqdq) to stitch them back together; at these
    // block sizes the sequential chain already outruns any storage device.
    let mut crc = !0u64;
    let mut p = 0usize;
    while p + 8 <= data.len() {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(data[p..p + 8].try_into().unwrap()));
        p += 8;
    }
    let mut c32 = crc as u32;
    if p + 4 <= data.len() {
        c32 = _mm_crc32_u32(c32, u32::from_le_bytes(data[p..p + 4].try_into().unwrap()));
        p += 4;
    }
    while p < data.len() {
        // `_mm_crc32_u32` would consume four bytes; the tail needs one at a time.
        c32 = _mm_crc32_u8(c32, data[p]);
        p += 1;
    }
    !c32
}

/// CRC-32C (Castagnoli). Uses SSE4.2 when the CPU has it.
pub fn crc32c(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::atomic::AtomicU8;
        use std::sync::atomic::Ordering;
        static HAS: AtomicU8 = AtomicU8::new(0);
        let mut h = HAS.load(Ordering::Relaxed);
        if h == 0 {
            h = if std::is_x86_feature_detected!("sse4.2") {
                2
            } else {
                1
            };
            HAS.store(h, Ordering::Relaxed);
        }
        if h == 2 {
            // SAFETY: the feature was detected at runtime.
            return unsafe { crc32c_hw(data) };
        }
    }
    crc32c_sw(data)
}

/// Which checksum to use for verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HashKind {
    /// XXH64: fast, 64-bit, no hardware requirement.
    #[default]
    Xxh64,
    /// CRC-32C (Castagnoli): hardware-accelerated where the CPU has the
    /// `crc32` instruction (x86 `SSE4.2`, `ARMv8` CRC extensions).
    Crc32c,
}

impl HashKind {
    /// Digest `data` with this algorithm, widened to `u64` so both kinds share
    /// one index format.
    #[must_use]
    pub fn hash(
        self,
        data: &[u8],
    ) -> u64 {
        match self {
            | Self::Xxh64 => xxh64(data),
            | Self::Crc32c => u64::from(crc32c(data)),
        }
    }

    /// The spelling used by `--hash` and by the index file format.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            | Self::Xxh64 => "xxh64",
            | Self::Crc32c => "crc32c",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh64_vectors() {
        // Reference vectors from the xxHash test suite.
        assert_eq!(xxh64(b""), 0xEF46DB3751D8E999);
        assert_eq!(xxh64(b"a"), 0xD24EC4F1A98C6E5B);
        assert_eq!(xxh64(b"abc"), 0x44BC2CF5AD770999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition"),
            0xFBCEA83C8A378BF1
        );
    }

    #[test]
    fn crc32c_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE3069283);
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(crc32c(&data), crc32c_sw(&data));
    }

    #[test]
    fn xxh64_long_input_matches_streaming_shape() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 253) as u8).collect();
        let a = xxh64(&data);
        let b = xxh64(&data);
        assert_eq!(a, b);
        assert_ne!(xxh64(&data[..999]), a);
    }
}
