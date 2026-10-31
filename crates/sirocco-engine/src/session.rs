//! Session orchestration: scan, decide, schedule, run, report.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use sirocco_core::Error;
use sirocco_core::Result;
use sirocco_core::delta::IoProfile;
use sirocco_core::delta::ProbeResult;
use sirocco_core::delta::{
    self,
};
use sirocco_core::hash::xxh64;
use sirocco_core::human::format_bytes;
use sirocco_core::human::format_duration;
use sirocco_core::human::format_rate;
use sirocco_core::plan::Entry;
use sirocco_core::plan::EntryKind;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::Plan;
use sirocco_core::plan::VerifyMode;
use sirocco_core::stats::Snapshot;
use sirocco_sys::Access;
use sirocco_sys::Create;
use sirocco_sys::OpenOpts;
use sirocco_sys::RawFile;
use sirocco_sys::Topology;
use sirocco_sys::align_up;
use sirocco_sys::buf::Buf;

use crate::config::CompressMode;
use crate::config::EngineConfig;
use crate::config::IndexMode;
use crate::config::NumaMode;
use crate::config::PreallocMode;
use crate::config::SmallCopy;
use crate::index::IndexStore;
use crate::index::mtime_ns;
use crate::journal::Journal;
use crate::pipeline::Shared;
use crate::pipeline::{
    self,
};
use crate::scan;
use crate::types::Chunk;
use crate::types::FileCtx;
use crate::types::Ready;

/// Result of a run.
pub struct Summary {
    /// The mode the run used.
    pub mode: Mode,
    /// Final counters.
    pub stats: Snapshot,
    /// Every failure, with the path and the reason.
    pub errors: Vec<String>,
    /// Files the scan planned, including ones that were skipped.
    pub files_planned: u64,
    /// Bytes the scan planned.
    pub bytes_planned: u64,
    /// Which backend did the work: `iocp`, `io_uring`, `blocking` or `dry-run`.
    pub backend: String,
    /// Direct-I/O alignment that was enforced.
    pub align: u64,
    /// Reader threads the engine actually ran, after normalisation and clamping.
    pub readers: usize,
    /// Writer threads the engine actually ran.
    pub writers: usize,
    /// Source volume, as `fs (root)`.
    pub volume_src: Option<String>,
    /// Destination volume, as `fs (root)`.
    pub volume_dst: Option<String>,
    /// The run stopped because the user interrupted it.
    pub interrupted: bool,
}

impl Summary {
    #[must_use]
    /// True when nothing failed and the run was not interrupted.

    pub const fn ok(&self) -> bool {
        self.stats.files_failed == 0 && self.errors.is_empty() && !self.interrupted
    }
}

/// One file's plan.
#[derive(Debug, PartialEq)]
enum Action {
    Skip,
    Full,
    Delta {
        est: f64,
    },
    Append {
        from: u64,
    },
    /// Block delta driven by a persisted index: only the destination is read.
    IndexedDelta {
        est: f64,
    },
}

/// Run a job.
///
/// # Errors
///
/// Returns [`sirocco_core::error::Error`] when the request cannot be honoured
/// (missing source, source equal to destination, unusable destination volume).
/// Per-file failures are reported in the [`Summary`], not here.
pub fn run(
    cfg: EngineConfig,
    src: PathBuf,
    dst: PathBuf,
) -> Result<Summary> {
    run_with(cfg, src, dst, None)
}

