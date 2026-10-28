//! `sirocco` command line entry point.

// The same domain exceptions as the engine, for the same reasons: byte and index
// arithmetic on the reporting path, helpers that exist to name a step, and a
// handful of `unwrap`s on values the preceding line just produced.
#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::indexing_slicing)]
#![allow(clippy::single_call_fn)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::too_many_lines)]
// The split runner creates the shared-memory segments its children publish into,
// and zeroing a freshly created segment is a raw write by definition. There is no
// safe wrapper that would be cheaper: the mapping is the interface.
#![allow(unsafe_code)]

mod args;

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use args::Cli;
use args::Cmd;
use args::HELP;
use sirocco_core::hash::xxh64;
use sirocco_core::human::format_bytes;
use sirocco_core::human::format_duration;
use sirocco_core::human::format_rate;
use sirocco_core::plan::EntryKind;
use sirocco_engine::EngineConfig;
use sirocco_engine::Summary;
use sirocco_engine::session;
use sirocco_sys::Access;
use sirocco_sys::Create;
use sirocco_sys::OpenOpts;
use sirocco_sys::RawFile;
use sirocco_sys::Topology;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        print!("{HELP}");
        return ExitCode::from(2);
    }
    let cli = match args::parse(&argv) {
        | Ok(c) => c,
        | Err(e) => {
            eprintln!("sirocco: {}", e.0);
            eprintln!("try `sirocco --help`");
            return ExitCode::from(2);
        },
    };
    match run(&cli) {
        | Ok(code) => code,
        | Err(e) => {
            eprintln!("sirocco: {e}");
            ExitCode::from(1)
        },
    }
}

