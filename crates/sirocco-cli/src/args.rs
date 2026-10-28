//! Command line parsing.
//!
//! Hand-rolled rather than pulled from a crate: the option set is large, the
//! syntax is regular, and this way `--buffer=1M` can accept the same size suffixes
//! everywhere without an extra dependency.

use std::path::PathBuf;

use sirocco_core::delta::DeltaCfg;
use sirocco_core::hash::HashKind;
use sirocco_core::human::parse_size;
use sirocco_core::plan::Mode;
use sirocco_core::plan::Order;
use sirocco_core::plan::Overwrite;
use sirocco_core::plan::VerifyMode;
use sirocco_engine::EngineConfig;
use sirocco_engine::config::CompressMode;
use sirocco_engine::config::IndexMode;
use sirocco_engine::config::NumaMode;
use sirocco_engine::config::PreallocMode;
use sirocco_engine::config::SmallCopy;
use sirocco_sys::ReactorKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Copy,
    Move,
    Sync,
    Delete,
    Bench,
    Info,
    Verify,
    Help,
    Version,
}

pub struct Cli {
    pub cmd: Cmd,
    /// The raw argument vector, so a split run can re-execute itself.
    pub raw: Vec<String>,
    pub srcs: Vec<PathBuf>,
    pub dst: Option<PathBuf>,
    pub cfg: EngineConfig,
    pub json: bool,
    pub privileged: bool,
    pub calibrate: bool,
    pub bench_size: u64,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            cmd: Cmd::Copy,
            raw: Vec::new(),
            srcs: Vec::new(),
            dst: None,
            cfg: EngineConfig::default(),
            json: false,
            privileged: false,
            calibrate: false,
            bench_size: 512 << 20,
        }
    }
}

pub struct ParseError(pub String);

type R<T> = Result<T, ParseError>;

fn size_of(
    v: &str,
    what: &str,
) -> R<u64> {
    parse_size(v).ok_or_else(|| ParseError(format!("invalid size for {what}: {v}")))
}