/// Run a job, optionally reporting into a caller-owned counter block.
///
/// A GUI creates an `Arc<Stats>`, passes it here, and reads it from its own update
/// loop; the engine fills it exactly as it would its own. No callback, no channel
/// per counter, no polling thread inside the engine.
///
/// # Errors
///
/// Same as [`run`].
pub fn run_with(
    cfg: EngineConfig,
    src: PathBuf,
    dst: PathBuf,
    stats: Option<Arc<sirocco_core::stats::Stats>>,
) -> Result<Summary> {
    let mut cfg = cfg;
    let mut dst = dst;
    let topo = Arc::new(Topology::detect());
    cfg.normalize_with(topo.physical_count(), topo.logical_count());
    sirocco_sys::interrupt::reset();

    if let Some(p) = cfg.process_priority {
        let _ = sirocco_sys::set_process_priority(p);
    }

    // `cp -r` semantics: put the source directory *inside* an existing
    // destination instead of merging its contents. Done here rather than in each
    // caller so `--into` and the GUI toggle cannot drift apart.
    if cfg.into
        && src.is_dir()
        && let Some(name) = src.file_name()
    {
        // Create the destination if it does not exist yet: "copy this folder into
        // that one" means the same thing whether or not the target is there, and
        // silently degrading to a merge would be surprising.
        if !dst.is_dir() {
            std::fs::create_dir_all(&dst).map_err(|e| Error::io(&dst, "mkdir", e))?;
        }
        dst = dst.join(name);
    }

    // Copying a path onto itself would truncate the source before reading it.
    // This runs *after* the `--into` remap: with `--into`, a destination that is
    // the source's parent resolves to the source itself, and checking the
    // un-remapped path would miss it.
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(&src), std::fs::canonicalize(&dst))
        && a == b
        && cfg.mode != Mode::Delete
    {
        return Err(Error::config(format!(
            "source and destination are the same path: {}",
            a.display()
        )));
    }

    // --- capabilities ---------------------------------------------------
    //
    // Everything below branches on what the process can actually do, not on a flag
    // the user has to remember: privileged runs get instant allocation and large
    // pages, unprivileged runs get the hole-based fallbacks.
    let caps = *sirocco_sys::caps::init();
    if caps.large_pages && !cfg.no_huge_pages {
        cfg.huge_pages = true;
    }

    // --- volumes -------------------------------------------------------
    let src_vol = sirocco_sys::volume_info(&src).ok();
    let dst_vol = sirocco_sys::volume_info(&dst).ok();
    let mut align = 4096u64;
    if let Some(v) = &dst_vol {
        align = align.max(u64::from(v.physical_sector.max(v.logical_sector)));
    }
    if let Some(v) = &src_vol {
        align = align.max(u64::from(v.logical_sector));
    }
    // On Linux, ask the filesystem what O_DIRECT really needs instead of guessing.
    if let Some(a) = sirocco_sys::dio_alignment(&dst) {
        align = align.max(u64::from(a));
    }
    align = align.clamp(512, 65536);

    // --- scan ----------------------------------------------------------
    if !cfg.quiet {
        eprintln!("sirocco: scanning {} ...", src.display());
    }
    let mut scan_cfg = cfg.scan.clone();
    scan_cfg.collect_links = cfg.preserve_links;
    // `--follow-symlinks` in a destructive mode walks *through* links, so the
    // entries carry paths outside the tree and the delete would remove them.
    // Refusing the combination is the only safe answer.
    if cfg.mode == Mode::Delete {
        scan_cfg.follow_symlinks = false;
    }
    let plan = if let Some(list) = &cfg.file_list {
        // A split worker copies exactly what its manifest says.
        scan::from_list(&src, &dst, list)?
    } else {
        scan::scan(&src, &dst, &scan_cfg, topo.logical_count())?
    };
    // --- preview and delete short-circuit the pipeline ------------------
    // The preview has to be decided first: `--dry-run` on a delete used to fall
    // through to `run_delete`, which removed the tree for real.
    if cfg.dry_run {
        return Ok(run_dry(&cfg, &plan, align));
    }
    // Before the empty-plan return, so `rm emptydir` still removes the root.
    if cfg.mode == Mode::Delete {
        return Ok(run_delete(&cfg, plan, &src));
    }

    if plan.entries.is_empty() {
        // An empty source tree is not an error: create the destination so the
        // result matches what `cp -r` would leave behind.
        if src.is_dir() {
            let _ = std::fs::create_dir_all(&dst);
        }
        if !cfg.quiet {
            eprintln!("sirocco: {} contains nothing to copy", src.display());
        }
        return Ok(Summary {
            mode: cfg.mode,
            stats: sirocco_core::stats::Stats::new().snapshot(),
            errors: Vec::new(),
            files_planned: 0,
            bytes_planned: 0,
            backend: native_backend_name(),
            align,
            readers: 0,
            writers: 0,
            volume_src: src_vol.map(|v| format!("{} ({})", v.fs, v.root.display())),
            volume_dst: dst_vol.map(|v| format!("{} ({})", v.fs, v.root.display())),
            interrupted: false,
        });
    }

    // `--xattrs` on a platform without them is a request that cannot be met.
    // Saying so once beats 200 000 identical errors, and beats ignoring the flag.
    let mut pre_errors: Vec<String> = Vec::new();
    if cfg.preserve_xattrs && !sirocco_sys::xattrs_supported() {
        cfg.preserve_xattrs = false;
        pre_errors.push(
            "extended attributes are not supported on this platform; --xattrs ignored".into(),
        );
    }

    // --- engine --------------------------------------------------------
    sirocco_sys::interrupt::install().ok();

    // --- resume journal ------------------------------------------------
    let mut already_done: std::collections::HashMap<String, crate::journal::Entry> =
        std::collections::HashMap::new();
    let mut journal: Option<Arc<Journal>> = None;
    if cfg.journal {
        // One rule, independent of whether the destination exists yet: the first
        // run creates it, so a path that depended on existence would move between
        // runs and the resume would never find the journal it wrote.
        let jp = if dst.is_dir() || !dst.exists() {
            dst.join(crate::journal::JOURNAL_NAME)
        } else {
            dst.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(crate::journal::JOURNAL_NAME)
        };
        // The journal lives inside the destination, which may not exist on a
        // first run; create it now rather than failing to open the journal.
        if let Some(parent) = jp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Journal::open(jp, &src) {
            | Ok((j, done)) => {
                already_done = done;
                if !already_done.is_empty() && !cfg.quiet {
                    eprintln!(
                        "sirocco: resuming, {} files already complete",
                        already_done.len()
                    );
                }
                journal = Some(Arc::new(j));
            },
            | Err(e) => eprintln!("sirocco: cannot open the journal: {e}"),
        }
    }

    // --- NUMA ------------------------------------------------------------
    let device_node = match cfg.numa {
        | NumaMode::Off => None,
        | NumaMode::Node(n) => Some(n),
        | NumaMode::Auto => sirocco_sys::numa_node_of(&dst),
    };
    let target_node = device_node.or_else(|| {
        topo.nodes
            .iter()
            .position(|n| !n.is_empty())
            .map(|n| n as u16)
    });

    let index_store = Arc::new(IndexStore::new(
        cfg.index_dir
            .clone()
            .unwrap_or_else(IndexStore::default_root),
        cfg.index.enabled(),
    ));

    // Progress counters can live in a shared mapping, so a parent process sees
    // them without a pipe, a socket or a publisher thread. `shm_guard` keeps the
    // mapping alive for as long as the counters are in use.
    // A mapping shorter than the counter block would make every `fetch_add`
    // write past the end of the segment, so the size is checked before the
    // pointer is adopted rather than assumed.
    let shm_guard = cfg.shm.as_ref().and_then(|p| {
        sirocco_sys::shm::Shm::open(p)
            .ok()
            .filter(|m| m.len() >= std::mem::size_of::<sirocco_core::stats::Counters>())
    });
    let stats = stats.unwrap_or_else(|| {
        Arc::new(shm_guard.as_ref().map_or_else(
            sirocco_core::stats::Stats::new,
            // SAFETY: the mapping is at least `Counters::SIZE` and outlives `run`.
            |m| unsafe {
                sirocco_core::stats::Stats::from_shared(m.as_ptr::<sirocco_core::stats::Counters>())
            },
        ))
    });

    let shared = Shared::new(
        Arc::new(cfg.clone()),
        align,
        topo.clone(),
        dst.clone(),
        journal.clone(),
        index_store.clone(),
        target_node,
        stats,
    )
    .map_err(|e| Error::other(format!("cannot allocate buffers: {e}")))?;
    shared
        .stats
        .files_total
        .store(plan.total_files, Ordering::Relaxed);
    shared
        .stats
        .bytes_total
        .store(plan.total_bytes, Ordering::Relaxed);
    for e in pre_errors {
        shared.error(e);
    }

    // --- placement -------------------------------------------------------
    let placement = sirocco_sys::affinity::plan(
        &topo,
        cfg.readers,
        cfg.writers,
        target_node,
        cfg.cpu_list.as_deref(),
    );
    if !cfg.quiet && cfg.verbose >= 1 {
        eprintln!(
            "sirocco: {} files, {} dirs, {} to copy; buffers {} × {} = {}",
            plan.total_files,
            plan.total_dirs,
            format_bytes(plan.total_bytes),
            cfg.buffer_count(),
            format_bytes(cfg.buffer_size as u64),
            format_bytes(shared.cfg.pool_bytes() as u64),
        );
        if placement.is_empty() {
            eprintln!(
                "sirocco: {} readers + {} writers (unpinned)",
                cfg.readers, cfg.writers
            );
        } else {
            let r: Vec<usize> = placement.readers.iter().map(|c| c.cpu).collect();
            let w: Vec<usize> = placement.writers.iter().map(|c| c.cpu).collect();
            eprintln!(
                "sirocco: {} readers on {:?}, {} writers on {:?}{}",
                r.len(),
                r,
                w.len(),
                w,
                if placement.oversubscribed {
                    " (oversubscribed)"
                } else {
                    ""
                }
            );
        }
        if let Some(n) = target_node {
            eprintln!("sirocco: numa node {n} for buffers and threads");
        }
    }

    let handles = pipeline::spawn(shared.clone(), placement);

    let stop = Arc::new(AtomicBool::new(false));
    let progress =
        if !cfg.quiet && cfg.verbose >= 1 && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            Some(spawn_progress(shared.clone(), stop.clone()))
        } else {
            None
        };

    // --- feed ----------------------------------------------------------
    let t0 = Instant::now();
    let ids = AtomicU64::new(1);
    let instant_alloc = cfg.prealloc == PreallocMode::Valid
        || cfg.prealloc == PreallocMode::Auto && caps.instant_alloc && probe_instant_alloc(&dst);
    if cfg.verbose >= 1 && !cfg.quiet {
        eprintln!("sirocco: capabilities: {caps}");
        eprintln!(
            "sirocco: allocation strategy: {}",
            if instant_alloc {
                "instant (SetFileValidData / fallocate)"
            } else if cfg.prealloc.reserves() {
                "hole-based extension"
            } else {
                "none"
            }
        );
    }

    // Directories first so files always have a parent.
    for e in &plan.entries {
        if e.kind == EntryKind::Dir {
            if let Err(err) = std::fs::create_dir_all(&e.dst) {
                shared.error(format!("mkdir {}: {err}", e.dst.display()));
            } else {
                shared.stats.dirs_created.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Same-volume moves are renames: no data movement at all.
    let same_volume = match (&src_vol, &dst_vol) {
        | (Some(a), Some(b)) => a.serial == b.serial && a.fs == b.fs,
        | _ => false,
    };

    // --- parallel feeders ------------------------------------------------
    //
    // One scheduler thread is a real bottleneck on trees with a million small
    // files: per file it does a stat, a decision, a create, a preallocation and a
    // few queue pushes, all serialised. The I/O itself is already parallel, but the
    // pipeline starves between files. Several feeders pulling from a shared cursor
    // keep the queues full; the cursor also bounds how far ahead they can run, so
    // `--order=physical` still reads roughly in layout order.
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let link_map = std::sync::Mutex::new(std::collections::HashMap::new());
    // With `--feeders=0` (the default) the count is a starting guess that a
    // controller adjusts; an explicit count is honoured as-is.
    let n_feeders = cfg.feeders;
    let gate = FeedGate::new(n_feeders);
    let auto_feeders = cfg.feeders_auto;
    let max_feeders = if auto_feeders {
        (topo.physical_count() * 2).clamp(2, 8)
    } else {
        n_feeders
    };
    let avg_file = plan.total_bytes / plan.total_files.max(1);
    let feed_by_bytes = avg_file >= cfg.buffer_size as u64;
    // Physical ordering only helps a rotating platter; on flash it just reorders
    // requests for nothing.
    let src_rotational = src_vol.as_ref().is_some_and(|v| v.rotational);
    let src_cluster = src_vol
        .as_ref()
        .map_or(4096, |v| u64::from(v.cluster.max(v.logical_sector)));
    let write_bps = write_throughput_estimate(&dst);

    std::thread::scope(|scope| {
        if auto_feeders {
            let s = shared.clone();
            let g = gate.clone();
            scope.spawn(move || feed_controller(s, g, max_feeders, feed_by_bytes, n_feeders));
        }
        for i in 0..max_feeders {
            let shared = shared.clone();
            let dst_root = dst.clone();
            let cfg_ref = &cfg;
            let ids = &ids;
            let done = &already_done;
            let store = &index_store;
            let plan_ref = &plan;
            let cursor = &cursor;
            let link_map = &link_map;
            let gate = gate.clone();
            scope.spawn(move || {
                let mut feeder = Feeder {
                    shared: shared.clone(),
                    cfg: cfg_ref,
                    align,
                    ids,
                    instant_alloc,
                    dst_root,
                    done,
                    index: store,
                    same_volume,
                    comp: sirocco_core::lz::Compressor::default(),
                    write_bps,
                    rotational: src_rotational,
                    cluster: src_cluster,
                    id: i,
                    last_dir: None,
                    links: link_map,
                };
                loop {
                    if shared.abort.load(Ordering::Relaxed) {
                        // Feeders park inside `acquire` waiting for a permit, and
                        // only the "no entries left" branch below used to wake
                        // them. Without this, Ctrl-C hangs the scope join.
                        gate.stop();
                        break;
                    }
                    if !gate.acquire() {
                        break;
                    }
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(e) = plan_ref.entries.get(idx) else {
                        // Nothing left: wake every parked feeder so the scope can
                        // finish instead of waiting on threads that will never get
                        // a permit.
                        gate.stop();
                        gate.release();
                        break;
                    };
                    match e.kind {
                        // Directories are created by `ensure_parent`, and anything
                        // that is not a file, directory or symlink is skipped.
                        // Nothing to do for a directory (the parent is created on
                        // demand) or for a device/socket we do not copy.
                        | EntryKind::Dir | EntryKind::Other => {},
                        | EntryKind::File => {
                            if let Err(err) = feeder.feed_file(e) {
                                let msg = err.to_string();
                                shared.error(format!("{}: {msg}", e.src.display()));
                                shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
                                // A hard-linked sibling waits on this file; without
                                // the flag it waits until the run ends.
                                shared.mark_failed(&e.dst);
                                // A listener gets the reason, not just a count.
                                if let Some(tx) = &cfg_ref.events {
                                    tx.send(crate::config::Event::Failed {
                                        rel: e.rel.clone(),
                                        error: msg,
                                    });
                                }
                            }
                        },
                        | EntryKind::Symlink => {
                            if let Err(err) = copy_symlink(e, cfg_ref) {
                                let msg = err.to_string();
                                shared.error(format!("{}: {msg}", e.src.display()));
                                shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
                                shared.mark_failed(&e.dst);
                                if let Some(tx) = &cfg_ref.events {
                                    tx.send(crate::config::Event::Failed {
                                        rel: e.rel.clone(),
                                        error: msg,
                                    });
                                }
                            }
                        },
                    }
                    gate.release();
                }
            });
        }
    });

    shared.close();
    for h in handles {
        let _ = h.join();
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = progress {
        let _ = h.join();
    }
    if !cfg.quiet {
        eprintln!();
    }

    // --- sync: remove destination entries that no longer exist ----------
    //
    // After the writers have finished, and only for paths the scan actually
    // covered: a source directory that failed to enumerate would otherwise take
    // its whole destination subtree with it.
    if cfg.mode == Mode::Sync && cfg.delete_extra {
        // Deleting "extras" is only sound if the scan saw everything. A directory
        // that could not be read would otherwise take its whole destination
        // subtree with it — and so would an interrupted walk, whose plan is
        // partial by construction.
        if sirocco_sys::interrupt::is_set() {
            if !cfg.quiet {
                eprintln!("sirocco: interrupted — skipping --delete-extra");
            }
        } else if plan.scan_errors > 0 {
            if !cfg.quiet {
                eprintln!("sirocco: scan reported errors; skipping --delete-extra");
            }
        } else if !filters_are_active(&cfg) {
            delete_extras(&dst, &plan, &shared, &cfg);
        } else if !cfg.quiet {
            // `keep` is the *filtered* plan, so anything the walk dropped looks
            // like an extra. Deleting on that basis removes files the user asked
            // to keep -- `--exclude='*.tmp'` would delete every `.tmp` already in
            // the destination. Refuse rather than guess.
            eprintln!(
                "sirocco: --delete-extra is skipped while include/exclude/depth/filesystem \
                 filters are active"
            );
        }
    }

    // --- move: drop the sources we successfully copied -------------------
    if cfg.mode == Mode::Move {
        // A size check is not enough: the destination's size is established before
        // any byte is written, so a failed copy still leaves a full-length file.
        // Only files the pipeline reported as finished are safe to delete.
        for e in &plan.entries {
            if e.kind == EntryKind::File && shared.is_complete(&e.dst) {
                let _ = std::fs::remove_file(&e.src);
            }
        }
        for e in plan.entries.iter().rev() {
            if e.kind == EntryKind::Dir {
                let _ = std::fs::remove_dir(&e.src);
            }
        }
    }

    if let Some(j) = &journal {
        j.flush();
        let clean = shared.stats.files_failed.load(Ordering::Relaxed) == 0;
        // An interrupted run usually has zero failures, so the old condition
        // deleted the journal exactly when it was needed. Keep it unless the run
        // genuinely finished.
        let finished = shared.stats.files_done.load(Ordering::Relaxed)
            + shared.stats.files_skipped.load(Ordering::Relaxed)
            >= plan.total_files;
        let interrupted = sirocco_sys::interrupt::is_set();
        if clean && finished && !interrupted {
            let p = j.path().to_path_buf();
            drop(journal.take());
            let _ = std::fs::remove_file(p);
        }
    }

    if let Some(tx) = &cfg.events {
        tx.send(crate::config::Event::Finished {
            files: shared.stats.files_done.load(Ordering::Relaxed),
            failed: shared.stats.files_failed.load(Ordering::Relaxed),
        });
    }

    let interrupted = sirocco_sys::interrupt::is_set();
    if interrupted && !cfg.quiet {
        eprintln!("sirocco: interrupted — partial results kept");
    }

    let snap = shared.stats.snapshot();
    // Nothing blocks on `pending` reaching zero, so a file whose work was
    // abandoned used to disappear from every bucket while the summary still
    // claimed success. Say so instead of leaving a hole in the arithmetic.
    let accounted = snap.files_done + snap.files_skipped + snap.files_failed;
    if accounted < plan.total_files {
        shared.error(format!(
            "{} of {} files were never accounted for (pipeline stopped early)",
            plan.total_files - accounted,
            plan.total_files
        ));
    }
    let errors = shared.errors.lock().unwrap().clone();
    let _ = t0;

    Ok(Summary {
        mode: cfg.mode,
        stats: snap,
        errors,
        files_planned: plan.total_files,
        bytes_planned: plan.total_bytes,
        backend: if cfg.reactor == sirocco_sys::ReactorKind::Native {
            native_backend_name()
        } else {
            "blocking".into()
        },
        align,
        readers: cfg.readers,
        writers: cfg.writers,
        volume_src: src_vol.map(|v| format!("{} ({})", v.fs, v.root.display())),
        volume_dst: dst_vol.map(|v| format!("{} ({})", v.fs, v.root.display())),
        interrupted,
    })
}

fn native_backend_name() -> String {
    #[cfg(target_os = "windows")]
    {
        "iocp".into()
    }
    #[cfg(target_os = "linux")]
    {
        "io_uring".into()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        "blocking".into()
    }
}

// ------------------------------------------------------------------ feeder

struct Feeder<'a> {
    shared: Arc<Shared>,
    cfg: &'a EngineConfig,
    align: u64,
    ids: &'a AtomicU64,
    /// Instant allocation is available (privilege held and the volume supports it).
    instant_alloc: bool,
    dst_root: PathBuf,
    /// Files already completed according to the resume journal.
    done: &'a std::collections::HashMap<String, crate::journal::Entry>,
    index: &'a IndexStore,
    /// Source and destination are on the same volume: a move is a rename.
    same_volume: bool,
    comp: sirocco_core::lz::Compressor,
    /// Rough write throughput of the destination, for the compression model.
    write_bps: f64,
    /// Source volume is rotational: physical ordering of chunks pays off.
    rotational: bool,
    /// Cluster size of the source volume, for the LCN mapping.
    cluster: u64,
    /// Feeder ordinal, for diagnostics.
    #[allow(dead_code)]
    id: usize,
    /// Last directory this feeder created, so a run of files in the same directory
    /// does not issue a `mkdir` each time.
    last_dir: Option<PathBuf>,
    /// First destination seen for each source inode, so extra hard links become
    /// links instead of second copies.
    links: &'a std::sync::Mutex<std::collections::HashMap<u64, PathBuf>>,
}

impl Feeder<'_> {
    fn feed_file(
        &mut self,
        e: &Entry,
    ) -> Result<()> {
        let ev = |ev: crate::config::Event| {
            if let Some(tx) = &self.cfg.events {
                tx.send(ev);
            }
        };

        if !self.done.is_empty() {
            let rel = e
                .dst
                .strip_prefix(&self.dst_root)
                .unwrap_or(&e.dst)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(&(size, mtime)) = self.done.get(&rel) {
                // Trust but verify, twice over: the source has to be the one the
                // entry was written for (a same-size edit would otherwise be
                // skipped as "already copied"), and the destination still has to
                // exist at that length.
                let same_source = size == e.size && mtime == mtime_ns(e.mtime);
                let ok = same_source && std::fs::metadata(&e.dst).is_ok_and(|m| m.len() == e.size);
                if ok {
                    self.shared
                        .stats
                        .files_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    // A resumed file is settled exactly like a skipped one: it is in
                    // the byte denominator, a hard-linked sibling may be waiting on
                    // it, and `move` still has to delete the source.
                    self.shared
                        .stats
                        .bytes_saved
                        .fetch_add(e.size, Ordering::Relaxed);
                    self.shared.mark_complete(&e.dst);
                    ev(crate::config::Event::Skipped { rel: e.rel.clone() });
                    return Ok(());
                }
            }
        }
        // Hard links: the first path with this inode is copied, the rest become
        // links to it. Detected from the scan's inode/file-id, so no extra I/O.
        if self.cfg.preserve_links && e.links > 1 && e.file_id != 0 {
            let existing = {
                let mut m = self.links.lock().unwrap();
                match m.entry(e.file_id) {
                    | std::collections::hash_map::Entry::Occupied(o) => Some(o.get().clone()),
                    | std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(e.dst.clone());
                        None
                    },
                }
            };
            if let Some(first) = existing
                && first != e.dst
            {
                let _ = self.ensure_parent(&e.dst);
                // Wait until the first copy is *finished*, not merely created: the
                // destination is preallocated at full size before a byte is
                // written, so `exists()` is true almost immediately and linking
                // then would share a half-written inode. Waiting is safe — the
                // other feeder holds its own permit and is pushing chunks, not
                // waiting on this one — and it is the only way to tell "not yet"
                // from "never".
                // An abort has to break the wait: the first copy may be one of the
                // files the interrupt stopped, and then it settles in neither set.
                while !self.shared.is_complete(&first)
                    && !self.shared.is_failed(&first)
                    && !self.shared.abort.load(Ordering::Relaxed)
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                // A failed sibling has no data to share; fall through to copying
                // this path's own bytes.
                let attempt = if self.shared.is_complete(&first) {
                    sirocco_sys::hard_link(&first, &e.dst)
                } else {
                    Err(std::io::Error::other("link target failed"))
                };
                match attempt {
                    | Ok(()) => {
                        // The link *is* the copy, so the path is complete: `move`
                        // needs to know that to remove the source.
                        self.shared.mark_complete(&e.dst);
                        self.shared.stats.files_done.fetch_add(1, Ordering::Relaxed);
                        self.shared
                            .stats
                            .bytes_saved
                            .fetch_add(e.size, Ordering::Relaxed);
                        ev(crate::config::Event::Done {
                            rel: e.rel.clone(),
                            size: e.size,
                            saved: e.size,
                        });
                        return Ok(());
                    },
                    | Err(err) => {
                        if self.cfg.verbose >= 2 {
                            eprintln!(
                                "  hard link {} failed ({err}); copying instead",
                                e.rel.display()
                            );
                        }
                    },
                }
            }
        }

        // A move within one volume needs no data movement at all.
        if self.cfg.mode == Mode::Move
            && self.same_volume
            && !e.dst.exists()
            && std::fs::rename(&e.src, &e.dst).is_ok()
        {
            self.shared.stats.files_done.fetch_add(1, Ordering::Relaxed);
            self.shared
                .stats
                .bytes_written
                .fetch_add(e.size, Ordering::Relaxed);
            ev(crate::config::Event::Done {
                rel: e.rel.clone(),
                size: e.size,
                saved: 0,
            });
            return Ok(());
        }

        let dst_md = std::fs::metadata(&e.dst).ok();
        let action = self.decide(e, dst_md.as_ref());
        if action != Action::Skip {
            ev(crate::config::Event::Started {
                rel: e.rel.clone(),
                size: e.size,
            });
        }

        if self.cfg.verbose >= 2 {
            eprintln!(
                "  {} -> {} [{}, {}]",
                e.rel.display(),
                action_name(&action),
                format_bytes(e.size),
                dst_md
                    .as_ref()
                    .map_or_else(|| "-".into(), |m| format_bytes(m.len()))
            );
        }

        match action {
            | Action::Skip => {
                self.shared
                    .stats
                    .files_skipped
                    .fetch_add(1, Ordering::Relaxed);
                // The file was in the plan, so it is in the progress denominator
                // while nothing is written for it; without this the bar sits at
                // 0 % and the ETA never terminates.
                self.shared
                    .stats
                    .bytes_saved
                    .fetch_add(e.size, Ordering::Relaxed);
                // Marked complete in every mode, not just `move`: a hard-linked
                // sibling waits for this flag before linking, and a skipped file
                // never reaches the pipeline to set it. `mark_complete` is only
                // *read* by `move`, so recording it elsewhere costs a hash insert.
                self.shared.mark_complete(&e.dst);
                ev(crate::config::Event::Skipped { rel: e.rel.clone() });
                Ok(())
            },
            | Action::Full => {
                // Small file with nothing else to do: let the filesystem copy it.
                if self.kernel_copy_allowed(e) && self.ensure_parent(&e.dst).is_ok() {
                    match sirocco_sys::copy_file_fast(&e.src, &e.dst)
                        .and_then(|()| self.finish_kernel_copy(e))
                    {
                        | Ok(()) => {
                            self.shared.mark_complete(&e.dst);
                            if let Some(j) = &self.shared.journal {
                                // Same key shape and same payload the pipeline
                                // records, or a resumed run would not recognise the
                                // file it wrote.
                                j.record(
                                    &e.dst
                                        .strip_prefix(&self.dst_root)
                                        .unwrap_or(&e.dst)
                                        .to_string_lossy()
                                        .replace('\\', "/"),
                                    e.size,
                                    mtime_ns(e.mtime),
                                );
                            }
                            let st = &self.shared.stats;
                            st.files_done.fetch_add(1, Ordering::Relaxed);
                            st.bytes_written.fetch_add(e.size, Ordering::Relaxed);
                            st.bytes_read.fetch_add(e.size, Ordering::Relaxed);
                            ev(crate::config::Event::Done {
                                rel: e.rel.clone(),
                                size: e.size,
                                saved: 0,
                            });
                            return Ok(());
                        },
                        | Err(err) => {
                            if self.cfg.verbose >= 2 {
                                eprintln!(
                                    "  kernel copy unavailable for {} ({err}); using the pipeline",
                                    e.rel.display()
                                );
                            }
                        },
                    }
                }
                self.copy_full(e)
            },
            | Action::Append { from } => self.copy_append(e, from),
            | Action::Delta { .. } => self.copy_delta(e),
            | Action::IndexedDelta { .. } => self.copy_indexed_delta(e),
        }
    }

    fn decide(
        &self,
        e: &Entry,
        dst_md: Option<&std::fs::Metadata>,
    ) -> Action {
        let Some(dm) = dst_md else {
            return Action::Full;
        };
        if !dm.is_file() {
            return Action::Full;
        }
        let dsize = dm.len();
        let dmtime = dm.modified().ok();

        let same_size = dsize == e.size;
        let same_time = match (&e.mtime, &dmtime) {
            | (Some(a), Some(b)) => times_equal(*a, *b),
            | _ => false,
        };

        let replace = match self.cfg.overwrite {
            | Overwrite::Never => false,
            | Overwrite::Always => true,
            | Overwrite::IfNewer => {
                match (&e.mtime, &dmtime) {
                    | (Some(a), Some(b)) => a > b,
                    | _ => true,
                }
            },
            | Overwrite::IfOlder => {
                match (&e.mtime, &dmtime) {
                    | (Some(a), Some(b)) => a < b,
                    | _ => true,
                }
            },
            | Overwrite::IfDifferentSize => !same_size,
            | Overwrite::IfDifferent => !(same_size && same_time),
            | Overwrite::IfContentDiffers => {
                if same_size {
                    match (content_equal(&e.src, &e.dst, self.align), same_time) {
                        | (Some(eq), _) => !eq,
                        | (None, _) => !same_time,
                    }
                } else {
                    true
                }
            },
        };

        if !replace {
            return Action::Skip;
        }

        // Incremental strategies only make sense for a destination that already
        // holds (most of) the data.
        if !self.cfg.delta_enabled || e.size < self.cfg.delta.min_size {
            return Action::Full;
        }

        // A valid index removes the destination read entirely: we hash the new
        // source block and compare it against what the destination already holds.
        if self.index.enabled() && dsize == e.size {
            let dmt = mtime_ns(dmtime);
            if let Some(idx) = self.index.load(&e.dst)
                && idx.matches(dsize, dmt, self.index_block(), self.cfg.hash)
            {
                return Action::IndexedDelta { est: 0.0 };
            }
        }

        // Growth with an intact prefix is worth probing on its own terms: it does
        // not depend on the destination being slow, it just avoids copying the
        // prefix at all. This has to come before the cost-model gate below, or
        // `--delta` would never append on same-speed media.
        if dsize < e.size {
            let probe = self.probe(e, dsize);
            if probe.probes > 0 && probe.changed == 0 && self.tail_matches(e, dsize) {
                return Action::Append { from: dsize };
            }
            return Action::Full;
        }

        // A destination longer than the source would keep its old tail: the delta
        // paths only rewrite blocks that differ and rely on a truncate that does
        // not happen when the source size is a multiple of the alignment. Copy the
        // whole file instead.
        if dsize > e.size {
            return Action::Full;
        }

        // Same size: a block delta only wins when the destination is materially
        // slower than the source. If the break-even changed fraction is at or below
        // the safety margin it can never win, so skip the probe entirely — it costs
        // two opens and up to 32 synchronous reads per candidate file.
        let prof = self.cfg.io_profile.unwrap_or_default();
        let be = delta::break_even(&prof);
        if be <= self.cfg.delta.margin {
            return Action::Full;
        }

        let probe = self.probe(e, dsize);
        let mut dcfg = self.cfg.delta;
        dcfg.inflate = (dcfg.block / Self::probe_unit(e.size)) as f64;
        match delta::decide(e.size, dsize, probe, &prof, &dcfg) {
            | delta::Decision::Full => Action::Full,
            | delta::Decision::Delta { est } => Action::Delta { est },
            | delta::Decision::Append { from } => {
                // Trust but verify: the tail of the destination must actually
                // match before we append to it.
                if self.tail_matches(e, from) {
                    Action::Append { from }
                } else {
                    Action::Full
                }
            },
        }
    }

    /// Block size used by the persistent index.
    ///
    /// Must be exactly the granularity the reader chunks at, or the hashes never
    /// line up: `normalize` pins `delta.block` to `io_unit`, and `copy_full` chunks
    /// at `io_unit.min(buffer_size)`.
    #[inline]
    fn index_block(&self) -> u64 {
        self.cfg.io_unit.min(self.cfg.buffer_size as u64).max(512)
    }

    /// Granularity of a single probe: a thirty-second of the file, clamped so a
    /// tiny file still gets a few probes and a huge one does not read megabytes
    /// just to decide.
    fn probe_unit(size: u64) -> u64 {
        (size / 32).clamp(4096, 64 << 10)
    }

    fn probe(
        &self,
        e: &Entry,
        dsize: u64,
    ) -> ProbeResult {
        let overlap = dsize.min(e.size);
        if overlap < 4096 {
            return ProbeResult::default();
        }
        let unit = Self::probe_unit(e.size).min(overlap).max(4096);
        let n = ((e.size / (4 << 20)).clamp(8, 32)) as u32;
        let seed = delta::probe_seed(e.size, 0, xxh64(e.rel.to_string_lossy().as_bytes()));
        let offsets = delta::probe_offsets(overlap, unit, n, seed);

        let Some(mut a) = self.shared.pool.take(256) else {
            return ProbeResult::default();
        };
        let Some(mut b) = self.shared.pool.take(256) else {
            return ProbeResult::default();
        };

        let src = RawFile::open(&e.src, &Self::src_opts()).ok();
        let dst = RawFile::open(&e.dst, &Self::dst_opts(false)).ok();
        let (Some(src), Some(dst)) = (src, dst) else {
            return ProbeResult {
                probes: 0,
                changed: 0,
                bytes_read: 0,
            };
        };

        let mut changed = 0u32;
        let mut probes = 0u32;
        let mut bytes = 0u64;
        for off in offsets {
            // `unit` is clamped to 64 KiB but the pool buffer is `buffer_size`,
            // which `--buffer=4K` makes smaller; slicing past it panics.
            let len = (unit.min(e.size - off) as usize).min(a.len());
            if len == 0 {
                continue;
            }
            let ra = src.pread(&mut a.as_mut_slice()[..len], off);
            let rb = dst.pread(&mut b.as_mut_slice()[..len], off);
            if let (Ok(na), Ok(nb)) = (ra, rb)
                && na == len
                && nb == len
            {
                bytes += (len * 2) as u64;
                probes += 1;
                if xxh64(a.slice(0, len)) != xxh64(b.slice(0, len)) {
                    changed += 1;
                }
            }
        }
        ProbeResult {
            probes,
            changed,
            bytes_read: bytes,
        }
    }

    /// Verify that `dst[from-4MiB..from]` equals `src[from-4MiB..from]`.
    fn tail_matches(
        &self,
        e: &Entry,
        from: u64,
    ) -> bool {
        if from == 0 {
            return true;
        }
        let window = (4 << 20).min(from);
        let start = from - window;
        let Some(mut a) = self.shared.pool.take(64) else {
            return false;
        };
        let Some(mut b) = self.shared.pool.take(64) else {
            return false;
        };
        let (Ok(src), Ok(dst)) = (
            RawFile::open(&e.src, &Self::src_opts()),
            RawFile::open(&e.dst, &Self::dst_opts(false)),
        ) else {
            return false;
        };
        let step = self.cfg.buffer_size as u64;
        let mut off = start;
        while off < from {
            let len = (step.min(from - off)) as usize;
            let ra = src.pread(&mut a.as_mut_slice()[..len], off);
            let rb = dst.pread(&mut b.as_mut_slice()[..len], off);
            match (ra, rb) {
                | (Ok(na), Ok(nb)) if na == len && nb == len => {
                    if a.slice(0, len) != b.slice(0, len) {
                        return false;
                    }
                },
                | _ => return false,
            }
            off += len as u64;
        }
        true
    }

    fn src_opts() -> OpenOpts {
        OpenOpts {
            access: Access::Read,
            create: Create::Open,
            direct: false,
            sequential: true,
            backup_semantics: true,
            ..Default::default()
        }
    }

    fn dst_opts(write: bool) -> OpenOpts {
        OpenOpts {
            access: if write {
                Access::Write
            } else {
                Access::Read
            },
            create: Create::Open,
            direct: false,
            sequential: true,
            ..Default::default()
        }
    }

    /// Create (and optionally preallocate) the destination.
    ///
    /// Returns `(clear_sparse_at_end, writer_creates_the_file)`.
    fn prepare_dst(
        &mut self,
        e: &Entry,
    ) -> Result<(bool, bool)> {
        self.ensure_parent(&e.dst)
            .map_err(|x| Error::io(&e.dst, "mkdir", x))?;

        // Small files: one chunk, written at its exact length through a buffered
        // handle. Neither a reservation nor a pre-created handle is needed — the
        // reservation alone is three metadata round trips, which would dominate the
        // cost of a 4 KiB file, and letting the writer create it saves another open.
        if e.size <= self.cfg.io_unit.min(self.cfg.buffer_size as u64)
            && self.cfg.compress == CompressMode::Off
        {
            return Ok((false, true));
        }

        let opts = OpenOpts {
            // FSCTL_SET_COMPRESSION needs both read and write access.
            access: if self.cfg.compress == CompressMode::Off {
                Access::Write
            } else {
                Access::ReadWrite
            },
            create: Create::Create,
            direct: false,
            sequential: true,
            sparse: self.cfg.sparse,
            io_priority: self.cfg.io_priority,
            ..Default::default()
        };
        let f = match RawFile::open(&e.dst, &opts) {
            | Ok(f) => f,
            | Err(first) => {
                // The same type mismatch the other way round: a directory where
                // the source has a file. Descending into it would delete the
                // children and leave the directory, so it goes whole.
                if !(may_correct_dst(self.cfg) && e.dst.is_dir()) {
                    return Err(Error::io(&e.dst, "create", first));
                }
                std::fs::remove_dir_all(&e.dst).map_err(|x| Error::io(&e.dst, "create", x))?;
                RawFile::open(&e.dst, &opts).map_err(|x| Error::io(&e.dst, "create", x))?
            },
        };
        self.maybe_compress(e, &f);
        let clear_sparse = self.reserve(&f, e.size);
        if !self.cfg.prealloc.reserves() {
            f.set_len(e.size)
                .map_err(|x| Error::io(&e.dst, "truncate", x))?;
        }
        Ok((clear_sparse, false))
    }

    /// Apply everything the pipeline would apply to a file the kernel copied.
    ///
    /// `copy_file_range` moves bytes and nothing else — no mode, no mtime, no
    /// ACL, no extended attributes. `keep_times` defaults to on and the skip
    /// decision compares mtimes, so leaving them behind made every small file
    /// look stale on the next run: the incremental sync never converged.
    fn finish_kernel_copy(
        &self,
        e: &Entry,
    ) -> std::io::Result<()> {
        // `Attributes`, not `Write`: a read-only source produces a read-only
        // destination, and `GENERIC_WRITE` / `O_WRONLY` on it is refused. The
        // attribute handle is enough for everything below.
        let f = RawFile::open(
            &e.dst,
            &OpenOpts {
                access: Access::Attributes,
                create: Create::Open,
                ..OpenOpts::default()
            },
        )?;
        if self.cfg.preserve_permissions && e.mode != 0 {
            sirocco_sys::set_mode(&f, e.mode)?;
        }
        if self.cfg.keep_times {
            if let Some(t) = e.mtime {
                f.set_modified(t)?;
            }
        } else {
            // `--no-preserve-times`: `CopyFileExW` carries the source's timestamp
            // across, so it has to be replaced explicitly rather than ignored.
            f.set_modified(std::time::SystemTime::now())?;
        }
        // `--verify=size` is allowed on this path, so it has to be checked here:
        // `finalize` never runs for a kernel copy.
        if self.cfg.verify == VerifyMode::Size {
            let got = std::fs::metadata(&e.dst)?.len();
            if got != e.size {
                return Err(std::io::Error::other(format!(
                    "verify: destination is {got} bytes, source is {}",
                    e.size
                )));
            }
        }
        Ok(())
    }

    /// Is a kernel-side copy acceptable for this file?
    ///
    /// Only when nothing downstream needs the bytes: verification by hash, the
    /// persistent index, hole punching and compression all require us to see the
    /// data, and a kernel copy would bypass every one of them.
    fn kernel_copy_allowed(
        &self,
        e: &Entry,
    ) -> bool {
        if self.cfg.small_copy == SmallCopy::Io {
            return false;
        }
        // The kernel path never sees the bytes, so anything that needs to see
        // them rules it out -- for `kernel` as much as for `auto`. Forcing the
        // mode used to skip these checks and silently drop verification.
        let plain_verify = matches!(
            self.cfg.verify,
            sirocco_core::plan::VerifyMode::None | sirocco_core::plan::VerifyMode::Size
        );
        // `verify=size` stays allowed: `finish_kernel_copy` checks it there.
        if !plain_verify
            || self.cfg.compress != CompressMode::Off
            || self.cfg.sparse
            || self.index.enabled()
            || self.cfg.preserve_acl
            || self.cfg.preserve_xattrs
            // `--sync` needs a writable handle to flush through, and the kernel
            // copy closes its own before we get here.
            || self.cfg.sync_at_end
        {
            return false;
        }
        e.size <= self.cfg.small_threshold
    }

    /// Create the destination's parent directory, remembering the last one so a
    /// run of files in the same directory does not issue a `mkdir` each time.
    fn ensure_parent(
        &mut self,
        dst: &Path,
    ) -> std::io::Result<()> {
        let Some(parent) = dst.parent() else {
            return Ok(());
        };
        if parent.as_os_str().is_empty() || self.last_dir.as_deref() == Some(parent) {
            return Ok(());
        }
        if let Err(e) = std::fs::create_dir_all(parent) {
            // A file sitting where the destination needs a directory blocks the
            // copy and would block it again on every mirror run. `delete_extra`
            // is what says the destination is ours to correct.
            if !(may_correct_dst(self.cfg) && parent.is_file()) {
                return Err(e);
            }
            std::fs::remove_file(parent)?;
            std::fs::create_dir_all(parent)?;
        }
        self.last_dir = Some(parent.to_path_buf());
        Ok(())
    }

    /// Decide whether transparent compression pays for this file and, if so, turn
    /// it on for the destination.
    ///
    /// The measurement is real: we run the LZ4 compressor over a sample and use the
    /// resulting ratio plus the measured compression throughput in the same cost
    /// model the delta planner uses. On a 3 GB/s `NVMe` nothing wins; on a 150 MB/s
    /// HDD anything below ~0.7 does.
    fn maybe_compress(
        &mut self,
        e: &Entry,
        dst: &RawFile,
    ) {
        if self.cfg.compress == CompressMode::Off || e.size < (64 << 10) {
            return;
        }
        let want = (256 << 10) as usize;
        let mut sample = vec![0u8; want * 3];
        let opts = OpenOpts {
            access: Access::Read,
            create: Create::Open,
            sequential: true,
            ..Default::default()
        };
        let Ok(src) = RawFile::open(&e.src, &opts) else {
            return;
        };
        let mut got = 0usize;
        for off in [0, e.size / 2, e.size.saturating_sub(want as u64)] {
            if got >= sample.len() {
                break;
            }
            let room = (sample.len() - got).min(want);
            match src.pread(&mut sample[got..got + room], off) {
                | Ok(n) if n > 0 => got += n,
                | _ => {},
            }
        }
        if got == 0 {
            return;
        }
        let mut scratch = vec![0u8; sirocco_core::lz::max_compressed_size(got)];
        let prof = sirocco_core::entropy::assess(&sample[..got], &mut self.comp, &mut scratch);
        let pays = self.cfg.compress == CompressMode::Force
            || sirocco_core::entropy::compress_pays(
                prof.lz4_ratio,
                self.write_bps,
                prof.compress_bps(),
                0.15,
            );
        if self.cfg.verbose >= 2 {
            eprintln!(
                "  compress {}: ratio {:.2}, entropy {:.2} bits/byte -> {}",
                e.rel.display(),
                prof.lz4_ratio,
                prof.entropy_bits,
                if pays { "on" } else { "off" }
            );
        }
        if pays {
            let e_rel = e.rel.display();
            if let Err(e) = dst.set_compressed()
                && self.cfg.verbose >= 2
            {
                eprintln!("  compress {e_rel}: not available ({e})");
            }
        }
    }

    /// Reserve space for `size` bytes on an already-created destination.
    ///
    /// Returns whether the file was extended through a hole, in which case the
    /// sparse flag must be cleared once the data is in place.
    ///
    /// `SetFileValidData` is the only zero-cost way to extend a file on NTFS; it
    /// needs `SeManageVolumePrivilege`. Without it, `SetEndOfFile` makes the
    /// filesystem zero-fill the new range — a full extra pass over the data, which
    /// halves throughput on ordinary hardware. Extending through a hole avoids the
    /// fill entirely, so we do that and clean up afterwards.
    ///
    /// Never truncates: the append and delta paths call this on a destination that
    /// already holds data.
    fn reserve(
        &self,
        f: &RawFile,
        size: u64,
    ) -> bool {
        if size == 0 {
            return false;
        }
        let want = align_up(size, self.align);
        match self.cfg.prealloc {
            | PreallocMode::Off => false,
            | PreallocMode::Valid => {
                if self.instant_alloc {
                    let _ = f.prealloc_valid(want);
                }
                false
            },
            | PreallocMode::Sparse => self.extend_through_hole(f, want),
            | PreallocMode::Auto => {
                if self.instant_alloc && f.prealloc_valid(want).unwrap_or(false) {
                    return false;
                }
                self.extend_through_hole(f, want)
            },
        }
    }

    fn extend_through_hole(
        &self,
        f: &RawFile,
        want: u64,
    ) -> bool {
        if f.set_sparse_flag(true).is_ok() && f.set_len(want).is_ok() {
            // With --sparse the holes are wanted; otherwise undo the flag later.
            !self.cfg.sparse
        } else {
            let _ = f.set_len(want);
            false
        }
    }

    fn copy_full(
        &mut self,
        e: &Entry,
    ) -> Result<()> {
        let (clear_sparse, writer_creates) = self.prepare_dst(e)?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let unit = self.cfg.io_unit.min(self.cfg.buffer_size as u64);
        // "Small" means exactly one chunk: `Chunk::exact` sets `create = true`, so
        // two chunks of the same file would each truncate what the other wrote.
        let small = e.size <= unit;
        let n = e.size.div_ceil(unit);

        // A source region that is not allocated needs no read at all: punching a
        // hole in the destination is one metadata call instead of a read plus a
        // write of the whole chunk. Only possible when the destination is allowed
        // to be sparse in the first place.
        // `clear_sparse` is only ever set when the destination was made sparse
        // for us, so it cannot add a case here; the test is just "sparse is on".
        let can_punch = !small && self.cfg.sparse;
        let holes = if can_punch {
            Self::plan_holes(e, unit, n)
        } else {
            None
        };
        let punched = holes.as_ref().is_some_and(|h| h.iter().any(|&x| x));

        let mut ctx = FileCtx::new(
            id,
            e.src.clone(),
            e.dst.clone(),
            e.rel.clone(),
            e.size,
            e.mtime,
        );
        ctx.sparse = self.cfg.sparse;
        // If we punched holes they are the point of the exercise; clearing the
        // sparse flag afterwards would materialise them into zero clusters.
        ctx.clear_sparse_at_end = clear_sparse && !punched;
        ctx.mtime_ns = mtime_ns(e.mtime);
        ctx.mode = e.mode;
        // `FileCtx::small` means "no reservation was made, so there is no padded
        // tail to cut". Compression takes the long path even for a small file, so
        // the destination was reserved to an alignment boundary and the tail is
        // real; forwarding `small` there left it in place and the destination came
        // out longer than its source.
        ctx.small = small && self.cfg.compress == CompressMode::Off;
        // An index file per tiny file would cost more inodes than it saves in
        // I/O, and below the delta threshold the decision is always a full copy.
        if self.index.enabled() && e.size >= self.cfg.delta.min_size {
            ctx.hashes = Some(std::sync::Mutex::new(vec![0u64; n as usize]));
        }
        let ctx = Arc::new(ctx);

        if e.size == 0 {
            // `prepare_dst` skips small files entirely, so an empty file has to be
            // created here.
            self.ensure_parent(&e.dst)
                .map_err(|x| Error::io(&e.dst, "mkdir", x))?;
            let opts = OpenOpts {
                access: Access::Write,
                create: Create::Create,
                ..Default::default()
            };
            // An empty file still has to exist. Failing here is a failure: the
            // old code marked the path complete regardless, and `move` then
            // deleted the only copy.
            let f = RawFile::open(&e.dst, &opts).map_err(|x| Error::io(&e.dst, "create", x))?;
            f.set_len(0).map_err(|x| Error::io(&e.dst, "truncate", x))?;
            if self.cfg.keep_times
                && let Some(t) = e.mtime
            {
                f.set_modified(t)
                    .map_err(|x| Error::io(&e.dst, "mtime", x))?;
            }
            self.shared.mark_complete(&e.dst);
            self.shared.stats.files_done.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        ctx.pending.store(n as u32, Ordering::Release);

        // --- physical ordering: on a rotating disk, walk the chunks in the order
        // they sit on the platter instead of the order they sit in the file.
        let order = if !small && self.cfg.reorder_lcn && self.rotational {
            self.lcn_order(e, unit, n)
        } else {
            None
        };

        let seq: Vec<u32> = order.unwrap_or_else(|| (0..n as u32).collect());
        for i in seq {
            let off = u64::from(i) * unit;
            let len = unit.min(e.size - off);
            let mut c = if small {
                Chunk::exact(ctx.clone(), off, len)
            } else {
                Chunk::new(ctx.clone(), off, off, len, self.align)
            };
            c.create = writer_creates;
            c.block = i;
            if holes.as_ref().is_some_and(|h| h[i as usize]) {
                self.shared.push_hole(c);
            } else {
                self.shared.push_chunk(c);
            }
        }
        Ok(())
    }

    /// Which chunks of `e` lie entirely in unallocated space.
    ///
    /// Reading them would return zeros and writing them would allocate clusters
    /// for nothing; punching a hole does neither.
    fn plan_holes(
        e: &Entry,
        unit: u64,
        n: u64,
    ) -> Option<Vec<bool>> {
        let f = RawFile::open(&e.src, &Self::src_opts()).ok()?;
        let ranges = f.allocated_ranges(e.size).ok()?;
        if ranges.is_empty() {
            // Everything is a hole (a fully sparse file), or the filesystem does
            // not report allocation at all — treat an empty answer as "unknown".
            return None;
        }
        let allocated: u64 = ranges.iter().map(|(_, l)| *l).sum();
        if allocated >= e.size {
            return None;
        }
        let mut holes = vec![true; n as usize];
        for (off, len) in ranges {
            let first = off / unit;
            let last = off.saturating_add(len).div_ceil(unit);
            for i in first..last.min(n) {
                holes[i as usize] = false;
            }
        }
        Some(holes)
    }

    /// Chunk indices ordered by the physical cluster they start on.
    ///
    /// Extents come back from FIEMAP / `FSCTL_GET_RETRIEVAL_POINTERS` as
    /// `(vcn, lcn, length)` runs, so the mapping is a lookup plus an offset. Holes
    /// sort last: there is nothing to seek to.
    fn lcn_order(
        &self,
        e: &Entry,
        unit: u64,
        n: u64,
    ) -> Option<Vec<u32>> {
        let f = RawFile::open(&e.src, &Self::src_opts()).ok()?;
        let extents = f.extents().ok()?;
        if extents.len() < 2 {
            return None;
        }
        let cluster = self.cluster.max(512);
        // The extents are sorted by VCN, so one cursor suffices: scanning the whole
        // list per chunk is O(chunks x extents), which on a fragmented file is
        // hundreds of millions of comparisons on the scheduler thread.
        let mut keyed: Vec<(u64, u32)> = Vec::with_capacity(n as usize);
        let mut cur = 0usize;
        for i in 0..n as u32 {
            let vcn = (u64::from(i) * unit) / cluster;
            while cur < extents.len() && extents[cur].vcn + extents[cur].len <= vcn {
                cur += 1;
            }
            let lcn = match extents.get(cur) {
                | Some(x) if vcn >= x.vcn && vcn < x.vcn + x.len => {
                    if x.sparse {
                        u64::MAX
                    } else {
                        x.lcn + (vcn - x.vcn)
                    }
                },
                | _ => u64::MAX,
            };
            keyed.push((lcn, i));
        }
        keyed.sort_by_key(|&(lcn, i)| (lcn, i));
        Some(keyed.into_iter().map(|(_, i)| i).collect())
    }

    fn copy_append(
        &self,
        e: &Entry,
        from: u64,
    ) -> Result<()> {
        // Keep the existing prefix; extend to the new size.
        let opts = OpenOpts {
            access: Access::Write,
            create: Create::Open,
            sparse: self.cfg.sparse,
            ..Default::default()
        };
        let f = RawFile::open(&e.dst, &opts).map_err(|x| Error::io(&e.dst, "open", x))?;
        let clear_sparse = self.reserve(&f, e.size);
        drop(f);

        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let unit = self.cfg.io_unit.min(self.cfg.buffer_size as u64);
        let mut ctx = FileCtx::new(
            id,
            e.src.clone(),
            e.dst.clone(),
            e.rel.clone(),
            e.size,
            e.mtime,
        );
        ctx.sparse = self.cfg.sparse;
        ctx.clear_sparse_at_end = clear_sparse;
        ctx.mtime_ns = mtime_ns(e.mtime);
        // Without this the append and delta paths write a destination whose
        // `need_mode` is false, so an incremental sync silently drops the
        // executable bit that the full path preserves.
        ctx.mode = e.mode;
        if self.index.enabled() {
            // The blocks before `start` keep their old hashes, which the index
            // already holds; only the rewritten tail needs recording, but a
            // partial index is not valid, so the whole file is hashed again by
            // copying the prefix. Simpler: skip indexing for appends.
        }
        let ctx = Arc::new(ctx);
        // Start at a chunk boundary so offsets stay aligned.
        let start = from / unit * unit;
        let n = (e.size - start).div_ceil(unit);
        ctx.pending.store(n as u32, Ordering::Release);
        for i in 0..n {
            let off = start + i * unit;
            let len = unit.min(e.size - off);
            let mut c = Chunk::new(ctx.clone(), off, off, len, self.align);
            c.block = (off / unit) as u32;
            self.shared.push_chunk(c);
        }
        self.shared
            .stats
            .bytes_saved
            .fetch_add(from, Ordering::Relaxed);
        Ok(())
    }

    /// Indexed block delta.
    ///
    /// The index says what the destination already contains, so the destination
    /// never has to be read: hash each source block, compare it against the
    /// recorded hash, and write only the blocks that differ.
    fn copy_indexed_delta(
        &mut self,
        e: &Entry,
    ) -> Result<()> {
        let block = self.index_block();
        let n_blocks = e.size.div_ceil(block);
        let Some(idx) = self.index.load(&e.dst) else {
            return self.copy_full(e);
        };
        let dmtime = std::fs::metadata(&e.dst)
            .ok()
            .and_then(|m| m.modified().ok());
        if !idx.matches(e.size, mtime_ns(dmtime), block, self.cfg.hash) {
            return self.copy_full(e);
        }

        let src =
            RawFile::open(&e.src, &Self::src_opts()).map_err(|x| Error::io(&e.src, "open", x))?;

        // Buffers first: once `pending` is published, bailing out would leave the
        // file permanently unfinished (the chunks already pushed still write, but
        // nothing ever finalises or truncates it).
        let Some(mut sbuf) = self.shared.pool.take(1 << 16) else {
            return self.copy_full(e);
        };

        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let mut ctx = FileCtx::new(
            id,
            e.src.clone(),
            e.dst.clone(),
            e.rel.clone(),
            e.size,
            e.mtime,
        );
        ctx.mtime_ns = mtime_ns(e.mtime);
        // Same reason as the append path: mode is part of the contract.
        ctx.mode = e.mode;
        // And the index needs the block hashes, or `--index` degrades to a full
        // copy for exactly the files that change most.
        if self.index.enabled() && e.size >= self.cfg.delta.min_size {
            let blocks = e.size.div_ceil(self.cfg.delta.block.max(1)) as usize;
            ctx.hashes = Some(std::sync::Mutex::new(vec![0u64; blocks]));
        }
        // Start from the recorded hashes and update the entries we rewrite, so the
        // index written at the end reflects the new content without a second pass.
        ctx.hashes = Some(std::sync::Mutex::new(idx.hashes.clone()));
        let ctx = Arc::new(ctx);
        ctx.pending.store(n_blocks as u32, Ordering::Release);
        // In verify mode the destination is read as well and each block must still
        // match the index; a mismatch means the index is stale for that block and
        // it gets rewritten even though the source did not change.
        let verify = self.cfg.index == IndexMode::Verify;
        let dst = if verify {
            match RawFile::open(&e.dst, &Self::dst_opts(false)) {
                | Ok(f) => Some(f),
                | Err(_) => return self.copy_full(e),
            }
        } else {
            None
        };
        let mut dbuf: Option<Buf> = if verify {
            match self.shared.pool.take(1 << 16) {
                | Some(b) => Some(b),
                | None => {
                    // Nothing pushed yet, so `copy_full` is still a clean retry.
                    return self.copy_full(e);
                },
            }
        } else {
            None
        };

        let mut written = 0u64;
        for i in 0..n_blocks {
            let off = i * block;
            let len = block.min(e.size - off) as usize;
            if src.pread(&mut sbuf.as_mut_slice()[..len], off).unwrap_or(0) != len {
                // `pending` is already published, so returning `Err` here would
                // leave the file permanently unfinished. Settle the blocks that
                // will never be queued and let the failure path run.
                self.abandon(&ctx, n_blocks - i, "short read during indexed delta");
                return Ok(());
            }
            let sh = self.cfg.hash.hash(sbuf.slice(0, len));
            let mut same = sh == idx.hashes[i as usize];
            if same && let (Some(dst), Some(db)) = (dst.as_ref(), dbuf.as_mut()) {
                same = match dst.pread(&mut db.as_mut_slice()[..len], off) {
                    | Ok(n) if n == len => {
                        self.cfg.hash.hash(db.slice(0, len)) == idx.hashes[i as usize]
                    },
                    | _ => false,
                };
            }
            if same {
                // Nothing is written for this block, so it is both "done" and
                // "saved": the progress denominator and the ETA depend on it.
                self.shared
                    .stats
                    .bytes_saved
                    .fetch_add(len as u64, Ordering::Relaxed);
                crate::pipeline::complete_external(&self.shared, &ctx);
                continue;
            }
            written += len as u64;
            let hash = self.cfg.hash.hash(sbuf.slice(0, len));
            if let Some(h) = &ctx.hashes
                && let Ok(mut v) = h.lock()
                && let Some(slot) = v.get_mut(i as usize)
            {
                *slot = sh;
            }
            let all_zero = self.cfg.sparse && crate::pipeline::is_zero_pub(sbuf.slice(0, len));
            let Some(fresh) = self.shared.pool.take(4096) else {
                self.abandon(&ctx, n_blocks - i, "no buffer for indexed delta");
                return Ok(());
            };
            let buf = std::mem::replace(&mut sbuf, fresh);
            let mut chunk = Chunk::new(ctx.clone(), off, off, len as u64, self.align);
            chunk.block = i as u32;
            self.shared.push_ready(Ready {
                chunk,
                buf,
                hash,
                all_zero,
            });
        }
        if self.cfg.verbose >= 2 {
            eprintln!(
                "  indexed delta {}: {} of {} bytes rewritten",
                e.rel.display(),
                written,
                e.size
            );
        }
        Ok(())
    }

    /// Give up on a file whose remaining blocks will never be queued.
    ///
    /// `pending` was published for the whole file up front, so every block that is
    /// never pushed has to be settled here. Without this the counter never reaches
    /// zero and the file is neither finished nor failed -- no truncate, no mtime,
    /// no index, and a summary that does not add up.
    fn abandon(
        &self,
        ctx: &Arc<FileCtx>,
        remaining: u64,
        why: &str,
    ) {
        ctx.fail_with(why.to_string());
        self.shared.error(format!("{}: {why}", ctx.dst.display()));
        if remaining == 0 {
            return;
        }
        // `complete_file` is the only thing that puts a file in a bucket, and it
        // only runs on the transition to zero. Subtracting the whole remainder
        // drove the count to zero with nothing left to settle it, so the file
        // vanished from `done + skipped + failed` entirely.
        let extra = remaining.saturating_sub(1).min(u64::from(u32::MAX)) as u32;
        if extra > 0 {
            ctx.pending.fetch_sub(extra, Ordering::AcqRel);
        }
        crate::pipeline::complete_external(&self.shared, ctx);
    }

    /// Block-level delta: read both sides block by block, queue the differing
    /// source blocks for writing.
    fn copy_delta(
        &mut self,
        e: &Entry,
    ) -> Result<()> {
        let opts = OpenOpts {
            access: Access::Write,
            create: Create::Open,
            sparse: self.cfg.sparse,
            ..Default::default()
        };
        let f = RawFile::open(&e.dst, &opts).map_err(|x| Error::io(&e.dst, "open", x))?;
        let clear_sparse = self.reserve(&f, e.size);
        drop(f);

        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let mut ctx = FileCtx::new(
            id,
            e.src.clone(),
            e.dst.clone(),
            e.rel.clone(),
            e.size,
            e.mtime,
        );
        ctx.sparse = self.cfg.sparse;
        ctx.clear_sparse_at_end = clear_sparse;
        ctx.mode = e.mode;
        let ctx = Arc::new(ctx);

        let block = self.cfg.delta.block.max(self.align);
        let n_blocks = e.size.div_ceil(block);

        let src =
            RawFile::open(&e.src, &Self::src_opts()).map_err(|x| Error::io(&e.src, "open", x))?;
        let dst = RawFile::open(&e.dst, &Self::dst_opts(false))
            .map_err(|x| Error::io(&e.dst, "open", x))?;

        let cap = block as usize;
        // Both buffers before `pending` is published: bailing out afterwards would
        // leave the file permanently unfinished.
        let Some(mut a) = self.shared.pool.take(1 << 16) else {
            return self.copy_full(e);
        };
        let Some(mut b) = self.shared.pool.take(1 << 16) else {
            return self.copy_full(e);
        };
        if a.len() < cap || b.len() < cap {
            return Err(Error::other("delta block larger than the buffer pool unit"));
        }
        // Everything that can fail has now been done, so publishing the chunk count
        // is safe: from here the loop always finishes and `finalize` runs.
        ctx.pending.store(n_blocks as u32, Ordering::Release);

        let mut written = 0u64;
        for i in 0..n_blocks {
            let off = i * block;
            let len = block.min(e.size - off) as usize;
            let ra = src.pread(&mut a.as_mut_slice()[..len], off);
            let rb = dst.pread(&mut b.as_mut_slice()[..len], off);
            let same = match (ra, rb) {
                | (Ok(na), Ok(nb)) if na == len && nb == len => a.slice(0, len) == b.slice(0, len),
                | _ => false,
            };
            if same {
                // Unchanged block: nothing to write, but the file may still be
                // complete after this one. The bytes count as saved, or progress
                // and the ETA would describe a full copy.
                self.shared
                    .stats
                    .bytes_saved
                    .fetch_add(len as u64, Ordering::Relaxed);
                crate::pipeline::complete_external(&self.shared, &ctx);
                continue;
            }
            written += len as u64;
            let hash = if self.cfg.verify == VerifyMode::None {
                0
            } else {
                self.cfg.hash.hash(a.slice(0, len))
            };
            let all_zero = self.cfg.sparse && crate::pipeline::is_zero_pub(a.slice(0, len));
            // Hand `a` to the writer and take a fresh buffer for the next block.
            let Some(fresh) = self.shared.pool.take(4096) else {
                self.abandon(&ctx, n_blocks - i, "no buffer for delta");
                break;
            };
            let buf: Buf = std::mem::replace(&mut a, fresh);
            let mut chunk = Chunk::new(ctx.clone(), off, off, len as u64, self.align);
            chunk.block = i as u32;
            if !self.shared.push_ready(Ready {
                chunk,
                buf,
                hash,
                all_zero,
            }) {
                // The writers are gone; nothing after this block can be written.
                self.abandon(&ctx, n_blocks - i, "writers stopped during delta");
                break;
            }
        }
        let _ = written;
        Ok(())
    }
}

/// Does `SetFileValidData` work here? Probing once beats extending every
/// destination and discovering halfway through that the privilege is missing.
fn probe_instant_alloc(dir: &Path) -> bool {
    let probe_dir = if dir.is_dir() {
        dir.to_path_buf()
    } else {
        dir.parent()
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
    };
    let path = probe_dir.join(".sirocco-alloc-probe.tmp");
    let opts = OpenOpts {
        access: Access::Write,
        create: Create::Create,
        ..Default::default()
    };
    let Ok(f) = RawFile::open(&path, &opts) else {
        return false;
    };
    let ok = f.prealloc_valid(1 << 20).unwrap_or(false);
    drop(f);
    let _ = std::fs::remove_file(&path);
    ok
}

/// Compare two timestamps the way a synchroniser should.
///
/// Exact equality first. A two-second tolerance is only applied when *both*
/// timestamps have zero sub-second precision, which is the signature of a
/// coarse-grained filesystem (FAT, some SMB shares). Applying the tolerance
/// unconditionally — as rsync and robocopy do — silently skips files that changed
/// within the same couple of seconds, which is exactly what a fast edit-and-sync
/// loop looks like.
fn times_equal(
    a: std::time::SystemTime,
    b: std::time::SystemTime,
) -> bool {
    if a == b {
        return true;
    }
    let d = a
        .duration_since(b)
        .or_else(|_| b.duration_since(a))
        .unwrap_or(Duration::ZERO);
    let coarse = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|d| d.subsec_nanos() == 0)
    };
    d <= Duration::from_secs(2) && coarse(a) && coarse(b)
}

