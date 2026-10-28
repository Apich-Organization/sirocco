//! Monotonic clock with ~ns resolution.

use std::sync::OnceLock;
use std::time::Instant;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds since process start (monotonic, cheap).
#[inline]
#[must_use]
pub fn now_ns() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

/// Returns elapsed nanoseconds since `t0`.
#[inline]
#[must_use]
pub fn elapsed_ns(t0: Instant) -> u64 {
    t0.elapsed().as_nanos() as u64
}

/// Convert a byte count and a duration in ns to MiB/s.
#[inline]
#[must_use]
pub fn mib_per_sec(
    bytes: u64,
    ns: u64,
) -> f64 {
    if ns == 0 {
        return 0.0;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / (ns as f64 / 1e9)
}