/// Parse `argv[1..]`.
///
/// `a` is the current argument, `c` the partially built config, `i` the cursor,
/// `p` the positionals and `n` the token count — short names because the loop
/// body uses them a dozen times per screen.
#[allow(clippy::many_single_char_names)]
pub fn parse(argv: &[String]) -> R<Cli> {
    let mut c = Cli {
        raw: argv.to_vec(),
        ..Cli::default()
    };
    let mut i = 0usize;
    let mut positional: Vec<String> = Vec::new();
    let mut cmd_set = false;

    while i < argv.len() {
        let a = argv[i].as_str();
        i += 1;

        if a == "--" {
            positional.extend(argv[i..].iter().cloned());
            break;
        }
        if !a.starts_with('-') {
            if cmd_set {
                positional.push(a.to_string());
            } else {
                c.cmd = match a {
                    | "copy" | "cp" => Cmd::Copy,
                    | "move" | "mv" => Cmd::Move,
                    | "sync" | "mirror" => Cmd::Sync,
                    | "delete" | "rm" | "del" => Cmd::Delete,
                    | "bench" => Cmd::Bench,
                    | "info" => Cmd::Info,
                    | "verify" | "diff" => Cmd::Verify,
                    | "help" => Cmd::Help,
                    | "version" => Cmd::Version,
                    | _ => {
                        // No explicit command: treat as `copy`.
                        positional.push(a.to_string());
                        cmd_set = true;
                        continue;
                    },
                };
                cmd_set = true;
            }
            continue;
        }

        // Split `--key=value`.
        let (key, inline) = match a.split_once('=') {
            | Some((k, v)) => (k, Some(v)),
            | None => (a, None),
        };
        let mut val = |what: &str| -> R<String> {
            if let Some(v) = inline {
                return Ok(v.to_string());
            }
            if i < argv.len() {
                let v = argv[i].clone();
                i += 1;
                Ok(v)
            } else {
                Err(ParseError(format!("{what} needs a value")))
            }
        };

        match key {
            | "-h" | "--help" => c.cmd = Cmd::Help,
            | "-V" | "--version" => c.cmd = Cmd::Version,
            | "-n" | "--dry-run" => c.cfg.dry_run = true,
            | "-f" | "--force" => c.cfg.overwrite = Overwrite::Always,
            | "-q" | "--quiet" => {
                c.cfg.quiet = true;
                c.cfg.verbose = 0;
            },
            | "-v" | "--verbose" => c.cfg.verbose = (c.cfg.verbose + 1).min(3),
            | "-r" | "--recursive" => {},
            | "-p" | "--preserve" => {
                c.cfg.keep_times = true;
                c.cfg.preserve_permissions = true;
            },
            | "--json" => c.json = true,
            | "--privileged" => c.privileged = true,
            | "--calibrate" => c.calibrate = true,
            | "--no-progress" => c.cfg.quiet = true,

            | "--buffer" => c.cfg.buffer_size = size_of(&val("--buffer")?, "--buffer")? as usize,
            | "--io-unit" | "--io-unit-size" => {
                c.cfg.io_unit = size_of(&val("--io-unit")?, "--io-unit")?;
            },
            | "--overlap" => {
                c.cfg.overlap = val("--overlap")?
                    .parse()
                    .map_err(|_| ParseError("--overlap expects a number".into()))?;
            },
            | "--readers" => {
                c.cfg.readers = val("--readers")?
                    .parse()
                    .map_err(|_| ParseError("--readers expects a number".into()))?;
            },
            | "--writers" => {
                c.cfg.writers = val("--writers")?
                    .parse()
                    .map_err(|_| ParseError("--writers expects a number".into()))?;
            },
            | "--threads" | "-j" => {
                let n: usize = val("--threads")?
                    .parse()
                    .map_err(|_| ParseError("--threads expects a number".into()))?;
                c.cfg.readers = (n / 2).max(1);
                c.cfg.writers = (n - c.cfg.readers).max(1);
            },
            | "--queue" => {
                c.cfg.queue_depth = val("--queue")?
                    .parse()
                    .map_err(|_| ParseError("--queue expects a number".into()))?;
            },

            | "--no-direct" => c.cfg.direct_io = false,
            | "--direct" => c.cfg.direct_io = true,
            | "--huge-pages" | "--hugepages" => c.cfg.huge_pages = true,
            | "--no-huge-pages" => {
                c.cfg.no_huge_pages = true;
                c.cfg.huge_pages = false;
            },
            | "--no-prealloc" => c.cfg.prealloc = PreallocMode::Off,
            | "--prealloc" => {
                let v = val("--prealloc")?;
                c.cfg.prealloc = PreallocMode::parses(&v)
                    .ok_or_else(|| ParseError(format!("unknown prealloc mode: {v}")))?;
            },
            | "--sparse" => c.cfg.sparse = true,
            | "--lcn-order" => c.cfg.reorder_lcn = true,
            | "--no-lcn-order" => c.cfg.reorder_lcn = false,
            | "--acl" => c.cfg.preserve_acl = true,
            | "--procs" => {
                c.cfg.procs = val("--procs")?
                    .parse()
                    .map_err(|_| ParseError("--procs expects a number".into()))?;
            },
            | "--files-from" => c.cfg.file_list = Some(PathBuf::from(val("--files-from")?)),
            | "--shm" => c.cfg.shm = Some(PathBuf::from(val("--shm")?)),
            | "--no-acl" => c.cfg.preserve_acl = false,
            | "--sync" => c.cfg.sync_at_end = true,

            | "--verify" => {
                let v = val("--verify")?;
                c.cfg.verify = VerifyMode::parses(&v)
                    .ok_or_else(|| ParseError(format!("unknown verify mode: {v}")))?;
            },
            | "--hash" => {
                let v = val("--hash")?;
                c.cfg.hash = match v.as_str() {
                    | "xxh64" | "xxh" => HashKind::Xxh64,
                    | "crc32c" | "crc" => HashKind::Crc32c,
                    | _ => return Err(ParseError(format!("unknown hash: {v}"))),
                };
            },
            | "--overwrite" | "--if" => {
                let v = val("--overwrite")?;
                c.cfg.overwrite = Overwrite::parses(&v)
                    .ok_or_else(|| ParseError(format!("unknown overwrite policy: {v}")))?;
            },
            | "--order" => {
                let v = val("--order")?;
                c.cfg.scan.order =
                    Order::parses(&v).ok_or_else(|| ParseError(format!("unknown order: {v}")))?;
            },
            | "--exclude" | "-x" => c.cfg.scan.exclude.push(val("--exclude")?),
            | "--exclude-from" => {
                let p = val("--exclude-from")?;
                c.cfg.scan.exclude.extend(read_patterns(&p)?);
            },
            | "--include-from" => {
                let p = val("--include-from")?;
                c.cfg.scan.include.extend(read_patterns(&p)?);
            },
            | "--include" => c.cfg.scan.include.push(val("--include")?),
            | "--max-depth" => {
                c.cfg.scan.max_depth = val("--max-depth")?
                    .parse()
                    .map_err(|_| ParseError("--max-depth expects a number".into()))?;
            },
            | "--follow-symlinks" | "-L" => c.cfg.scan.follow_symlinks = true,
            | "--one-filesystem" | "-X" => c.cfg.scan.one_filesystem = true,

            | "--compress" => {
                let v = val("--compress")?;
                c.cfg.compress = CompressMode::parses(&v)
                    .ok_or_else(|| ParseError(format!("unknown compress mode: {v}")))?;
            },
            | "--delta" => c.cfg.delta_enabled = true,
            | "--no-delta" => c.cfg.delta_enabled = false,
            | "--delta-block" => {
                c.cfg.delta.block = size_of(&val("--delta-block")?, "--delta-block")?;
            },
            | "--delta-min" => {
                c.cfg.delta.min_size = size_of(&val("--delta-min")?, "--delta-min")?;
            },
            | "--probes" => {
                c.cfg.delta.probes = val("--probes")?
                    .parse()
                    .map_err(|_| ParseError("--probes expects a number".into()))?;
            },
            | "--small-copy" => {
                let v = val("--small-copy")?;
                c.cfg.small_copy = SmallCopy::parses(&v)
                    .ok_or_else(|| ParseError(format!("unknown small-copy mode: {v}")))?;
            },
            | "--small" => {
                c.cfg.small_threshold = size_of(&val("--small")?, "--small")?;
            },
            | "--small-batch" => {
                c.cfg.small_batch = val("--small-batch")?
                    .parse()
                    .map_err(|_| ParseError("--small-batch expects a number".into()))?;
            },

            | "--pin" => c.cfg.pin_threads = true,
            | "--feeders" => {
                c.cfg.feeders = val("--feeders")?
                    .parse()
                    .map_err(|_| ParseError("--feeders expects a number".into()))?;
            },
            | "--cpu-list" => {
                let v = val("--cpu-list")?;
                c.cfg.cpu_list = Some(
                    sirocco_sys::affinity::parse_cpu_list(&v)
                        .ok_or_else(|| ParseError(format!("invalid --cpu-list: {v}")))?,
                );
                c.cfg.pin_threads = true;
            },
            | "--numa" => {
                let v = val("--numa")?;
                c.cfg.numa = match v.as_str() {
                    | "auto" => NumaMode::Auto,
                    | "off" | "none" => NumaMode::Off,
                    | other => {
                        NumaMode::Node(
                            other
                                .parse()
                                .map_err(|_| ParseError(format!("invalid --numa node: {other}")))?,
                        )
                    },
                };
            },
            | "--no-numa" => c.cfg.numa = NumaMode::Off,
            | "--io-priority" => {
                let v = val("--io-priority")?;
                c.cfg.io_priority = Some(
                    sirocco_sys::io_priority_from_name(&v)
                        .ok_or_else(|| ParseError(format!("unknown io priority: {v}")))?,
                );
            },
            | "--process-priority" => {
                let v = val("--process-priority")?;
                c.cfg.process_priority = Some(
                    sirocco_sys::process_priority_from_name(&v)
                        .ok_or_else(|| ParseError(format!("unknown process priority: {v}")))?,
                );
            },
            | "--thread-priority" => {
                let v = val("--thread-priority")?;
                c.cfg.thread_priority = Some(
                    sirocco_sys::thread_priority_from_name(&v)
                        .ok_or_else(|| ParseError(format!("unknown thread priority: {v}")))?,
                );
            },
            | "--engine" => {
                let v = val("--engine")?;
                c.cfg.reactor = match v.as_str() {
                    | "native" | "auto" => ReactorKind::Native,
                    | "blocking" | "threads" => ReactorKind::Blocking,
                    | _ => return Err(ParseError(format!("unknown engine: {v}"))),
                };
            },
            | "--links" | "-H" => c.cfg.preserve_links = true,
            | "--no-links" => c.cfg.preserve_links = false,
            | "--xattrs" | "-A" => c.cfg.preserve_xattrs = true,
            | "--delete-extra" => c.cfg.delete_extra = true,
            | "--journal" => c.cfg.journal = true,
            | "--index" => {
                // `--index` on its own means "trust the recorded metadata".
                let v = inline.unwrap_or("trust");
                c.cfg.index = IndexMode::parses(v)
                    .ok_or_else(|| ParseError(format!("unknown index mode: {v}")))?;
            },
            | "--no-index" => c.cfg.index = IndexMode::Off,
            | "--index-dir" => c.cfg.index_dir = Some(PathBuf::from(val("--index-dir")?)),
            | "--into" => c.cfg.into = true,
            | "--size" => c.bench_size = size_of(&val("--size")?, "--size")?,

            | _ => return Err(ParseError(format!("unknown option: {key}"))),
        }
    }

    // Positional handling.
    match c.cmd {
        | Cmd::Help | Cmd::Version => {},
        | Cmd::Delete | Cmd::Info | Cmd::Bench => {
            c.srcs = positional.into_iter().map(PathBuf::from).collect();
        },
        | Cmd::Verify => {
            if positional.len() != 2 {
                return Err(ParseError("verify needs <src> <dst>".into()));
            }
            c.srcs = vec![PathBuf::from(&positional[0])];
            c.dst = Some(PathBuf::from(&positional[1]));
        },
        | _ => {
            if positional.len() < 2 {
                return Err(ParseError(
                    "expected <source>... <destination>; see --help".into(),
                ));
            }
            let mut v = positional;
            let dst = PathBuf::from(v.pop().unwrap());
            c.srcs = v.into_iter().map(PathBuf::from).collect();
            c.dst = Some(dst);
        },
    }

    if c.cfg.verbose > 1 && c.cfg.delta.probes == DeltaCfg::default().probes {
        // keep default
    }
    c.cfg.mode = match c.cmd {
        | Cmd::Move => Mode::Move,
        | Cmd::Sync => Mode::Sync,
        | Cmd::Delete => Mode::Delete,
        | _ => Mode::Copy,
    };
    if c.cmd == Cmd::Sync {
        c.cfg.delete_extra = true;
    }
    Ok(c)
}