/// Rough sequential write throughput of the destination, used only by the
/// compression cost model. A rotational device is assumed to be slow.
fn write_throughput_estimate(dst: &Path) -> f64 {
    match sirocco_sys::volume_info(dst) {
        | Ok(v) if v.rotational => 150.0e6,
        | Ok(_) => 600.0e6,
        | Err(_) => 300.0e6,
    }
}

fn action_name(a: &Action) -> String {
    match a {
        | Action::Skip => "skip".into(),
        | Action::Full => "full".into(),
        | Action::Delta { est } => format!("delta({:.0}%)", est * 100.0),
        | Action::IndexedDelta { .. } => "indexed-delta".into(),
        | Action::Append { from } => format!("append(+{})", format_bytes(*from)),
    }
}

/// Cheap full comparison of two files (only used for `--overwrite=if-content`).
fn content_equal(
    a: &Path,
    b: &Path,
    _align: u64,
) -> Option<bool> {
    let ma = std::fs::metadata(a).ok()?;
    let mb = std::fs::metadata(b).ok()?;
    if ma.len() != mb.len() {
        return Some(false);
    }
    let fa = RawFile::open(
        a,
        &OpenOpts {
            access: Access::Read,
            ..Default::default()
        },
    )
    .ok()?;
    let fb = RawFile::open(
        b,
        &OpenOpts {
            access: Access::Read,
            ..Default::default()
        },
    )
    .ok()?;
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    let mut off = 0u64;
    let size = ma.len();
    while off < size {
        let len = ((1u64 << 20).min(size - off)) as usize;
        let na = fa.pread(&mut ba[..len], off).ok()?;
        let nb = fb.pread(&mut bb[..len], off).ok()?;
        if na != len || nb != len {
            return Some(false);
        }
        if ba[..len] != bb[..len] {
            return Some(false);
        }
        off += len as u64;
    }
    Some(true)
}

