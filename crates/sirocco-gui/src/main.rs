//! Sirocco — desktop application.
//!
//! One screen: configuration on the left, live state on the right. Everything
//! that changes while the job runs comes from two places — the `Stats` block the
//! engine writes directly (no polling thread, no channel per counter) and a plain
//! `mpsc` feed of per-file events that the update loop drains once per frame.
//!
//! # Several sources
//!
//! The engine runs one source per session, so the GUI drives a *sequence* of
//! them: `[src1, src2, …] → dst/<name>`. Each source gets its own `Stats` (the
//! engine resets the totals when a run starts) and its own counter block is
//! swapped in as the run advances. The window shows the sum, so the progress bar
//! never jumps back to zero when the second source begins.
//!
//! # No console
//!
//! The release binary is a GUI-subsystem executable, so double-clicking it does
//! not open a terminal next to it. Diagnostics therefore go to a log file
//! (`%LOCALAPPDATA%\Sirocco\sirocco-gui.log`, or the temp directory when that is
//! not writable) instead of to a console nobody can see.

// No console window next to the window. `cargo test` is exempt: the harness
// prints to stdout, and a GUI-subsystem test binary loses that output.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]
// The same domain exceptions as the rest of the workspace. A view is a tree of
// small named functions — one per card — so `single_call_fn` is the normal shape
// here, not a smell; the rest is layout arithmetic on `f32`.
#![allow(clippy::single_call_fn)]
#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::unwrap_used)]

mod charts;
mod theme;
mod ui;

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use std::time::Instant;

use iced::Element;
use iced::Subscription;
use iced::Task;
use iced::Theme;

use sirocco_core::delta::IoProfile;
use sirocco_core::hash::HashKind;
use sirocco_core::human::format_bytes;
use sirocco_core::human::format_duration;
use sirocco_core::human::format_rate;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::VerifyMode;
use sirocco_core::stats::Snapshot;
use sirocco_engine::EngineConfig;
use sirocco_engine::Event;
use sirocco_engine::config::CompressMode;
use sirocco_engine::config::IndexMode;
use sirocco_engine::config::NumaMode;
use sirocco_engine::config::PreallocMode;
use sirocco_engine::config::SmallCopy;
use sirocco_engine::session;
use sirocco_sys::ReactorKind;

use theme as th;

/// Samples kept for the throughput chart: 660 × 90 ms ≈ 60 s.
pub const SAMPLES: usize = 660;
/// Rows in the finished-file feed.
const FEED_CAP: usize = 300;
/// Rows shown as "in flight". Well above the real concurrency, but bounded so a
/// pathological job cannot grow the heap.
const ACTIVE_CAP: usize = 64;
/// Events read per frame, and the depth of the channel feeding them.
///
/// The engine produces roughly one event per file; the window only ever shows a
/// few hundred rows. Draining without a cap means one frame does work
/// proportional to a whole directory, which is exactly when the UI should be
/// responsive.
const EVENT_BUDGET: usize = 1024;

fn main() -> iced::Result {
    install_panic_log();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The boot function is the first argument (it used to be a trailing
    // `run_with`), and the title is a builder method.
    let result = iced::application(move || boot(&args), App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1300.0, 900.0),
            min_size: Some(iced::Size::new(980.0, 620.0)),
            // Set by the screenshot test so the window is not hidden behind
            // whatever else is on the desktop.
            level: if std::env::var_os("SIROCCO_GUI_ONTOP").is_some() {
                iced::window::Level::AlwaysOnTop
            } else {
                iced::window::Level::Normal
            },
            ..iced::window::Settings::default()
        })
        .antialiasing(true)
        .run();
    // A window that cannot be created (no GPU, no compositor) has no console to
    // complain on any more, so it says so in the log before exiting.
    if let Err(e) = &result {
        log_line(&format!("cannot start the window: {e}"));
    }
    result
}

/// Initial state, plus the first task.
///
/// `SIROCCO_GUI_SRC` / `SIROCCO_GUI_DST` pre-fill the form and start
/// immediately: the smoke test uses them, and they are handy for a "make it a
/// shortcut" setup. `--src` / `--dst` / `--privileged` do the same for a
/// relaunch through the elevation prompt.
fn boot(args: &[String]) -> (App, Task<Message>) {
    let mut app = App::default();
    app.apply_args(args);
    // Only the environment pre-fills *and* starts: that is the smoke-test path.
    // A relaunch through the elevation prompt fills the form and leaves the
    // decision to press Start where it belongs.
    let auto = app.ready() && std::env::var_os("SIROCCO_GUI_SRC").is_some();
    (
        app,
        if auto {
            Task::done(Message::Start)
        } else {
            Task::none()
        },
    )
}

// ---------------------------------------------------------------- logging

/// Where diagnostics go now that the binary has no console.
#[must_use]
pub fn log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from);
    base.join("Sirocco").join("sirocco-gui.log")
}

/// Append one line to the log. Never fails loudly: a log that cannot be written
/// must not be the reason a copy does not happen.
pub fn log_line(msg: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Record panics, since a GUI build has nowhere else to print them.
fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!(
            "panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        ));
    }));
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// What the job is doing, as the header pill shows it.
pub enum Phase {
    /// No job has run since the form was last cleared.
    Idle,
    /// A job is running.
    Running,
    /// The last job finished with nothing to report.
    Done,
    /// The last job finished with at least one failure.
    Failed,
    /// The last job was stopped by the user.
    Cancelled,
}

#[derive(Clone, Debug)]
/// State of one line in the file feed.
pub enum RowState {
    /// In flight.
    Active,
    /// Copied and closed.
    Done,
    /// Already current at the destination.
    Skipped,
    /// Failed, with the reason.
    Failed(String),
}

/// One line of the file feed.
pub struct FileRow {
    /// Source-relative path, with `/` separators.
    pub rel: String,
    /// Size in bytes, or 0 when unknown.
    pub size: u64,
    /// Current state.
    pub state: RowState,
}

/// Totals from the sources that have already finished.
///
/// Kept separately because the engine owns one counter block per session and
/// zeroes the plan totals when a run starts; adding the finished runs here is
/// what makes the progress bar and the FILES card describe the whole request
/// rather than the current source.
#[derive(Clone, Copy, Default, Debug)]
pub struct Totals {
    /// Files copied.
    pub files_done: u64,
    /// Files already current.
    pub files_skipped: u64,
    /// Files that failed.
    pub files_failed: u64,
    /// Files planned.
    pub files_total: u64,
    /// Bytes that needed no further work.
    pub bytes_done: u64,
    /// Bytes planned.
    pub bytes_total: u64,
    /// Seconds spent.
    pub elapsed: f64,
}

impl Totals {
    /// Fold one finished source in.
    fn add(
        &mut self,
        s: &Snapshot,
    ) {
        self.files_done = self.files_done.saturating_add(s.files_done);
        self.files_skipped = self.files_skipped.saturating_add(s.files_skipped);
        self.files_failed = self.files_failed.saturating_add(s.files_failed);
        self.files_total = self.files_total.saturating_add(s.files_total);
        self.bytes_done = self.bytes_done.saturating_add(s.bytes_accounted());
        self.bytes_total = self.bytes_total.saturating_add(s.bytes_total);
        self.elapsed += s.elapsed_secs;
    }

    /// Base plus one live source, shaped like a single run's snapshot.
    #[must_use]
    pub fn merge(
        &self,
        cur: &Snapshot,
    ) -> Snapshot {
        let mut s = *cur;
        s.files_done = self.files_done.saturating_add(cur.files_done);
        s.files_skipped = self.files_skipped.saturating_add(cur.files_skipped);
        s.files_failed = self.files_failed.saturating_add(cur.files_failed);
        s.files_total = self.files_total.saturating_add(cur.files_total);
        s.bytes_total = self.bytes_total.saturating_add(cur.bytes_total);
        // `bytes_accounted()` is written + saved, and a punched hole is counted
        // in both, so the merged figure is carried as plain written bytes.
        s.bytes_written = self.bytes_done.saturating_add(cur.bytes_accounted());
        s.bytes_saved = 0;
        s.bytes_holes = 0;
        s.elapsed_secs = self.elapsed + cur.elapsed_secs;
        s
    }
}

/// One message from the job thread.
// `End` carries a whole summary while `Start` carries a handle; the size gap is
// the point (the UI needs the summary, not a boxed copy of it).
#[allow(clippy::large_enum_variant)]
enum JobMsg {
    /// A source is about to run; this is its counter block.
    Start {
        index: usize,
        name: String,
        stats: Arc<sirocco_core::stats::Stats>,
    },
    /// A source finished.
    End {
        result: Result<session::Summary, String>,
    },
    /// Every source is done.
    AllDone,
}

/// The handle to a running job.
pub struct Job {
    /// Sources in the request, so the header can say "2 of 5".
    pub total: usize,
    /// Index of the source in flight.
    pub index: usize,
    /// Display name of the source in flight.
    pub name: String,
    /// Counters of the source in flight.
    pub stats: Arc<sirocco_core::stats::Stats>,
    events: Receiver<Event>,
    msgs: Receiver<JobMsg>,
    /// Totals from the sources that already finished.
    pub base: Totals,
    /// Summary of the last finished source, for its mode/backend/volume fields.
    last: Option<session::Summary>,
    /// Every error seen so far, from every source.
    errors: Vec<String>,
    /// Bytes at the previous sample, for an instantaneous rate.
    last_bytes: u64,
    last_at: Instant,
    smoothed: f64,
    /// The user asked to stop; shared with the job thread so it stops feeding
    /// sources instead of starting the next one.
    cancel: Arc<AtomicBool>,
    /// The last source reported an interrupt.
    cancelled: bool,
}

