//! Integration tests for the copy engine.
//!
//! These drive the public API against real directories and compare the result
//! with an independent walk. They are not shipped code, so the lints that ask
//! production code to handle every failure are off for the file: a test that
//! cannot build its fixture has already failed, and `unwrap()` states that in
//! one word. Offset arithmetic and slicing are the subject under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::field_reassign_with_default,
    clippy::redundant_clone,
    clippy::type_complexity,
    clippy::use_self,
    dead_code
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use sirocco_core::hash::xxh64;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::VerifyMode;
use sirocco_engine::config::EngineConfig;
use sirocco_engine::config::PreallocMode;
use sirocco_engine::session;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sirocco-eng-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn sub(
        &self,
        name: &str,
    ) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pattern(
    n: usize,
    seed: u64,
) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

/// Digest every file under `root`: relative path -> (len, hash).
fn digest(root: &Path) -> BTreeMap<String, (u64, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let md = std::fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                let data = std::fs::read(&p).unwrap();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, (data.len() as u64, xxh64(&data)));
            }
        }
    }
    out
}

fn make_tree(root: &Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(root.join("deep/a/b")).unwrap();
    std::fs::create_dir_all(root.join("empty_dir")).unwrap();
    std::fs::write(root.join("empty.bin"), b"").unwrap();
    std::fs::write(root.join("tiny.txt"), b"hello\n").unwrap();
    std::fs::write(root.join("exact_4k.bin"), vec![0x5Au8; 4096]).unwrap();
    std::fs::write(root.join("one_mib.bin"), pattern(1 << 20, 1)).unwrap();
    std::fs::write(root.join("odd.bin"), pattern((1 << 20) + 12345, 2)).unwrap();
    std::fs::write(root.join("sub/zeros.bin"), vec![0u8; 3 << 20]).unwrap();
    std::fs::write(root.join("sub/text.txt"), b"abcdefgh\n".repeat(200_000)).unwrap();
    std::fs::write(root.join("deep/a/b/big.bin"), pattern(9 << 20, 3)).unwrap();
    let many = root.join("many");
    std::fs::create_dir_all(&many).unwrap();
    for i in 0..120u64 {
        std::fs::write(
            many.join(format!("f{i:03}.dat")),
            pattern((i as usize * 37) % 5000, i + 9),
        )
        .unwrap();
    }
}

fn base_cfg() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.quiet = true;
    c.verbose = 0;
    c
}

fn copy(
    cfg: EngineConfig,
    src: &Path,
    dst: &Path,
) -> session::Summary {
    session::run(cfg, src.to_path_buf(), dst.to_path_buf()).expect("session failed")
}

#[test]
fn full_copy_is_byte_exact() {
    let d = TempDir::new("copy");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");
    let s = copy(base_cfg(), &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "errors: {:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));
    assert!(
        dst.join("empty_dir").is_dir(),
        "empty directories are created"
    );
    assert_eq!(s.stats.files_done, s.files_planned);
}

#[test]
fn verification_modes_all_pass() {
    let d = TempDir::new("verify");
    let src = d.sub("src");
    make_tree(&src);
    for (i, mode) in [
        VerifyMode::None,
        VerifyMode::Size,
        VerifyMode::SizeMtime,
        VerifyMode::Hash,
        VerifyMode::Sample,
    ]
    .into_iter()
    .enumerate()
    {
        let dst = d.sub(&format!("dst{i}"));
        let mut cfg = base_cfg();
        cfg.verify = mode;
        let s = copy(cfg, &src, &dst);
        assert_eq!(s.stats.files_failed, 0, "{mode:?}: {:?}", s.errors);
        assert_eq!(
            digest(&src),
            digest(&dst),
            "{mode:?} produced a different tree"
        );
    }
}

#[test]
fn second_run_skips_everything() {
    let d = TempDir::new("skip");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");
    copy(base_cfg(), &src, &dst);
    let s = copy(base_cfg(), &src, &dst);
    assert_eq!(s.stats.files_done, 0, "nothing should be rewritten");
    assert_eq!(s.stats.files_skipped, s.files_planned);
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn changed_file_is_updated() {
    let d = TempDir::new("update");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");
    copy(base_cfg(), &src, &dst);

    std::fs::write(src.join("tiny.txt"), b"different contents\n").unwrap();
    let mut big = std::fs::read(src.join("deep/a/b/big.bin")).unwrap();
    big[1000] ^= 0xFF;
    std::fs::write(src.join("deep/a/b/big.bin"), &big).unwrap();

    let s = copy(base_cfg(), &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));
    assert_eq!(s.stats.files_done, 2, "exactly the two changed files");
}

#[test]
fn growth_is_appended() {
    let d = TempDir::new("append");
    let src = d.sub("src");
    let dst = d.sub("dst");
    std::fs::create_dir_all(&src).unwrap();
    let base = pattern(24 << 20, 11);
    std::fs::write(src.join("log.bin"), &base).unwrap();
    copy(base_cfg(), &src, &dst);

    let mut grown = base.clone();
    grown.extend_from_slice(&pattern(8 << 20, 12));
    std::fs::write(src.join("log.bin"), &grown).unwrap();

    let mut cfg = base_cfg();
    cfg.verify = VerifyMode::Hash;
    let s = copy(cfg, &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));
    assert!(
        s.stats.bytes_saved >= 24 << 20,
        "the untouched prefix should not be rewritten (saved {})",
        s.stats.bytes_saved
    );
}

