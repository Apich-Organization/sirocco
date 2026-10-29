<p align="center">
  <img src="logo.svg" alt="Sirocco" width="120">
</p>

<h1 align="center">Sirocco</h1>

<p align="center">
  A file copy engine for Windows and Linux. Direct I/O, kernel-assisted small
  files, and a measured choice between full copy and block-level delta.
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#what-it-does-differently">Why it exists</a> ·
  <a href="#the-gui">GUI</a> ·
  <a href="#command-line">CLI</a> ·
  <a href="#testing">Testing</a> ·
  <a href="README.zh-CN.md">简体中文</a>
</p>

---

Sirocco copies files the way a storage engineer would: it opens the device with
direct I/O, asks the filesystem where the data physically lives, decides per file
whether a full copy or a block-level delta is cheaper, and reports what the
pipeline was actually doing while it ran.

It is not a wrapper around `std::fs::copy`. Every layer is written against the
platform interface — `CreateFileW`/`ReadFile`/`WriteFile` on an IOCP port on
Windows, `open(O_DIRECT)` and a hand-rolled `io_uring` on Linux — with no async
runtime, no buffered file abstraction, and no third-party crate in the copy path.

```console
$ sirocco copy D:\data E:\backup --verify=hash --pin
sirocco: 12431 files, 812 dirs, 48.2 GiB to copy; buffers 136 × 1.00 MiB = 136 MiB; 4 readers + 4 writers
  ...
sirocco copy summary
  backend      : iocp (align 4096)
  files        : 12431 copied, 0 skipped, 0 failed (of 12431)
  data         : 48.2 GiB written, 48.2 GiB read, 48.2 GiB verified, 0 B saved
  elapsed      : 5m12s
  throughput   : 158 MiB/s (write), 158 MiB/s (read)
```

The design and the reasoning behind each default are in **[DESIGN.md](DESIGN.md)**
(also available in [中文](DESIGN.zh-CN.md)). This file is the practical one.

## Quick start

```bash
cargo build --release                 # CLI: target/release/sirocco
cargo build --release -p sirocco-gui  # GUI, built separately
```

> The GUI is deliberately **not** a default workspace member. `iced` pulls in
> wgpu, winit, Wayland and X11, which a headless build machine cannot satisfy.
> Keeping it out means `cargo build` and `cargo test` stay dependency-free on
> Windows and need only `libc` on Linux.

Requirements: a Rust toolchain that satisfies the `rust-version` in
`Cargo.toml`. Nothing else — no build script, no pkg-config, no vendored C.

## What it does differently

Five things, each with the measurement that justifies it.

**1. Direct I/O by default, and a fallback when it does not pay.**
`FILE_FLAG_NO_BUFFERING` / `O_DIRECT` keep the copy out of the page cache, so the
bytes go to the device once instead of being written twice (once to cache, once
to disk). On the test machine that is 159 MiB/s with the IOCP backend against
138 MiB/s with the portable blocking backend, on the same job. `--no-direct`
puts the cache back when the destination is fast and the source is not — but the
number it produces (723 MiB/s) is a page-cache number, not a durability one.

**2. Small files go to the kernel.**
A user-space pipeline is the wrong tool for a directory of 4 KiB files: the
per-file cost is dominated by opens, not by bytes. Below `--buffer` size the copy
is handed to `CopyFile2` on Windows and `copy_file_range` on Linux. Measured on
20 000 × 4 KiB files: **33 s** through the user-space pipeline, **17 s** through
the kernel path — on par with single-threaded `robocopy`. Any feature that needs
to see the data (`--verify=hash`, `--index`, `--compress`, `--sparse`) drops back
to the pipeline automatically.

**3. Preallocation that does not double the write.**
`SetEndOfFile` on NTFS zero-fills the extension, so "reserve the space first"
costs a full extra pass over the file. Sirocco extends with a sparse hole and
clears the sparse flag afterwards: **67.8 MiB/s → 159 MiB/s** on the same job.
With `SeManageVolumePrivilege` it uses `SetFileValidData` and skips the hole
entirely.

**4. Delta only when the numbers say so.**
Block-level incremental copy is not free — it costs a read of the destination to
find out what changed. Sirocco probes a few blocks, extrapolates with a Wilson
confidence bound, and compares the two costs using throughputs it measured rather
than assumed. On two equally fast volumes it correctly concludes that a full copy
is cheaper and never probes at all. Point it at a slow destination (network
share, USB disk) and `--calibrate` makes the model say what you already suspect.

**5. It tells you where the time went.**
The GUI shows read, queue and write occupancy as three bars, and states a
conclusion — `destination-bound`, `source-bound`, `metadata-bound`, `flowing`.
That is the question a throughput number cannot answer, and it is usually the
first thing worth knowing when a copy is slower than expected.

## The GUI

```bash
sirocco-gui
```

