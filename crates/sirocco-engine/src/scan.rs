//! Directory scanning.
//!
//! The walk is parallel: threads pop directories from a shared stack, so a tree
//! with a million files is enumerated by all cores at once instead of one. Each
//! worker appends to its own vector, merged at the end — no lock on the hot path.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use sirocco_core::Result;
use sirocco_core::pattern;
use sirocco_core::plan::Entry;
use sirocco_core::plan::EntryKind;
use sirocco_core::plan::Plan;
use sirocco_core::plan::ScanCfg;

/// Identity key used for physical ordering (MFT index / inode).
///
/// On Windows this is a full `CreateFileW` + query per file, so it is only paid
/// when `--order=physical` actually asks for it.
fn order_key(
    path: &Path,
    want: bool,
) -> u64 {
    if !want {
        return 0;
    }
    sirocco_sys::file_order_key(path).unwrap_or(0)
}

/// Permission bits (Unix) or attribute bits (Windows).
fn file_mode(md: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = md;
        0
    }
}

/// File identity and link count.
fn identity(
    path: &Path,
    md: &std::fs::Metadata,
) -> (u64, u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Free: the inode and link count came with the metadata we already read.
        let _ = path;
        (md.ino(), md.nlink() as u32)
    }
    #[cfg(windows)]
    {
        // Windows metadata does not expose either value stably, so this costs one
        // open. Only paid when links are being preserved.
        let _ = md;
        sirocco_sys::file_identity(path).unwrap_or((0, 1))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, md);
        (0, 1)
    }
}

/// Identity of a directory, for cycle detection.
///
/// An inode is unique within a filesystem, not across them, so the pair is the
/// key: `--follow-symlinks` across a mount point used to prune a subtree whenever
/// two volumes happened to hand out the same inode number.
fn dir_key(
    path: &Path,
    md: &std::fs::Metadata,
) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        (md.dev(), md.ino())
    }
    #[cfg(windows)]
    {
        let _ = md;
        let (id, _) = sirocco_sys::file_identity(path).unwrap_or((0, 1));
        (device_id(path), id)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, md);
        (0, 0)
    }
}

/// Volume identity, used by `--one-filesystem`.
fn device_id(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map_or(0, |m| m.dev())
    }
    #[cfg(windows)]
    {
        // Compare the root component (drive / UNC share).
        let s = path
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let mut h = 1_469_598_103_934_665_603u64;
        for b in s.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(1_099_511_628_211);
        }
        h
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        0
    }
}

struct WalkState {
    stack: Mutex<Vec<(PathBuf, usize)>>,
    active: AtomicUsize,
    errors: AtomicUsize,
    out: Mutex<Vec<Vec<Entry>>>,
    cfg: ScanCfg,
    src_root: PathBuf,
    dst_root: PathBuf,
    root_dev: u64,
    /// Directory identities already walked.
    ///
    /// Only used when following symlinks: without it a link that points at one of
    /// its own ancestors makes the walk run forever. Empty otherwise, so the cost
    /// is one `is_empty` check per directory.
    seen_dirs: Mutex<std::collections::HashSet<(u64, u64)>>,
}

/// Build a plan from an explicit list of source-relative paths.
///
/// This is what a split run hands to its children: the parent has already decided
/// which files belong to which worker, so the child must not walk the tree again.
pub fn from_list(
    src_root: &Path,
    dst_root: &Path,
    list: &Path,
) -> Result<Plan> {
    let text =
        std::fs::read_to_string(list).map_err(|e| sirocco_core::Error::io(list, "read", e))?;
    let mut plan = Plan::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The manifest is a plain text file, so it must not be able to name
        // anything outside the source root, and entries that are not valid UTF-8
        // simply cannot be represented in it.
        let rel = PathBuf::from(line.replace('/', std::path::MAIN_SEPARATOR_STR));
        if rel.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            continue;
        }
        let src = src_root.join(&rel);
        // Vanished between the parent scan and now.
        let Ok(md) = std::fs::symlink_metadata(&src) else {
            continue;
        };
        let kind = if md.is_dir() {
            EntryKind::Dir
        } else if md.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        let (file_id, links) = if kind == EntryKind::File {
            identity(&src, &md)
        } else {
            (0, 1)
        };
        plan.push(Entry {
            dst: dst_root.join(&rel),
            src,
            rel,
            size: if kind == EntryKind::File {
                md.len()
            } else {
                0
            },
            mtime: md.modified().ok(),
            kind,
            order: 0,
            file_id,
            links,
            mode: file_mode(&md),
        });
    }
    Ok(plan)
}