fn copy_symlink(
    e: &Entry,
    cfg: &EngineConfig,
) -> std::io::Result<()> {
    let target = std::fs::read_link(&e.src)?;
    if let Some(p) = e.dst.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // A real directory cannot be replaced with `remove_file`, so the link was
    // never created and the error was discarded: `sync` failed on that path every
    // run without ever converging.
    match std::fs::symlink_metadata(&e.dst) {
        | Ok(md) if md.is_dir() => {
            if !may_correct_dst(cfg) {
                return Err(std::io::Error::other(
                    "a directory is in the way of the symbolic link",
                ));
            }
            std::fs::remove_dir_all(&e.dst)?;
        },
        | Ok(_) => std::fs::remove_file(&e.dst)?,
        | Err(_) => {},
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &e.dst)?;
    }
    #[cfg(windows)]
    {
        // Unlike Unix, creating a symlink on Windows needs Developer Mode or
        // SeCreateSymbolicLinkPrivilege. Ignoring the error made every link
        // silently absent while the run reported zero failures.
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(&target, &e.dst)?;
        } else {
            std::os::windows::fs::symlink_file(&target, &e.dst)?;
        }
    }
    Ok(())
}

/// True when the scan dropped paths the destination walk would not see.
///
/// `delete_extras` compares the destination against the plan, and the plan is
/// already filtered: a path the filters removed is indistinguishable from a path
/// the source does not have.
/// May the engine correct the destination when a copy is blocked?
///
/// Only in mirror mode: that is the one mode where the destination is declared
/// to be a copy of the source, and `delete_extras` already applies the same rule
/// (plus the filter checks) to the sweep at the end of the run. `cp --delete-extra`
/// used to reach these paths too, which turned one type mismatch into a
/// `remove_dir_all` in a mode that promises not to delete anything.
const fn may_correct_dst(cfg: &EngineConfig) -> bool {
    matches!(cfg.mode, Mode::Sync) && cfg.delete_extra
}

