//! Engine configuration.

use std::path::PathBuf;

use sirocco_core::delta::DeltaCfg;
use sirocco_core::hash::HashKind;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::ScanCfg;
use sirocco_core::plan::VerifyMode;
use sirocco_sys::ReactorKind;

/// What to do when the data looks compressible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompressMode {
    /// Never.
    #[default]
    Off,
    /// Compress only when the measured cost model says it wins.
    Auto,
    /// Always compress when the ratio is better than 1.
    Force,
}

impl CompressMode {
    /// Parse a compression mode name or its alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "off" | "none" | "no" => Self::Off,
            | "auto" => Self::Auto,
            | "force" | "on" | "always" => Self::Force,
            | _ => return None,
        })
    }
}

/// How to reserve space for the destination before writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PreallocMode {
    /// Do not reserve anything; the first write extends the file.
    Off,
    /// `SetFileValidData` when privileged, otherwise the sparse-extension trick.
    #[default]
    Auto,
    /// Only `SetFileValidData` (needs `SeManageVolumePrivilege` / `CAP_SYS_ADMIN`).
    Valid,
    /// Always extend through a hole, then clear the sparse flag when finished.
    Sparse,
}

impl PreallocMode {
    /// True when the destination is sized before the first write.
    #[inline]
    #[must_use]
    pub fn reserves(self) -> bool {
        self != Self::Off
    }

    /// Parse a preallocation mode name or its alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "off" | "no" | "none" => Self::Off,
            | "auto" => Self::Auto,
            | "valid" | "instant" => Self::Valid,
            | "sparse" | "hole" => Self::Sparse,
            | _ => return None,
        })
    }
}

/// Persistent block index, used to skip reading the source on re-sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// No index: every run decides from scratch.
    #[default]
    Off,
    /// Trust the recorded `(size, mtime)`; never re-read the source for unchanged
    /// blocks. Fast, and correct under the same assumption rsync makes.
    Trust,
    /// Also re-read the source and compare hashes, falling back to a full copy the
    /// moment the index turns out to be stale.
    Verify,
}

impl IndexMode {
    /// Parse an index mode name or its alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "off" | "no" | "none" => Self::Off,
            | "on" | "trust" | "auto" => Self::Trust,
            | "verify" | "safe" => Self::Verify,
            | _ => return None,
        })
    }

    /// True for any mode other than [`IndexMode::Off`].
    #[must_use]
    pub fn enabled(self) -> bool {
        self != Self::Off
    }
}

/// One per-file notification, for a UI that wants to show the work as it happens.
///
/// Deliberately a plain `std::sync::mpsc` channel: the engine sends at most a
/// handful per file, the consumer drains it on its own thread, and nothing in the
/// hot path has to know what a widget is.
#[derive(Clone, Debug)]
pub enum Event {
    /// A file is about to be processed.
    Started {
        /// Source-relative path.
        rel: std::path::PathBuf,
        /// Size in bytes.
        size: u64,
    },
    /// A file finished successfully.
    Done {
        /// Source-relative path.
        rel: std::path::PathBuf,
        /// Size in bytes.
        size: u64,
        /// Bytes that did not have to be transferred.
        saved: u64,
    },
    /// The destination was already current; nothing was written.
    Skipped {
        /// Source-relative path of the file that was already current.
        rel: std::path::PathBuf,
    },
    /// A file failed.
    Failed {
        /// Source-relative path.
        rel: std::path::PathBuf,
        /// Why it failed, in one line.
        error: String,
    },
    /// The run is over. Sent once, after the last file event.
    Finished {
        /// Files copied.
        files: u64,
        /// Files that failed.
        failed: u64,
    },
}

/// How small files are copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SmallCopy {
    /// Use the kernel-side copy when nothing else needs the data; fall back to the
    /// userspace pipeline otherwise.
    #[default]
    Auto,
    /// Always ask the kernel (`CopyFileExW` / `copy_file_range`).
    Kernel,
    /// Always use the userspace pipeline.
    Io,
}

