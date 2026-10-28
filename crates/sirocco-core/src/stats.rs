//! Progress counters shared by all pipeline threads — and, optionally, by several
//! *processes*.
//!
//! Everything is a relaxed atomic: these numbers are for reporting, never for
//! control flow, so the ordering does not matter and the cost is one `lock xadd`
//! per update — amortised over a 1 MiB chunk, i.e. noise.
//!
//! # Why the counters are `#[repr(C)]`
//!
//! A split run needs the children's progress in the parent. The obvious designs
//! are a pipe (a syscall per update), a socket (a thread per process) or a shared
//! memory segment plus a publisher thread (still a thread). All of them add cost to
//! a path that runs millions of times.
//!
//! Instead the counters themselves live in a shared mapping: the child allocates
//! its `Stats` *inside* the mapped region, so every `fetch_add` the pipeline
//! already does lands in memory the parent can read. Zero extra threads, zero IPC,
//! zero syscalls on the hot path — the parent simply loads the same cache lines.

// --- `unsafe` is allowed here, and only here -------------------------------
// The counter block may live in a shared mapping owned by another process,
// so it is addressed through a raw pointer.
#![allow(unsafe_code)]

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The shareable part: a fixed layout of atomics, no padding surprises.
#[repr(C)]
pub struct Counters {
    /// Layout guard, so a stale or foreign mapping is rejected instead of read.
    pub magic: AtomicU64,
    /// Files the scan planned, i.e. the denominator of the file progress.
    pub files_total: AtomicU64,
    /// Files copied and closed successfully.
    pub files_done: AtomicU64,
    /// Files left alone because the destination was already current.
    pub files_skipped: AtomicU64,
    /// Files that failed for any reason.
    pub files_failed: AtomicU64,
    /// Directories created at the destination.
    pub dirs_created: AtomicU64,
    /// Bytes the scan planned, i.e. the denominator of the byte progress.
    pub bytes_total: AtomicU64,
    /// Bytes read from the source.
    pub bytes_read: AtomicU64,
    /// Bytes written to the destination.
    pub bytes_written: AtomicU64,
    /// Bytes read back and compared.
    pub bytes_verified: AtomicU64,
    /// Bytes not transferred because the delta path already had them.
    pub bytes_saved: AtomicU64,
    /// Bytes skipped because the source did not allocate them (punched instead).
    pub bytes_holes: AtomicU64,
    /// Failures recorded during the run.
    pub errors: AtomicU64,
    /// Gauges for the live pipeline view: operations currently outstanding on
    /// readers and writers, and chunks waiting in the queues. Two atomics per
    /// chunk, against the four syscalls that chunk already costs.
    pub readers_active: AtomicU64,
    /// Writer operations outstanding, including verify read-backs.
    pub writers_active: AtomicU64,
    /// Chunks waiting in the read and write queues together.
    pub queued: AtomicU64,
}

impl Counters {
    /// Layout guard: `"SIROCO01"` as big-endian ASCII. A mapping whose first word
    /// is not this is stale or belongs to something else.
    pub const MAGIC: u64 = 0x5349_524f_434f_3031;
    /// Size of one counter block, for sizing a shared mapping.
    pub const SIZE: usize = std::mem::size_of::<Self>();

    const fn zeroed() -> Self {
        // SAFETY: every field is an `AtomicU64`, for which all-zero is a valid
        // representation.
        unsafe { std::mem::zeroed() }
    }

    fn stamp(&self) {
        self.magic.store(Self::MAGIC, Ordering::Release);
    }

    /// True when this block was stamped by a Sirocco process.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == Self::MAGIC
    }

    fn get(a: &AtomicU64) -> u64 {
        a.load(Ordering::Relaxed)
    }

    /// Read every counter once. The values are not a consistent instant — a
    /// writer may land between two loads — which is fine for a progress display.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            rate_bps: 0.0,
            files_rate: 0.0,
            files_total: Self::get(&self.files_total),
            files_done: Self::get(&self.files_done),
            files_skipped: Self::get(&self.files_skipped),
            files_failed: Self::get(&self.files_failed),
            dirs_created: Self::get(&self.dirs_created),
            bytes_total: Self::get(&self.bytes_total),
            bytes_read: Self::get(&self.bytes_read),
            bytes_written: Self::get(&self.bytes_written),
            bytes_verified: Self::get(&self.bytes_verified),
            bytes_saved: Self::get(&self.bytes_saved),
            bytes_holes: Self::get(&self.bytes_holes),
            errors: Self::get(&self.errors),
            readers_active: Self::get(&self.readers_active),
            writers_active: Self::get(&self.writers_active),
            queued: Self::get(&self.queued),
            elapsed_secs: 0.0,
        }
    }
}