const fn filters_are_active(cfg: &EngineConfig) -> bool {
    !cfg.scan.include.is_empty()
        || !cfg.scan.exclude.is_empty()
        || cfg.scan.one_filesystem
        || cfg.scan.max_depth != usize::MAX
        || cfg.file_list.is_some()
}

/// Remove destination paths the source does not have.
///
/// Two things this has to get right beyond "delete what is not in the plan":
///
/// 1. A type mismatch. When the source has a file where the destination has a
///    directory, descending deletes the children one at a time and leaves the
///    directory itself — which the plan keeps, so the copy can never create the
///    file and the next run repeats the whole thing. The directory has to go
///    whole.
/// 2. Reporting. Every failure here used to be discarded, so a locked extra
///    survived while the run reported a clean mirror.
fn delete_extras(
    root: &Path,
    plan: &Plan,
    shared: &Shared,
    cfg: &EngineConfig,
) {
    use std::collections::HashSet;
    let verbose = cfg.verbose;
    let mut keep: HashSet<PathBuf> = plan.entries.iter().map(|e| e.dst.clone()).collect();
    let keep_files: HashSet<PathBuf> = plan
        .entries
        .iter()
        .filter(|e| e.kind != EntryKind::Dir)
        .map(|e| e.dst.clone())
        .collect();
    // The resume journal lives inside the destination and is not part of the
    // source, but deleting it would silently break the next resume.
    keep.insert(root.join(crate::journal::JOURNAL_NAME));
    if !root.is_dir() {
        return;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            shared.error(format!("scan destination {}: unreadable", d.display()));
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let is_dir = ent.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                if keep_files.contains(&p) {
                    // The source has a file here; a directory blocks the copy
                    // and emptying it would not help.
                    match std::fs::remove_dir_all(&p) {
                        | Ok(()) => {
                            if verbose >= 2 {
                                eprintln!("  deleted extra tree {}", p.display());
                            }
                        },
                        | Err(e) => shared.error(format!("delete {}: {e}", p.display())),
                    }
                } else {
                    stack.push(p.clone());
                    dirs.push(p);
                }
            } else if !keep.contains(&p) {
                match std::fs::remove_file(&p) {
                    | Ok(()) => {
                        if verbose >= 2 {
                            eprintln!("  deleted extra {}", p.display());
                        }
                    },
                    | Err(e) => shared.error(format!("delete {}: {e}", p.display())),
                }
            }
        }
    }
    for d in dirs.iter().rev() {
        // Only drop directories that no longer exist on the source; an empty
        // directory that is still part of the source must survive.
        if !keep.contains(d)
            && let Err(e) = std::fs::remove_dir(d)
            && e.kind() != std::io::ErrorKind::DirectoryNotEmpty
        {
            shared.error(format!("delete dir {}: {e}", d.display()));
        }
    }
}