impl SmallCopy {
    /// Parse a small-file strategy name or its alias.
    #[must_use]
    pub fn parses(s: &str) -> Option<Self> {
        Some(match s {
            | "auto" => Self::Auto,
            | "kernel" | "os" => Self::Kernel,
            | "io" | "pipeline" => Self::Io,
            | _ => return None,
        })
    }
}

/// Where threads are allowed to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NumaMode {
    /// Follow the storage device's node.
    #[default]
    Auto,
    /// Ignore NUMA entirely.
    Off,
    /// Force a specific node.
    Node(u16),
}

/// Where per-file progress goes.
///
/// The CLI has no listener, so it leaves this `None`. A GUI wants one, and the
/// difference between an unbounded channel and a bounded one matters there: a
/// kernel-path copy of 200 000 small files produces events far faster than a
/// window can draw them, and an unbounded queue turns that into a memory spike.
/// `Bounded` drops the row instead, which costs a line in a list nobody can read
/// at that rate and keeps the counters exact.
#[derive(Debug, Clone)]
pub enum EventSink {
    /// Never blocks, never drops.
    Unbounded(std::sync::mpsc::Sender<Event>),
    /// Drops the event when the receiver has fallen behind.
    Bounded(std::sync::mpsc::SyncSender<Event>),
}

impl EventSink {
    /// Send, or drop. A listener that cannot keep up must not slow the copy.
    pub fn send(
        &self,
        ev: Event,
    ) {
        match self {
            | Self::Unbounded(tx) => {
                let _ = tx.send(ev);
            },
            | Self::Bounded(tx) => {
                let _ = tx.try_send(ev);
            },
        }
    }
}

