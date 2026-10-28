//! The incremental-sync decision.
//!
//! # The problem
//!
//! A file exists on both sides. We know nothing about how similar the two copies
//! are, and finding out exactly costs a full read of both. So we must choose
//! between three strategies using *incomplete* information:
//!
//! | strategy | cost |
//! |---|---|
//! | full overwrite | `S/R + S/W` |
//! | block delta | `S/R + S/Rd + r·S/W` |
//! | append (prefix identical) | `(S-D)/R + (S-D)/W` |
//!
//! where `r` is the fraction of differing blocks — the unknown.
//!
//! # The estimator
//!
//! We take `k` block-aligned probes at deterministic pseudo-random offsets, read
//! both sides, and count how many differ. `r̂ = changed/k` is an estimate of `r`,
//! and because `k` is small the point estimate is not enough: a file with 1 changed
//! block in 1000 will show 0 changes in 64 probes. So we use the **Wilson score
//! upper bound** at 95% confidence and compare *that* against the break-even point.
//! Being wrong in the "delta" direction costs a wasted read of the destination;
//! being wrong in the "full" direction costs the writes we were trying to avoid.
//! The upper bound biases us toward the safe choice, which is full copy.
//!
//! # Why block delta rarely wins on fast local media
//!
//! Delta beats full exactly when `r < 1 − W/Rd`. On `NVMe` `W ≈ Rd`, so the
//! threshold collapses to zero and *nothing* wins. The technique only pays off
//! when the destination is materially slower than its own read path — a network
//! share, a USB stick, an SMR drive, or a copy into a filesystem with expensive
//! random writes. Sirocco measures both rates at startup and lets the inequality
//! decide, instead of guessing.
//!
//! The append case is different: it never needs to read the common prefix at all,
//! so it wins whenever the file grew and the prefix is intact. That is the case
//! worth optimising, and it is what makes repeated syncs of logs, VM images and
//! append-only datasets cheap.

use std::fmt;

/// Measured (or assumed) throughputs, bytes/second.
#[derive(Clone, Copy, Debug)]
pub struct IoProfile {
    /// Sequential read throughput of the source volume.
    pub read_bps: f64,
    /// Sequential write throughput of the destination volume.
    pub write_bps: f64,
    /// Read throughput *of the destination* volume, which is what a block delta
    /// pays to compare against.
    pub dest_read_bps: f64,
}

impl Default for IoProfile {
    fn default() -> Self {
        // Conservative defaults: an ordinary SATA SSD.
        Self {
            read_bps: 500.0e6,
            write_bps: 450.0e6,
            dest_read_bps: 500.0e6,
        }
    }
}

impl IoProfile {
    /// Build a profile from measured values. Zero or negative rates are clamped
    /// to 1 B/s so the cost model can divide by them.
    #[must_use]
    pub const fn new(
        read_bps: f64,
        write_bps: f64,
        dest_read_bps: f64,
    ) -> Self {
        Self {
            read_bps: read_bps.max(1.0),
            write_bps: write_bps.max(1.0),
            dest_read_bps: dest_read_bps.max(1.0),
        }
    }
}

/// Outcome of the sampling phase.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeResult {
    /// How many sub-blocks were sampled.
    pub probes: u32,
    /// How many of them differed between source and destination.
    pub changed: u32,
    /// Bytes actually read during probing (both sides).
    pub bytes_read: u64,
}

impl ProbeResult {
    /// The raw changed fraction. An empty sample counts as fully changed, which
    /// makes the caller fall back to a full copy rather than assume a match.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        if self.probes == 0 {
            1.0
        } else {
            f64::from(self.changed) / f64::from(self.probes)
        }
    }
}

/// What to do with a file that exists on both sides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// Byte-for-byte overwrite.
    Full,
    /// Copy only the differing blocks. `est` is the estimated changed fraction.
    Delta {
        /// Estimated fraction of blocks that differ, in `0.0..=1.0`.
        est: f64,
    },
    /// Destination is a strict prefix of the source: copy the tail only.
    Append {
        /// Offset in the source from which the destination stops matching.
        from: u64,
    },
}

impl fmt::Display for Decision {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            | Self::Full => write!(f, "full"),
            | Self::Delta { est } => write!(f, "delta({:.0}% changed)", est * 100.0),
            | Self::Append { from } => write!(f, "append(from {from})"),
        }
    }
}

