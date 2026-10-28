//! A self-contained LZ4 block codec.
//!
//! Wire-compatible with the LZ4 block format (the one `LZ4_compress_default`
//! produces), so a compressed chunk can be handed to any LZ4 implementation. We
//! ship our own rather than pulling a dependency because:
//!
//! * the compressor keeps its hash table alive across calls, so a 1 MiB block does
//!   not pay for a 256 KiB allocation every time;
//! * the decompressor is written against a `&mut [u8]` with hard bounds checks and
//!   no panics, which matters when the input came from disk;
//! * it is ~300 lines we fully understand.
//!
//! Format reminder, one sequence at a time:
//! `token(1) [litlen-ext] literals offset(2) [matchlen-ext]`, where the token's
//! high nibble is `min(litlen,15)` and the low nibble is `min(matchlen-4,15)`.

// The `try_into().unwrap()` calls here all convert fixed-size slices; the
// length is guaranteed by the surrounding bounds check.
#![allow(clippy::unwrap_used)]

/// LZ4 minimum match length.
const MINMATCH: usize = 4;
/// The last 5 bytes of a block must always be literals.
const LASTLITERALS: usize = 5;
/// No match may start closer than this to the end.
const MFLIMIT: usize = 12;

/// Why a block could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum LzError {
    /// Output buffer too small.
    OutputTooSmall,
    /// Input ended in the middle of a sequence.
    Truncated,
    /// Offset pointed before the start of the output.
    BadOffset,
}

/// Worst-case output size for `n` input bytes.
#[inline]
#[must_use]
pub const fn max_compressed_size(n: usize) -> usize {
    n + n / 255 + 16
}

/// Reusable compressor state.
pub struct Compressor {
    ht: Vec<u32>,
    hlog: u32,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new(16)
    }
}

impl Compressor {
    /// `hlog` is the log2 of the match-finder table size; it is clamped to
    /// 12..=20 (4 K..1 M entries) so the table stays inside L2.
    #[must_use]
    pub fn new(hlog: u32) -> Self {
        let hlog = hlog.clamp(12, 20);
        Self {
            ht: vec![u32::MAX; 1 << hlog],
            hlog,
        }
    }

    #[inline(always)]
    const fn hash(
        &self,
        v: u32,
    ) -> u32 {
        v.wrapping_mul(2_654_435_761) >> (32 - self.hlog)
    }