/// Everything that influences how the engine behaves.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// What to do: copy, move, mirror or delete.
    pub mode: Mode,
    /// When an existing destination may be replaced.
    pub overwrite: Overwrite,
    /// How thoroughly to check the result.
    pub verify: VerifyMode,
    /// Which digest the verification and the index use.
    pub hash: HashKind,

    // --- I/O shape
    /// Size of one pooled buffer. Also the default maximum size of a single
    /// request.
    pub buffer_size: usize,
    /// Maximum bytes per individual read/write. Must divide `buffer_size`.
    pub io_unit: u64,
    /// Outstanding operations per thread.
    pub overlap: usize,
    /// Reader threads. 0 lets [`EngineConfig::normalize_with`] derive it.
    pub readers: usize,
    /// Writer threads. 0 lets [`EngineConfig::normalize_with`] derive it.
    pub writers: usize,
    /// Scheduler threads that feed the pipeline. 0 = automatic: start from a
    /// guess and let the throughput controller adjust it while the run is going.
    pub feeders: usize,
    /// True when the caller asked for automatic tuning (set by `normalize`).
    pub feeders_auto: bool,
    /// Restrict placement to these logical CPUs (overrides the NUMA policy).
    pub cpu_list: Option<Vec<usize>>,
    /// Where the buffers and threads should live in NUMA terms.
    pub numa: NumaMode,
    /// Use `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` for bulk data.
    pub direct_io: bool,
    /// Back the buffer pool with large pages when the privilege allows it.
    pub huge_pages: bool,
    /// Explicitly refuse large pages even when the privilege is available.
    pub no_huge_pages: bool,
    /// How the destination is sized before writing.
    pub prealloc: PreallocMode,
    /// Punch holes for zero and unallocated regions.
    pub sparse: bool,
    /// Order the chunks of a file by their physical cluster number, so an HDD
    /// reads them in the order they sit on the platter. Ignored on SSDs, where it
    /// is pure overhead.
    pub reorder_lcn: bool,
    /// Copy the security descriptor (Windows DACL) / POSIX ACL (Linux).
    pub preserve_acl: bool,
    /// Which I/O backend to build.
    pub reactor: ReactorKind,
    /// Depth of each pipeline queue. 0 derives it from the thread counts.
    pub queue_depth: usize,

    // --- scheduling / affinity
    /// Pin each I/O thread to a CPU chosen by the affinity planner.
    pub pin_threads: bool,
    /// I/O priority hint, 0 (very low) to 4 (critical).
    pub io_priority: Option<u8>,
    /// Process priority class, in the platform's own encoding.
    pub process_priority: Option<u32>,
    /// Thread priority level, in the platform's own encoding.
    pub thread_priority: Option<i32>,

    // --- behaviour
    /// Whether to ask the filesystem to compress the destination.
    pub compress: CompressMode,
    /// Allow the block-delta and append paths. Off forces a full copy.
    pub delta_enabled: bool,
    /// Tuning for the delta decision (block size, probe count, confidence).
    pub delta: DeltaCfg,
    /// Measured read/write throughput. `None` uses conservative defaults, which
    /// correctly rule delta out on same-speed media and skip the probe.
    pub io_profile: Option<sirocco_core::delta::IoProfile>,
    /// Files at or below this size are candidates for the kernel fast path.
    pub small_threshold: u64,
    /// Whether the kernel fast path is allowed at all.
    pub small_copy: SmallCopy,
    /// Flush the destination to the device before reporting success.
    pub sync_at_end: bool,
    /// Restore the source's modification time on the destination.
    pub keep_times: bool,
    /// Copy permission bits (Unix) or attributes (Windows).
    pub preserve_permissions: bool,
    /// Preserve extended attributes and ACLs (Linux) / the DACL (Windows).
    pub preserve_xattrs: bool,
    /// Replace duplicate source files with hard links at the destination.
    pub preserve_links: bool,
    /// Plan only: scan, decide, report, change nothing.
    pub dry_run: bool,
    /// Record completed files so an interrupted run can resume.
    pub journal: bool,
    /// Copy only the source-relative paths listed in this file (one per line).
    pub file_list: Option<PathBuf>,
    /// Coarse-grained process split: 0 or 1 means "stay in one process".
    pub procs: usize,
    /// Publish the progress counters into this shared-memory segment so the
    /// parent process can read them without a pipe or a publisher thread.
    pub shm: Option<PathBuf>,
    /// Whether the persistent block index is used, and how far it is trusted.
    pub index: IndexMode,
    /// Where index files live. `None` picks a per-user cache directory.
    pub index_dir: Option<PathBuf>,
    /// `cp -r` semantics: copy the source *directory itself* into an existing
    /// destination instead of merging its contents.
    pub into: bool,
    /// In `Sync` mode, delete destination files the source does not have.
    pub delete_extra: bool,
    /// How many times to retry a transient failure.
    pub retries: u32,
    /// Verbosity, 0..=3.
    pub verbose: u8,
    /// Suppress the human-readable summary.
    pub quiet: bool,
    /// Scanner settings: filters, ordering, symlink policy.
    pub scan: ScanCfg,
    /// Also write the summary to this file.
    pub log_file: Option<PathBuf>,
    /// Where per-file progress goes, if anyone is listening.
    pub events: Option<EventSink>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Copy,
            overwrite: Overwrite::IfDifferent,
            verify: VerifyMode::None,
            hash: HashKind::Xxh64,
            buffer_size: 1 << 20,
            io_unit: 1 << 20,
            overlap: 8,
            readers: 0, // 0 = derive from the CPU count
            writers: 0,
            feeders: 0,
            feeders_auto: false,
            cpu_list: None,
            numa: NumaMode::Auto,
            direct_io: true,
            huge_pages: false,
            no_huge_pages: false,
            prealloc: PreallocMode::Auto,
            sparse: false,
            reorder_lcn: true,
            preserve_acl: false,
            reactor: ReactorKind::Native,
            queue_depth: 0,
            pin_threads: false,
            io_priority: None,
            process_priority: None,
            thread_priority: None,
            compress: CompressMode::Off,
            delta_enabled: true,
            delta: DeltaCfg::default(),
            io_profile: None,
            small_threshold: 256 << 10,
            small_copy: SmallCopy::Auto,
            sync_at_end: false,
            keep_times: true,
            preserve_permissions: true,
            preserve_xattrs: false,
            preserve_links: true,
            dry_run: false,
            journal: false,
            file_list: None,
            procs: 1,
            shm: None,
            index: IndexMode::Off,
            index_dir: None,
            into: false,
            delete_extra: false,
            retries: 2,
            verbose: 1,
            quiet: false,
            scan: ScanCfg::default(),
            log_file: None,
            events: None,
        }
    }
}