/// Tuning for the estimator.
#[derive(Clone, Copy, Debug)]
pub struct DeltaCfg {
    /// Files below this size are always copied whole; probing overhead dominates.
    pub min_size: u64,
    /// Comparison granularity.
    pub block: u64,
    /// Number of probes.
    pub probes: u32,
    /// Confidence z-score (1.96 = 95%).
    pub z: f64,
    /// Extra safety margin subtracted from the break-even threshold.
    pub margin: f64,
    /// Correction exponent for sub-block probing.
    ///
    /// We probe at `probe_unit` granularity but compare at `block` granularity. A
    /// block counts as changed when *any* of its `block/probe_unit` sub-blocks
    /// changed, so `r_block = 1 - (1 - r_probe)^inflate`. Setting this to 1
    /// disables the correction.
    pub inflate: f64,
}

impl Default for DeltaCfg {
    fn default() -> Self {
        Self {
            min_size: 8 << 20,
            block: 1 << 20,
            probes: 32,
            z: 1.96,
            margin: 0.10,
            inflate: 1.0,
        }
    }
}

/// Wilson score interval upper bound for a binomial proportion.
#[must_use]
pub fn wilson_upper(
    changed: u32,
    n: u32,
    z: f64,
) -> f64 {
    if n == 0 {
        return 1.0;
    }
    let nf = f64::from(n);
    let p = f64::from(changed) / nf;
    let z2 = z * z;
    let denom = 1.0 + z2 / nf;
    let center = p + z2 / (2.0 * nf);
    let margin = z * (p * (1.0 - p) / nf + z2 / (4.0 * nf * nf)).sqrt();
    ((center + margin) / denom).clamp(0.0, 1.0)
}

/// Break-even changed-fraction: delta wins below this value.
#[must_use]
pub fn break_even(prof: &IoProfile) -> f64 {
    1.0 - prof.write_bps / prof.dest_read_bps
}

/// Seconds to copy `s` bytes from scratch.
#[inline]
#[must_use]
pub fn cost_full(
    s: u64,
    prof: &IoProfile,
) -> f64 {
    let s = s as f64;
    s / prof.read_bps + s / prof.write_bps
}

/// Seconds to run a block delta over `s` bytes with a changed fraction `r`.
#[inline]
#[must_use]
pub fn cost_delta(
    s: u64,
    r: f64,
    prof: &IoProfile,
) -> f64 {
    let s = s as f64;
    s / prof.read_bps + s / prof.dest_read_bps + r * s / prof.write_bps
}

/// Seconds to append `s - d` bytes.
#[inline]
#[must_use]
pub fn cost_append(
    s: u64,
    d: u64,
    prof: &IoProfile,
) -> f64 {
    let n = s.saturating_sub(d) as f64;
    n / prof.read_bps + n / prof.write_bps
}

/// Decide how to update an existing destination.
///
/// `probe` must have been taken over the **overlap** region, i.e. offsets inside
/// `[0, min(src_size, dst_size))`.
#[must_use]
pub fn decide(
    src_size: u64,
    dst_size: u64,
    probe: ProbeResult,
    prof: &IoProfile,
    cfg: &DeltaCfg,
) -> Decision {
    if src_size == 0 {
        return Decision::Full;
    }
    if src_size < cfg.min_size {
        return Decision::Full;
    }

    // Growth with an intact prefix is the cheapest possible update.
    if dst_size < src_size && probe.probes > 0 && probe.changed == 0 {
        // The engine must still verify the tail before trusting this.
        return Decision::Append { from: dst_size };
    }

    let r_wilson = wilson_upper(probe.changed, probe.probes, cfg.z);
    // Lift the sub-block estimate to block granularity.
    let r_upper = if cfg.inflate > 1.0 {
        1.0 - (1.0 - r_wilson).powf(cfg.inflate)
    } else {
        r_wilson
    };
    let be = break_even(prof);
    let threshold = (be - cfg.margin).max(0.0);

    if r_upper >= threshold {
        return Decision::Full;
    }
    let c_full = cost_full(src_size, prof);
    let c_delta = cost_delta(src_size, r_upper, prof);
    if c_delta < c_full {
        Decision::Delta { est: r_upper }
    } else {
        Decision::Full
    }
}