enum Backing {
    /// A private allocation, the normal case.
    Owned(Box<Counters>),
    /// A mapping shared with other processes. The mapping itself is owned by the
    /// `Shm` handle, which outlives the `Stats`.
    Shared(*mut Counters),
}

/// Shortest interval between two samples that is allowed to move the estimate.
///
/// Without this the estimator would depend on *who* calls `snapshot()`: the GUI
/// samples on every frame (16 ms), the CLI progress thread every 250 ms, and a
/// burst of short intervals full of zero deltas drags the average towards zero —
/// which reads as a stalled copy and an ETA of hours. The interval is measured
/// from the last *accepted* sample, so the cadence only decides how often the
/// number is refreshed, not what it converges to.
const SAMPLE_MIN_SECS: f64 = 0.20;

/// Weight of the newest sample. A third is responsive enough to show a stall
/// within a second and steady enough that the ETA does not jump every tick.
const SAMPLE_WEIGHT: f64 = 0.30;

/// Samples needed before a rate is published at all. One 200 ms window is mostly
/// startup noise; two is 400 ms, which is already the real speed.
const SAMPLE_WARMUP: u32 = 2;

/// State of the throughput estimator.
///
/// The old ETA was `elapsed * (1 - p) / p`, which is only correct when the rate
/// is constant from the first instant. It is not: the scan phase produces no
/// progress at all, and at `p = 2 %` the formula multiplies any error by 50. The
/// estimate below is a smoothed instantaneous rate instead — remaining work
/// divided by how fast work is actually being accounted for *now*.
#[derive(Default)]
struct RateState {
    /// When the last accepted sample was taken; `None` until the first one.
    last_at: Option<Instant>,
    /// Bytes accounted for at the last accepted sample.
    last_bytes: u64,
    /// Files finished at the last accepted sample.
    last_files: u64,
    /// Exponential moving average of the byte rate, in bytes/second.
    ema_bps: f64,
    /// Exponential moving average of the file rate, in files/second.
    ema_files: f64,
    /// Accepted samples so far, against [`SAMPLE_WARMUP`].
    samples: u32,
}

/// Fold one sample into an exponential moving average.
///
/// A zero average means "no estimate yet", not "the rate is zero", so the first
/// sample after a stall seeds the average directly instead of being averaged
/// against a zero that would take several ticks to climb out of.
fn blend(
    ema: f64,
    sample: f64,
) -> f64 {
    if ema <= 0.0 {
        sample
    } else {
        sample.mul_add(SAMPLE_WEIGHT, ema * (1.0 - SAMPLE_WEIGHT))
    }
}

/// Live progress counters plus the throughput estimator.
pub struct Stats {
    backing: Backing,
    started: Mutex<Option<Instant>>,
    rate: Mutex<RateState>,
}