/// Enumerate `src_root` into a [`Plan`] with destinations rooted at `dst_root`.
pub fn scan(
    src_root: &Path,
    dst_root: &Path,
    cfg: &ScanCfg,
    threads: usize,
) -> Result<Plan> {
    let root_md =
        std::fs::metadata(src_root).map_err(|e| sirocco_core::Error::io(src_root, "stat", e))?;
    let root_dev = device_id(src_root);

    let mut plan = Plan::default();
    if root_md.is_file() {
        // Single-file source: destination is the file itself (or a directory).
        let dst = if dst_root.is_dir() {
            dst_root.join(src_root.file_name().unwrap_or_default())
        } else {
            dst_root.to_path_buf()
        };
        plan.push(Entry {
            src: src_root.to_path_buf(),
            dst,
            rel: PathBuf::from(src_root.file_name().unwrap_or_default()),
            size: root_md.len(),
            mtime: root_md.modified().ok(),
            kind: EntryKind::File,
            order: order_key(src_root, cfg.order == sirocco_core::plan::Order::Physical),
            file_id: 0,
            links: 1,
            mode: file_mode(&root_md),
        });
        return Ok(plan);
    }

    // The root has to be in the set before the walk starts: a link pointing at
    // the root otherwise passes the cycle check once and duplicates the whole
    // top level under `<dst>/<link>/`.
    let mut seen = std::collections::HashSet::new();
    if cfg.follow_symlinks
        && let Ok(md) = std::fs::metadata(src_root)
    {
        seen.insert(dir_key(src_root, &md));
    }

    let state = Arc::new(WalkState {
        stack: Mutex::new(vec![(src_root.to_path_buf(), 0)]),
        active: AtomicUsize::new(1),
        errors: AtomicUsize::new(0),
        out: Mutex::new(Vec::new()),
        cfg: cfg.clone(),
        src_root: src_root.to_path_buf(),
        dst_root: dst_root.to_path_buf(),
        root_dev,
        seen_dirs: Mutex::new(seen),
    });

    let n = threads.clamp(1, 32);
    std::thread::scope(|s| {
        for i in 0..n {
            let st = state.clone();
            s.spawn(move || walk_worker(st, i));
        }
    });

    for v in state.out.lock().unwrap().drain(..) {
        for e in v {
            plan.push(e);
        }
    }
    plan.scan_errors = state.errors.load(Ordering::Relaxed) as u64;
    plan.sort(cfg.order);
    Ok(plan)
}

fn walk_worker(
    st: Arc<WalkState>,
    id: usize,
) {
    let mut local: Vec<Entry> = Vec::new();
    loop {
        // A cancel during the walk must take effect here too, otherwise a large
        // or slow tree keeps the UI in "stopping" for minutes. Dropping the queue
        // is what lets the other workers reach their exit condition.
        if sirocco_sys::interrupt::flag().load(Ordering::Relaxed) {
            let mut stack = st.stack.lock().unwrap();
            if !stack.is_empty() {
                stack.clear();
            }
        }
        let next = {
            let mut stack = st.stack.lock().unwrap();
            stack.pop()
        };
        if let Some((dir, depth)) = next {
            if let Err(e) = read_dir_into(&st, &dir, depth, &mut local) {
                // Unreadable directory: record it and keep going, but remember
                // that the plan is now incomplete.
                st.errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("sirocco: cannot read {}: {e}", dir.display());
            }
            st.active.fetch_sub(1, Ordering::AcqRel);
        } else {
            if st.active.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::yield_now();
        }
    }
    st.out.lock().unwrap().push(local);
    let _ = id;
}