/// NUMA placement, as the pick list offers it.
///
/// The engine's [`NumaMode`] carries the node number inside the variant, which a
/// pick list cannot express: the list needs a fixed set of labels, and the node
/// is a number the user types or drags. So the two halves are kept apart here and
/// joined in [`App::engine_config`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumaChoice {
    /// Follow the storage device's node.
    Auto,
    /// Ignore NUMA entirely.
    Off,
    /// Force a specific node.
    Node,
}

/// Large-page policy, as the pick list offers it.
///
/// The engine has two booleans (`huge_pages`, `no_huge_pages`) because "not
/// asked for" and "explicitly refused" are different: the unset state lets the
/// capability probe turn large pages on when the privilege is held. A checkbox
/// can only express two of the three, and mapping the unchecked state to
/// `no_huge_pages = true` would permanently opt out of the fast path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HugeChoice {
    /// Enable them when the privilege allows.
    Auto,
    /// Always ask for them.
    On,
    /// Never ask for them.
    Off,
}

/// Everything the window shows, plus the handle to the running job.
// A settings panel is a bag of toggles by nature; one bool per option is the
// clearest representation of that.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    // --- configuration
    /// Copy, move or mirror.
    pub mode: Mode,
    /// What to copy. One or many; the engine takes one at a time.
    pub sources: Vec<PathBuf>,
    /// The "or type a path" field.
    pub path_input: String,
    /// Destination path, as typed or dropped.
    pub dst: String,
    /// Put the source folder *inside* the destination (`cp -r` semantics).
    pub into: bool,
    /// How thoroughly to check the result.
    pub verify: VerifyMode,
    /// When an existing destination may be replaced.
    pub overwrite: Overwrite,
    /// Punch holes for empty and unallocated regions.
    pub sparse: bool,
    /// Keep hard links instead of copying the data twice.
    pub links: bool,
    /// Copy permission bits and the DACL / POSIX ACL.
    pub acl: bool,
    /// Copy extended attributes and the DACL / POSIX ACL.
    pub xattrs: bool,
    /// Copy permission bits or file attributes.
    pub perms: bool,
    /// Restore the source's modification time on the destination.
    pub keep_times: bool,
    /// Remember block hashes so a re-sync can skip reading.
    pub index: IndexMode,
    /// Where index files live; empty means the per-user cache directory.
    pub index_dir: String,
    /// Keep a resume journal.
    pub journal: bool,
    /// Flush the destination to the device before reporting success.
    pub sync: bool,
    /// Plan only, change nothing.
    pub dry_run: bool,
    /// Requested I/O thread count; 0 means automatic.
    pub threads: u32,
    /// Comma- or space-separated exclude globs.
    pub exclude: String,
    /// Comma- or space-separated include globs.
    pub include: String,
    /// Order in which files are fed to the pipeline.
    pub order: Order,
    /// Descend into symbolic links instead of recreating them.
    pub follow_links: bool,
    /// Stay on the source volume; skip mount points inside the tree.
    pub one_fs: bool,
    /// Copy only the paths listed in this file (one per line).
    pub files_from: String,
    /// Read exclude globs from this file.
    pub exclude_from: String,
    /// Read include globs from this file.
    pub include_from: String,
    /// Directory depth limit; 0 means unlimited.
    pub max_depth: u32,

    // --- engine shape
    /// Digest used by verification and the index.
    pub hash: HashKind,
    /// Pooled buffer size, as a label the user picked (`1M`, `8M`, ...).
    pub buffer: String,
    /// Maximum bytes per request, as a label the user picked.
    pub io_unit: String,
    /// Outstanding operations per I/O thread.
    pub overlap: u32,
    /// Pipeline queue depth; 0 derives it from the thread counts.
    pub queue: u32,
    /// Scheduler threads; 0 lets the throughput controller decide.
    pub feeders: u32,
    /// Explicit reader count; 0 follows the thread split.
    pub readers: u32,
    /// Explicit writer count; 0 follows the thread split.
    pub writers: u32,
    /// Use `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` for bulk data.
    pub direct: bool,
    /// Who copies files at or below the small threshold.
    pub small_copy: SmallCopy,
    /// Files at or below this size are candidates for the kernel path.
    pub small: String,
    /// How the destination is sized before writing.
    pub prealloc: PreallocMode,
    /// Order a file's chunks by physical cluster (spinning disks).
    pub lcn: bool,
    /// Large-page policy.
    pub huge: HugeChoice,
    /// Whether to ask the filesystem to compress the destination.
    pub compress: CompressMode,
    /// Allow the block-delta and append paths.
    pub delta: bool,
    /// Comparison granularity for the delta decision.
    pub delta_block: String,
    /// Smallest file worth probing.
    pub delta_min: String,
    /// Number of probes taken on each side.
    pub probes: u32,
    /// Measured throughput, from the calibration button; `None` uses the
    /// conservative defaults, which correctly rule delta out on equal media.
    pub io_profile: Option<IoProfile>,
    /// A calibration is running.
    pub calibrating: bool,
    /// Result channel of a running calibration; drained in `poll`.
    calib_rx: Option<std::sync::mpsc::Receiver<Result<IoProfile, String>>>,
    /// Destination the running calibration was started against. A measurement
    /// describes one volume, so a result that arrives after the destination
    /// changed is not about anything the user is looking at.
    calib_for: Option<PathBuf>,
    /// When the running calibration started, so a wedged one can be given up on.
    calib_at: Option<Instant>,
    /// One line describing the last calibration.
    pub calibration: String,
    /// Which I/O backend to build.
    pub backend: ReactorKind,

    // --- scheduling
    /// Pin each I/O thread to a CPU chosen by the affinity planner.
    pub pin: bool,
    /// Restrict placement to these logical CPUs (`0-3,8-11`); empty means all.
    pub cpu_list: String,
    /// Where buffers and threads should live in NUMA terms.
    pub numa: NumaChoice,
    /// Node index used when `numa` is [`NumaChoice::Node`].
    pub numa_node: u32,
    /// I/O priority hint, by name; `"default"` leaves it alone.
    pub io_prio: String,
    /// Process priority class, by name.
    pub proc_prio: String,
    /// Thread priority level, by name.
    pub thread_prio: String,
    /// Ask the OS for the privileges the fast paths need before starting.
    pub privileged: bool,

    // --- layout
    /// Whether the advanced groups are expanded.
    pub advanced: bool,

    // --- live state
    /// Current phase, shown in the header pill.
    pub phase: Phase,
    /// Throughput samples, oldest first; one per tick, about a minute's worth.
    pub history: Vec<f32>,
    /// Files currently being worked on.
    pub active: Vec<FileRow>,
    /// Finished files, newest first.
    pub feed: VecDeque<FileRow>,
    /// One line of prose for the header.
    pub status: String,
    /// A request that could not even start.
    pub error: Option<String>,
    /// Result of the last finished job, summed over every source.
    pub summary: Option<session::Summary>,
    /// Frames rendered, used to drive the canvas animations.
    pub tick: u64,
    /// Reader count of the last job, so the pipeline meters keep their scale
    /// after the run ends.
    pub last_readers: usize,
    /// Writer count of the last job.
    pub last_writers: usize,
    /// Queue capacity of the last job, so the meter keeps a sane scale at rest.
    pub last_cap: usize,
    /// One line describing what this machine can do, for the rail footer.
    ///
    /// Written once at boot and never again: it describes the *machine*, not a
    /// run. The outcome of the last privilege request lives in `priv_note`.
    pub caps: String,
    /// What the last privilege request actually returned.
    pub priv_note: Option<String>,
    /// Whether this process already has administrator rights.
    pub elevated: bool,
    /// Physical cores, from the topology probe.
    pub cores: usize,
    /// Logical CPUs, which is what bounds `readers + writers`.
    pub logical: usize,

    /// A stop was requested. Re-asserted every tick because the engine resets
    /// the process-wide flag when a job thread starts, which would otherwise
    /// swallow a Stop pressed in the first few milliseconds.
    stop_requested: bool,

    job: Option<Job>,
}

impl App {
    /// True when the form describes a job that can be started.
    fn ready(&self) -> bool {
        !self.sources.is_empty() && !self.dst.trim().is_empty()
    }