#[test]
fn sparse_zero_blocks_are_punched() {
    let d = TempDir::new("sparse");
    let src = d.sub("src");
    let dst = d.sub("dst");
    std::fs::create_dir_all(&src).unwrap();
    let mut data = vec![0u8; 16 << 20];
    // One 1 MiB island of real data in a sea of zeros.
    data[8 << 20..9 << 20].copy_from_slice(&pattern(1 << 20, 5));
    std::fs::write(src.join("sparse.img"), &data).unwrap();

    let mut cfg = base_cfg();
    cfg.sparse = true;
    let s = copy(cfg, &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));
    assert!(
        s.stats.bytes_saved >= 8 << 20,
        "zero regions should be punched, saved {}",
        s.stats.bytes_saved
    );
}

#[test]
fn overwrite_policies_behave() {
    let d = TempDir::new("policy");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");
    copy(base_cfg(), &src, &dst);

    let mut never = base_cfg();
    never.overwrite = Overwrite::Never;
    assert_eq!(copy(never, &src, &dst).stats.files_done, 0);

    let mut always = base_cfg();
    always.overwrite = Overwrite::Always;
    assert_eq!(copy(always, &src, &dst).stats.files_skipped, 0);

    let mut content = base_cfg();
    content.overwrite = Overwrite::IfContentDiffers;
    assert_eq!(copy(content, &src, &dst).stats.files_done, 0);
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn move_removes_the_source() {
    let d = TempDir::new("move");
    let src = d.sub("src");
    make_tree(&src);
    let before = digest(&src);
    let dst = d.sub("dst");
    let mut cfg = base_cfg();
    cfg.mode = Mode::Move;
    copy(cfg, &src, &dst);
    assert_eq!(digest(&dst), before);
    assert!(!src.join("tiny.txt").exists());
}

#[test]
fn delete_removes_the_tree() {
    let d = TempDir::new("delete");
    let src = d.sub("src");
    make_tree(&src);
    let mut cfg = base_cfg();
    cfg.mode = Mode::Delete;
    copy(cfg, &src, &d.sub("unused"));
    assert!(!src.exists(), "delete should remove the whole tree");
}

#[test]
fn engines_and_layouts_agree() {
    let d = TempDir::new("engines");
    let src = d.sub("src");
    make_tree(&src);
    let want = digest(&src);
    let variants: Vec<(&str, Box<dyn Fn(&mut EngineConfig)>)> = vec![
        ("native", Box::new(|_c: &mut EngineConfig| {})),
        (
            "blocking",
            Box::new(|c: &mut EngineConfig| {
                c.reactor = sirocco_sys::ReactorKind::Blocking;
            }),
        ),
        (
            "nodirect",
            Box::new(|c: &mut EngineConfig| c.direct_io = false),
        ),
        (
            "noprealloc",
            Box::new(|c: &mut EngineConfig| c.prealloc = PreallocMode::Off),
        ),
        (
            "small-buffer",
            Box::new(|c: &mut EngineConfig| {
                c.buffer_size = 64 << 10;
                c.io_unit = 64 << 10;
            }),
        ),
        (
            "single-thread",
            Box::new(|c: &mut EngineConfig| {
                c.readers = 1;
                c.writers = 1;
                c.overlap = 2;
            }),
        ),
        (
            "physical-order",
            Box::new(|c: &mut EngineConfig| c.scan.order = Order::Physical),
        ),
    ];
    for (name, tweak) in variants {
        let dst = d.sub(&format!("dst-{name}"));
        let mut cfg = base_cfg();
        tweak(&mut cfg);
        let s = copy(cfg, &src, &dst);
        assert_eq!(s.stats.files_failed, 0, "{name}: {:?}", s.errors);
        assert_eq!(digest(&dst), want, "{name} produced a different tree");
    }
}

#[test]
fn excludes_and_includes_filter() {
    use sirocco_core::plan::ScanCfg;
    let d = TempDir::new("filter");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");
    let mut cfg = base_cfg();
    cfg.scan = ScanCfg {
        exclude: vec!["*.bin".into(), "many".into()],
        ..ScanCfg::default()
    };
    copy(cfg, &src, &dst);
    let got = digest(&dst);
    assert!(!got.keys().any(|k| k.ends_with(".bin")), "exclude failed");
    assert!(
        !got.keys().any(|k| k.starts_with("many/")),
        "exclude failed"
    );
    assert!(got.contains_key("tiny.txt"));
}

#[test]
fn mtime_is_preserved() {
    let d = TempDir::new("mtime");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"x").unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(src.join("a.txt"))
        .unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
    drop(f);
    let dst = d.sub("dst");
    copy(base_cfg(), &src, &dst);
    let got = std::fs::metadata(dst.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        got.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        1_600_000_000
    );
}

