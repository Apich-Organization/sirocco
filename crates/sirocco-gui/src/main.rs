//! Sirocco — desktop application.
//!
//! One screen: configuration on the left, live state on the right. Everything
//! that changes while the job runs comes from two places — the `Stats` block the
//! engine writes directly (no polling thread, no channel per counter) and a plain
//! `mpsc` feed of per-file events that the update loop drains once per frame.

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
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
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
    // The boot function is the first argument (it used to be a trailing
    // `run_with`), and the title is a builder method.
    iced::application(boot, App::update, App::view)
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
        .run()
}

/// Initial state, plus the first task.
///
/// `SIROCCO_GUI_SRC` / `SIROCCO_GUI_DST` pre-fill the form and start
/// immediately: the smoke test uses them, and they are handy for a "make it a
/// shortcut" setup.
fn boot() -> (App, Task<Message>) {
    let app = App::default();
    let auto = app.src_ready();
    (
        app,
        if auto {
            Task::done(Message::Start)
        } else {
            Task::none()
        },
    )
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

struct Running {
    stats: Arc<sirocco_core::stats::Stats>,
    events: Receiver<Event>,
    result: Receiver<Result<session::Summary, String>>,
    readers: usize,
    writers: usize,
    queue_cap: usize,
    /// Bytes at the previous sample, for an instantaneous rate.
    last_bytes: u64,
    last_at: Instant,
    smoothed: f64,
}

/// NUMA placement, as the pick list offers it.
///
/// The engine's [`NumaMode`] carries the node number inside the variant, which a
/// pick list cannot express: the list needs a fixed set of labels, and the node
/// is a number the user types or drags. So the two halves are kept apart here and
/// joined in [`App::config`].
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
    /// Source path, as typed or dropped.
    pub src: String,
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
    /// How many small files to hand to the kernel at once.
    pub small_batch: u32,
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
    calib_rx: Option<std::sync::mpsc::Receiver<Option<IoProfile>>>,
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
    /// Result of the last finished job.
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
    /// Physical cores, from the topology probe.
    pub cores: usize,
    /// Logical CPUs, which is what bounds `readers + writers`.
    pub logical: usize,

    /// A stop was requested. Re-asserted every tick because the engine resets
    /// the process-wide flag when the job thread starts, which would otherwise
    /// swallow a Stop pressed in the first few milliseconds.
    stop_requested: bool,

    running: Option<Running>,
}