    /// `--src` / `--dst` / `--privileged`, so a relaunch through the elevation
    /// prompt lands on the same form.
    fn apply_args(
        &mut self,
        args: &[String],
    ) {
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            match (arg.as_str(), args.get(i + 1)) {
                | ("--src", Some(v)) => {
                    self.add_sources([PathBuf::from(v)]);
                    i += 2;
                },
                | ("--dst", Some(v)) => {
                    self.dst.clone_from(v);
                    i += 2;
                },
                | ("--privileged", _) => {
                    self.privileged = true;
                    i += 1;
                },
                | _ => i += 1,
            }
        }
    }

    /// Add sources, skipping ones already in the list.
    ///
    /// Compared canonically where both paths resolve, so picking a folder and
    /// then dropping a file inside it does not produce two entries for one
    /// directory (`D:\a` vs `D:\a\`).
    fn add_sources(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) {
        for p in paths {
            if p.as_os_str().is_empty() {
                continue;
            }
            if self.sources.iter().any(|q| same_path(q, &p)) {
                continue;
            }
            self.sources.push(p);
        }
        self.into = self.into && self.single_dir();
    }

    /// True when the request is exactly one folder.
    #[must_use]
    pub fn single_dir(&self) -> bool {
        self.sources.len() == 1 && self.sources.first().is_some_and(|p| p.is_dir())
    }

    /// True when more than one source will be nested into the destination.
    #[must_use]
    pub const fn nested(&self) -> bool {
        self.sources.len() > 1
    }

    /// Thread counts the engine will actually use, mirroring `normalize_with`.
    #[must_use]
    pub fn resolved_threads(&self) -> (usize, usize) {
        let cores = self.cores.max(1);
        let (mut r, mut w) = if self.threads == 0 {
            let r = (cores / 2).max(1);
            (r, (cores - r).max(1))
        } else {
            let r = (self.threads as usize / 2).max(1);
            (r, (self.threads as usize - r).max(1))
        };
        let logical = self.logical.max(2);
        while r + w > logical {
            if r >= w && r > 1 {
                r -= 1;
            } else if w > 1 {
                w -= 1;
            } else {
                break;
            }
        }
        (r, w)
    }

    /// The delta block size that will actually be used.
    ///
    /// `normalize_with` replaces the block with `io_unit` when the caller left it
    /// at the default, so showing the picked label would print a number the run
    /// never uses. Showing the derived one keeps the card honest.
    #[must_use]
    pub fn effective_delta_block(&self) -> &str {
        // `normalize_with` derives the block from `io_unit` in two cases: the
        // caller left it at the default, and the index is on. Both have to be
        // reflected here or the card prints a number the run replaces.
        if self.delta_block.trim().is_empty()
            || self.index != IndexMode::Off
            || sirocco_core::human::parse_size(&self.delta_block)
                == Some(sirocco_core::delta::DeltaCfg::default().block)
        {
            &self.io_unit
        } else {
            &self.delta_block
        }
    }

    /// The label the DELTA BLOCK pick list shows.
    #[must_use]
    pub fn delta_block_label(&self) -> &str {
        if self.delta_block.trim().is_empty() {
            "auto"
        } else {
            self.effective_delta_block()
        }
    }

    /// The reader/writer counts `engine_config` will hand to the engine.
    ///
    /// `(0, 0)` means "let the engine derive it", which the rail prints as
    /// `auto`. An explicit slider overrides its own side only.
    #[must_use]
    pub fn thread_pair(&self) -> (usize, usize) {
        let (sr, sw) = self.resolved_threads();
        let (sr, sw) = if self.threads > 0 {
            (sr, sw)
        } else {
            (0, 0)
        };
        // The engine clamps each side to 64 and then shrinks the pair to the
        // logical CPU count. Printing the raw slider values put two different
        // thread counts on one screen.
        let mut r = if self.readers > 0 {
            self.readers as usize
        } else {
            sr
        }
        .clamp(1, 64);
        let mut w = if self.writers > 0 {
            self.writers as usize
        } else {
            sw
        }
        .clamp(1, 64);
        let logical = self.logical.max(2);
        while r + w > logical {
            if r >= w && r > 1 {
                r -= 1;
            } else if w > 1 {
                w -= 1;
            } else {
                break;
            }
        }
        (r, w)
    }

    /// Raise the privileges the fast paths need, and record what came back.
    ///
    /// Same four names the CLI asks for, for the same reason: on Windows they
    /// have to be enabled on the token before the first `CreateFileW`, and on
    /// Linux this is a read-only probe of `CapEff`. Large pages follow the
    /// privilege rather than the other way round, because asking for a page size
    /// the process cannot lock only produces a slower fallback.
    fn apply_privileges(&mut self) -> HugeChoice {
        let wanted = [
            sirocco_sys::privilege_names::MANAGE_VOLUME,
            sirocco_sys::privilege_names::LOCK_MEMORY,
            sirocco_sys::privilege_names::BACKUP,
            sirocco_sys::privilege_names::RESTORE,
        ];
        match sirocco_sys::set_privileges(&wanted) {
            | Ok(held) => {
                // The privilege is what large pages need, so holding it turns
                // `Auto` on for *this run* rather than leaving the probe to
                // discover it again. The form is not rewritten: a pick list that
                // flips by itself and never flips back is a bug report.
                let huge = if held.iter().any(|p| p.contains("LockMemory"))
                    && self.huge == HugeChoice::Auto
                {
                    HugeChoice::On
                } else {
                    self.huge
                };
                // "0 of 4" is the normal answer for an unelevated process: the
                // privileges are not in the token at all, so `AdjustTokenPrivileges`
                // cannot enable them. Saying which case it is saves the user from
                // concluding the switch is broken.
                self.priv_note = Some(if held.is_empty() {
                    if self.elevated {
                        "the system refused all four privileges".to_string()
                    } else {
                        "not elevated — 0 of 4 privileges are in this token; restart as administrator"
                            .to_string()
                    }
                } else if held.len() < wanted.len() {
                    format!(
                        "{} of {} privileges enabled: {}",
                        held.len(),
                        wanted.len(),
                        held.join(", ")
                    )
                } else {
                    format!("privileges enabled: {}", held.join(", "))
                });
                return huge;
            },
            | Err(e) => {
                // Not fatal: the run continues on the unprivileged paths, and
                // saying so beats a dialog the user cannot act on.
                self.priv_note = Some(format!("privileges unavailable: {e}"));
            },
        }
        self.huge
    }

    /// A sentence the user must read before pressing Start.
    #[must_use]
    pub fn caveat(&self) -> Option<(&'static str, iced::Color)> {
        match self.mode {
            | Mode::Move => {
                Some((
                    "Move deletes each source file after it has been copied and verified.",
                    th::WARN,
                ))
            },
            | Mode::Sync => {
                Some((
                    "Sync also deletes files in the destination that are not in the source.",
                    th::WARN,
                ))
            },
            | _ if self.overwrite == Overwrite::Always => {
                Some((
                    "Overwrite is set to Always: existing destinations are replaced unconditionally.",
                    th::WARN,
                ))
            },
            | _ => None,
        }
    }

    /// The cumulative counters to display: finished sources plus the live one.
    #[must_use]
    pub fn live(&self) -> Option<Snapshot> {
        self.job.as_ref().map(|j| j.base.merge(&j.stats.snapshot()))
    }

    /// True while the source in flight is still being walked.
    ///
    /// The engine stores `files_total` / `bytes_total` once the scan finishes, so
    /// zero means "not known yet". The progress card needs this separately from
    /// the merged snapshot: with several sources the merged totals are never
    /// zero, and the bar would claim 100 % of the sources already done while the
    /// next one is still being scanned.
    #[must_use]
    pub fn current_source_scanning(&self) -> bool {
        self.job.as_ref().is_some_and(|j| {
            let s = j.stats.snapshot();
            s.files_total == 0 && s.bytes_total == 0
        })
    }

    /// Totals of the sources that have already finished.
    #[must_use]
    pub fn finished_totals(&self) -> Totals {
        self.job.as_ref().map_or_else(Totals::default, |j| j.base)
    }
}

impl Default for App {
    fn default() -> Self {
        let topo = sirocco_sys::Topology::detect();
        let caps = sirocco_sys::caps::init();
        let extra = caps.enabled();
        // "unprivileged" reads as "nothing works", which is not what it means:
        // it only says the volume-management privilege is missing.
        let tier = match caps.tier() {
            | "unprivileged" => "standard",
            | other => other,
        };
        let caps_line = if extra.is_empty() {
            tier.to_string()
        } else {
            format!("{tier} · {}", extra.join(", "))
        };
        let src = std::env::var("SIROCCO_GUI_SRC").unwrap_or_default();
        Self {
            mode: Mode::Copy,
            sources: if src.trim().is_empty() {
                Vec::new()
            } else {
                vec![PathBuf::from(src.trim())]
            },
            path_input: String::new(),
            dst: std::env::var("SIROCCO_GUI_DST").unwrap_or_default(),
            into: false,
            verify: VerifyMode::None,
            overwrite: Overwrite::IfDifferent,
            sparse: false,
            links: true,
            acl: false,
            xattrs: false,
            perms: true,
            keep_times: true,
            index: IndexMode::Off,
            index_dir: String::new(),
            journal: false,
            sync: false,
            dry_run: false,
            threads: 0,
            exclude: String::new(),
            include: String::new(),
            order: Order::Walk,
            follow_links: false,
            one_fs: false,
            files_from: String::new(),
            exclude_from: String::new(),
            include_from: String::new(),
            max_depth: 0,
            hash: HashKind::Xxh64,
            buffer: "1M".into(),
            io_unit: "1M".into(),
            overlap: 8,
            queue: 0,
            feeders: 0,
            readers: 0,
            writers: 0,
            direct: true,
            small_copy: SmallCopy::Auto,
            small: "256K".into(),
            prealloc: PreallocMode::Auto,
            lcn: true,
            huge: HugeChoice::Auto,
            compress: CompressMode::Off,
            delta: true,
            // Empty means "follow the I/O unit", which is what the engine derives
            // when the block is left at its default.
            delta_block: String::new(),
            delta_min: "8M".into(),
            probes: 32,
            io_profile: None,
            calibrating: false,
            calib_rx: None,
            calib_for: None,
            calib_at: None,
            calibration: String::new(),
            backend: ReactorKind::Native,
            pin: false,
            cpu_list: String::new(),
            numa: NumaChoice::Auto,
            numa_node: 0,
            io_prio: "default".into(),
            proc_prio: "default".into(),
            thread_prio: "default".into(),
            privileged: false,
            // Test hook: the screenshot harness cannot click, and the advanced
            // groups are the part most likely to break a layout.
            advanced: std::env::var_os("SIROCCO_GUI_ADVANCED").is_some(),
            phase: Phase::Idle,
            history: Vec::with_capacity(SAMPLES),
            active: Vec::new(),
            feed: VecDeque::new(),
            status: "Ready".into(),
            error: None,
            summary: None,
            tick: 0,
            last_readers: 0,
            last_writers: 0,
            last_cap: 1,
            caps: caps_line,
            priv_note: None,
            elevated: caps.elevated,
            cores: topo.physical_count(),
            logical: topo.logical_count(),
            stop_requested: false,
            job: None,
        }
    }
}