#[test]
fn delta_path_is_exercised() {
    let d = TempDir::new("delta");
    let src = d.sub("src");
    let dst = d.sub("dst");
    std::fs::create_dir_all(&src).unwrap();
    let mut data = pattern(48 << 20, 21);
    std::fs::write(src.join("img.bin"), &data).unwrap();
    copy(base_cfg(), &src, &dst);

    // Change one 1 MiB block in the middle.
    for b in data.iter_mut().skip(20 << 20).take(1 << 20) {
        *b ^= 0x5A;
    }
    std::fs::write(src.join("img.bin"), &data).unwrap();

    let mut cfg = base_cfg();
    cfg.verify = VerifyMode::Hash;
    cfg.delta.min_size = 4 << 20;
    cfg.delta.block = 1 << 20;
    let s = copy(cfg, &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn indexed_delta_rewrites_only_changed_blocks() {
    use sirocco_engine::config::IndexMode;
    let d = TempDir::new("index");
    let src = d.sub("src");
    let dst = d.sub("dst");
    std::fs::create_dir_all(&src).unwrap();
    let mut data = pattern(32 << 20, 3);
    std::fs::write(src.join("img.bin"), &data).unwrap();

    let mut cfg = base_cfg();
    cfg.index = IndexMode::Trust;
    cfg.index_dir = Some(d.sub("index-cache"));
    copy(cfg.clone(), &src, &dst);
    assert_eq!(digest(&src), digest(&dst));

    // Change exactly one block in the middle.
    for b in data.iter_mut().skip(8 << 20).take(1 << 20) {
        *b ^= 0xA5;
    }
    std::fs::write(src.join("img.bin"), &data).unwrap();

    let s = copy(cfg.clone(), &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(
        digest(&src),
        digest(&dst),
        "indexed delta produced wrong data"
    );
    assert!(
        s.stats.bytes_written <= 2 << 20,
        "expected one 1 MiB block to be rewritten, wrote {}",
        s.stats.bytes_written
    );
    // The index must be refreshed, so a third run is still correct.
    for b in data.iter_mut().skip(16 << 20).take(1 << 20) {
        *b ^= 0x5A;
    }
    std::fs::write(src.join("img.bin"), &data).unwrap();
    copy(cfg, &src, &dst);
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn parallel_feeders_produce_the_same_tree() {
    let d = TempDir::new("feeders");
    let src = d.sub("src");
    make_tree(&src);
    let want = digest(&src);
    for n in [1usize, 2, 4, 8] {
        let dst = d.sub(&format!("dst-{n}"));
        let mut cfg = base_cfg();
        cfg.feeders = n;
        let s = copy(cfg, &src, &dst);
        assert_eq!(s.stats.files_failed, 0, "feeders={n}: {:?}", s.errors);
        assert_eq!(digest(&dst), want, "feeders={n} produced a different tree");
    }
}

#[test]
fn readers_and_writers_stay_on_disjoint_cores() {
    use sirocco_sys::affinity;
    let topo = sirocco_sys::Topology::detect();
    let plan = affinity::plan(&topo, 2, 2, None, None);
    if plan.is_empty() {
        return;
    }
    for r in &plan.readers {
        for w in &plan.writers {
            assert_ne!(
                topo.cpus[r.cpu].core, topo.cpus[w.cpu].core,
                "reader and writer share a physical core"
            );
        }
    }
}

#[test]
fn hard_links_are_preserved() {
    let d = TempDir::new("links");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), pattern(4096, 5)).unwrap();
    if std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).is_err() {
        return; // filesystem without hard links
    }
    let dst = d.sub("dst");
    let s = copy(base_cfg(), &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(digest(&src), digest(&dst));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(dst.join("a.bin")).unwrap();
        let b = std::fs::metadata(dst.join("b.bin")).unwrap();
        assert_eq!(a.ino(), b.ino(), "destination files are not linked");
        assert!(a.nlink() >= 2);
    }
    #[cfg(windows)]
    {
        let a = sirocco_sys::file_identity(&dst.join("a.bin")).unwrap();
        let b = sirocco_sys::file_identity(&dst.join("b.bin")).unwrap();
        assert_eq!(a.0, b.0, "destination files are not linked");
        assert!(a.1 >= 2);
    }
}

#[cfg(unix)]
#[test]
fn permissions_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDir::new("perm");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let dst = d.sub("dst");
    copy(base_cfg(), &src, &dst);
    let mode = std::fs::metadata(dst.join("run.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "executable bit was lost");
}

#[test]
fn capabilities_are_reported() {
    let caps = sirocco_sys::caps::init();
    let tier = caps.tier();
    assert!(
        tier == "privileged" || tier == "elevated" || tier == "unprivileged",
        "unexpected tier {tier}"
    );
}

/// `move` over a tree that is already current must still remove the sources.
///
/// Only paths the pipeline reported as finished are deleted, and a skipped file
/// never went through the pipeline — so the first version of this deleted nothing
/// on a second run and left every source behind.
#[test]
fn move_removes_sources_that_were_already_current() {
    let d = TempDir::new("move-skip");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");

    // Copy first, so the destination is already current when the move starts.
    let first = copy(base_cfg(), &src, &dst);
    assert_eq!(first.stats.files_failed, 0, "{:?}", first.errors);

    let mut cfg = base_cfg();
    cfg.mode = Mode::Move;
    let second = copy(cfg, &src, &dst);
    assert_eq!(second.stats.files_failed, 0, "{:?}", second.errors);
    assert!(
        second.stats.files_skipped > 0,
        "expected a fully current tree"
    );

    // `digest` walks the tree; an empty map means every source is gone.
    let leftovers = digest(&src);
    assert!(leftovers.is_empty(), "sources left behind: {leftovers:?}");
}

/// A filtered mirror must not treat the filtered-out paths as extras.
///
/// `delete_extras` compares the destination with the plan, and the plan is
/// already filtered: without a guard, `sync --exclude='*.tmp'` deletes every
/// `.tmp` that was already in the destination.
#[test]
fn delete_extra_refuses_when_filters_are_active() {
    let d = TempDir::new("delete-filtered");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("keep.txt"), b"keep").unwrap();

    let dst = d.sub("dst");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("keep.txt"), b"keep").unwrap();
    std::fs::write(dst.join("stale.tmp"), b"scratch").unwrap();

    let mut cfg = base_cfg();
    cfg.mode = Mode::Sync;
    cfg.scan.exclude.push("*.tmp".into());
    let s = copy(cfg, &src, &dst);

    assert!(
        dst.join("stale.tmp").is_file(),
        "an excluded path was deleted as if it were an extra"
    );
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
}