fn run_delete(
    cfg: &EngineConfig,
    plan: Plan,
    root: &Path,
) -> Summary {
    let stats = sirocco_core::stats::Stats::new();
    let mut errors = Vec::new();
    // `plan.total_files` counts regular files only, while the loop below also
    // removes symlinks. Counting them here keeps `done <= total`.
    let links = plan
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Symlink)
        .count() as u64;
    let total = plan.total_files + links;
    stats.files_total.store(total, Ordering::Relaxed);

    // Files first, then directories deepest-first. The scan order is not a
    // guarantee (the walk is parallel), so the ordering has to be explicit.
    let mut dirs: Vec<&Entry> = plan
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Dir)
        .collect();
    dirs.sort_by_key(|e| std::cmp::Reverse(e.rel.components().count()));

    for e in plan.entries.iter().filter(|e| e.kind != EntryKind::Dir) {
        let r = match e.kind {
            | EntryKind::File => std::fs::remove_file(&e.src),
            | EntryKind::Symlink => {
                // A directory symlink or junction is removed with `rmdir`, not
                // `unlink`, on Windows; `remove_file` fails and then the parent
                // `remove_dir` fails as non-empty.
                if std::fs::symlink_metadata(&e.src).is_ok_and(|m| m.is_dir()) {
                    std::fs::remove_dir(&e.src)
                } else {
                    std::fs::remove_file(&e.src)
                }
            },
            | _ => Ok(()),
        };
        match r {
            | Ok(()) => {
                // `Other` is a device, socket or FIFO: nothing was removed, so
                // counting it would over-report the delete summary.
                if matches!(e.kind, EntryKind::File | EntryKind::Symlink) {
                    stats.files_done.fetch_add(1, Ordering::Relaxed);
                }
            },
            | Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
            | Err(err) => errors.push(format!("delete {}: {err}", e.src.display())),
        }
    }
    // `dirs_created` stays at zero: a delete creates nothing, and reporting the
    // removed directories under that name is the kind of number people check.
    for e in dirs {
        match std::fs::remove_dir(&e.src) {
            | Ok(()) => {},
            | Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
            | Err(err) => errors.push(format!("delete {}: {err}", e.src.display())),
        }
    }
    // Remove the root itself when it was given as a directory.
    if root.is_dir()
        && let Err(err) = std::fs::remove_dir(root)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        errors.push(format!("delete {}: {err}", root.display()));
    }
    Summary {
        mode: cfg.mode,
        stats: stats.snapshot(),
        errors,
        files_planned: total,
        bytes_planned: plan.total_bytes,
        backend: "none".into(),
        align: 0,
        readers: 0,
        writers: 0,
        volume_src: None,
        volume_dst: None,
        interrupted: sirocco_sys::interrupt::is_set(),
    }
}