/// Every way the UI can change the state.
///
/// Each variant carries the new value of the field it is named after, so a doc
/// comment per variant would only restate the name.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum Message {
    ModePicked(Mode),
    PathTyped(String),
    PathSubmitted,
    AddFiles,
    AddFolders,
    PickedSources(Vec<PathBuf>),
    RemoveSource(usize),
    ClearSources,
    Dst(String),
    BrowseDst,
    PickedDst(Option<PathBuf>),
    Into(bool),
    VerifyPicked(VerifyMode),
    OverwritePicked(Overwrite),
    Sparse(bool),
    Links(bool),
    Acl(bool),
    Xattrs(bool),
    Perms(bool),
    KeepTimes(bool),
    IndexPicked(IndexMode),
    IndexDir(String),
    Journal(bool),
    Sync(bool),
    DryRun(bool),
    Threads(f32),
    Exclude(String),
    Include(String),
    OrderPicked(Order),
    FollowLinks(bool),
    OneFs(bool),
    FilesFrom(String),
    ExcludeFrom(String),
    IncludeFrom(String),
    MaxDepth(u32),
    HashPicked(HashKind),
    BufferPicked(String),
    IoUnitPicked(String),
    Overlap(u32),
    Queue(u32),
    Feeders(u32),
    Readers(u32),
    Writers(u32),
    Direct(bool),
    SmallCopyPicked(SmallCopy),
    SmallThresholdPicked(String),
    PreallocPicked(PreallocMode),
    Lcn(bool),
    HugePicked(HugeChoice),
    CompressPicked(CompressMode),
    Delta(bool),
    DeltaBlockPicked(String),
    DeltaMinPicked(String),
    Probes(u32),
    Calibrate,
    BackendPicked(ReactorKind),
    Pin(bool),
    CpuList(String),
    NumaPicked(NumaChoice),
    NumaNode(u32),
    IoPrioPicked(String),
    ProcPrioPicked(String),
    ThreadPrioPicked(String),
    Privileged(bool),
    Elevate,
    ToggleAdvanced,
    Start,
    Cancel,
    Tick,
    Dropped(PathBuf),
    Clear,
}

impl App {
    fn title(&self) -> String {
        match self.phase {
            | Phase::Running => "Sirocco — working".into(),
            | _ => "Sirocco".into(),
        }
    }

    // iced's application builder hands these to the runtime as callbacks, so
    // they take `&self` whether or not they read it.
    #[allow(clippy::unused_self)]
    fn theme(&self) -> Theme {
        th::theme()
    }