// SAFETY: the shared pointer refers to a mapping that is alive for the whole run
// and is only ever touched through atomics.
//
// `Backing::Shared` is a raw pointer, which is not `Send` by default; the safety
// comment above is the justification, so the lint's automatic objection does not
// apply.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for Stats {}
unsafe impl Sync for Stats {}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    /// A private counter block, started now.
    #[must_use]
    pub fn new() -> Self {
        let c = Box::new(Counters::zeroed());
        c.stamp();
        Self {
            backing: Backing::Owned(c),
            started: Mutex::new(Some(Instant::now())),
            rate: Mutex::new(RateState::default()),
        }
    }

    /// Attach to a counters block living in shared memory.
    ///
    /// # Safety
    /// `ptr` must point at a live mapping of at least `Counters::SIZE` bytes that
    /// outlives this `Stats`.
    pub unsafe fn from_shared(ptr: *mut Counters) -> Self {
        let s = Self {
            backing: Backing::Shared(ptr),
            started: Mutex::new(Some(Instant::now())),
            rate: Mutex::new(RateState::default()),
        };
        if !s.counters().is_valid() {
            s.counters().stamp();
        }
        s
    }

    /// The counter block, wherever it lives.
    #[inline]
    #[must_use]
    pub fn counters(&self) -> &Counters {
        match &self.backing {
            | Backing::Owned(b) => b,
            // SAFETY: guaranteed by the constructor's contract.
            | Backing::Shared(p) => unsafe { &**p },
        }
    }

    /// Add to one counter. Kept as a method so callers do not have to pick an
    /// ordering — these are reporting counters, so it is always relaxed.
    #[inline]
    pub fn add(
        &self,
        c: &AtomicU64,
        n: u64,
    ) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    /// Seconds since the run started, scan included.
    #[must_use]
    pub fn elapsed_secs(&self) -> f64 {
        // A poisoned mutex here still holds a usable `Instant`; recovering from
        // it beats turning a reporting call into a panic.
        let guard = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
    }

    /// Take a snapshot and advance the rate estimator.
    ///
    /// Sampling cadence is up to the caller; the estimator uses the actual time
    /// between accepted calls, so a slower or faster poll only changes how often
    /// the number is refreshed, not what it converges to. Calls closer together
    /// than `SAMPLE_MIN_SECS` are ignored, and nothing is published until
    /// `SAMPLE_WARMUP` samples exist — the first call therefore reports a rate
    /// of zero rather than a division by a near-zero interval.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.counters().snapshot();
        s.elapsed_secs = self.elapsed_secs();

        let now = Instant::now();
        let bytes = s.bytes_accounted();
        let files = s.files_done + s.files_skipped;
        if let Ok(mut st) = self.rate.lock() {
            if let Some(prev) = st.last_at {
                let dt = (now - prev).as_secs_f64();
                if dt >= SAMPLE_MIN_SECS {
                    let db = bytes.saturating_sub(st.last_bytes) as f64;
                    let df = files.saturating_sub(st.last_files) as f64;
                    st.ema_bps = blend(st.ema_bps, db / dt);
                    st.ema_files = blend(st.ema_files, df / dt);
                    st.last_at = Some(now);
                    st.last_bytes = bytes;
                    st.last_files = files;
                    st.samples = st.samples.saturating_add(1);
                }
            } else {
                st.last_at = Some(now);
                st.last_bytes = bytes;
                st.last_files = files;
            }
            if st.samples >= SAMPLE_WARMUP {
                s.rate_bps = st.ema_bps;
                s.files_rate = st.ema_files;
            }
        }
        s
    }
}

impl std::ops::Deref for Stats {
    type Target = Counters;

    #[inline]
    fn deref(&self) -> &Counters {
        self.counters()
    }
}

/// One consistent-enough read of the counters, plus the derived rates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    /// Smoothed instantaneous throughput, bytes/second.
    pub rate_bps: f64,
    /// Smoothed instantaneous file completion rate, files/second.
    pub files_rate: f64,
    /// Files the scan planned.
    pub files_total: u64,
    /// Files copied and closed.
    pub files_done: u64,
    /// Files that were already current.
    pub files_skipped: u64,
    /// Files that failed.
    pub files_failed: u64,
    /// Directories created.
    pub dirs_created: u64,
    /// Bytes the scan planned.
    pub bytes_total: u64,
    /// Bytes read from the source.
    pub bytes_read: u64,
    /// Bytes written to the destination.
    pub bytes_written: u64,
    /// Bytes read back and compared.
    pub bytes_verified: u64,
    /// Bytes the delta path did not have to transfer.
    pub bytes_saved: u64,
    /// Bytes skipped because the source had no allocation there.
    pub bytes_holes: u64,
    /// Recorded errors.
    pub errors: u64,
    /// Reader operations outstanding.
    pub readers_active: u64,
    /// Writer operations outstanding.
    pub writers_active: u64,
    /// Chunks waiting in the two queues.
    pub queued: u64,
    /// Seconds since the run started, scan included.
    pub elapsed_secs: f64,
}

impl Snapshot {
    /// Planned bytes that no longer need any work: written, punched, or already
    /// present at the destination.
    ///
    /// `bytes_holes` is a *subset* of `bytes_saved` — the punch path counts a
    /// punched chunk in both — so the two are not added together. Leaving the
    /// saved bytes out is what made a delta run stall at "87 %" forever: it had
    /// finished, but most of the file never needed writing.
    #[must_use]
    pub fn bytes_accounted(&self) -> u64 {
        self.bytes_written
            .saturating_add(self.bytes_saved.max(self.bytes_holes))
    }

    /// Current write throughput. Prefers the smoothed estimate and falls back to
    /// the run average only before the estimator has warmed up.
    #[must_use]
    pub fn write_rate(&self) -> f64 {
        if self.rate_bps > 0.0 {
            self.rate_bps
        } else if self.elapsed_secs > 0.0 {
            self.bytes_accounted() as f64 / self.elapsed_secs
        } else {
            0.0
        }
    }