fn read_patterns(path: &str) -> R<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| ParseError(format!("{path}: {e}")))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(std::string::ToString::to_string)
        .collect())
}

pub const HELP: &str = r"sirocco — fast, low-level file copy engine

USAGE
  sirocco <command> [options] <source>... <destination>

COMMANDS
  copy   <src>... <dst>     copy files or trees
  move   <src>... <dst>     move (rename when on the same volume)
  sync   <src> <dst>        mirror: copy changes, delete extras
  delete <path>...          delete files or trees
  verify <src> <dst>        compare two trees by content
  bench  [dir]              end-to-end throughput benchmark
  info   [path]             show volume, device and topology information

I/O SHAPE
  --buffer=SIZE             pooled buffer size (default 1M)
  --io-unit=SIZE            max bytes per request (default 1M, must divide --buffer)
  --overlap=N               outstanding requests per thread (default 8)
  --readers=N               reader threads (default: half the physical cores)
  --writers=N               writer threads (default: the rest)
  --threads=N, -j N         split N threads between readers and writers
  --queue=N                 pipeline queue depth
  --feeders=N               scheduler threads feeding the pipeline
  --cpu-list=LIST           pin to these logical cpus (0-3,8-11)
  --numa=auto|off|N         NUMA policy for buffers and threads
  --pin                     pin reader/writer threads to disjoint cores
  --engine=native|blocking  IOCP / io_uring, or a portable thread pool
  --no-direct               disable O_DIRECT / FILE_FLAG_NO_BUFFERING
  --small-copy=MODE         auto (default), kernel, io: who copies small files
  --huge-pages              back the buffer pool with 2 MiB pages
  --no-huge-pages           never use large pages, even when permitted
  --prealloc=MODE           auto (default), valid, sparse, off
  --sparse                  punch holes for all-zero and unallocated regions
  --lcn-order / --no-lcn-order
                            order a file's chunks by physical cluster (HDD)
  --acl / --no-acl          copy the DACL / POSIX ACL
  --xattrs, -A              copy extended attributes (Linux)
  --procs=N                 split the job across N processes, by subtree
  --shm=PATH                publish progress counters to a shared segment
  --files-from=FILE         copy only the paths listed (one per line)
  --journal                 write a resume log, so a rerun skips finished files
  --sync                    flush every destination file at the end