    #[allow(clippy::unused_self)]
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(90)).map(|_| Message::Tick),
            iced::event::listen_with(|event, _status, _id| {
                match event {
                    | iced::Event::Window(iced::window::Event::FileDropped(p)) => {
                        Some(Message::Dropped(p))
                    },
                    | iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                        ..
                    }) => Some(Message::Cancel),
                    | _ => None,
                }
            }),
        ])
    }

    // One arm per message, one line per arm: this is a dispatch table, and
    // splitting it would only move the length somewhere else.
    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        msg: Message,
    ) -> Task<Message> {
        match msg {
            | Message::ModePicked(m) => self.mode = m,
            | Message::PathTyped(s) => self.path_input = s,
            | Message::PathSubmitted => {
                let typed = self.path_input.trim().trim_matches('"').to_string();
                if !typed.is_empty() {
                    self.add_sources([PathBuf::from(typed)]);
                    self.path_input.clear();
                }
            },
            | Message::AddFiles => return Task::perform(pick_files(), Message::PickedSources),
            | Message::AddFolders => return Task::perform(pick_folders(), Message::PickedSources),
            | Message::PickedSources(v) => {
                self.add_sources(v);
                return Task::none();
            },
            | Message::RemoveSource(i) => {
                if i < self.sources.len() {
                    self.sources.remove(i);
                    self.into = self.into && self.single_dir();
                }
            },
            | Message::ClearSources => {
                self.sources.clear();
                self.into = false;
            },
            | Message::Dst(s) => {
                // The measured rates describe the destination volume; keeping them
                // after the path changes would feed the cost model numbers for the
                // wrong device.
                if s.trim() != self.dst.trim() {
                    self.io_profile = None;
                    self.calibration.clear();
                    // A measurement of the old volume cannot answer for the new
                    // one, and leaving it running keeps the button disabled.
                    if self.calib_for.as_deref() != Some(std::path::Path::new(s.trim())) {
                        self.calib_rx = None;
                        self.calib_at = None;
                        self.calibrating = false;
                    }
                }
                self.dst = s;
            },
            | Message::BrowseDst => return Task::perform(pick_folder(), Message::PickedDst),
            | Message::PickedDst(Some(p)) => {
                // Through `Dst`, not straight into the field: the browse button
                // has to invalidate the measured rates exactly like typing does.
                return self.update(Message::Dst(p.to_string_lossy().to_string()));
            },
            | Message::PickedDst(None) => {},
            | Message::Into(v) => self.into = v,
            | Message::VerifyPicked(v) => self.verify = v,
            | Message::OverwritePicked(o) => self.overwrite = o,
            | Message::Sparse(v) => self.sparse = v,
            | Message::Links(v) => self.links = v,
            | Message::Acl(v) => self.acl = v,
            | Message::Xattrs(v) => self.xattrs = v,
            | Message::Perms(v) => self.perms = v,
            | Message::KeepTimes(v) => self.keep_times = v,
            | Message::IndexPicked(m) => self.index = m,
            | Message::IndexDir(s) => self.index_dir = s,
            | Message::Journal(v) => self.journal = v,
            | Message::Sync(v) => self.sync = v,
            | Message::DryRun(v) => self.dry_run = v,
            | Message::Threads(v) => self.threads = v as u32,
            | Message::Exclude(s) => self.exclude = s,
            | Message::Include(s) => self.include = s,
            | Message::OrderPicked(o) => self.order = o,
            | Message::FollowLinks(v) => self.follow_links = v,
            | Message::OneFs(v) => self.one_fs = v,
            | Message::FilesFrom(s) => self.files_from = s,
            | Message::ExcludeFrom(s) => self.exclude_from = s,
            | Message::IncludeFrom(s) => self.include_from = s,
            | Message::MaxDepth(n) => self.max_depth = n,
            | Message::HashPicked(h) => self.hash = h,
            | Message::BufferPicked(s) => {
                // The engine clamps the unit to the buffer, so the list must too:
                // otherwise the field keeps showing a value that is no longer used.
                self.buffer = s;
                let limit = sirocco_core::human::parse_size(&self.buffer).unwrap_or(1 << 20);
                if sirocco_core::human::parse_size(&self.io_unit).is_none_or(|v| v > limit) {
                    self.io_unit = self.buffer.clone();
                }
            },
            | Message::IoUnitPicked(s) => self.io_unit = s,
            | Message::Overlap(n) => self.overlap = n,
            | Message::Queue(n) => {
                // `normalize_with` floors the depth at 8; a slider that offers 1
                // and then runs 8 is a number the run does not have.
                self.queue = if n == 0 { 0 } else { n.max(8) };
            },
            | Message::Feeders(n) => self.feeders = n,
            | Message::Readers(n) => self.readers = n,
            | Message::Writers(n) => self.writers = n,
            | Message::Direct(v) => self.direct = v,
            | Message::SmallCopyPicked(m) => self.small_copy = m,
            | Message::SmallThresholdPicked(s) => self.small = s,
            | Message::PreallocPicked(m) => self.prealloc = m,
            | Message::Lcn(v) => self.lcn = v,
            | Message::HugePicked(h) => self.huge = h,
            | Message::CompressPicked(m) => self.compress = m,
            | Message::Delta(v) => self.delta = v,
            | Message::DeltaBlockPicked(s) => self.delta_block = s,
            | Message::DeltaMinPicked(s) => self.delta_min = s,
            | Message::Probes(n) => self.probes = n,
            | Message::Calibrate => {
                // A plain thread, like the job itself: the calibration does real
                // I/O for a couple of seconds and would block the UI executor.
                if !self.calibrating && !self.is_running() && !self.dst.trim().is_empty() {
                    let dst = PathBuf::from(self.dst.trim());
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.calib_rx = Some(rx);
                    self.calib_for = Some(dst.clone());
                    self.calib_at = Some(Instant::now());
                    self.calibrating = true;
                    self.calibration = "measuring…".into();
                    std::thread::Builder::new()
                        .name("sirocco-calibrate".into())
                        .spawn(move || {
                            // The reason travels with the result: "calibration
                            // failed" with no explanation is what made this
                            // button look broken.
                            let r = session::calibrate(&dst).map_err(|e| e.to_string());
                            let _ = tx.send(r);
                        })
                        .ok();
                }
            },
            | Message::BackendPicked(k) => self.backend = k,
            | Message::Pin(v) => self.pin = v,
            | Message::CpuList(s) => self.cpu_list = s,
            | Message::NumaPicked(n) => self.numa = n,
            | Message::NumaNode(n) => self.numa_node = n,
            | Message::IoPrioPicked(s) => self.io_prio = s,
            | Message::ProcPrioPicked(s) => self.proc_prio = s,
            | Message::ThreadPrioPicked(s) => self.thread_prio = s,
            | Message::Privileged(v) => self.privileged = v,
            | Message::Elevate => {
                // `set_privileges` can only enable what the token already holds.
                // Getting the rest means a new process with a UAC prompt, which
                // is the one thing this window cannot do to itself.
                let mut args = Vec::new();
                for s in &self.sources {
                    args.push("--src".to_string());
                    args.push(s.to_string_lossy().into_owned());
                }
                if !self.dst.trim().is_empty() {
                    args.push("--dst".to_string());
                    args.push(self.dst.trim().to_string());
                }
                args.push("--privileged".to_string());
                return match sirocco_sys::relaunch_elevated(&args) {
                    | Ok(()) => iced::exit(),
                    | Err(e) => {
                        self.error = Some(format!("cannot restart elevated: {e}"));
                        Task::none()
                    },
                };
            },
            | Message::ToggleAdvanced => self.advanced = !self.advanced,
            | Message::Clear => {
                // Wiping the feed while a job is running would desynchronise the
                // view from the engine, so the button is disabled instead.
                if !self.is_running() {
                    self.feed.clear();
                    self.active.clear();
                    self.history.clear();
                    self.error = None;
                    self.summary = None;
                    self.phase = Phase::Idle;
                    self.status = "Ready".into();
                }
            },
            | Message::Dropped(p) => {
                // A drop during a run is ignored: the job already captured its
                // paths, so rewriting them would only desynchronise the form.
                //
                // A drop is always a *source*. Filling the destination from the
                // second drop onwards made "drop three files here" impossible and
                // made the same gesture mean two different things.
                if !self.is_running() {
                    self.add_sources([p]);
                }
            },
            | Message::Start => return self.start(),
            | Message::Cancel => {
                if let Some(job) = self.job.as_ref() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                if self.job.is_some() {
                    self.stop_requested = true;
                    sirocco_sys::interrupt::flag().store(true, Ordering::Relaxed);
                    self.status = "Stopping…".into();
                }
            },
            | Message::Tick => self.poll(),
        }
        Task::none()
    }

    /// Translate the form into an [`EngineConfig`].
    ///
    /// Kept out of [`App::start`] so the mapping is one readable block: every
    /// control in the rail appears here exactly once, and a field the GUI does not
    /// offer is visible as one left at `EngineConfig::default()`.
    fn engine_config(
        &self,
        readers: usize,
        writers: usize,
        exclude: Vec<String>,
        include: Vec<String>,
        huge: HugeChoice,
    ) -> EngineConfig {
        let scan = sirocco_core::plan::ScanCfg {
            exclude,
            include,
            order: self.order,
            follow_symlinks: self.follow_links,
            one_filesystem: self.one_fs,
            // `ScanCfg::max_depth` counts levels to descend, so 0 copies only the
            // top level. The UI's 0 means "no limit", which is `usize::MAX`.
            max_depth: if self.max_depth == 0 {
                usize::MAX
            } else {
                self.max_depth as usize
            },
            ..sirocco_core::plan::ScanCfg::default()
        };

        // An explicit count overrides its own side only; the other side keeps the
        // split. `--readers` / `--writers` override both on the command line
        // because argv has no notion of "unset", but a slider does, and silently
        // discarding the other half's value would be worse than diverging.
        let (split_readers, split_writers) = if self.threads > 0 {
            (readers, writers)
        } else {
            (0, 0)
        };

        // A label the pick list offered; `parse_size` is the same parser the CLI
        // uses, so `--buffer=1M` and this menu mean the same number.
        let size_of =
            |label: &str, fallback: u64| sirocco_core::human::parse_size(label).unwrap_or(fallback);

        EngineConfig {
            mode: self.mode,
            quiet: true,
            verbose: 0,
            overwrite: self.overwrite,
            verify: self.verify,
            hash: self.hash,
            sparse: self.sparse,
            preserve_links: self.links,
            preserve_acl: self.acl,
            preserve_xattrs: self.xattrs,
            preserve_permissions: self.perms,
            keep_times: self.keep_times,
            journal: self.journal,
            sync_at_end: self.sync,
            dry_run: self.dry_run,
            // `into` only means something for a single folder source; with
            // several, each one is nested by construction.
            into: self.into && self.single_dir(),
            index: self.index,
            index_dir: (!self.index_dir.trim().is_empty())
                .then(|| PathBuf::from(self.index_dir.trim())),
            file_list: (!self.files_from.trim().is_empty())
                .then(|| PathBuf::from(self.files_from.trim())),
            // --- engine shape
            buffer_size: size_of(&self.buffer, 1 << 20) as usize,
            // `normalize_with` clamps the request to the buffer anyway; clamping
            // here keeps the number on screen equal to the number used.
            io_unit: size_of(&self.io_unit, 1 << 20).min(size_of(&self.buffer, 1 << 20)),
            overlap: self.overlap as usize,
            queue_depth: self.queue as usize,
            feeders: self.feeders as usize,
            direct_io: self.direct,
            small_copy: self.small_copy,
            small_threshold: size_of(&self.small, 256 << 10),
            prealloc: self.prealloc,
            reorder_lcn: self.lcn,
            // Three states, not two: `Auto` leaves both flags false so the
            // capability probe can turn large pages on when the privilege exists.
            huge_pages: matches!(huge, HugeChoice::On),
            no_huge_pages: matches!(huge, HugeChoice::Off),
            compress: self.compress,
            delta_enabled: self.delta,
            delta: sirocco_core::delta::DeltaCfg {
                min_size: size_of(&self.delta_min, 8 << 20),
                block: size_of(&self.delta_block, 1 << 20),
                probes: self.probes,
                ..sirocco_core::delta::DeltaCfg::default()
            },
            io_profile: self.io_profile,
            reactor: self.backend,
            // --- scheduling
            // `--cpu-list` implies pinning on the command line: a placement
            // restriction nothing honours is a silent no-op.
            pin_threads: self.pin || !self.cpu_list.trim().is_empty(),
            cpu_list: sirocco_sys::affinity::parse_cpu_list(self.cpu_list.trim()),
            numa: match self.numa {
                | NumaChoice::Auto => NumaMode::Auto,
                | NumaChoice::Off => NumaMode::Off,
                | NumaChoice::Node => NumaMode::Node(self.numa_node as u16),
            },
            io_priority: sirocco_sys::io_priority_from_name(&self.io_prio),
            process_priority: sirocco_sys::process_priority_from_name(&self.proc_prio),
            thread_priority: sirocco_sys::thread_priority_from_name(&self.thread_prio),
            // Mirroring is the *point* of Sync; without this it silently leaves
            // stale files behind, which is worse than not offering the mode.
            // The engine honours this only in Sync, and the GUI always mirrors
            // in Sync, so there is nothing to toggle.
            delete_extra: self.mode == Mode::Sync,
            // 0 means "let the engine decide", which is what `resolved_threads`
            // already mirrors for the labels.
            readers: if self.readers > 0 {
                self.readers as usize
            } else {
                split_readers
            },
            writers: if self.writers > 0 {
                self.writers as usize
            } else {
                split_writers
            },
            scan,
            ..EngineConfig::default()
        }
    }

    /// Validate, then start the job.
    ///
    /// Every check runs before anything is spawned or any privilege is raised, so
    /// a typo never costs the previous run's report.
    // One linear validation-and-launch block: every check must run before the
    // next, and splitting it would only scatter the "nothing has happened yet"
    // invariant across helpers.
    #[allow(clippy::too_many_lines)]
    fn start(&mut self) -> Task<Message> {
        if self.is_running() {
            return Task::none();
        }
        self.error = None;
        if self.sources.is_empty() {
            self.error = Some("Add at least one file or folder to copy.".into());
            return Task::none();
        }
        if self.dst.trim().is_empty() {
            self.error = Some("A destination is required.".into());
            return Task::none();
        }
        let dst = PathBuf::from(self.dst.trim());
        for src in &self.sources {
            if !src.exists() {
                self.error = Some(format!("{} does not exist", src.display()));
                return Task::none();
            }
        }
        if self.nested() && dst.exists() && !dst.is_dir() {
            self.error = Some(
                "Several sources need a folder as the destination — each one is placed inside it."
                    .into(),
            );
            return Task::none();
        }
        // A destination inside a source is copied into itself: the second run
        // duplicates the duplicate until the disk fills. This applies to every
        // source, not just the single-source case — with several, the scan of
        // one source would still walk into the destination the others write to.
        for src in &self.sources {
            if src.is_dir() && is_inside(&dst, src) {
                self.error = Some(format!(
                    "The destination is inside the source {}.",
                    src.display()
                ));
                return Task::none();
            }
        }

        // A CPU list that does not parse would silently fall back to the default
        // placement, which is the opposite of what the user asked for.
        if !self.cpu_list.trim().is_empty()
            && sirocco_sys::affinity::parse_cpu_list(self.cpu_list.trim()).is_none()
        {
            self.error = Some(format!(
                "cannot parse the CPU list: {}",
                self.cpu_list.trim()
            ));
            return Task::none();
        }

        let (exclude, e1) = pattern_set(&self.exclude, &self.exclude_from);
        let (include, e2) = pattern_set(&self.include, &self.include_from);
        if let Some(e) = e1.or(e2) {
            self.error = Some(e);
            return Task::none();
        }

        // The flag is cleared here, on the UI thread, so a Stop pressed before
        // the job thread reaches its own reset is not discarded.
        sirocco_sys::interrupt::reset();
        self.stop_requested = false;
        // The note describes the privilege request of one run; leaving the last
        // one on screen after the switch is turned off describes a run that asked
        // for nothing.
        self.priv_note = None;
        let huge = if self.privileged {
            self.apply_privileges()
        } else {
            self.huge
        };

        // With several sources the destination is the folder they land in, so it
        // has to exist before the first per-source run starts nesting into it.
        if self.nested()
            && let Err(e) = std::fs::create_dir_all(&dst)
        {
            self.error = Some(format!("cannot create {}: {e}", dst.display()));
            return Task::none();
        }

        self.summary = None;
        self.feed.clear();
        self.active.clear();
        self.history.clear();

        let (readers, writers) = self.thread_pair();
        let mut cfg = self.engine_config(readers, writers, exclude, include, huge);
        // Normalise here so the meters and the queue capacity describe the config
        // the engine will actually run with. `run_with` normalises again, which is
        // idempotent, so the numbers on screen and the numbers in force agree.
        cfg.normalize_with(self.cores.max(1), self.logical.max(2));

        // Bounded: a kernel-path copy of 200k small files can outrun the 90 ms
        // drain, and an unbounded channel turns that into a memory spike plus one
        // enormous frame. A dropped row costs a line in the feed, not a counter.
        let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel(EVENT_BUDGET);
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));

        // `queue_depth` is per queue and `Counters.queued` sums both, so the
        // capacity shown has to be the sum. It comes from the normalised config,
        // which means an explicit depth and the engine's floor both apply — a
        // meter that can read "16 / 2" is worse than no meter.
        let queue_cap = 2 * cfg.queue_depth.max(1);
        let (final_readers, final_writers) = (cfg.readers, cfg.writers);

        let sources = self.sources.clone();
        let cancel_for_job = cancel.clone();
        std::thread::Builder::new()
            .name("sirocco-gui-job".into())
            .spawn(move || {
                run_sources(&cfg, &sources, &dst, &ev_tx, &msg_tx, &cancel_for_job);
            })
            .ok();

        self.last_readers = final_readers;
        self.last_writers = final_writers;
        self.last_cap = queue_cap.max(1);
        self.job = Some(Job {
            total: self.sources.len(),
            index: 0,
            name: display_name(self.sources.first()),
            stats: Arc::new(sirocco_core::stats::Stats::new()),
            events: ev_rx,
            msgs: msg_rx,
            base: Totals::default(),
            last: None,
            errors: Vec::new(),
            last_bytes: 0,
            last_at: Instant::now(),
            smoothed: 0.0,
            cancel,
            cancelled: false,
        });
        self.phase = Phase::Running;
        self.status = self.running_status();
        Task::none()
    }

    /// The header line while a job runs.
    fn running_status(&self) -> String {
        let verb = match self.mode {
            | _ if self.dry_run => "Previewing",
            | Mode::Move => "Moving",
            | Mode::Sync => "Mirroring",
            | _ => "Copying",
        };
        match self.job.as_ref() {
            | Some(j) if j.total > 1 => {
                format!("{verb} {} of {} — {}", j.index + 1, j.total, j.name)
            },
            | _ => format!("{verb}…"),
        }
    }

    /// One frame: read the engine's counters, drain the event feed, fold both
    /// into the visible state.
    // The steps are ordered and share local state (`snap`, `events`, `msgs`);
    // splitting them would mean passing six values through each helper.
    #[allow(clippy::too_many_lines)]
    fn poll(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.poll_calibration();

        if self.stop_requested {
            // The engine resets the flag once when a run starts; keep asserting
            // it until the job actually stops.
            sirocco_sys::interrupt::flag().store(true, Ordering::Relaxed);
        }

        // Read everything out of the job handle first, then mutate `self`: the
        // handle borrows `self.job` and the row lists borrow `self`.
        let Some((msgs, events, snap, rate, now, name)) = self.read_job() else {
            return;
        };

        if let Some(job) = self.job.as_mut() {
            job.last_bytes = snap.bytes_accounted();
            job.last_at = now;
            job.smoothed = rate;
        }
        if self.history.len() >= SAMPLES {
            self.history.remove(0);
        }
        self.history.push(rate as f32);

        // Events first: a source that ended and the next one that started arrive
        // in the same tick, and the old source's rows belong to the old name.
        //
        // The prefix is only added when there is more than one source; with a
        // single folder, `src/a/b.txt` is noise that pushes the interesting part
        // of the path out of the row.
        let prefix = if self.job.as_ref().is_some_and(|j| j.total > 1) {
            name.as_str()
        } else {
            ""
        };
        for ev in events {
            match ev {
                | Event::Started { rel, size } => self.mark_active(prefix, &rel, size),
                | Event::Done { rel, size, .. } => {
                    self.mark_done(prefix, &rel, size, RowState::Done);
                },
                | Event::Skipped { rel } => {
                    self.mark_done(prefix, &rel, 0, RowState::Skipped);
                },
                | Event::Failed { rel, error } => {
                    self.mark_done(prefix, &rel, 0, RowState::Failed(error));
                },
                | Event::Finished { .. } => {},
            }
        }

        let mut finished = false;
        for msg in msgs {
            match msg {
                | JobMsg::Start { index, name, stats } => {
                    if let Some(job) = self.job.as_mut() {
                        job.index = index;
                        job.name = name;
                        job.stats = stats;
                        job.last_bytes = 0;
                        job.last_at = Instant::now();
                        job.smoothed = 0.0;
                    }
                    self.status = self.running_status();
                },
                | JobMsg::End { result } => self.source_finished(result),
                | JobMsg::AllDone => finished = true,
            }
        }
        if finished {
            self.finish_job();
        }
    }

    /// Drain the job's channels and sample its counters.
    ///
    /// Returns `None` when no job is running. The name is the source the events
    /// belong to, so rows can be labelled when several sources are in play.
    #[allow(clippy::type_complexity)]
    fn read_job(&self) -> Option<(Vec<JobMsg>, Vec<Event>, Snapshot, f64, Instant, String)> {
        let job = self.job.as_ref()?;
        let mut msgs = Vec::new();
        while let Ok(m) = job.msgs.try_recv() {
            msgs.push(m);
        }
        let snap = job.stats.snapshot();
        let now = Instant::now();
        // Same accounting as the progress bar: bytes that never needed writing
        // (delta hits, punched holes, hard links) count as progress, or the chart
        // would show a slow run while the bar races to the end.
        let done = snap.bytes_accounted();
        let dt = (now - job.last_at).as_secs_f64().max(1e-6);
        let instant = done.saturating_sub(job.last_bytes) as f64 / dt;
        // Light smoothing: per-tick deltas are noisy at 90 ms, but a cumulative
        // average hides a stall completely.
        let rate = instant.mul_add(0.45, job.smoothed * 0.55);
        // Bounded per frame: an unbounded drain makes one frame do work
        // proportional to a whole directory, which is exactly when the window
        // should stay responsive. The rest is read on the next tick.
        let mut events = Vec::new();
        while events.len() < EVENT_BUDGET {
            match job.events.try_recv() {
                | Ok(ev) => events.push(ev),
                | Err(_) => break,
            }
        }
        Some((msgs, events, snap, rate, now, job.name.clone()))
    }

    /// A source finished: fold its totals in, or record why it failed.
    fn source_finished(
        &mut self,
        result: Result<session::Summary, String>,
    ) {
        let reason = {
            let Some(job) = self.job.as_mut() else {
                return;
            };
            match result {
                | Ok(sum) => {
                    job.base.add(&sum.stats);
                    job.errors.extend(sum.errors.iter().cloned());
                    job.cancelled |= sum.interrupted;
                    // A file whose terminal event was dropped by the bounded
                    // channel must not stay "in flight" through the next source.
                    let reason = if sum.interrupted {
                        Some("cancelled")
                    } else if sum.stats.files_failed > 0 {
                        Some("run ended")
                    } else {
                        None
                    };
                    job.last = Some(sum);
                    reason
                },
                | Err(e) => {
                    // The request itself was refused (missing source, source
                    // equal to destination, unusable volume). The other sources
                    // still run.
                    job.errors.push(e);
                    Some("run ended")
                },
            }
        };
        self.flush_active(reason);
    }

    /// The last source is done: build the cumulative report and settle the phase.
    fn finish_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let base = job.base;
        let mut sum = job.last.unwrap_or_else(|| empty_summary(self.mode));
        sum.stats = base.merge(&Snapshot::default());
        sum.stats.elapsed_secs = base.elapsed;
        sum.errors = job.errors;
        sum.files_planned = base.files_total;
        sum.bytes_planned = base.bytes_total;
        sum.interrupted = job.cancelled;

        self.phase = if sum.interrupted {
            Phase::Cancelled
        } else if sum.stats.files_failed > 0 || !sum.errors.is_empty() {
            Phase::Failed
        } else {
            Phase::Done
        };
        self.status = describe(&sum, self.mode);
        self.flush_active(if sum.interrupted {
            Some("cancelled")
        } else if sum.stats.files_failed > 0 {
            Some("run ended")
        } else {
            None
        });
        self.summary = Some(sum);
        self.stop_requested = false;
    }

    /// A calibration writes real data; on a slow or full destination it can
    /// block far longer than the couple of seconds it is designed for. Without a
    /// deadline the button stays disabled on "Measuring…" with no way out.
    fn poll_calibration(&mut self) {
        if self.calibrating
            && self
                .calib_at
                .is_some_and(|t| t.elapsed() > Duration::from_secs(120))
        {
            self.calib_rx = None;
            self.calib_at = None;
            self.calibrating = false;
            self.calibration = "calibration timed out".to_string();
        }

        let Some(rx) = &self.calib_rx else {
            return;
        };
        match rx.try_recv() {
            | Ok(result) => {
                self.calib_rx = None;
                self.calib_at = None;
                self.calibrating = false;
                // A failed measurement keeps the previous profile: throwing away
                // a good one because a retry failed is not an improvement. A
                // result for a destination the user has since changed is a
                // measurement of some other volume; showing it would be a lie
                // that looks exactly like a valid one.
                let moved = self
                    .calib_for
                    .as_deref()
                    .is_none_or(|p| p != Path::new(self.dst.trim()));
                if moved {
                    self.calibration = "destination changed; measure again".to_string();
                } else {
                    match result {
                        | Ok(p) => {
                            self.io_profile = Some(p);
                            self.calibration = format!(
                                "read {} · write {} · dest read {}",
                                format_rate(p.read_bps),
                                format_rate(p.write_bps),
                                format_rate(p.dest_read_bps)
                            );
                        },
                        | Err(e) => self.calibration = format!("failed: {e}"),
                    }
                }
            },
            // The thread died (panic, or the spawn itself failed): stop
            // pretending it is still measuring, or the button stays disabled.
            | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.calib_rx = None;
                self.calib_at = None;
                self.calibrating = false;
                self.calibration = "calibration failed".to_string();
            },
            | Err(std::sync::mpsc::TryRecvError::Empty) => {},
        }
    }

    /// A file started. Updated in place if it is already listed, because a
    /// skipped-then-retried file must not produce two rows.
    fn mark_active(
        &mut self,
        source: &str,
        rel: &Path,
        size: u64,
    ) {
        let rel = label(source, rel);
        if let Some(row) = self.active.iter_mut().find(|r| r.rel == rel) {
            row.size = size;
            row.state = RowState::Active;
            return;
        }
        if self.active.len() >= ACTIVE_CAP {
            self.active.remove(0);
        }
        self.active.push(FileRow {
            rel,
            size,
            state: RowState::Active,
        });
    }

    /// A file reached a terminal state: move it out of the in-flight list and into
    /// the feed, in one row rather than two.
    fn mark_done(
        &mut self,
        source: &str,
        rel: &Path,
        size: u64,
        state: RowState,
    ) {
        let rel = label(source, rel);
        let size = match self.active.iter().position(|r| r.rel == rel) {
            | Some(i) => {
                let row = self.active.remove(i);
                row.size.max(size)
            },
            | None => size,
        };
        self.feed.push_front(FileRow { rel, size, state });
        while self.feed.len() > FEED_CAP {
            self.feed.pop_back();
        }
    }

    /// Settle the rows still in flight when the run ends.
    ///
    /// `reason` is `None` when the job finished normally; otherwise it is what
    /// the row should say, because "cancelled" on a run that died of its own
    /// error is the first thing a user reports as a bug.
    fn flush_active(
        &mut self,
        reason: Option<&str>,
    ) {
        for row in self.active.drain(..) {
            self.feed.push_front(FileRow {
                rel: row.rel,
                size: row.size,
                state: match reason {
                    | None => RowState::Done,
                    | Some(why) => RowState::Failed(why.to_string()),
                },
            });
        }
        while self.feed.len() > FEED_CAP {
            self.feed.pop_back();
        }
    }

    fn view(&self) -> Element<'_, Message> {
        ui::view(self)
    }
}