fn run_dry(
    cfg: &EngineConfig,
    plan: &Plan,
    align: u64,
) -> Summary {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut dirs = 0u64;
    for e in &plan.entries {
        match e.kind {
            | EntryKind::Dir => {
                dirs += 1;
                if cfg.verbose >= 2 {
                    eprintln!("mkdir {}", e.dst.display());
                }
            },
            | EntryKind::File => {
                files += 1;
                bytes += e.size;
                if cfg.verbose >= 1 {
                    println!("{}\t{}", format_bytes(e.size), e.rel.display());
                }
            },
            | _ => {},
        }
    }
    if cfg.verbose >= 1 {
        eprintln!(
            "dry run: {files} files, {bytes} bytes, {dirs} dirs (would copy {} )",
            format_bytes(bytes)
        );
    }
    let stats = sirocco_core::stats::Stats::new().snapshot();
    Summary {
        mode: cfg.mode,
        stats,
        errors: Vec::new(),
        files_planned: files,
        bytes_planned: bytes,
        backend: "dry-run".into(),
        align,
        readers: 0,
        writers: 0,
        volume_src: None,
        volume_dst: None,
        interrupted: false,
    }
}

fn spawn_progress(
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let s = shared.stats.snapshot();
            let pct = s.progress() * 100.0;
            let eta = s.eta_secs().map_or_else(|| "--".into(), format_duration);
            let mut err = std::io::stderr();
            let _ = write!(
                err,
                "\r  {:5.1}%  {} / {}  {}/s  {} files  eta {}   ",
                pct,
                format_bytes(s.bytes_accounted()),
                format_bytes(s.bytes_total),
                format_rate(s.write_rate()),
                s.files_done + s.files_skipped,
                eta
            );
            let _ = err.flush();
            if last.elapsed().as_secs() > 3600 {
                last = Instant::now();
            }
        }
    })
}