impl EngineConfig {
    /// Clamp and derive values that depend on each other.
    pub fn normalize(
        &mut self,
        physical_cores: usize,
    ) {
        self.normalize_with(physical_cores, physical_cores * 2);
    }

    /// Clamp and derive, using the real CPU counts.
    ///
    /// `logical_cpus` is what bounds `readers + writers`: past that point the
    /// affinity planner would have to put a reader and a writer on the same core.
    pub fn normalize_with(
        &mut self,
        physical_cores: usize,
        logical_cpus: usize,
    ) {
        self.buffer_size = self.buffer_size.clamp(4096, 64 << 20);
        self.io_unit = self.io_unit.clamp(4096, self.buffer_size as u64);
        // Requests must tile the buffer exactly, otherwise the tail handling gets
        // messy for no benefit.
        if !(self.buffer_size as u64).is_multiple_of(self.io_unit) {
            self.io_unit = self.buffer_size as u64;
        }
        self.overlap = self.overlap.clamp(1, 256);
        let cores = physical_cores.max(1);
        if self.readers == 0 {
            self.readers = (cores / 2).max(1);
        }
        if self.writers == 0 {
            // `--readers=100` on an 8-core box used to underflow here: a panic in
            // debug, a 64-writer config in release.
            self.writers = cores.saturating_sub(self.readers).max(1);
        }
        self.readers = self.readers.clamp(1, 64);
        self.writers = self.writers.clamp(1, 64);
        // Never ask for more I/O threads than there are logical CPUs: past that
        // point they only add scheduling noise, and the affinity planner would be
        // forced to put a reader and a writer on the same core.
        let logical = logical_cpus.max(2);
        while self.readers + self.writers > logical {
            if self.readers >= self.writers && self.readers > 1 {
                self.readers -= 1;
            } else if self.writers > 1 {
                self.writers -= 1;
            } else {
                break;
            }
        }
        if self.feeders == 0 {
            self.feeders_auto = true;
            self.feeders = (physical_cores / 4).clamp(1, 8);
        }
        self.feeders = self.feeders.clamp(1, 64);
        if self.queue_depth == 0 {
            self.queue_depth = (self.readers + self.writers) * self.overlap;
        }
        self.queue_depth = self.queue_depth.clamp(8, 1 << 20);
        // Comparison granularity must equal transfer granularity: the block
        // hashes stored in the persistent index are produced by the reader at
        // `io_unit` size, and the delta paths compare at the same size.
        // Only derive the granularity when the caller did not choose one:
        // `--delta-block` used to be parsed and then unconditionally replaced.
        //
        // The persistent index is the exception: its entries are keyed by the
        // granularity the *reader* hashes at, which is `io_unit`. A different
        // `delta.block` would make every `write_index` call reject its own hash
        // vector, so the index would silently never be written.
        if self.delta.block == sirocco_core::delta::DeltaCfg::default().block
            || self.index.enabled()
        {
            self.delta.block = self.io_unit;
        }
    }

    /// Number of pooled buffers required so that no thread ever waits on the pool
    /// while it still has work to submit.
    ///
    /// In-flight buffers = readers' outstanding + writers' outstanding + `Ready`
    /// items sitting in the write queue. Capped so the pool never exceeds 512 MiB.
    #[must_use]
    pub fn buffer_count(&self) -> usize {
        let want = (self.readers + self.writers) * self.overlap + self.queue_depth.min(1024) + 8;
        let cap = ((512usize << 20) / self.buffer_size).max(16);
        want.min(cap)
    }

    /// Bytes of RAM the buffer pool will pin.
    #[must_use]
    pub fn pool_bytes(&self) -> usize {
        self.buffer_count() * self.buffer_size
    }

    /// The scan ordering, which is also the pipeline feeding order.
    #[must_use]
    pub const fn order(&self) -> Order {
        self.scan.order
    }
}