POLICY
  --overwrite=POLICY        if-different (default), never, always, if-newer,
                            if-older, if-size, if-content
  -f, --force               same as --overwrite=always
  -n, --dry-run             show what would happen
  --verify=MODE             none (default), size, size-mtime, hash, sample
  --hash=ALGO               xxh64 (default) or crc32c
  --compress=MODE           off (default), auto, force
  --delta / --no-delta      enable or disable incremental updates
  --delta-block=SIZE        delta comparison granularity (default 1M)
  --delta-min=SIZE          smallest file worth probing (default 8M)
  --index[=trust|verify]    persist block hashes so a re-sync skips reading
                            the source (verify also validates the index)
  --index-dir=PATH          where the block index lives
  --order=ORDER             walk (default), size-asc, size-desc, physical, name
  --exclude=GLOB, -x GLOB   skip matching paths (repeatable)
  --exclude-from=FILE       read exclude patterns from a file
  --include-from=FILE       read include patterns from a file
  --into                    copy the source directory itself into <dst>
  --include=GLOB            only include matching files (repeatable)
  --follow-symlinks, -L     dereference symlinks
  --one-filesystem, -X      do not cross filesystem boundaries
  --delete-extra            remove destination files missing from the source

SCHEDULING
  --pin                     pin reader/writer threads to disjoint physical cores
  --io-priority=LEVEL       verylow, low, normal, high, critical
  --process-priority=LEVEL  idle, below, normal, above, high, realtime
  --thread-priority=LEVEL   idle, lowest, below, normal, above, highest
  --privileged              enable privileged operations (large pages, valid data)

MISC
  -v, --verbose             more output (repeatable)
  -q, --quiet               no progress output
  --json                    machine-readable summary
  --calibrate               measure read/write throughput for the delta model
  -h, --help                this text
  -V, --version             version
";