/// Concurrency limit for the scheduler threads, adjustable while the run is in
/// progress.
///
/// The right number of feeders genuinely depends on the workload and the
/// filesystem: a handful of large files needs one (a single file already fills the
/// queue with chunks), while a directory full of 4 KiB files is limited by the
/// directory lock, not by the thread count. No static formula knows that, so the
/// count is a permit that a controller adjusts from measured throughput.
struct FeedGate {
    /// Permits currently available.
    available: Mutex<usize>,
    /// Permits currently held by a running feeder.
    held: AtomicUsize,
    cv: Condvar,
    stop: AtomicBool,
}

impl FeedGate {
    fn new(n: usize) -> Arc<Self> {
        Arc::new(Self {
            available: Mutex::new(n.max(1)),
            held: AtomicUsize::new(0),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    /// Take a slot, or return false once the run is over.
    fn acquire(&self) -> bool {
        let mut n = self.available.lock().unwrap();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return false;
            }
            if *n > 0 {
                *n -= 1;
                self.held.fetch_add(1, Ordering::AcqRel);
                return true;
            }
            n = self.cv.wait(n).unwrap();
        }
    }

    fn release(&self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
        let mut n = self.available.lock().unwrap();
        *n += 1;
        drop(n);
        self.cv.notify_one();
    }

    /// Change the *total* concurrency, not the number of free slots.
    ///
    /// Writing `available = n` directly would hand back the permits the running
    /// feeders are still holding, so every adjustment would inflate the limit and
    /// the controller would quietly disable itself.
    fn set(
        &self,
        n: usize,
    ) {
        let held = self.held.load(Ordering::Acquire);
        let mut a = self.available.lock().unwrap();
        *a = n.max(1).saturating_sub(held);
        drop(a);
        self.cv.notify_all();
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }
}

/// Climb towards the feeder count that maximises throughput.
///
/// Deliberately dumb: step by one, keep the best result seen, and reverse when a
/// step makes things worse. The landscape is unimodal enough (too few feeders
/// starves the queue, too many contend on the directory lock) that this converges
/// in a handful of samples, and it costs one atomic read every 600 ms.
fn feed_controller(
    shared: Arc<Shared>,
    gate: Arc<FeedGate>,
    max: usize,
    by_bytes: bool,
    start: usize,
) {
    let mut current = start.max(1);
    let mut best = current;
    let mut best_rate = 0.0f64;
    let mut dir: i32 = 1;
    let mut last = feed_metric(&shared, by_bytes);
    let mut last_t = Instant::now();

    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(600));
        if gate.stop.load(Ordering::Relaxed) || shared.abort.load(Ordering::Relaxed) {
            return;
        }
        shared.update_queue_gauge();
        let now = feed_metric(&shared, by_bytes);
        let dt = last_t.elapsed().as_secs_f64().max(1e-9);
        let rate = now.saturating_sub(last) as f64 / dt;
        last = now;
        last_t = Instant::now();

        if rate > best_rate {
            best_rate = rate;
            best = current;
        } else if best_rate > 0.0 && rate < best_rate * 0.92 {
            // That step hurt: go back and try the other direction.
            current = best;
            dir = -dir;
        }

        let next = (current as i32 + dir).clamp(1, max as i32) as usize;
        if next == current {
            dir = -dir;
        }
        current = next;
        gate.set(current);

        if shared.stats.snapshot().progress() > 0.85 {
            break;
        }
    }
    // Let the tail run without the gate second-guessing it.
    gate.set(best.max(1));
    if shared.cfg.verbose >= 2 {
        eprintln!("sirocco: feeders settled at {best}");
    }
}

#[inline]
fn feed_metric(
    shared: &Shared,
    by_bytes: bool,
) -> u64 {
    let s = &shared.stats;
    if by_bytes {
        s.bytes_written.load(Ordering::Relaxed) + s.bytes_holes.load(Ordering::Relaxed)
    } else {
        s.files_done.load(Ordering::Relaxed) + s.files_skipped.load(Ordering::Relaxed)
    }
}

/// Split a job into `n` disjoint groups of source-relative paths, balanced by
/// bytes, plus the list of directories to create up front.
///
/// The split is deliberately **coarse**: whole runs of the walk order, which keeps
/// sibling files of one directory together. Splitting per file would put several
/// processes on the same directory and, on NTFS, on the same directory lock — the
/// exact contention the split is supposed to avoid.
pub fn partition(
    src: &Path,
    dst: &Path,
    cfg: &EngineConfig,
    n: usize,
) -> Result<(Vec<Vec<std::path::PathBuf>>, Vec<std::path::PathBuf>, u64)> {
    let topo = Topology::detect();
    let plan = scan::scan(src, dst, &cfg.scan, topo.logical_count())?;
    let n = n.max(1);
    let target = (plan.total_bytes / n as u64).max(1);
    let mut groups: Vec<Vec<std::path::PathBuf>> = vec![Vec::new(); n];
    let mut dirs = Vec::new();
    let mut gi = 0usize;
    let mut acc = 0u64;
    for e in &plan.entries {
        match e.kind {
            | EntryKind::Dir => dirs.push(e.dst.clone()),
            | EntryKind::File => {
                groups[gi].push(e.rel.clone());
                acc += e.size;
                if acc >= target && gi + 1 < n {
                    gi += 1;
                    acc = 0;
                }
            },
            // Symlinks and special files were dropped here, so a split run
            // silently copied fewer entries than the plan and reported success.
            // They cost no bytes, so they ride along with the current group.
            | _ => groups[gi].push(e.rel.clone()),
        }
    }
    Ok((groups, dirs, plan.total_bytes))
}

/// Print a summary block.
pub fn print_summary(s: &Summary) {
    let d = s.stats;
    println!();
    println!("sirocco {} summary", s.mode.name());
    println!("  backend      : {} (align {})", s.backend, s.align);
    if let Some(v) = &s.volume_src {
        println!("  source       : {v}");
    }
    if let Some(v) = &s.volume_dst {
        println!("  destination  : {v}");
    }
    println!(
        "  files        : {} copied, {} skipped, {} failed (of {})",
        d.files_done, d.files_skipped, d.files_failed, s.files_planned
    );
    println!(
        "  data         : {} written, {} read, {} verified, {} punched",
        format_bytes(d.bytes_written),
        format_bytes(d.bytes_read),
        format_bytes(d.bytes_verified),
        format_bytes(d.bytes_holes)
    );
    if d.bytes_saved > 0 && d.bytes_holes == 0 {
        println!("  saved        : {}", format_bytes(d.bytes_saved));
    }
    println!("  elapsed      : {}", format_duration(d.elapsed_secs));
    println!(
        "  throughput   : {} (write), {} (read)",
        format_rate(d.write_rate()),
        format_rate(d.read_rate())
    );
    if s.interrupted {
        println!("  status       : INTERRUPTED (partial result)");
    }
    if !s.errors.is_empty() {
        println!("  errors       : {}", s.errors.len());
        for e in s.errors.iter().take(20) {
            println!("    ! {e}");
        }
        if s.errors.len() > 20 {
            println!("    ... and {} more", s.errors.len() - 20);
        }
    }
}

/// Effective order used for the run (exposed for the CLI banner).
#[must_use]
pub const fn describe_order(o: Order) -> &'static str {
    o.name()
}

/// Calibrate read/write throughput with a small temporary transfer.
///
/// Used by `--calibrate` and by the GUI's "measure" button to feed the delta
/// cost model real numbers instead of the defaults.
///
/// `dst` is whatever the user pointed at: an existing directory, a directory
/// that does not exist yet, or the target *file* of a single-file copy. The
/// measurement is taken in the nearest existing directory at or above it, which
/// is the volume the copy will actually write to.
///
/// # Errors
///
/// Returns the OS error from creating, writing or reading the temporary file.
/// The file is removed on every path out, including an early return.
pub fn calibrate(dst: &Path) -> std::io::Result<IoProfile> {
    // 64 MiB is large enough to leave the drive's own write cache behind and
    // small enough that nobody minds it appearing for a second.
    const SIZE: u64 = 64 << 20;
    const CHUNK: usize = 1 << 20;

    let dir = measure_dir(dst)?;
    let path = dir.join(format!(".sirocco-calib-{}.tmp", std::process::id()));
    let _guard = CalibFile(path.clone());

    // Direct I/O needs aligned memory; the buffer pool's allocator is the one
    // already in the tree, so the calibration cannot drift from the engine's
    // own alignment rules.
    let mut src = sirocco_sys::align::AlignedBlock::new(CHUNK, false)?;
    for (i, b) in src.as_mut_slice().iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(31) as u8;
    }
    let mut sink = sirocco_sys::align::AlignedBlock::new(CHUNK, false)?;

    // The first attempt asks for `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` so the
    // numbers describe the device rather than the page cache. Filesystems that
    // refuse it (most network shares) get the buffered fallback.
    let (f, direct) = match open_calib(&path, true) {
        | Ok(f) => (f, true),
        | Err(_) => (open_calib(&path, false)?, false),
    };

    let t0 = Instant::now();
    let mut off = 0u64;
    while off < SIZE {
        write_all_at(&f, src.as_slice(), off)?;
        off += CHUNK as u64;
    }
    // Includes the device flush: a rate that excluded it would describe RAM.
    f.flush()?;
    let w = SIZE as f64 / t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let mut off = 0u64;
    while off < SIZE {
        read_all_at(&f, sink.as_mut_slice(), off)?;
        off += CHUNK as u64;
    }
    let r = SIZE as f64 / t1.elapsed().as_secs_f64();
    drop(f);

    // A buffered read-back is served from the page cache and would report several
    // GB/s on any machine, which makes the delta cost model think comparing is
    // free. Without direct I/O the write rate is the honest stand-in: it is a
    // measured device rate, never an optimistic one.
    let dest_read = if direct { r } else { r.min(w) };
    Ok(IoProfile::new(r, w, dest_read))
}

/// A temporary file that deletes itself, so no early return can leave a 64 MiB
/// file behind in the user's destination.
struct CalibFile(PathBuf);

impl Drop for CalibFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The nearest existing directory at or above `path`.
///
/// A destination that does not exist yet is the common case: the user types a
/// new folder name and presses Measure before pressing Start.
fn measure_dir(path: &Path) -> std::io::Result<PathBuf> {
    let start = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let mut cur = Some(start);
    while let Some(p) = cur {
        let p = if p.as_os_str().is_empty() {
            Path::new(".")
        } else {
            p
        };
        if p.is_dir() {
            return Ok(p.to_path_buf());
        }
        cur = p.parent();
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no existing directory at or above {}", path.display()),
    ))
}

fn open_calib(
    path: &Path,
    direct: bool,
) -> std::io::Result<RawFile> {
    RawFile::open(
        path,
        &OpenOpts {
            // Read *and* write: the same handle measures both directions. Opening
            // write-only made every calibration fail at the read-back step.
            access: Access::ReadWrite,
            create: Create::Create,
            direct,
            sequential: true,
            ..OpenOpts::default()
        },
    )
}

fn write_all_at(
    f: &RawFile,
    buf: &[u8],
    off: u64,
) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = f.pwrite(&buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "calibration write made no progress",
            ));
        }
        done += n;
    }
    Ok(())
}

fn read_all_at(
    f: &RawFile,
    buf: &mut [u8],
    off: u64,
) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = f.pread(&mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "calibration read came back short",
            ));
        }
        done += n;
    }
    Ok(())
}