// ---------------------------------------------------------------- job thread

/// Run every source in order, reporting each one over the same two channels.
///
/// The engine's `run_with` is one source per call and resets the counter totals
/// when it starts, so a multi-source request is a sequence of sessions with a
/// fresh `Stats` each. The UI sums them; see [`Totals`].
fn run_sources(
    cfg: &EngineConfig,
    sources: &[PathBuf],
    dst: &Path,
    events: &std::sync::mpsc::SyncSender<Event>,
    msgs: &std::sync::mpsc::Sender<JobMsg>,
    cancel: &AtomicBool,
) {
    let nested = sources.len() > 1;
    for (i, src) in sources.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let stats = Arc::new(sirocco_core::stats::Stats::new());
        if msgs
            .send(JobMsg::Start {
                index: i,
                name: display_name(Some(src)),
                stats: stats.clone(),
            })
            .is_err()
        {
            return;
        }
        let mut c = cfg.clone();
        c.events = Some(sirocco_engine::config::EventSink::Bounded(events.clone()));
        // Several sources always land inside the destination directory; one
        // source keeps the engine's own rule (a folder merges, a file is the
        // destination itself).
        let target = if nested {
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.to_path_buf()
        };
        let r = session::run_with(c, src.clone(), target, Some(stats)).map_err(|e| e.to_string());
        let interrupted = r.as_ref().is_ok_and(|s| s.interrupted);
        if msgs.send(JobMsg::End { result: r }).is_err() {
            return;
        }
        if interrupted {
            break;
        }
    }
    let _ = msgs.send(JobMsg::AllDone);
}

