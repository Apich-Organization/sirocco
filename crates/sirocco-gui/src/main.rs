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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use std::time::Instant;

use iced::Element;
use iced::Subscription;
use iced::Task;
use iced::Theme;

use sirocco_core::human::format_bytes;
use sirocco_core::human::format_duration;
use sirocco_core::human::format_rate;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::VerifyMode;
use sirocco_engine::EngineConfig;
use sirocco_engine::Event;
use sirocco_engine::session;

use theme as th;

/// Samples kept for the throughput chart: 660 × 90 ms ≈ 60 s.
pub const SAMPLES: usize = 660;
/// Rows in the finished-file feed.
const FEED_CAP: usize = 300;
/// Rows shown as "in flight". Well above the real concurrency, but bounded so a
/// pathological job cannot grow the heap.
const ACTIVE_CAP: usize = 64;

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
    /// Remember block hashes so a re-sync can skip reading.
    pub index: bool,
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
    pub caps: String,
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
            index: false,
            journal: false,
            sync: false,
            dry_run: false,
            threads: 0,
            exclude: String::new(),
            include: String::new(),
            order: Order::Walk,
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
    Index(bool),
    Journal(bool),
    Sync(bool),
    DryRun(bool),
    Threads(f32),
    Exclude(String),
    Include(String),
    OrderPicked(Order),
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

    fn update(
        &mut self,
        msg: Message,
    ) -> Task<Message> {
        match msg {
            | Message::ModePicked(m) => self.mode = m,
            | Message::Src(s) => self.src = s,
            | Message::Dst(s) => self.dst = s,
            | Message::BrowseSrc => return Task::perform(pick_folder(), Message::PickedSrc),
            | Message::BrowseDst => return Task::perform(pick_folder(), Message::PickedDst),
            | Message::PickedSrc(Some(p)) => self.src = p.to_string_lossy().to_string(),
            | Message::PickedDst(Some(p)) => self.dst = p.to_string_lossy().to_string(),
            | Message::PickedSrc(None) | Message::PickedDst(None) => {},
            | Message::Into(v) => self.into = v,
            | Message::VerifyPicked(v) => self.verify = v,
            | Message::OverwritePicked(o) => self.overwrite = o,
            | Message::Sparse(v) => self.sparse = v,
            | Message::Links(v) => self.links = v,
            | Message::Acl(v) => self.acl = v,
            | Message::Index(v) => self.index = v,
            | Message::Journal(v) => self.journal = v,
            | Message::Sync(v) => self.sync = v,
            | Message::DryRun(v) => self.dry_run = v,
            | Message::Threads(v) => self.threads = v as u32,
            | Message::Exclude(s) => self.exclude = s,
            | Message::Include(s) => self.include = s,
            | Message::OrderPicked(o) => self.order = o,
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
                        self.src = s;
                    } else {
                        self.dst = s;
                    }
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

    fn start(&mut self) {
        if self.is_running() {
            return;
        }
        self.error = None;
        self.summary = None;
        self.feed.clear();
        self.active.clear();
        self.history.clear();

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

        // Clear the process-wide flag here, on the UI thread, so a Stop pressed
        // before the job thread reaches its own reset is not discarded.
        sirocco_sys::interrupt::reset();
        self.stop_requested = false;

        let (readers, writers) = self.resolved_threads();
        let scan = sirocco_core::plan::ScanCfg {
            exclude: split_patterns(&self.exclude),
            include: split_patterns(&self.include),
            order: self.order,
            ..sirocco_core::plan::ScanCfg::default()
        };

        let mut cfg = EngineConfig {
            mode: self.mode,
            quiet: true,
            verbose: 0,
            overwrite: self.overwrite,
            verify: self.verify,
            sparse: self.sparse,
            preserve_links: self.links,
            preserve_acl: self.acl,
            journal: self.journal,
            sync_at_end: self.sync,
            dry_run: self.dry_run,
            into: self.into,
            index: if self.index {
                sirocco_engine::config::IndexMode::Trust
            } else {
                sirocco_engine::config::IndexMode::Off
            },
            // Mirroring is the *point* of Sync; without this it silently leaves
            // stale files behind, which is worse than not offering the mode.
            delete_extra: self.mode == Mode::Sync,
            // 0 means "let the engine decide", which is what `resolved_threads`
            // already mirrors for the labels.
            readers: if self.threads > 0 {
                readers
            } else {
                0
            },
            writers: if self.threads > 0 {
                writers
            } else {
                0
            },
            scan,
            ..EngineConfig::default()
        };

        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        cfg.events = Some(ev_tx);

        let stats = Arc::new(sirocco_core::stats::Stats::new());
        let stats_for_job = stats.clone();
        // `queue_depth` is per queue and `Counters.queued` sums both of them, so
        // the capacity shown next to it has to be the sum too — otherwise the
        // meter reads "128 / 64" on a perfectly healthy run.
        let queue_cap = 2 * (readers + writers) * cfg.overlap;

        std::thread::Builder::new()
            .name("sirocco-gui-job".into())
            .spawn(move || {
                let r = session::run_with(cfg, src, dst, Some(stats_for_job))
                    .map_err(|e| e.to_string());
                let _ = res_tx.send(r);
            })
            .ok();

        self.last_readers = readers;
        self.last_writers = writers;
        self.last_cap = queue_cap.max(1);
        self.running = Some(Running {
            stats,
            events: ev_rx,
            result: res_rx,
            readers,
            writers,
            queue_cap,
            last_bytes: 0,
            last_at: Instant::now(),
            smoothed: 0.0,
        });
        self.phase = Phase::Running;
        self.status = match self.mode {
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
        let mut events = Vec::new();
        while let Ok(ev) = run.events.try_recv() {
            events.push(ev);
        }
        let result = run.result.try_recv();
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
            | Ok(Ok(sum)) => {
                self.phase = if sum.interrupted {
                    Phase::Cancelled
                } else if sum.stats.files_failed > 0 || !sum.errors.is_empty() {
                    Phase::Failed
                } else {
                    Phase::Done
                };
                self.status = match self.mode {
                    // A preview reports a plan, not counters: "Copied 0 files" would
                    // be technically true and completely useless.
                    | _ if sum.backend == "dry-run" => {
                        format!(
                            "Preview: {} files, {} to copy",
                            sum.files_planned,
                            format_bytes(sum.bytes_planned)
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
                        format!(
                            "Copied {} files in {}",
                            sum.stats.files_done,
                            format_duration(sum.stats.elapsed_secs)
                        )
                    },
                };
                let cancelled = sum.interrupted;
                self.summary = Some(sum);
                self.flush_active(cancelled);
                self.running = None;
                self.stop_requested = false;
            },
            | Ok(Err(e)) => {
                self.phase = Phase::Failed;
                self.status = "Stopped with an error".into();
                self.error = Some(e);
                self.flush_active(false);
                self.running = None;
                self.stop_requested = false;
            },
            | Err(TryRecvError::Empty) => {
                let _ = (readers, writers);
            },
            | Err(TryRecvError::Disconnected) => {
                self.phase = Phase::Failed;
                self.status = "Stopped with an error".into();
                self.error = Some("the worker stopped unexpectedly".into());
                self.flush_active(false);
                self.running = None;
                self.stop_requested = false;
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
    fn flush_active(
        &mut self,
        cancelled: bool,
    ) {
        for row in self.active.drain(..) {
            self.feed.push_front(FileRow {
                rel: row.rel,
                size: row.size,
                state: if cancelled {
                    RowState::Failed("cancelled".into())
                } else {
                    RowState::Done
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