    /// Average read throughput over the whole run.
    #[must_use]
    pub fn read_rate(&self) -> f64 {
        if self.elapsed_secs > 0.0 {
            self.bytes_read as f64 / self.elapsed_secs
        } else {
            0.0
        }
    }

    /// Fraction of the planned bytes already accounted for.
    #[must_use]
    pub fn progress(&self) -> f64 {
        if self.bytes_total == 0 {
            if self.files_total == 0 {
                return 1.0;
            }
            return (self.files_done + self.files_skipped) as f64 / self.files_total as f64;
        }
        (self.bytes_accounted() as f64 / self.bytes_total as f64).clamp(0.0, 1.0)
    }

    /// Estimated seconds remaining, or `None` when no honest estimate exists.
    ///
    /// Three cases return `None` instead of a number: nothing has moved yet (no
    /// rate to divide by), the run is already complete, or the plan carries no
    /// bytes at all. An ETA derived from a rate of zero is infinity, and one
    /// derived from the first 10 ms of a run is noise.
    #[must_use]
    pub fn eta_secs(&self) -> Option<f64> {
        if self.progress() >= 1.0 {
            return None;
        }
        if self.bytes_total > 0 {
            let remaining = self.bytes_total.saturating_sub(self.bytes_accounted());
            if remaining == 0 {
                return None;
            }
            let rate = self.write_rate();
            return (rate > 0.0).then(|| remaining as f64 / rate);
        }
        // No byte totals (an empty plan, or a metadata-only run): fall back to
        // files, which is the only progress signal available.
        let remaining = self
            .files_total
            .saturating_sub(self.files_done + self.files_skipped);
        if remaining == 0 || self.files_rate <= 0.0 {
            return None;
        }
        Some(remaining as f64 / self.files_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        written: u64,
        saved: u64,
        holes: u64,
        total: u64,
        rate: f64,
    ) -> Snapshot {
        Snapshot {
            rate_bps: rate,
            bytes_written: written,
            bytes_saved: saved,
            bytes_holes: holes,
            bytes_total: total,
            files_total: 1,
            files_done: 0,
            elapsed_secs: 1.0,
            ..Snapshot::default()
        }
    }

    /// The bug that made every delta run stall: most of the file is never
    /// written, so a numerator of `bytes_written` alone never reaches the plan.
    #[test]
    fn delta_run_reaches_one_hundred_percent() {
        let s = snap(100, 900, 0, 1000, 100.0);
        assert!((s.progress() - 1.0).abs() < 1e-9);
        assert_eq!(s.eta_secs(), None);
    }

    /// A punched chunk is counted in both `bytes_saved` and `bytes_holes`; adding
    /// the two together would report 200 % progress.
    #[test]
    fn holes_are_not_counted_twice() {
        let s = snap(100, 400, 400, 500, 100.0);
        assert!((s.progress() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn eta_is_remaining_over_the_live_rate() {
        let s = snap(250, 0, 0, 1000, 500.0);
        let eta = s.eta_secs().unwrap();
        assert!((eta - 1.5).abs() < 1e-9, "eta was {eta}");
    }

    /// Before the estimator has a rate there is no honest number to show, and a
    /// zero rate would divide to infinity.
    #[test]
    fn no_rate_means_no_estimate() {
        let s = snap(250, 0, 0, 1000, 0.0);
        // The fallback average is `bytes / elapsed`, so a run with no progress at
        // all has nothing to divide by.
        let stalled = snap(0, 0, 0, 1000, 0.0);
        assert_eq!(stalled.eta_secs(), None);
        // With progress but no published rate the average stands in.
        assert!(s.eta_secs().unwrap() > 0.0);
    }

    /// `snapshot()` must not let a caller's polling frequency decide the answer:
    /// samples inside `SAMPLE_MIN_SECS` are ignored.
    #[test]
    fn rapid_polling_does_not_move_the_estimate() {
        let stats = Stats::new();
        stats.bytes_total.store(1 << 30, Ordering::Relaxed);
        stats.bytes_written.store(1 << 20, Ordering::Relaxed);
        let first = stats.snapshot();
        assert_eq!(first.rate_bps, 0.0);
        // A hundred immediate re-polls, all with a zero delta, would drag a naive
        // average to zero. They must be ignored instead.
        for _ in 0..100 {
            assert_eq!(stats.snapshot().rate_bps, 0.0);
        }
    }
}