/// How a source is named in the header and in front of its feed rows.
fn display_name(p: Option<&PathBuf>) -> String {
    p.and_then(|p| p.file_name()).map_or_else(
        || p.map_or_else(String::new, |p| p.display().to_string()),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Prefix a source-relative path with its source when there is more than one.
///
/// Two sources can both contain `a/b.txt`; without the prefix the feed shows two
/// identical rows and the user cannot tell which one failed.
fn label(
    source: &str,
    rel: &Path,
) -> String {
    let rel = rel.to_string_lossy().replace('\\', "/");
    if source.is_empty() {
        rel
    } else {
        format!("{source}/{rel}")
    }
}

/// A summary for a request that never produced one.
fn empty_summary(mode: Mode) -> session::Summary {
    session::Summary {
        mode,
        stats: Snapshot::default(),
        errors: Vec::new(),
        files_planned: 0,
        bytes_planned: 0,
        backend: String::new(),
        align: 0,
        readers: 0,
        writers: 0,
        volume_src: None,
        volume_dst: None,
        interrupted: false,
    }
}

/// `1 file` / `2 files`, because "Copied 1 files" is the kind of detail that
/// makes a careful program look careless.
fn files(n: u64) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// One sentence describing a finished job.
fn describe(
    sum: &session::Summary,
    mode: Mode,
) -> String {
    let took = format_duration(sum.stats.elapsed_secs);
    if sum.interrupted {
        return format!(
            "Stopped after {} files, {} copied, in {took}",
            sum.stats.files_done + sum.stats.files_skipped,
            sum.stats.files_done,
        );
    }
    if sum.backend == "dry-run" {
        // A preview reports a plan, not counters: "Copied 0 files" would be
        // technically true and completely useless.
        return format!(
            "Preview: {} files, {} to copy",
            sum.files_planned,
            format_bytes(sum.bytes_planned)
        );
    }
    let failed = sum.stats.files_failed;
    match mode {
        | Mode::Move if failed > 0 => {
            format!(
                "Moved {}, {failed} failed, in {took}",
                files(sum.stats.files_done)
            )
        },
        | Mode::Move => format!("Moved {} in {took}", files(sum.stats.files_done)),
        | Mode::Sync => {
            format!(
                "Mirrored in {took} — {} copied, {} already current",
                sum.stats.files_done, sum.stats.files_skipped
            )
        },
        // "Copied 0 files" over a tree where everything was already current
        // reads as a broken tool, not as success; and a run with failures must
        // not be described in the success voice.
        | _ if failed > 0 => {
            format!(
                "Copied {}, {failed} failed, in {took}",
                files(sum.stats.files_done)
            )
        },
        | _ if sum.stats.files_skipped > 0 => {
            format!(
                "Copied {} in {took} — {} already current",
                files(sum.stats.files_done),
                sum.stats.files_skipped
            )
        },
        | _ => format!("Copied {} in {took}", files(sum.stats.files_done)),
    }
}

// ---------------------------------------------------------------- helpers

/// True when two paths name the same thing.
///
/// `canonicalize` is the reliable answer but fails for a path that does not
/// exist, which is normal for a destination; the literal comparison covers that.
fn same_path(
    a: &Path,
    b: &Path,
) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        | (Ok(x), Ok(y)) => x == y,
        | _ => false,
    }
}

/// True when `dst` sits inside `src`.
///
/// `dst` usually does not exist yet, so its parent is canonicalised instead and
/// the final component appended — which is enough to catch the case that matters
/// (a destination the scan will also walk into).
fn is_inside(
    dst: &Path,
    src: &Path,
) -> bool {
    let Ok(root) = src.canonicalize() else {
        return false;
    };
    let full = if dst.exists() {
        dst.canonicalize().ok()
    } else {
        dst.parent()
            .and_then(|p| p.canonicalize().ok())
            .zip(dst.file_name())
            .map(|(p, n)| p.join(n))
    };
    full.is_some_and(|d| d != root && d.starts_with(&root))
}