/// Deterministic probe offsets (block aligned) for a file of `size` bytes.
///
/// Determinism matters: two runs over the same file must make the same decision,
/// otherwise incremental sync becomes unpredictable.
#[must_use]
pub fn probe_offsets(
    size: u64,
    block: u64,
    count: u32,
    seed: u64,
) -> Vec<u64> {
    let block = block.max(512);
    if size == 0 {
        return Vec::new();
    }
    let n_blocks = size.div_ceil(block);
    let count = u64::from(count).min(n_blocks) as usize;
    if count == 0 {
        return Vec::new();
    }
    let last = (n_blocks - 1) * block;
    if count == 1 {
        return vec![0];
    }
    let mut out = Vec::with_capacity(count);
    out.push(0);
    out.push(last);
    let mut s = seed | 1;
    while out.len() < count {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let b = s % n_blocks;
        let off = b * block;
        if !out.contains(&off) {
            out.push(off);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Seed derived from stable file identity, so the same file always gets the same
/// probe pattern.
#[must_use]
pub fn probe_seed(
    size: u64,
    mtime_ns: u128,
    name_hash: u64,
) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in [size, mtime_ns as u64, (mtime_ns >> 64) as u64, name_hash] {
        h ^= v;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Block ranges covering `[0, size)`.
pub fn block_ranges(
    size: u64,
    block: u64,
) -> impl Iterator<Item = (u64, u64)> {
    let block = block.max(512);
    let n = size.div_ceil(block);
    (0..n).map(move |i| {
        let off = i * block;
        (off, block.min(size - off))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_is_monotone_and_bounded() {
        assert!(wilson_upper(0, 32, 1.96) < 0.15);
        assert!(wilson_upper(16, 32, 1.96) > 0.5);
        assert_eq!(wilson_upper(0, 0, 1.96), 1.0);
        let a = wilson_upper(1, 32, 1.96);
        let b = wilson_upper(2, 32, 1.96);
        assert!(b > a);
    }

    #[test]
    fn fast_local_media_never_picks_delta() {
        let prof = IoProfile::new(3.0e9, 2.8e9, 3.0e9);
        let cfg = DeltaCfg::default();
        // Even 1 changed block in 32 is enough to prefer the full copy.
        let d = decide(
            1 << 30,
            1 << 30,
            ProbeResult {
                probes: 32,
                changed: 1,
                bytes_read: 0,
            },
            &prof,
            &cfg,
        );
        assert_eq!(d, Decision::Full);
        // And with zero changed blocks there is nothing to gain either.
        let d = decide(
            1 << 30,
            1 << 30,
            ProbeResult {
                probes: 32,
                changed: 0,
                bytes_read: 0,
            },
            &prof,
            &cfg,
        );
        assert_eq!(d, Decision::Full);
    }

    #[test]
    fn slow_destination_picks_delta() {
        // 3 GB/s source, 150 MB/s destination write, 500 MB/s destination read.
        let prof = IoProfile::new(3.0e9, 150.0e6, 500.0e6);
        let cfg = DeltaCfg::default();
        let d = decide(
            1 << 30,
            1 << 30,
            ProbeResult {
                probes: 32,
                changed: 2,
                bytes_read: 0,
            },
            &prof,
            &cfg,
        );
        match d {
            | Decision::Delta { est } => assert!(est < 0.35),
            | other => panic!("expected delta, got {other:?}"),
        }
    }

    #[test]
    fn growth_with_intact_prefix_appends() {
        let prof = IoProfile::default();
        let cfg = DeltaCfg::default();
        let d = decide(
            100 << 20,
            60 << 20,
            ProbeResult {
                probes: 32,
                changed: 0,
                bytes_read: 0,
            },
            &prof,
            &cfg,
        );
        assert_eq!(d, Decision::Append { from: 60 << 20 });
    }

    #[test]
    fn small_files_are_always_full() {
        let prof = IoProfile::default();
        let cfg = DeltaCfg::default();
        let d = decide(
            4096,
            2048,
            ProbeResult {
                probes: 2,
                changed: 0,
                bytes_read: 0,
            },
            &prof,
            &cfg,
        );
        assert_eq!(d, Decision::Full);
    }

    #[test]
    fn probe_offsets_are_deterministic_and_aligned() {
        let a = probe_offsets(100 << 20, 1 << 20, 32, 12345);
        let b = probe_offsets(100 << 20, 1 << 20, 32, 12345);
        assert_eq!(a, b);
        assert!(a.len() <= 32);
        assert_eq!(a[0], 0);
        assert!(*a.last().unwrap() < 100 << 20);
        assert!(a.iter().all(|o| o % (1 << 20) == 0));
    }

    #[test]
    fn block_ranges_cover_everything() {
        let v: Vec<_> = block_ranges(1300, 512).collect();
        assert_eq!(v, vec![(0, 512), (512, 512), (1024, 276)]);
    }
}