    /// Compress `src` into `dst`.
    ///
    /// Returns the number of bytes written, or `None` when `dst` is smaller than
    /// `max_compressed_size(src.len())`.
    ///
    /// # Panics
    ///
    /// Never: the length check above bounds every index the encoder writes.
    // The encoder is one straight-line loop on purpose: the match search, the
    // literal run and the block terminator share state, and splitting them into
    // helpers costs more in register pressure than the line count is worth.
    #[allow(clippy::too_many_lines)]
    pub fn compress(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Option<usize> {
        let n = src.len();
        if dst.len() < max_compressed_size(n) {
            return None;
        }
        if n < MFLIMIT + 1 {
            let mut op = 0;
            op = emit_last_literals(src, 0, n, dst, op);
            return Some(op);
        }

        let mflimit = n - MFLIMIT;
        let matchlimit = n - LASTLITERALS;
        self.ht.fill(u32::MAX);

        let mut op = 0usize;
        let mut anchor = 0usize;
        let mut ip = 0usize;

        while ip < mflimit {
            // --- search for a 4-byte match, accelerating on misses
            let mut found: Option<(usize, usize)> = None;
            let mut probe = ip;
            let mut misses = 0u32;
            while probe < mflimit {
                let v = u32::from_le_bytes(src[probe..probe + 4].try_into().unwrap());
                let h = self.hash(v) as usize;
                let cand = self.ht[h];
                self.ht[h] = probe as u32;
                if cand != u32::MAX {
                    let r = cand as usize;
                    if r < probe
                        && probe - r <= 0xFFFF
                        && u32::from_le_bytes(src[r..r + 4].try_into().unwrap()) == v
                    {
                        found = Some((probe, r));
                        break;
                    }
                }
                misses += 1;
                // Skip faster when the data does not match (incompressible input).
                probe += if misses > 8 {
                    1 + (misses >> 5) as usize
                } else {
                    1
                };
            }

            let Some((mut mip, mut mref)) = found else {
                break;
            };

            // --- extend the match backwards into the pending literals
            while mip > anchor && mref > 0 && src[mip - 1] == src[mref - 1] {
                mip -= 1;
                mref -= 1;
            }

            // --- literal run
            let lit_len = mip - anchor;
            let token_pos = op;
            op += 1;
            let mut token: u8 = if lit_len >= 15 {
                0xF0
            } else {
                (lit_len as u8) << 4
            };
            if lit_len >= 15 {
                let mut rem = lit_len - 15;
                while rem >= 255 {
                    dst[op] = 255;
                    op += 1;
                    rem -= 255;
                }
                dst[op] = rem as u8;
                op += 1;
            }
            dst[op..op + lit_len].copy_from_slice(&src[anchor..mip]);
            op += lit_len;

            // --- offset (little endian, 1..=65535)
            let offset = mip - mref;
            dst[op] = (offset & 0xFF) as u8;
            dst[op + 1] = (offset >> 8) as u8;
            op += 2;

            // --- match length
            let mut mlen = MINMATCH;
            while mip + mlen < matchlimit && src[mref + mlen] == src[mip + mlen] {
                mlen += 1;
            }
            let ml_code = mlen - MINMATCH;
            if ml_code >= 15 {
                token |= 0x0F;
                let mut rem = ml_code - 15;
                while rem >= 255 {
                    dst[op] = 255;
                    op += 1;
                    rem -= 255;
                }
                dst[op] = rem as u8;
                op += 1;
            } else {
                token |= ml_code as u8;
            }
            dst[token_pos] = token;

            ip = mip + mlen;
            anchor = ip;

            // --- seed the table with the positions just behind the cursor
            if ip >= 2 && ip < mflimit {
                for back in [ip - 2, ip - 1] {
                    if back + 4 <= n {
                        let v = u32::from_le_bytes(src[back..back + 4].try_into().unwrap());
                        let h = self.hash(v) as usize;
                        self.ht[h] = back as u32;
                    }
                }
            }
        }

        op = emit_last_literals(src, anchor, n, dst, op);
        Some(op)
    }
}

/// Emit the trailing literal-only sequence `src[from..to]`.
fn emit_last_literals(
    src: &[u8],
    from: usize,
    to: usize,
    dst: &mut [u8],
    mut op: usize,
) -> usize {
    let lit_len = to - from;
    let token_pos = op;
    op += 1;
    let token: u8 = if lit_len >= 15 {
        0xF0
    } else {
        (lit_len as u8) << 4
    };
    if lit_len >= 15 {
        let mut rem = lit_len - 15;
        while rem >= 255 {
            dst[op] = 255;
            op += 1;
            rem -= 255;
        }
        dst[op] = rem as u8;
        op += 1;
    }
    dst[op..op + lit_len].copy_from_slice(&src[from..to]);
    op += lit_len;
    dst[token_pos] = token;
    op
}

/// Decompress an LZ4 block into exactly `dst.len()` bytes.
///
/// Returns the number of bytes produced, which is always `dst.len()` on success.
///
/// # Errors
///
/// [`LzError::Truncated`] if the input ends mid-sequence, [`LzError::BadOffset`]
/// if a match points before the start of the output, or
/// [`LzError::OutputTooSmall`] if the block expands past `dst`.
pub fn decompress(
    src: &[u8],
    dst: &mut [u8],
) -> Result<usize, LzError> {
    let mut ip = 0usize;
    let mut op = 0usize;
    let n = src.len();
    let cap = dst.len();

    loop {
        if ip >= n {
            return Err(LzError::Truncated);
        }
        let token = src[ip];
        ip += 1;

        // literals
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if ip >= n {
                    return Err(LzError::Truncated);
                }
                let b = src[ip];
                ip += 1;
                lit_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if op + lit_len > cap || ip + lit_len > n {
            return Err(LzError::OutputTooSmall);
        }
        dst[op..op + lit_len].copy_from_slice(&src[ip..ip + lit_len]);
        op += lit_len;
        ip += lit_len;

        if ip >= n {
            // end of block: the final sequence carries no match
            break;
        }
        if ip + 2 > n {
            return Err(LzError::Truncated);
        }
        let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(LzError::BadOffset);
        }

        let mut ml = (token & 0x0F) as usize;
        if ml == 15 {
            loop {
                if ip >= n {
                    return Err(LzError::Truncated);
                }
                let b = src[ip];
                ip += 1;
                ml += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let mlen = ml + MINMATCH;
        if op + mlen > cap {
            return Err(LzError::OutputTooSmall);
        }
        // Overlapping copies are legal and must be byte-by-byte.
        for i in 0..mlen {
            dst[op + i] = dst[op + i - offset];
        }
        op += mlen;
    }
    Ok(op)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> (usize, usize) {
        let mut c = Compressor::default();
        let mut buf = vec![0u8; max_compressed_size(data.len())];
        let n = c.compress(data, &mut buf).unwrap();
        let mut out = vec![0u8; data.len()];
        let m = decompress(&buf[..n], &mut out).unwrap();
        assert_eq!(m, data.len());
        assert_eq!(&out[..], data, "roundtrip mismatch");
        (data.len(), n)
    }

    #[test]
    fn empty_and_tiny() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"abc");
        roundtrip(b"0123456789ab");
    }

    #[test]
    fn repetitive() {
        let data = vec![b'x'; 100_000];
        let (n, c) = roundtrip(&data);
        assert!(c < n / 100, "expected strong compression, got {c} from {n}");
    }

    #[test]
    fn text() {
        let mut data = Vec::new();
        for i in 0..5000 {
            data.extend_from_slice(
                format!("line {i}: the quick brown fox jumps over the lazy dog\n").as_bytes(),
            );
        }
        let (n, c) = roundtrip(&data);
        assert!(c < n / 2, "expected decent compression, got {c} from {n}");
    }

    #[test]
    fn incompressible() {
        // xorshift noise: nothing should match, output stays ~input size
        let mut s = 0x1234_5678_9abc_def0u64;
        let mut data = vec![0u8; 200_000];
        for b in &mut data {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *b = s as u8;
        }
        let (n, c) = roundtrip(&data);
        assert!(c <= n + n / 255 + 16);
        assert!(c > n, "noise should not compress");
    }

    #[test]
    fn overlapping_runs() {
        // "abcabcabc..." exercises the overlapping copy path
        let data: Vec<u8> = (0..50_000).map(|i| b"abc"[(i % 3) as usize]).collect();
        roundtrip(&data);
    }

    #[test]
    fn rejects_corrupt_input() {
        let mut out = vec![0u8; 64];
        // offset pointing before the start
        assert_eq!(
            decompress(&[0x00, 0x01, 0x00], &mut out),
            Err(LzError::BadOffset)
        );
    }
}