Built on [iced](https://iced.rs): configuration on the left, live state on the right.

**Right side, top to bottom**: status bar → four cards (speed, files, elapsed,
remaining) → progress bar → a 60-second throughput chart and the pipeline gauges
→ the file activity stream.

The activity stream has two sections: **IN FLIGHT** (up to 64 rows) and
**RECENT** (most recent first, up to 300). A failed file gets a second, red line
with the reason.

**Left side** covers the engine's main capabilities:

| Group | What is there |
|---|---|
| Mode | Copy / Move / Mirror (the last two show an irreversible-action warning) |
| Paths | Source and destination, each with a browse button; folders can be dropped on the window |
| Options | hard links, sparse holes, permissions and ACL, block index, resume, flush on close, preview |
| Verify | off / size / size + time / hash / sample |
| Overwrite | 7 policies |
| Filter | exclude and include globs, scan order (walk / small first / large first / physical / by name) |
| Parallelism | I/O thread count (`0` = adaptive; the label shows how it resolved into readers and writers) |

There are exactly two channels between the UI and the engine, and neither adds a
thread: the GUI allocates an `Arc<Stats>`, hands it to `session::run_with()`, and
the engine's counters land in that memory, which the UI reads every 90 ms. Per-file
events go over an `mpsc` channel and are drained in the same tick.

**Deliberately not in the GUI** (all still available on the CLI): `--procs`
multi-process splitting (it re-executes the binary with a manifest — batch
territory), `delete` mode (a delete button in a GUI is a disaster waiting for a
mis-click), multiple sources, NUMA / affinity / buffer tuning, compression, and
extended attributes. The reasoning is in DESIGN.md §15.5.

`Esc` stops the current job.

## Command line

```bash
# First full copy with full verification, threads pinned to physical cores
sirocco copy src dst --verify=hash --pin

# Daily sync: only what changed, delete what disappeared
sirocco sync src dst

# Spinning disk: read in physical order, larger buffers
sirocco copy src dst --order=physical --buffer=8M --io-unit=8M --overlap=16

# Many small files: smaller buffers, more queue depth
sirocco copy src dst --buffer=256K --io-unit=256K --overlap=32 --threads=16

# Resume after an interruption: rerun the same command
sirocco copy src dst --journal

# See what would happen, without doing it
sirocco copy src dst --dry-run -v
```

### I/O shape

| Option | Meaning |
|---|---|
| `--buffer=SIZE` | pooled buffer size (default `1M`) |
| `--io-unit=SIZE` | max bytes per request (default `1M`, must divide `--buffer`) |
| `--overlap=N` | outstanding requests per thread (default `8`) |
| `--readers=N` / `--writers=N` | reader / writer thread counts (default: half the physical cores each) |
| `--threads=N`, `-j N` | split N threads between readers and writers |
| `--queue=N` | pipeline queue depth |
| `--feeders=N` | scheduler threads (default `0` = adaptive, hill-climbing on measured throughput) |
| `--cpu-list=LIST` | run only on these logical CPUs (`0-3,8-11`) |
| `--numa=auto\|off\|N` | NUMA policy for buffers and threads |
| `--small-copy=auto\|kernel\|io` | who copies small files |
| `--engine=native\|blocking` | IOCP / io_uring, or a portable thread pool |
| `--no-direct` | disable direct I/O (write into the page cache) |
| `--huge-pages` | back the buffer pool with 2 MiB pages |
| `--prealloc=auto\|valid\|sparse\|off` | destination space reservation |
| `--sparse` | punch holes for all-zero and unallocated regions |
| `--lcn-order` / `--no-lcn-order` | order a file's chunks by physical cluster (HDD) |
| `--procs=N` | coarse multi-process split by subtree, progress over shared memory |
| `--shm=PATH` | publish progress counters to a shared segment |
| `--files-from=FILE` | copy only the listed paths |
| `--journal` | write a resume log; a rerun skips finished files |
| `--sync` | flush every destination file when it is closed |

### Policy

| Option | Meaning |
|---|---|
| `--overwrite=POLICY` | `if-different` (default), `never`, `always`, `if-newer`, `if-older`, `if-size`, `if-content` |
| `-f, --force` | same as `--overwrite=always` |
| `-n, --dry-run` | report only |
| `--verify=MODE` | `none` (default), `size`, `size-mtime`, `hash`, `sample` |
| `--hash=xxh64\|crc32c` | verification hash |
| `--compress=off\|auto\|force` | transparent NTFS compression |
| `--delta` / `--no-delta` | incremental updates |
| `--delta-block=SIZE` | delta granularity (default `1M`) |
| `--delta-min=SIZE` | smallest file worth probing (default `8M`) |
| `--order=walk\|size-asc\|size-desc\|physical\|name` | processing order |
| `--exclude=GLOB` / `--include=GLOB` | glob filters (repeatable) |
| `--exclude-from=FILE` / `--include-from=FILE` | filters from a file (`#` comments) |
| `--into` | `cp -r` semantics: put the source directory itself inside the destination |
| `--index[=trust\|verify]` | persist block hashes so a re-sync does not read the destination |
| `--links` / `--no-links` | preserve hard links (default: preserve) |
| `--xattrs`, `-A` | copy extended attributes (Linux) |
| `--acl` / `--no-acl` | copy the DACL / POSIX ACL |
| `--follow-symlinks`, `-L` / `--one-filesystem`, `-X` | symlink and mount-boundary handling |
| `--delete-extra` | remove destination files that no longer exist in the source (`sync` does this by default) |

### Scheduling

| Option | Meaning |
|---|---|
| `--pin` | pin readers and writers to disjoint physical cores |
| `--io-priority=LEVEL` | `verylow`, `low`, `normal`, `high`, `critical` |
| `--process-priority=LEVEL` | `idle`, `below`, `normal`, `above`, `high`, `realtime` |
| `--thread-priority=LEVEL` | `idle`, `lowest`, `below`, `normal`, `above`, `highest` |
| `--privileged` | force the privileged paths (probed automatically by default) |

### Other

| Option | Meaning |
|---|---|
| `-v` / `-q` | more / less output |
| `--json` | machine-readable summary |
| `--calibrate` | measure read and write throughput for the delta cost model |

`sirocco --help` prints the same list, always in sync with the parser.

## Platform support

| | Windows | Linux |
|---|---|---|
| Asynchronous I/O | IOCP | `io_uring` (registered buffers and files, optional SQPOLL / IOPOLL) |
| Direct I/O | `FILE_FLAG_NO_BUFFERING` | `O_DIRECT` |
| Physical layout | `FSCTL_GET_RETRIEVAL_POINTERS` | `FIEMAP` |
| Small files | `CopyFile2` | `copy_file_range` |
| Preallocation | `SetFileValidData` / sparse hole | `fallocate` |
| Large pages | `MEM_LARGE_PAGES` | `MAP_HUGETLB` |
| Status | builds and passes the full suite | builds and passes the full suite on Debian sid |
| Architectures | x86_64, ARM64 (no Win32 API assumes x86) | x86_64, ARM64 (syscall numbers from `libc::SYS_*`) |
| Third-party crates | none | `libc` only |

> The Linux side is verified on real hardware: Debian sid on a current kernel,
> btrfs. That includes `io_uring`, `copy_file_range`, `statx` alignment probing,
> `FIEMAP` and `SIGINT` interruption.

Two portability notes worth stating because they are easy to get wrong: syscall
numbers differ per architecture (`ioprio_set` is 251 on x86_64 and 30 on aarch64;
`statx` is 332 and 291; `mbind` is 237 and 235), so they are taken from
`libc::SYS_*` rather than written as constants; and CRC32C has both an SSE4.2 and
a table-driven path, selected at run time by `is_x86_feature_detected!`. The
`io_uring` structure layouts are checked at compile time with `const` assertions.

## Testing

```bash
cargo test                 # 73 unit and integration tests (74 on Linux)
python tests/e2e.py        # 21 end-to-end acceptance tests
python tests/e2e.py append # only the tests whose name matches
```

The end-to-end suite deliberately does not trust the program under test: it
recomputes the digest of both trees with Python's `hashlib` and compares them
independently. It covers empty files, 4 KiB boundaries, `1 MiB ± 1`, all-zero
content, text, 120 small files, appends, mid-file rewrites, mtime preservation,
move, delete, mirror, excludes, the blocking backend, copies with direct I/O
disabled, sparse sources, physical ordering and several buffer sizes.

`cargo clippy --workspace --all-targets` is clean, and `cargo +nightly fmt --check`
is enforced. The lint configuration is strict on purpose — `clippy::all`,
`pedantic`, `nursery` and `missing_docs` at `deny` — and every exception is
written down at the crate root with the reason it exists. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## Known trade-offs

* **No CoW filesystem optimisation** (btrfs / XFS reflink). A reflink copy is
  instant and free, which would make most of this engine pointless on those
  filesystems; it is a deliberate exclusion, not an omission.
* **No "pack small files into a stream" mode.** For a local copy that is double
  the I/O. Small files are aggregated at the queue level instead.
* **No user-space compression for local copies.** The cost model says it loses;
  the only compression used is NTFS transparent compression, and only when
  measured entropy says it pays.
* **ACLs need `--acl`.** Copying a DACL or POSIX ACL across filesystems fails
  often enough that it should be a decision, not a default.
* **`--delta` will not engage between two equally fast volumes.** That is the
  model working: it costs more than it saves. `--calibrate` feeds it measured
  throughput so it can tell a slow destination from a fast one.
* **The index trusts timestamps.** It records the destination's block hashes
  keyed by the destination's `(size, mtime)`. A file rewritten externally with
  its timestamp restored will not be noticed; `--index=verify` re-reads the
  destination to check.

## License

MIT OR Apache-2.0, at your option.