/// Inline globs plus the lines of a pattern file.
///
/// Same shape as `--exclude` / `--exclude-from`: blank lines and `#` comments are
/// skipped, everything else is a pattern. The error is returned rather than
/// swallowed: a pattern file that cannot be read must not silently become "no
/// patterns", which would copy exactly what the user excluded.
fn pattern_set(
    inline: &str,
    from_file: &str,
) -> (Vec<String>, Option<String>) {
    let mut out = split_patterns(inline);
    let path = from_file.trim();
    if path.is_empty() {
        return (out, None);
    }
    match std::fs::read_to_string(path) {
        | Ok(text) => {
            for line in text.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    out.push(line.to_string());
                }
            }
            (out, None)
        },
        | Err(e) => (out, Some(format!("cannot read pattern file {path}: {e}"))),
    }
}

fn split_patterns(s: &str) -> Vec<String> {
    s.split([',', '\n', '\r'])
        .flat_map(|p| p.split_whitespace())
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .pick_folder()
        .await
        .map(|h| h.path().to_path_buf())
}

async fn pick_files() -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .pick_files()
        .await
        .map(|v| v.into_iter().map(|h| h.path().to_path_buf()).collect())
        .unwrap_or_default()
}

async fn pick_folders() -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .pick_folders()
        .await
        .map(|v| v.into_iter().map(|h| h.path().to_path_buf()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- shared helpers

impl App {
    /// True when the finished job was a preview: it reported a plan, not counters.
    #[must_use]
    pub fn is_preview(&self) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|s| s.backend == "dry-run")
    }

    /// Current counters, or `None` when no job is running.
    #[must_use]
    pub fn snap(&self) -> Option<Snapshot> {
        self.live()
    }

    /// (readers, writers, readers busy, writers busy, queued, queue capacity)
    #[must_use]
    pub fn gauges(&self) -> (usize, usize, usize, usize, usize, usize) {
        self.job.as_ref().map_or(
            // Idle: keep the last shape, all meters empty.
            (self.last_readers, self.last_writers, 0, 0, 0, self.last_cap),
            |job| {
                let s = job.stats.snapshot();
                (
                    self.last_readers,
                    self.last_writers,
                    (s.readers_active as usize).min(self.last_readers),
                    (s.writers_active as usize).min(self.last_writers),
                    s.queued as usize,
                    self.last_cap.max(1),
                )
            },
        )
    }

    /// True while a job owns the pipeline.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// Rows currently in the feed, in-flight ones included.
    #[must_use]
    pub fn shown_count(&self) -> usize {
        self.feed.len() + self.active.len()
    }
}

/// Bytes in a compact human form (`1.50 MiB`).
#[must_use]
pub fn human_bytes(n: u64) -> String {
    format_bytes(n)
}

/// A transfer rate in a compact human form (`120 MiB/s`).
#[must_use]
pub fn human_rate(v: f64) -> String {
    format_rate(v)
}

/// A duration as `1m 05s`, or `--` when there is nothing to show.
#[must_use]
pub fn human_time(secs: f64) -> String {
    format_duration(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a config from a default app with one or two fields overridden.
    fn config_of(app: &App) -> EngineConfig {
        app.engine_config(4, 4, Vec::new(), Vec::new(), app.huge)
    }

    /// A copy that descends zero levels is not a copy. `ScanCfg` counts levels,
    /// so the UI's "0 = no limit" has to be translated, not forwarded.
    #[test]
    fn default_depth_is_unlimited() {
        assert_eq!(config_of(&App::default()).scan.max_depth, usize::MAX);

        let deep = App {
            max_depth: 3,
            ..App::default()
        };
        assert_eq!(config_of(&deep).scan.max_depth, 3);
    }

    /// Three states, because "not asked for" lets the capability probe decide
    /// while "off" refuses it. A checkbox cannot express that.
    #[test]
    fn large_pages_keeps_all_three_states() {
        let auto = config_of(&App {
            huge: HugeChoice::Auto,
            ..App::default()
        });
        assert!(
            !auto.huge_pages && !auto.no_huge_pages,
            "auto must leave both flags false"
        );

        let on = config_of(&App {
            huge: HugeChoice::On,
            ..App::default()
        });
        assert!(on.huge_pages && !on.no_huge_pages);

        let off = config_of(&App {
            huge: HugeChoice::Off,
            ..App::default()
        });
        assert!(!off.huge_pages && off.no_huge_pages);
    }

    /// `--cpu-list` turns pinning on in the CLI for the same reason: the
    /// affinity planner is ignored unless `pin_threads` is set.
    #[test]
    fn cpu_list_implies_pinning() {
        let cfg = config_of(&App {
            pin: false,
            cpu_list: "0-1".into(),
            ..App::default()
        });
        assert!(cfg.pin_threads);
        assert_eq!(cfg.cpu_list.as_deref(), Some(&[0usize, 1][..]));
    }

    /// An explicit count replaces its own side; the other keeps the split, so
    /// moving one slider does not silently reset the other.
    #[test]
    fn explicit_counts_override_one_side_only() {
        let cfg = config_of(&App {
            threads: 8,
            readers: 3,
            ..App::default()
        });
        assert_eq!(cfg.readers, 3);
        assert_eq!(cfg.writers, 4);
    }

    /// `into` is a `cp -r` flag for a single folder. With several sources the
    /// engine already nests each one, so forwarding it would nest twice.
    #[test]
    fn into_only_applies_to_a_single_folder() {
        let dir = std::env::temp_dir();
        let single = App {
            sources: vec![dir.clone()],
            into: true,
            ..App::default()
        };
        assert!(config_of(&single).into);

        let many = App {
            sources: vec![dir.clone(), dir],
            into: true,
            ..App::default()
        };
        assert!(!config_of(&many).into);
    }

    /// Duplicates are dropped, including the `D:\a` vs `D:\a\` spelling that a
    /// picker and a drop produce for the same folder.
    #[test]
    fn sources_are_deduplicated() {
        let mut app = App::default();
        let dir = std::env::temp_dir();
        app.add_sources([dir.clone(), dir]);
        assert_eq!(app.sources.len(), 1);

        app.add_sources([PathBuf::new()]);
        assert_eq!(app.sources.len(), 1, "an empty path is not a source");
    }

    /// A request with no source, or no destination, must not start.
    #[test]
    fn start_requires_both_ends() {
        let mut app = App::default();
        let _ = app.start();
        assert!(app.error.is_some());
        assert!(!app.is_running());

        let mut app = App {
            sources: vec![std::env::temp_dir()],
            ..App::default()
        };
        let _ = app.start();
        assert!(app.error.is_some());
        assert!(!app.is_running());
    }

    /// The totals of several sources have to look like one run: the progress bar
    /// reads `bytes_accounted()` over `bytes_total`.
    #[test]
    fn totals_merge_across_sources() {
        let mut base = Totals::default();
        base.add(&Snapshot {
            files_total: 10,
            files_done: 10,
            bytes_total: 1000,
            bytes_written: 1000,
            elapsed_secs: 2.0,
            ..Snapshot::default()
        });

        let second = Snapshot {
            files_total: 5,
            files_done: 2,
            bytes_total: 500,
            bytes_written: 200,
            elapsed_secs: 1.0,
            ..Snapshot::default()
        };

        let live = base.merge(&second);
        assert_eq!(live.files_total, 15);
        assert_eq!(live.files_done, 12);
        assert_eq!(live.bytes_total, 1500);
        assert_eq!(live.bytes_accounted(), 1200);
        assert!((live.elapsed_secs - 3.0).abs() < f64::EPSILON);
    }

    /// Two sources can hold the same relative path; the feed has to tell them
    /// apart or a failure looks like it belongs to the wrong file.
    #[test]
    fn feed_rows_are_prefixed_by_source() {
        assert_eq!(label("a", Path::new("x/y.txt")), "a/x/y.txt");
        assert_eq!(label("", Path::new("x/y.txt")), "x/y.txt");
    }

    /// The whole multi-source path, end to end: two folders in, each nested under
    /// its own name, two `End` messages, one `AllDone`.
    ///
    /// This is the part a window cannot be unit-tested through, so it is tested
    /// as the plain function it is.
    #[test]
    fn several_sources_run_in_sequence_and_nest() {
        let root = std::env::temp_dir().join(format!("sirocco-gui-ms-{}", std::process::id()));
        let a = root.join("alpha");
        let b = root.join("beta");
        let dst = root.join("out");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("one.txt"), b"one").unwrap();
        std::fs::write(b.join("two.txt"), b"two").unwrap();

        let cfg = EngineConfig {
            quiet: true,
            verbose: 0,
            ..EngineConfig::default()
        };
        let (ev_tx, _ev_rx) = std::sync::mpsc::sync_channel(64);
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        run_sources(
            &cfg,
            &[a, b],
            &dst,
            &ev_tx,
            &msg_tx,
            &AtomicBool::new(false),
        );

        let mut starts = 0;
        let mut ends = 0;
        let mut all_done = false;
        // `recv` rather than `iter`: the sender lives in this test, so the
        // channel never closes and `iter()` would wait for a drop that cannot
        // happen.
        while let Ok(m) = msg_rx.recv() {
            match m {
                | JobMsg::Start { .. } => starts += 1,
                | JobMsg::End { result } => {
                    if let Err(e) = result {
                        panic!("source failed: {e}");
                    }
                    ends += 1;
                },
                | JobMsg::AllDone => {
                    all_done = true;
                    break;
                },
            }
        }
        assert_eq!(starts, 2);
        assert_eq!(ends, 2);
        assert!(all_done);
        assert_eq!(
            std::fs::read(dst.join("alpha").join("one.txt")).unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(dst.join("beta").join("two.txt")).unwrap(),
            b"two"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