impl App {
    /// True when the environment supplied a complete job.
    fn src_ready(&self) -> bool {
        !self.src.trim().is_empty() && !self.dst.trim().is_empty()
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
                self.priv_note = Some(if held.is_empty() {
                    "privileges refused".to_string()
                } else {
                    format!("privileges: {}", held.join(", "))
                });
                return huge;
            },
            | Err(e) => {
                // Not fatal: the run continues on the unprivileged paths, and
                // saying so beats a dialog the user cannot act on.
                self.priv_note = Some(format!("privileges unavailable ({e})"));
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
        Self {
            mode: Mode::Copy,
            src: std::env::var("SIROCCO_GUI_SRC").unwrap_or_default(),
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
            small_batch: 512,
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
            cores: topo.physical_count(),
            logical: topo.logical_count(),
            stop_requested: false,
            running: None,
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
    Src(String),
    Dst(String),
    BrowseSrc,
    BrowseDst,
    PickedSrc(Option<PathBuf>),
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
    SmallBatch(u32),
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
            | Message::Src(s) => {
                // `into` only means something for a folder. Leaving it set when the
                // source becomes a file produces "needs a folder" with no visible
                // control to turn it off.
                if !std::path::Path::new(s.trim()).is_dir() {
                    self.into = false;
                }
                self.src = s;
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
            | Message::BrowseSrc => return Task::perform(pick_folder(), Message::PickedSrc),
            | Message::BrowseDst => return Task::perform(pick_folder(), Message::PickedDst),
            | Message::PickedSrc(Some(p)) => {
                return self.update(Message::Src(p.to_string_lossy().to_string()));
            },
            | Message::PickedDst(Some(p)) => {
                // Through `Dst`, not straight into the field: the browse button
                // has to invalidate the measured rates exactly like typing does.
                return self.update(Message::Dst(p.to_string_lossy().to_string()));
            },
            | Message::PickedSrc(None) | Message::PickedDst(None) => {},
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
            | Message::SmallBatch(n) => self.small_batch = n,
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
                            let _ = tx.send(session::calibrate(&dst).ok());
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
                // Dropping a folder fills the first empty field, then the other.
                // A drop during a run is ignored: the job already captured its
                // paths, so rewriting them would only desynchronise the form.
                if !self.is_running() {
                    let s = p.to_string_lossy().to_string();
                    if self.src.is_empty() {
                        return self.update(Message::Src(s));
                    }
                    // Through `Dst`, not straight into the field: a dropped
                    // destination has to invalidate the measured rates exactly
                    // like a typed one.
                    return self.update(Message::Dst(s));
                }
            },
            | Message::Start => self.start(),
            | Message::Cancel => {
                if self.running.is_some() {
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
            into: self.into,
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
            small_batch: self.small_batch as usize,
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

    fn start(&mut self) {
        if self.is_running() {
            return;
        }
        // Validation first: a typo in the destination must not destroy the last
        // run's feed, chart and failure list.
        if self.src.trim().is_empty() || self.dst.trim().is_empty() {
            self.error = Some("Both a source and a destination are required.".into());
            return;
        }
        let src = PathBuf::from(self.src.trim());
        let dst = PathBuf::from(self.dst.trim());
        if !src.exists() {
            self.error = Some(format!("{} does not exist", src.display()));
            return;
        }
        if self.into && !src.is_dir() {
            self.error = Some("“Copy into” needs a folder as the source.".into());
            return;
        }
        // A destination inside the source is copied into itself: the second run
        // duplicates the duplicate until the disk fills.
        if src.is_dir() && is_inside(&dst, &src) {
            self.error = Some("The destination is inside the source.".into());
            return;
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
            return;
        }

        let (exclude, e1) = pattern_set(&self.exclude, &self.exclude_from);
        let (include, e2) = pattern_set(&self.include, &self.include_from);
        if let Some(e) = e1.or(e2) {
            self.error = Some(e);
            return;
        }

        // Every check above can fail without side effects, which is why they run
        // first: a typo in the CPU list must not escalate the token or rewrite
        // the capability line. Only from here does the run actually start.
        //
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

        // Same ordering as the CLI's `apply_privileged`: raise the privileges
        // first, then let the capability probe that follows see the result.
        // Every check above can fail without side effects, so the previous run's
        // results survive a typo in any of the fields they read. Below this point
        // the run actually starts.
        self.error = None;
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
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        cfg.events = Some(sirocco_engine::config::EventSink::Bounded(ev_tx));

        let stats = Arc::new(sirocco_core::stats::Stats::new());
        let stats_for_job = stats.clone();
        // `queue_depth` is per queue and `Counters.queued` sums both, so the
        // capacity shown has to be the sum. It comes from the normalised config,
        // which means an explicit depth and the engine's floor both apply — a
        // meter that can read "16 / 2" is worse than no meter.
        let queue_cap = 2 * cfg.queue_depth.max(1);
        // Read before the config is moved into the job thread; these are the
        // numbers the meters have to divide by.
        let (final_readers, final_writers) = (cfg.readers, cfg.writers);

        std::thread::Builder::new()
            .name("sirocco-gui-job".into())
            .spawn(move || {
                let r = session::run_with(cfg, src, dst, Some(stats_for_job))
                    .map_err(|e| e.to_string());
                let _ = res_tx.send(r);
            })
            .ok();

        self.last_readers = final_readers;
        self.last_writers = final_writers;
        self.last_cap = queue_cap.max(1);
        self.running = Some(Running {
            stats,
            events: ev_rx,
            result: res_rx,
            readers: final_readers,
            writers: final_writers,
            queue_cap,
            last_bytes: 0,
            last_at: Instant::now(),
            smoothed: 0.0,
        });
        self.phase = Phase::Running;
        self.status = match self.mode {
            | _ if self.dry_run => "Previewing…".into(),
            | Mode::Move => "Moving…".into(),
            | Mode::Sync => "Mirroring…".into(),
            | _ => "Copying…".into(),
        };
    }

    /// One frame: read the engine's counters, drain the event feed, fold both
    /// into the visible state.
    // The steps are ordered and share local state (`snap`, `events`, `result`);
    // splitting them would mean passing six values through each helper.
    #[allow(clippy::too_many_lines)]
    fn poll(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        // A calibration writes real data; on a slow or full destination it can
        // block far longer than the couple of seconds it is designed for. Without
        // a deadline the button stays disabled on "Measuring…" with no way out.
        if self.calibrating
            && self
                .calib_at
                .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(120))
        {
            self.calib_rx = None;
            self.calib_at = None;
            self.calibrating = false;
            self.calibration = "calibration timed out".to_string();
        }

        if let Some(rx) = &self.calib_rx {
            match rx.try_recv() {
                | Ok(p) => {
                    self.calib_rx = None;
                    self.calib_at = None;
                    self.calibrating = false;
                    // A failed measurement keeps the previous profile: throwing
                    // away a good one because a retry failed is not an improvement.
                    // A result for a destination the user has since changed is a
                    // measurement of some other volume; showing it would be a lie
                    // that looks exactly like a valid one.
                    if self.calib_for.as_deref() != Some(std::path::Path::new(self.dst.trim())) {
                        self.calibration = "destination changed; measure again".to_string();
                    } else if let Some(p) = p {
                        self.io_profile = Some(p);
                        self.calibration = format!(
                            "read {} · write {} · dest read {}",
                            format_rate(p.read_bps),
                            format_rate(p.write_bps),
                            format_rate(p.dest_read_bps)
                        );
                    } else {
                        self.calibration = "calibration failed".to_string();
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

        // Read everything out of the job handle first, then mutate `self`: the
        // handle borrows `self.running` and the row lists borrow `self`.
        if self.stop_requested {
            // The engine resets the flag once when the job starts; keep asserting
            // it until the job actually stops.
            sirocco_sys::interrupt::flag().store(true, Ordering::Relaxed);
        }

        let Some(run) = self.running.as_ref() else {
            return;
        };
        let snap = run.stats.snapshot();
        let now = Instant::now();
        // Same accounting as the progress bar: bytes that never needed writing
        // (delta hits, punched holes, hard links) count as progress, or the chart
        // would show a slow run while the bar races to the end.
        let done_bytes = snap.bytes_accounted();
        let dt = (now - run.last_at).as_secs_f64().max(1e-6);
        let instant = done_bytes.saturating_sub(run.last_bytes) as f64 / dt;
        // Light smoothing: per-tick deltas are noisy at 90 ms, but a cumulative
        // average hides a stall completely.
        let rate = instant.mul_add(0.45, run.smoothed * 0.55);
        // Bounded per frame: an unbounded drain makes one frame do work
        // proportional to a whole directory, which is exactly when the window
        // should stay responsive. The rest is read on the next tick.
        let mut events = Vec::new();
        let mut drained = false;
        while events.len() < EVENT_BUDGET {
            match run.events.try_recv() {
                | Ok(ev) => events.push(ev),
                | Err(_) => {
                    drained = true;
                    break;
                },
            }
        }
        // The result is only taken once the queue is empty. Taking it while events
        // were still buffered dropped the receiver (and those events) a line later,
        // so the FILES feed ended short of the count the card reported.
        let result = if drained {
            run.result.try_recv().ok()
        } else {
            None
        };
        let readers = run.readers;
        let writers = run.writers;

        if let Some(run) = self.running.as_mut() {
            run.last_bytes = done_bytes;
            run.last_at = now;
            run.smoothed = rate;
        }
        if self.history.len() >= SAMPLES {
            self.history.remove(0);
        }
        self.history.push(rate as f32);

        for ev in events {
            match ev {
                | Event::Started { rel, size } => self.mark_active(&rel, size),
                | Event::Done { rel, size, .. } => self.mark_done(&rel, size, RowState::Done),
                | Event::Skipped { rel } => self.mark_done(&rel, 0, RowState::Skipped),
                | Event::Failed { rel, error } => {
                    self.mark_done(&rel, 0, RowState::Failed(error));
                },
                | Event::Finished { .. } => {},
            }
        }

        match result {
            | Some(Ok(sum)) => {
                self.phase = if sum.interrupted {
                    Phase::Cancelled
                } else if sum.stats.files_failed > 0 || !sum.errors.is_empty() {
                    Phase::Failed
                } else {
                    Phase::Done
                };
                self.status = match self.mode {
                    // A stopped run must not be described in the completed voice:
                    // the pill already says STOPPED, and for a Move the difference
                    // is whether the tree is half-moved.
                    | _ if sum.interrupted => {
                        format!(
                            "Stopped after {} files, {} copied, in {}",
                            sum.stats.files_done + sum.stats.files_skipped,
                            sum.stats.files_done,
                            format_duration(sum.stats.elapsed_secs)
                        )
                    },
                    // A preview reports a plan, not counters: "Copied 0 files" would
                    // be technically true and completely useless.
                    | _ if sum.backend == "dry-run" => {
                        format!(
                            "Preview: {} files, {} to copy",
                            sum.files_planned,
                            format_bytes(sum.bytes_planned)
                        )
                    },
                    | Mode::Move if sum.stats.files_failed > 0 => {
                        format!(
                            "Moved {} files, {} failed, in {}",
                            sum.stats.files_done,
                            sum.stats.files_failed,
                            format_duration(sum.stats.elapsed_secs)
                        )
                    },
                    | Mode::Move => {
                        format!(
                            "Moved {} files in {}",
                            sum.stats.files_done,
                            format_duration(sum.stats.elapsed_secs)
                        )
                    },
                    | Mode::Sync => {
                        format!(
                            "Mirrored in {} — {} copied, {} already current",
                            format_duration(sum.stats.elapsed_secs),
                            sum.stats.files_done,
                            sum.stats.files_skipped
                        )
                    },
                    | _ => {
                        // "Copied 0 files" over a tree where everything was already
                        // current reads as a broken tool, not as success; and a run
                        // with failures must not be described in the success voice.
                        if sum.stats.files_failed > 0 {
                            format!(
                                "Copied {} files, {} failed, in {}",
                                sum.stats.files_done,
                                sum.stats.files_failed,
                                format_duration(sum.stats.elapsed_secs)
                            )
                        } else if sum.stats.files_skipped > 0 {
                            format!(
                                "Copied {} files in {} — {} already current",
                                sum.stats.files_done,
                                format_duration(sum.stats.elapsed_secs),
                                sum.stats.files_skipped
                            )
                        } else {
                            format!(
                                "Copied {} files in {}",
                                sum.stats.files_done,
                                format_duration(sum.stats.elapsed_secs)
                            )
                        }
                    },
                };
                let cancelled = sum.interrupted;
                self.summary = Some(sum);
                self.flush_active(cancelled.then_some("cancelled"));
                self.running = None;
                self.stop_requested = false;
            },
            | Some(Err(e)) => {
                self.phase = Phase::Failed;
                self.status = "Stopped with an error".into();
                self.error = Some(e);
                self.flush_active(Some("run ended"));
                self.running = None;
                self.stop_requested = false;
            },
            | None => {
                let _ = (readers, writers);
            },
        }
    }

    /// A file started. Updated in place if it is already listed, because a
    /// skipped-then-retried file must not produce two rows.
    fn mark_active(
        &mut self,
        rel: &std::path::Path,
        size: u64,
    ) {
        let rel = rel.to_string_lossy().replace('\\', "/");
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
        rel: &std::path::Path,
        size: u64,
        state: RowState,
    ) {
        let rel = rel.to_string_lossy().replace('\\', "/");
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

    /// Anything still in flight when the job ends. A clean run finishes only
    /// after every chunk has landed, so those are done; an interrupted one may
    /// have left them half-written, which must not be reported as success.
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

/// Split a comma/space separated pattern list.
/// Inline globs plus the lines of a pattern file.
///
/// Same shape as `--exclude` / `--exclude-from`: blank lines and `#` comments are
/// skipped, everything else is a pattern. The error is returned rather than
/// swallowed: a pattern file that cannot be read must not silently become "no
/// patterns", which would copy exactly what the user excluded.
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
    pub fn snap(&self) -> Option<sirocco_core::stats::Snapshot> {
        self.running.as_ref().map(|r| r.stats.snapshot())
    }

    /// (readers, writers, readers busy, writers busy, queued, queue capacity)
    #[must_use]
    pub fn gauges(&self) -> (usize, usize, usize, usize, usize, usize) {
        self.running.as_ref().map_or(
            // Idle: keep the last shape, all meters empty.
            (self.last_readers, self.last_writers, 0, 0, 0, self.last_cap),
            |run| {
                let s = run.stats.snapshot();
                (
                    run.readers,
                    run.writers,
                    (s.readers_active as usize).min(run.readers),
                    (s.writers_active as usize).min(run.writers),
                    s.queued as usize,
                    run.queue_cap.max(1),
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
}