/// `--follow-symlinks` has to descend into a linked directory.
#[cfg(unix)]
#[test]
fn follow_symlinks_descends_linked_directories() {
    let d = TempDir::new("follow");
    let real = d.sub("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("inner.txt"), b"inner").unwrap();

    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::os::unix::fs::symlink(&real, src.join("linked")).unwrap();

    let dst = d.sub("dst");
    let mut cfg = base_cfg();
    cfg.scan.follow_symlinks = true;
    let s = copy(cfg, &src, &dst);

    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert!(
        dst.join("linked/inner.txt").is_file(),
        "the linked directory was not descended"
    );
}

/// Every file the plan counted must end up in exactly one bucket.
#[test]
fn counters_account_for_every_planned_file() {
    let d = TempDir::new("counters");
    let src = d.sub("src");
    make_tree(&src);
    let dst = d.sub("dst");

    let s = copy(base_cfg(), &src, &dst);
    let settled = s.stats.files_done + s.stats.files_skipped + s.stats.files_failed;
    assert_eq!(settled, s.files_planned, "errors: {:?}", s.errors);
}

/// `--dry-run` on a delete must not delete.
///
/// The delete branch used to be taken before the dry-run branch, so a preview of
/// `rm` removed the tree for real.
#[test]
fn dry_run_delete_changes_nothing() {
    let d = TempDir::new("dry-delete");
    let src = d.sub("src");
    make_tree(&src);
    let before = digest(&src);

    let mut cfg = base_cfg();
    cfg.mode = Mode::Delete;
    cfg.dry_run = true;
    let s = copy(cfg, &src, &d.sub("dst"));

    assert_eq!(s.stats.files_done, 0, "a preview reported deletions");
    assert_eq!(digest(&src), before, "a preview deleted something");
}

/// `--max-depth` counts levels below the root, so N means N levels.
///
/// The old code only gated the descent, which let one extra level through and
/// made every value behave like N+1.
#[test]
fn max_depth_counts_levels_below_the_root() {
    let d = TempDir::new("depth");
    let src = d.sub("src");
    std::fs::create_dir_all(src.join("a/b")).unwrap();
    std::fs::write(src.join("top.txt"), b"top").unwrap();
    std::fs::write(src.join("a/mid.txt"), b"mid").unwrap();
    std::fs::write(src.join("a/b/deep.txt"), b"deep").unwrap();

    let dst = d.sub("dst");
    let mut cfg = base_cfg();
    cfg.scan.max_depth = 1;
    let s = copy(cfg, &src, &dst);

    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert!(dst.join("top.txt").is_file(), "level 1 was not copied");
    assert!(
        !dst.join("a/mid.txt").exists(),
        "level 2 leaked through --max-depth=1"
    );
}

/// A small file the kernel copied still has to keep its timestamp.
///
/// `copy_file_range` moves bytes and nothing else. `keep_times` defaults to on
/// and the skip decision compares mtimes, so a destination without one is
/// re-copied on every run and the incremental sync never converges.
#[test]
fn small_file_keeps_its_timestamp() {
    let d = TempDir::new("small-mtime");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    let path = src.join("small.txt");
    std::fs::write(&path, b"a small file").unwrap();

    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();

    let dst = d.sub("dst");
    let s = copy(base_cfg(), &src, &dst);
    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);

    let got = std::fs::metadata(dst.join("small.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let delta = got
        .duration_since(when)
        .or_else(|_| when.duration_since(got))
        .unwrap();
    assert!(
        delta < std::time::Duration::from_secs(2),
        "destination mtime is {delta:?} away from the source's"
    );
}

/// A directory where the source has a file has to go whole.
///
/// Descending instead deleted the children one at a time and kept the directory,
/// so the copy could never create the file and every run repeated the damage.
#[test]
fn delete_extra_removes_a_directory_that_blocks_a_file() {
    let d = TempDir::new("delete-type");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("thing"), b"file contents").unwrap();

    let dst = d.sub("dst");
    std::fs::create_dir_all(dst.join("thing")).unwrap();
    std::fs::write(dst.join("thing/nested.txt"), b"stale").unwrap();

    let mut cfg = base_cfg();
    cfg.mode = Mode::Sync;
    cfg.delete_extra = true;
    let s = copy(cfg, &src, &dst);

    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert!(
        dst.join("thing").is_file(),
        "the blocking directory survived the mirror"
    );
}

/// A second `move` over an up-to-date hard-link group must not hang.
///
/// Both siblings are skipped, and a skipped file never enters the pipeline — so
/// the second one waited for a `mark_complete` that could not arrive.
#[test]
fn move_over_a_current_hard_link_group_finishes() {
    let d = TempDir::new("link-skip");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), pattern(64 << 10, 7)).unwrap();
    std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();

    let dst = d.sub("dst");
    let first = copy(base_cfg(), &src, &dst);
    assert_eq!(first.stats.files_failed, 0, "{:?}", first.errors);

    let mut cfg = base_cfg();
    cfg.mode = Mode::Move;
    let second = copy(cfg, &src, &dst);

    assert_eq!(second.stats.files_failed, 0, "{:?}", second.errors);
    assert!(
        second.stats.files_skipped >= 2,
        "expected both links to be current, got {} skipped / {} errors",
        second.stats.files_skipped,
        second.errors.len()
    );
}