fn run(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.cmd {
        | Cmd::Help => {
            print!("{HELP}");
            Ok(ExitCode::SUCCESS)
        },
        | Cmd::Version => {
            println!("sirocco {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        },
        | Cmd::Info => cmd_info(cli),
        | Cmd::Bench => cmd_bench(cli),
        | Cmd::Verify => cmd_verify(cli),
        | Cmd::Copy | Cmd::Move | Cmd::Sync | Cmd::Delete => cmd_transfer(cli),
    }
}

// ------------------------------------------------------------------ transfer

fn cmd_transfer(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if cli.cfg.procs > 1 && cli.srcs.len() == 1 && cli.dst.is_some() {
        // Each worker only sees its own manifest, so its "extras" are everybody
        // else's output. Mirroring is a whole-tree decision; keep it in one process.
        if cli.cfg.mode == sirocco_core::plan::Mode::Sync && cli.cfg.delete_extra {
            eprintln!(
                "sirocco: --procs cannot be combined with --delete-extra; running single-process"
            );
        } else {
            return cmd_split(cli);
        }
    }
    let mut cfg = cli.cfg.clone();
    if cli.privileged {
        apply_privileged(&mut cfg);
    }
    if cli.calibrate
        && let Some(d) = cli.dst.as_deref()
    {
        match session::calibrate(d) {
            | Ok(p) => {
                eprintln!(
                    "sirocco: calibrated read {} write {} dest-read {}",
                    format_rate(p.read_bps),
                    format_rate(p.write_bps),
                    format_rate(p.dest_read_bps)
                );
                // The whole point of calibrating is to feed the cost model;
                // measuring and then discarding it made --calibrate useless.
                cfg.io_profile = Some(p);
            },
            | Err(e) => eprintln!("sirocco: calibration failed: {e}"),
        }
    }

    let dst = cli.dst.clone().unwrap_or_default();
    let mut failures = 0u32;

    for src in &cli.srcs {
        if !src.exists() {
            eprintln!("sirocco: {} does not exist", src.display());
            failures += 1;
            continue;
        }
        // Multiple sources always land inside the destination directory. `--into`
        // is handled by the engine, which owns the source/destination pair.
        let nest = cli.srcs.len() > 1;
        let target = if nest {
            let _ = std::fs::create_dir_all(&dst);
            dst.join(src.file_name().unwrap_or_default())
        } else {
            dst.clone()
        };

        match session::run(cfg.clone(), src.clone(), target) {
            | Ok(s) => {
                if cli.json {
                    print_json(&s);
                } else if !cfg.quiet {
                    session::print_summary(&s);
                }
                if s.stats.files_failed > 0 || !s.errors.is_empty() || s.interrupted {
                    failures += 1;
                }
                if s.interrupted {
                    // Stop feeding further sources; the user asked us to stop.
                    break;
                }
            },
            | Err(e) => {
                eprintln!("sirocco: {e}");
                failures += 1;
            },
        }
    }

    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Coarse-grained multi-process split.
///
/// One process per *group of subtrees*, not per file: several processes working in
/// the same directory contend on the NTFS directory lock and on the same device
/// queue, which costs more than the extra parallelism buys. The parent partitions
/// by bytes while keeping sibling files together, creates the directories once,
/// then hands each worker a manifest.
fn cmd_split(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let src = cli.srcs[0].clone();
    let dst = cli.dst.clone().unwrap();
    let n = cli.cfg.procs.clamp(2, 64);

    let mut cfg = cli.cfg.clone();
    cfg.normalize_with(
        sirocco_sys::Topology::detect().physical_count(),
        sirocco_sys::Topology::detect().logical_count(),
    );
    let (groups, dirs, total) = session::partition(&src, &dst, &cfg, n)?;
    if !cfg.dry_run {
        for d in &dirs {
            let _ = std::fs::create_dir_all(d);
        }
    }
    let active: Vec<&Vec<std::path::PathBuf>> = groups.iter().filter(|g| !g.is_empty()).collect();
    if active.is_empty() {
        eprintln!("sirocco: nothing to split");
        return Ok(ExitCode::SUCCESS);
    }

    let tmp = std::env::temp_dir().join(format!("sirocco-split-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let exe = std::env::current_exe()?;
    let t0 = Instant::now();

    // One shared segment per worker. The child's counters live *inside* the
    // mapping, so every update it already performs lands in memory we can read —
    // no pipe, no socket, and no publisher thread anywhere.
    let mut segments = Vec::with_capacity(active.len());
    for i in 0..active.len() {
        let path = tmp.join(format!("stats-{i}.shm"));
        let shm = sirocco_sys::shm::Shm::create(&path, sirocco_core::stats::Counters::SIZE)?;
        // SAFETY: the segment is exactly one Counters block, freshly created.
        unsafe {
            std::ptr::write_bytes(shm.as_ptr::<u8>(), 0, sirocco_core::stats::Counters::SIZE);
        }
        segments.push(shm);
    }

    let mut children = Vec::new();
    for (i, group) in active.iter().enumerate() {
        let manifest = tmp.join(format!("part-{i}.txt"));
        let mut body = String::new();
        for rel in *group {
            body.push_str(
                &rel.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
            body.push('\n');
        }
        std::fs::write(&manifest, body)?;

        let mut args: Vec<String> = cli.raw.clone();
        // The child must not split again, must report JSON, and must stay quiet so
        // its output does not interleave with the others.
        args.push("--procs=1".into());
        args.push("--json".into());
        args.push("-q".into());
        args.push(format!("--files-from={}", manifest.display()));
        args.push(format!(
            "--shm={}",
            tmp.join(format!("stats-{i}.shm")).display()
        ));

        // The numbers come from shared memory, so the child's stdout is not read
        // until after it exits — piping it would deadlock as soon as a worker
        // printed more than the pipe buffer (a long error list does exactly that).
        let child = std::process::Command::new(&exe)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        children.push((i, child, group.len()));
    }

    if !cli.cfg.quiet {
        eprintln!(
            "sirocco: split into {} processes ({} files)",
            children.len(),
            active.iter().map(|g| g.len()).sum::<usize>()
        );
    }

    // --- live aggregate progress -----------------------------------------
    let total_bytes = total;
    let show = !cli.cfg.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut last = Instant::now();
    let mut last_bytes = 0u64;
    loop {
        let mut running = 0;
        for (_, child, _) in &mut children {
            if child.try_wait()?.is_none() {
                running += 1;
            }
        }
        if show {
            let snap = sum_snapshots(&segments);
            let done = snap.bytes_written + snap.bytes_holes;
            let pct = if total_bytes == 0 {
                100.0
            } else {
                (done as f64 / total_bytes as f64 * 100.0).min(100.0)
            };
            let dt = last.elapsed().as_secs_f64().max(1e-9);
            let rate = (done.saturating_sub(last_bytes)) as f64 / dt;
            last = Instant::now();
            last_bytes = done;
            let mut err = std::io::stderr();
            let _ = write!(
                err,
                "
  {:5.1}%  {} / {}  {}/s  {} files  {} procs   ",
                pct,
                format_bytes(done),
                format_bytes(total_bytes),
                format_rate(rate),
                snap.files_done + snap.files_skipped,
                running
            );
            let _ = err.flush();
        }
        if running == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if show {
        let _ = writeln!(std::io::stderr());
    }

    let mut files = 0u64;
    let mut failed = 0u64;
    let mut written = 0u64;
    let mut read = 0u64;
    let mut bad = 0;
    for (i, mut child, _) in children {
        let status = child.wait()?;
        if !status.success() {
            bad += 1;
            eprintln!("sirocco: worker {i} failed ({status})");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // The shared counters are the ground truth; fall back to the JSON summaries if
    // a worker never published (it would have to have crashed before starting).
    let published = sum_snapshots(&segments);
    if published.files_done > 0 {
        files = published.files_done;
        failed = published.files_failed;
        written = published.bytes_written;
        read = published.bytes_read;
    }

    let el = t0.elapsed().as_secs_f64().max(1e-9);
    println!();
    println!("sirocco split summary ({} processes)", active.len());
    println!("  files        : {files} copied, {failed} failed");
    println!(
        "  data         : {} written, {} read",
        format_bytes(written),
        format_bytes(read)
    );
    println!("  elapsed      : {}", format_duration(el));
    println!("  throughput   : {}", format_rate(written as f64 / el));

    Ok(if bad == 0 && failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Sum the counters published by every worker.
fn sum_snapshots(segments: &[sirocco_sys::shm::Shm]) -> sirocco_core::stats::Snapshot {
    let mut total = sirocco_core::stats::Snapshot::default();
    for shm in segments {
        // SAFETY: each segment is one Counters block created by us.
        let c: &sirocco_core::stats::Counters = unsafe { &*shm.as_ptr() };
        if !c.is_valid() {
            continue;
        }
        let s = c.snapshot();
        total.files_done += s.files_done;
        total.files_skipped += s.files_skipped;
        total.files_failed += s.files_failed;
        total.bytes_written += s.bytes_written;
        total.bytes_read += s.bytes_read;
        total.bytes_holes += s.bytes_holes;
        total.bytes_saved += s.bytes_saved;
    }
    total
}

fn apply_privileged(cfg: &mut EngineConfig) {
    let wanted = [
        sirocco_sys::privilege_names::MANAGE_VOLUME,
        sirocco_sys::privilege_names::LOCK_MEMORY,
        sirocco_sys::privilege_names::BACKUP,
        sirocco_sys::privilege_names::RESTORE,
    ];
    match sirocco_sys::set_privileges(&wanted) {
        | Ok(held) => {
            if !cfg.quiet {
                eprintln!("sirocco: privileges held: [{}]", held.join(", "));
            }
            if held.iter().any(|p| p.contains("LockMemory")) {
                cfg.huge_pages = true;
            }
        },
        | Err(e) => eprintln!("sirocco: cannot adjust privileges: {e}"),
    }
}

fn print_json(s: &Summary) {
    let d = s.stats;
    println!("{{");
    println!("  \"mode\": \"{}\",", s.mode.name());
    println!("  \"backend\": \"{}\",", s.backend);
    println!("  \"align\": {},", s.align);
    println!("  \"files_planned\": {},", s.files_planned);
    println!("  \"bytes_planned\": {},", s.bytes_planned);
    println!("  \"files_copied\": {},", d.files_done);
    println!("  \"files_skipped\": {},", d.files_skipped);
    println!("  \"files_failed\": {},", d.files_failed);
    println!("  \"bytes_read\": {},", d.bytes_read);
    println!("  \"bytes_written\": {},", d.bytes_written);
    println!("  \"bytes_verified\": {},", d.bytes_verified);
    println!("  \"bytes_saved\": {},", d.bytes_saved);
    println!("  \"bytes_holes\": {},", d.bytes_holes);
    println!("  \"elapsed_secs\": {:.3},", d.elapsed_secs);
    println!("  \"write_bps\": {:.0},", d.write_rate());
    println!("  \"read_bps\": {:.0},", d.read_rate());
    println!("  \"errors\": [");
    for (i, e) in s.errors.iter().enumerate() {
        let comma = if i + 1 == s.errors.len() {
            ""
        } else {
            ","
        };
        println!("    \"{}\"{}", escape(e), comma);
    }
    println!("  ]");
    println!("}}");
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ------------------------------------------------------------------ info

// Keeps the same signature as every other `cmd_*` so the dispatch stays uniform;
// `info` happens to be infallible.
#[allow(clippy::unnecessary_wraps)]
fn cmd_info(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = cli
        .srcs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let vol = sirocco_sys::volume_info(&path);
    let topo = Topology::detect();

    println!("sirocco {} — system information", env!("CARGO_PKG_VERSION"));
    println!("  path          : {}", path.display());
    match &vol {
        | Ok(v) => {
            println!("  volume        : {}", v.root.display());
            println!("  filesystem    : {}", v.fs);
            println!(
                "  sector        : logical {} B, physical {} B, cluster {} B",
                v.logical_sector, v.physical_sector, v.cluster
            );
            println!(
                "  media         : {}",
                if v.rotational {
                    "rotational (HDD)"
                } else {
                    "solid state"
                }
            );
            println!(
                "  capacity      : {} free of {}",
                format_bytes(v.free_bytes),
                format_bytes(v.total_bytes)
            );
        },
        | Err(e) => println!("  volume        : unavailable ({e})"),
    }
    println!(
        "  cpus          : {} logical, {} physical cores, {} NUMA nodes",
        topo.logical_count(),
        topo.physical_count(),
        topo.nodes.len()
    );
    for (i, node) in topo.nodes.iter().enumerate() {
        println!("    node {i}: {} cpus", node.len());
    }

    let mut cfg = cli.cfg.clone();
    cfg.normalize_with(topo.physical_count(), topo.logical_count());
    println!(
        "  buffer plan   : {} × {} = {}",
        cfg.buffer_count(),
        format_bytes(cfg.buffer_size as u64),
        format_bytes(cfg.pool_bytes() as u64)
    );
    println!(
        "  threads       : {} readers + {} writers, overlap {}",
        cfg.readers, cfg.writers, cfg.overlap
    );
    println!("  direct i/o    : {}", cfg.direct_io);
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ bench

fn cmd_bench(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let dir = cli.srcs.first().cloned().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir)?;
    let src = dir.join(".sirocco-bench-src.bin");
    let dst = dir.join(".sirocco-bench-dst.bin");
    let size = cli.bench_size;

    eprintln!("sirocco bench: generating {} ...", format_bytes(size));
    write_pattern(&src, size)?;

    let mut cfg = cli.cfg.clone();
    if cli.privileged {
        apply_privileged(&mut cfg);
    }
    cfg.verify = sirocco_core::plan::VerifyMode::None;
    cfg.quiet = true;
    let t = sirocco_sys::Topology::detect();
    cfg.normalize_with(t.physical_count(), t.logical_count());

    eprintln!("sirocco bench: copying ...");
    let t0 = Instant::now();
    let sum = session::run(cfg.clone(), src.clone(), dst.clone())?;
    let el = t0.elapsed().as_secs_f64().max(1e-9);
    let bytes = sum.stats.bytes_written.max(1);

    println!();
    println!("sirocco bench — {} payload", format_bytes(size));
    println!("  engine        : {}", sum.backend);
    println!(
        "  buffers       : {} × {}",
        cfg.buffer_count(),
        format_bytes(cfg.buffer_size as u64)
    );
    println!(
        "  threads       : {} readers + {} writers, overlap {}",
        cfg.readers, cfg.writers, cfg.overlap
    );
    println!("  elapsed       : {}", format_duration(el));
    println!("  throughput    : {} copy", format_rate(bytes as f64 / el));
    println!(
        "  i/o           : {} written, {} read",
        format_bytes(sum.stats.bytes_written),
        format_bytes(sum.stats.bytes_read)
    );
    if !sum.errors.is_empty() {
        println!("  errors        : {}", sum.errors.len());
        for e in sum.errors.iter().take(5) {
            println!("    ! {e}");
        }
    }

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    Ok(ExitCode::SUCCESS)
}

fn write_pattern(
    path: &Path,
    size: u64,
) -> std::io::Result<()> {
    let f = RawFile::open(
        path,
        &OpenOpts {
            access: Access::Write,
            create: Create::Create,
            ..Default::default()
        },
    )?;
    let mut buf = vec![0u8; 1 << 20];
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for b in &mut buf {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = s as u8;
    }
    // Mix in compressible regions so the benchmark is not pure noise.
    for (i, b) in buf.iter_mut().enumerate() {
        if (i / 4096) % 3 == 0 {
            *b = (i % 7) as u8;
        }
    }
    let mut off = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(buf.len());
        f.pwrite(&buf[..n], off)?;
        off += n as u64;
    }
    f.flush()?;
    Ok(())
}

// ------------------------------------------------------------------ verify

fn cmd_verify(cli: &Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let src = &cli.srcs[0];
    let dst = cli.dst.as_ref().unwrap();
    let topo = Topology::detect();
    let mut cfg = cli.cfg.clone();
    cfg.normalize_with(topo.physical_count(), topo.logical_count());
    let plan = sirocco_engine::scan::scan(src, dst, &cfg.scan, topo.logical_count())?;

    let mut checked = 0u64;
    let mut bad = 0u64;
    let mut missing = 0u64;
    let mut bytes = 0u64;
    let t0 = Instant::now();

    for e in &plan.entries {
        if e.kind != EntryKind::File {
            continue;
        }
        if !e.dst.exists() {
            println!("missing: {}", e.dst.display());
            missing += 1;
            continue;
        }
        match hash_file(&e.src, cfg.buffer_size) {
            | Ok((ha, na)) => {
                match hash_file(&e.dst, cfg.buffer_size) {
                    | Ok((hb, nb)) => {
                        checked += 1;
                        bytes += na;
                        if ha != hb || na != nb {
                            println!("differ: {}", e.rel.display());
                            bad += 1;
                        }
                    },
                    | Err(err) => {
                        println!("unreadable: {} ({err})", e.dst.display());
                        bad += 1;
                    },
                }
            },
            | Err(err) => {
                println!("unreadable: {} ({err})", e.src.display());
                bad += 1;
            },
        }
    }

    let el = t0.elapsed().as_secs_f64().max(1e-9);
    println!();
    println!(
        "verified {checked} files ({}), {bad} differ, {missing} missing, {} in {}",
        format_bytes(bytes),
        format_rate(bytes as f64 / el),
        format_duration(el)
    );
    Ok(if bad == 0 && missing == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn hash_file(
    path: &Path,
    buf_size: usize,
) -> std::io::Result<(u64, u64)> {
    let f = RawFile::open(
        path,
        &OpenOpts {
            access: Access::Read,
            sequential: true,
            ..Default::default()
        },
    )?;
    let mut buf = vec![0u8; buf_size.max(4096)];
    let mut h = 0u64;
    let mut off = 0u64;
    loop {
        let n = f.pread(&mut buf, off)?;
        if n == 0 {
            break;
        }
        h ^= xxh64(&buf[..n]).rotate_left(17) ^ off;
        off += n as u64;
    }
    Ok((h, off))
}