fn read_dir_into(
    st: &WalkState,
    dir: &Path,
    depth: usize,
    out: &mut Vec<Entry>,
) -> std::io::Result<()> {
    let rd = std::fs::read_dir(dir)?;
    let mut subdirs = Vec::new();
    let mut seen = 0usize;
    for ent in rd {
        // Checked every 256 entries: a directory with a million children is
        // common enough (mail spools, build caches) that it must be interruptible.
        seen += 1;
        if seen.is_multiple_of(256) && sirocco_sys::interrupt::flag().load(Ordering::Relaxed) {
            return Ok(());
        }
        let Ok(ent) = ent else {
            st.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let path = ent.path();
        let Ok(ft) = ent.file_type() else {
            st.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let rel = match path.strip_prefix(&st.src_root) {
            | Ok(r) => r.to_path_buf(),
            | Err(_) => continue,
        };

        // `max_depth` counts levels below the root, so the root's own children
        // are level 1. Gating only the descent let one extra level through, which
        // made every `--max-depth=N` behave like `N+1`.
        if rel.components().count() > st.cfg.max_depth {
            continue;
        }

        if !st.cfg.include.is_empty() && !pattern::any_match(&st.cfg.include, &rel) {
            // A directory may still contain matching files; only skip files.
            if !ft.is_dir() {
                continue;
            }
        }
        if pattern::any_match(&st.cfg.exclude, &rel) {
            continue;
        }

        let md = if ft.is_symlink() && !st.cfg.follow_symlinks {
            if let Ok(m) = std::fs::symlink_metadata(&path) {
                m
            } else {
                st.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        } else {
            if let Ok(m) = std::fs::metadata(&path) {
                m
            } else {
                st.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        if st.cfg.one_filesystem && device_id(&path) != st.root_dev {
            continue;
        }

        // With `--follow-symlinks` a link is whatever it points at: a linked
        // directory has to be descended and a linked file has to be copied with
        // its content. Deciding from `ft` first made the flag a no-op.
        let kind = if ft.is_symlink() && !st.cfg.follow_symlinks {
            EntryKind::Symlink
        } else if md.is_dir() {
            EntryKind::Dir
        } else if md.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        let mode = file_mode(&md);
        let (file_id, links) = if kind == EntryKind::File && st.cfg.collect_links {
            identity(&path, &md)
        } else {
            (0, 1)
        };

        let dst = st.dst_root.join(&rel);
        out.push(Entry {
            src: path.clone(),
            dst,
            rel: rel.clone(),
            size: if kind == EntryKind::File {
                md.len()
            } else {
                0
            },
            mtime: md.modified().ok(),
            kind,
            order: order_key(&path, st.cfg.order == sirocco_core::plan::Order::Physical),
            file_id,
            links,
            mode,
        });

        if kind == EntryKind::Dir && depth + 1 < st.cfg.max_depth {
            // A followed link can point back at an ancestor. Remembering the
            // directory identity is what stops the walk from looping.
            let fresh = if st.cfg.follow_symlinks {
                let id = dir_key(&path, &md);
                st.seen_dirs.lock().is_ok_and(|mut s| s.insert(id))
            } else {
                true
            };
            if fresh {
                subdirs.push((path, depth + 1));
            }
        }
    }

    if !subdirs.is_empty() {
        let mut stack = st.stack.lock().unwrap();
        st.active.fetch_add(subdirs.len(), Ordering::AcqRel);
        stack.extend(subdirs);
    }
    Ok(())
}