/// A small file copied with compression and preallocation on must still be the
/// same length as its source.
///
/// The compression path reserves the destination to an alignment boundary instead
/// of its exact size, so the truncate that removes the padded tail has to run for
/// small files too.
#[test]
fn compressed_small_file_is_not_longer_than_its_source() {
    let d = TempDir::new("compress-small");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    // Deliberately not a multiple of any plausible alignment.
    let body = b"compressible text ".repeat(37);
    std::fs::write(src.join("note.txt"), &body).unwrap();

    let dst = d.sub("dst");
    let mut cfg = base_cfg();
    cfg.compress = sirocco_engine::config::CompressMode::Auto;
    cfg.prealloc = PreallocMode::Auto;
    let s = copy(cfg, &src, &dst);

    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    let got = std::fs::metadata(dst.join("note.txt")).unwrap().len();
    assert_eq!(got, body.len() as u64, "destination has a padded tail");
}

/// A read-only source has to survive the kernel fast path.
///
/// The destination inherits the source's mode, so reopening it to apply the
/// metadata with a writable handle is refused — every small read-only file failed
/// with EACCES while the destination was in fact correct.
#[test]
fn read_only_small_file_is_copied() {
    let d = TempDir::new("readonly-small");
    let src = d.sub("src");
    std::fs::create_dir_all(&src).unwrap();
    let path = src.join("ro.txt");
    std::fs::write(&path, b"read only").unwrap();

    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();

    let dst = d.sub("dst");
    let s = copy(base_cfg(), &src, &dst);
    let copied = dst.join("ro.txt");

    // Restore both before asserting: a failed assertion would otherwise leave a
    // file the temporary-directory cleanup cannot remove on Windows.
    for p in [&copied, &path] {
        if let Ok(md) = std::fs::metadata(p) {
            let mut perms = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(p, perms);
        }
    }

    assert_eq!(s.stats.files_failed, 0, "{:?}", s.errors);
    assert_eq!(std::fs::read(&copied).unwrap(), b"read only");
}
