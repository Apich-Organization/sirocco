<p align="center">
  <img src="logo.svg" alt="Sirocco" width="90">
</p>

<h1 align="center">Sirocco — Design</h1>

<p align="center">
  <a href="README.md">README</a> ·
  <a href="README.zh-CN.md">README (中文)</a> ·
  <b>Design</b> ·
  <a href="DESIGN.zh-CN.md">设计文档 (中文)</a>
</p>

---

This document describes the system as it is built, and argues for each decision
that is not obvious. It is not a roadmap: anything described here exists, and
anything that does not exist is listed under [Known limits](#16-known-limits)
rather than planned.

The organising idea is that a copy engine's job is to move bytes between two
devices with the smallest possible amount of *interference* — from the page
cache, from a runtime, from a scheduler that guesses. Everything below follows
from that.

## 1. Scope and stance

| | |
|---|---|
| Name | **Sirocco** — the Mediterranean wind |
| Binary | `sirocco` (CLI), `sirocco-gui` (desktop) |
| Version | `Cargo.toml`, `[workspace.package]` |
| Language | Rust, edition 2024; the floor is `rust-version` in `Cargo.toml` |
| Dependencies | Windows: **none** — hand-written `kernel32` / `advapi32` / `ntdll` bindings. Linux: `libc` only |
| Deliberately absent | Tokio, async-std, windows-sys, clap, liburing, libacl |

"No dependencies" is not purity for its own sake. An I/O engine needs control
over the lifetime of every `OVERLAPPED`, over which SQE slot is reused, over
buffer registration, over affinity and I/O priority. Every abstraction layer
above the syscall either hides those or adds an allocation on the hot path. The
argument is not that a runtime cannot be fast; it is that the engine's whole
value is in decisions a runtime does not let you make.

Sirocco is not a general-purpose filesystem API. It is one program with one
job, and the design is allowed to assume that.

## 2. Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│ sirocco-cli        argument parsing / reporting / bench / info        │
│ sirocco-gui        iced front end over the same engine               │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-engine     session orchestration, scan, decision, pipeline    │
│   ├─ scan       parallel directory walk (work-stealing)              │
│   ├─ session    skip / full / append / delta decision, then summary   │
│   ├─ pipeline   reader×R and writer×W, each with its own reactor      │
│   ├─ journal    resume log                                           │
│   └─ config     every knob, and their normalisation                  │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-core       OS-independent algorithms                          │
│   ├─ delta      cost model + Wilson upper bound                      │
│   ├─ entropy    measured compressibility, not estimated              │
│   ├─ lz         LZ4 block codec (own implementation, wire compatible)│
│   ├─ hash       XXH64 / CRC32C                                       │
│   ├─ queue      bounded lock-free MPMC (Vyukov)                      │
│   ├─ plan       job description types                                │
│   └─ pattern    include / exclude glob matching                      │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-sys        platform layer: RawFile / Reactor / Topology       │
│   ├─ win/       CreateFileW + IOCP + FSCTL + SetFileValidData         │
│   ├─ lnx/       open(O_DIRECT) + raw io_uring + FIEMAP + fallocate    │
│   └─ blocking   portable fallback (one handle per thread)             │
└──────────────────────────────────────────────────────────────────────┘
```

**The load-bearing constraint**: a `Reactor` is owned by exactly one thread.

On Windows a file handle can be associated with only one completion port, so
"one handle set per thread" is not an optimisation but a requirement. On Linux
each thread owns its own ring, which is the same shape. Because the constraint
holds on both platforms, the layer above is written once.

Two corollaries fall out of it, and both are invariants rather than guidelines:

1. The handle cache is keyed by `(file id << 3) | kind`, and `kind` distinguishes
   direct from buffered. A cache that does not make that distinction hands a
   `NO_BUFFERING` handle to the unaligned tail read, which fails with
   `ERROR_INVALID_PARAMETER` on Windows and `EINVAL` on Linux.
2. A shutdown signal must be deliverable when every queue is full. Every blocking
   push has an abort path; without one, an interrupt hangs the join.

## 3. Platform layer

### 3.1 Windows

| Capability | Mechanism |
|---|---|
| Direct I/O | `FILE_FLAG_NO_BUFFERING` + a 4 KiB-aligned pooled buffer |
| Asynchrony | `FILE_FLAG_OVERLAPPED` + `CreateIoCompletionPort` + `GetQueuedCompletionStatusEx` (batched reaping) |
| Completion records | `OVERLAPPED` lives in a pre-allocated contiguous slab; the index is recovered by pointer arithmetic, so submitting costs no allocation |
| Error translation | `OVERLAPPED.Internal` is an NTSTATUS; `GetOverlappedResult` converts it to a Win32 error |
| Preallocation | `SetFileValidData` with `SeManageVolumePrivilege`, otherwise a sparse hole (§9) |
| Sparse | `FSCTL_SET_SPARSE` (set and clear), `FSCTL_SET_ZERO_DATA` to punch |
| Physical layout | `FSCTL_GET_RETRIEVAL_POINTERS` (VCN→LCN); the MFT record number is the low 64 bits of `FileIdInfo` |
| Media type | `IOCTL_STORAGE_QUERY_PROPERTY`: `StorageDeviceSeekPenaltyProperty`, `StorageAccessAlignmentProperty` |
| Compression | `FSCTL_SET_COMPRESSION` (NTFS transparent compression) |
| Topology | `GetLogicalProcessorInformationEx(RelationAll)`, parsed into `PROCESSOR_RELATIONSHIP` / `NUMA_NODE_RELATIONSHIP` |
| Affinity | `SetThreadGroupAffinity` — processor-group aware, so >64 logical CPUs is correct rather than truncated |
| I/O priority | `SetFileInformationByHandle(FileIoPriorityHintInfo)` — **per handle**, not per thread |
| Process / thread priority | `SetPriorityClass` / `SetThreadPriority` |
| Large pages | `VirtualAlloc(MEM_LARGE_PAGES)`, requires `SeLockMemoryPrivilege` |
| Privileges | `OpenProcessToken` + `AdjustTokenPrivileges` |

The last row is a trap worth naming: `AdjustTokenPrivileges` returns success even
when it changed nothing. The only way to know whether a privilege is held is to
call `GetLastError` afterwards and compare against `ERROR_NOT_ALL_ASSIGNED`. A
probe that trusts the return value reports capabilities the process does not
have, and the failure surfaces later as an unexplained slow path.

### 3.2 Linux

| Capability | Mechanism |
|---|---|
| Direct I/O | `open(O_DIRECT)` |
| Asynchrony | Hand-rolled `io_uring`: `io_uring_setup` / `enter` / `register`, with the SQ and CQ rings mmap'd directly |
| Setup flags | `IORING_SETUP_COOP_TASKRUN \| SINGLE_ISSUER`, optionally `IOPOLL` / `SQPOLL` |
| Fixed buffers | `IORING_REGISTER_BUFFERS` + `READ_FIXED` / `WRITE_FIXED`, which removes `get_user_pages` from every request |
| Preallocation | `fallocate`; punching is `fallocate(PUNCH_HOLE \| KEEP_SIZE)` |
| Physical layout | `FS_IOC_FIEMAP` |
| Media type | `/sys/block/<dev>/queue/rotational` |
| Topology | `/sys/devices/system/cpu/cpuN/topology/*` plus `sched_getaffinity` |
| Affinity | `sched_setaffinity` |
| I/O priority | `ioprio_set`, best-effort class, mapped 0..4 → data 7..0 |
| Large pages | `mmap(MAP_HUGETLB)`, falling back to `madvise(MADV_HUGEPAGE)` |
| Cache control | `posix_fadvise(SEQUENTIAL / DONTNEED)` |
| Privileges | Read `CapEff` from `/proc/self/status` — capabilities are granted at `exec` time and cannot be raised later |

Two portability decisions are not obvious and are therefore written down:

- **Syscall numbers come from `libc::SYS_*`, never literals.** They differ per
  architecture: `ioprio_set` is 251 on x86_64 and 30 on aarch64, `statx` is 332
  and 291, `mbind` is 237 and 235. A hardcoded number does not fail loudly; it
  invokes a different syscall.
- **The `io_uring` structure layouts are checked at compile time** with `const`
  assertions on `size_of` and field offsets. If a future kernel revises the ABI,
  the build fails instead of the ring silently misbehaving.

### 3.3 The portable fallback

`--engine=blocking` is a thread pool over `pread`/`pwrite`. It exists for two
reasons: it is the reference implementation the fast path is cross-checked
against, and it is what runs when the native backend is unavailable (a kernel
with `io_uring` disabled by sysctl or seccomp, an older Windows without the
completion-port APIs the fast path assumes).

It also produced the clearest platform lesson in the project. The first version
shared one `RawFile` among the worker threads; on the cloud-synced volume used
for testing, concurrent `ReadFile` calls through that shared handle **randomly
returned zero bytes**, because the `OVERLAPPED` offset was ignored and the call
degenerated to the shared file pointer. One handle per worker thread fixed it
completely. The IOCP backend had always worked that way, so the fix applied to
the fallback alone — but the rule it established ("a handle belongs to the
thread that reads from it") now holds everywhere.

## 4. The data path

```
 scheduler ──Chunk──▶ read queue ──▶ [reader × R] ──Ready──▶ write queue ──▶ [writer × W] ──▶ device
    │                                     │                                        │
    │                                  buffer pool ◀───────────────────────────────┘
    │                                     │
    └─ prepare_dst ─ create / preallocate / compression attribute ─┘
```

A **chunk** is `(FileCtx, src_off, rlen, dst_off, wlen, len, tail)`. One 1 MiB
chunk is one read and one write; it is never split further unless `--io-unit`
asks for a smaller request.

Readers touch only source handles, writers only destination handles. With
`--pin`, the two sets are pinned to disjoint physical cores, so a reader and a
writer never share an SMT sibling's execution units.

The buffer pool is one large `VirtualAlloc` / `mmap` (optionally 2 MiB pages),
carved into fixed-size slots handed out from an atomic free list. Because the
backing allocation is page-aligned and the slots are a power-of-two division of
it, every buffer address satisfies direct-I/O alignment by construction — there
is no per-request alignment arithmetic to get wrong.

Backpressure is structural: both queues are bounded lock-free MPMC (Vyukov)
rings. A full queue yields rather than sleeps, so nothing is dropped, nothing
grows without bound, and a stalled writer cannot turn into unbounded memory.

### 4.1 The tail, and why alignment is a state machine

Direct I/O requires the offset, the length *and* the buffer address to be sector
aligned. File lengths are almost never a multiple of 4 KiB. The sequence that
handles this is the part of the engine most likely to be got wrong, so it is
fixed:

1. Preallocate the destination, rounded up to 4 KiB.
2. Write chunks rounded up to 4 KiB, with the buffer tail zero-filled.
3. Read the **final** chunk through a separate, non-direct handle — a direct read
   past end-of-file fails — then zero the rest of the buffer.
4. After the last write, truncate to the exact length and restore the mtime.

Step 3 is why the handle cache key carries a `kind`. If a buffered tail read
hits a cached direct handle, the result is an immediate `EINVAL`, and the
temptation is to treat it as a transient device error and retry — which it is
not.

## 5. Deciding what to copy

This is the only genuinely algorithmic part of the project, and the part the
problem statement left open: *decide, from incomplete information, whether an
incremental copy is worth it.*

### 5.1 Three strategies and their cost

Let `S` be the file size, `r` the fraction of blocks that differ (unknown),
and `R`, `W`, `Rd` the measured source-read, destination-write and
destination-read throughputs:

| Strategy | Time |
|---|---|
| Full overwrite | `S/R + S/W` |
| Block delta, no index | `S/R + S/Rd + r·S/W` |
| Append (common prefix) | `(S−D)/R + (S−D)/W` |

A block delta beats a full copy exactly when

```
S/R + S/Rd + r·S/W  <  S/R + S/W
      ⟺  r < 1 − W/Rd
```

That inequality is the whole argument. **On a fast disk, incremental sync is
usually a bad idea**: on NVMe `W ≈ Rd`, the threshold collapses to zero, and no
value of `r` satisfies it. It only pays when the destination is much slower than
the source path — a network share, a USB disk, SMR media, anything with write
amplification. A tool should not guess which case it is in, so Sirocco *measures*
both rates and lets the inequality decide.

### 5.2 Estimating `r` from too few samples

`r` is unknown, so it is sampled: `k` block-aligned probes (deterministic
pseudo-random, plus first and last), read from both sides, counting the `c` that
differ.

The point estimate `c/k` is not enough. One changed block in a thousand files
will usually not be hit by 64 samples at all. So the decision uses the **upper
bound of the Wilson score interval** at 95 %:

```
p̂ = c/k,  z = 1.96
upper = ( p̂ + z²/2k + z·√(p̂(1−p̂)/k + z²/4k²) ) / (1 + z²/k)
```

Using the upper bound rather than the point estimate is a deliberate asymmetry of
costs: mistaking a delta for a full copy costs one redundant read of the
destination, while mistaking a full copy for a delta costs a wasted write of
every byte. The bound pushes the decision toward the cheap mistake.

### 5.3 The sub-block correction

Probes are 64 KiB; the comparison granularity is 1 MiB. A 1 MiB block counts as
changed if *any* sub-block inside it changed, so

```
r_block = 1 − (1 − r_probe)^(block/probe_unit)
```

with `inflate = 16` as the exponent. Without this step the estimate is
systematically low, and the common case — 4 KiB rewritten inside every 1 MiB —
would look like a 0.4 % delta when it is a 100 % delta.

### 5.4 Append is the real winner

When the destination is shorter than the source and sampling says the common
prefix is unchanged, the copy becomes an append: read only the tail, write only
the tail, and never touch the prefix. For logs, VM images and append-only
datasets this is where "the second sync is nearly free" comes from.

Append is not blind trust. Before committing, the last 4 MiB of the destination
is compared byte-for-byte with the source; any difference falls back to a full
copy.

### 5.5 The decision table

| Condition | Action |
|---|---|
| Destination missing | Full |
| Same size and exactly equal mtime (or both timestamps have zero sub-second part and differ by ≤ 2 s, which is FAT/SMB) | Skip |
| `size < min_size` (8 MiB by default) | Full — the probe would cost more than it saves |
| Destination smaller, prefix sample clean, last 4 MiB identical | **Append** |
| `r_upper < (1 − W/Rd) − margin` | **Block delta** |
| Anything else | Full |

The timestamp rule deserves its own note. rsync and robocopy apply a
two-second tolerance unconditionally; in a "edit, then sync immediately"
workflow that silently skips files. Sirocco compares exactly first and only
enables the tolerance when both sides report zero sub-second precision, which is
the signature of a coarse-grained filesystem rather than of a real match. The
end-to-end suite caught the original behaviour, which is why the rule is
explicit here.

## 6. Small files

The bottleneck for small files is not bandwidth, it is per-file latency: an
open, a read, a write and a close cost tens of microseconds each even on a good
disk, and a few hundred thousand of them in series is tens of seconds.

Sirocco attacks this at the queue level rather than by batching into a stream.
Files below `--small` enter the pipeline in batches, so the read and write
requests of a whole batch are in flight together and the device queue is
immediately deep. An LRU of 96 handles per thread means a directory of small
files barely re-opens anything.

The alternative — pack many files into one stream and unpack at the destination —
was rejected: for a plain file copy it requires a second pass to split the data
back out, which doubles total I/O. Bundling is only a win when the bundle itself
is the artefact, as in an archive or a container format.

## 7. Compression

On a local disk, "compress then write" is usually a pessimisation: it saves
write volume but costs CPU, and the bytes still have to be written unless the
filesystem compresses them. So the decision is made by measurement, not by
entropy:

1. Sample three 256 KiB spans (head, middle, tail).
2. **Run LZ4 on them for real**, obtaining a measured ratio and a measured
   compression throughput `C`. Entropy is recorded as context only: JPEG and
   compiled objects both sit near 8 bits/byte, and the latter still compresses
   2:1.
3. Apply the same cost model:
   ```
   ratio·S/W + S/C  <  S/W   ⟺   ratio < 1 − W/C
   ```
   A 3 GB/s NVMe with a 600 MB/s LZ4 never satisfies it, so compression turns
   itself off. A 150 MB/s HDD satisfies `ratio < 0.75` immediately, so it turns
   itself on.
4. When it pays, the destination gets the **NTFS transparent compression**
   attribute and the kernel does the work. User-space compression is not applied
   to the bytes on the way out.

The LZ4 block codec is an own implementation (wire compatible, a few hundred
lines). It exists because step 2 needs a real measurement, and because a
container or network format may want it later. It carries round-trip tests over
random, all-zero, text, overlapping-match and corrupt input.

## 8. Verification and the index

| Mode | What it does | Cost |
|---|---|---|
| `none` | Nothing | 0 |
| `size` | Compare final size | one `stat` |
| `size-mtime` | Size and mtime | one `stat` |
| `hash` | Reader hashes the source block; writer reads the destination block back and compares | one extra read of the destination |
| `sample` | Head, middle and tail blocks | negligible |

XXH64 is the default digest at roughly 13 GB/s, far above any storage device;
CRC32C is available where the CPU has the instruction. Verification runs inside
the writer thread, so it is parallel for free and can name the block that failed
rather than the file.

The persistent index (`--index=trust|verify`) stores destination block hashes
keyed by the destination's `(size, mtime)`. The choice of key is the design: the
index answers "has this destination file changed since I last wrote it", so a
change on the *source* side is irrelevant to its validity, and a resync can skip
reading the destination entirely. `verify` additionally re-reads the source and
compares hashes, falling back to a full copy the moment the index turns out to be
stale.

## 9. Preallocation, and a trap that had to be written down

Intuition says "extend the file to its final size first" reduces fragmentation.
On NTFS it also sets the valid data length to zero, so any write that skips
ahead triggers **zero-fill from the start of the gap** — effectively writing the
data twice. Measured on a 128 MiB copy:

| Configuration | Throughput |
|---|---|
| Preallocate via `SetEndOfFile` (unprivileged) | 67.8 MiB/s |
| No preallocation | 158 MiB/s |
| **Preallocate through a sparse hole** | **159 MiB/s** |

So the strategy is:

1. Try `SetFileValidData`, which is free when `SeManageVolumePrivilege` is held.
2. Otherwise mark the file sparse and extend — the extension becomes a hole and
   produces no zero-fill — then **clear the sparse flag** after the last write
   (`FSCTL_SET_SPARSE` with `SetSparse=FALSE`), so the output is
   indistinguishable from an ordinary file.

The privilege probe runs once per session, on a scratch file in the destination
directory, not once per file. Probing per file would mean every file pays for an
extension, a failure and a rollback.

`--prealloc=auto|valid|sparse|off` overrides the choice.

## 10. Scheduling, affinity, NUMA

- Readers default to half the physical cores, writers to the rest; `--threads`
  overrides the split.
- With `--pin`, readers and writers are pinned to **disjoint** physical cores —
  processor-group masks on Windows, `sched_setaffinity` on Linux. The point is
  that the two halves never share an SMT sibling.
- `--io-priority` is per handle on Windows (`FileIoPriorityHintInfo`) and the
  best-effort `ioprio_set` class on Linux. It is deliberately not a thread
  priority: the hint belongs to the request, and a thread issuing requests to two
  different devices should be able to rank them differently.
- `--process-priority` and `--thread-priority` control the process class and the
  thread level; on Linux they map to nice values.
- NUMA: the topology records the node of every logical CPU. The engine places
  buffers with `mbind` and threads with `set_mempolicy` when a node is requested,
  and reports the node it chose. On a single-node machine this is a no-op, which
  is why it is safe to leave on `auto`.

Feeder threads — the ones that walk the plan and push chunks — are the one
scheduling decision with no static answer, so they are adaptive (§14.3).

## 11. Physical layout

On a spinning disk, sequential and random access differ by two orders of
magnitude. Sirocco addresses this at two levels.

**File level.** `--order=physical` sorts the plan by a physical key: the NTFS MFT
record number (low 64 bits of `FileIdInfo`) on Windows, the inode number on
Linux. That approximates the order the data sits on the platter. It costs one
extra open per file, which is why it is opt-in.

**Chunk level.** `physical_extents()` returns the VCN→LCN map from
`FSCTL_GET_RETRIEVAL_POINTERS` or `FIEMAP`. Ordering a single file's chunks by
LCN makes its reads arrive in disk order. Holes sort last, since there is nothing
to seek to.

Both are gated on the media type (`IncursSeekPenalty`, `queue/rotational`).
On an SSD, physical ordering is pure overhead, so it is off by default there.

## 12. Privileges and capability probing

Every valuable system feature hangs off a privilege. Rather than ask the user to
remember a set of flags, the engine measures what it can do at startup and forks
on the result.

| Capability | Windows basis | Linux basis | What it unlocks |
|---|---|---|---|
| `instant_alloc` | `SeManageVolumePrivilege` | `fallocate` (unprivileged) | `SetFileValidData`, skipping the zero-fill |
| `large_pages` | `SeLockMemoryPrivilege` | `CAP_IPC_LOCK` | 2 MiB page buffer pool |
| `backup` | `SeBackupPrivilege` | `CAP_DAC_READ_SEARCH` | open files a DACL denies |
| `restore` | `SeRestorePrivilege` | `CAP_FOWNER` | restore owner and permissions |
| `rt_io` | — | `CAP_SYS_ADMIN` | `IOPRIO_CLASS_RT` |
| `rt_priority` | `SeIncreaseBasePriorityPrivilege` | `CAP_SYS_NICE` | real-time thread priority |
| `mlock` | working-set quota | `CAP_IPC_LOCK` | lock buffer pages |
| `noatime` | — | file ownership | `O_NOATIME` |

The engine prints one line at startup, for example:

```
sirocco: capabilities: unprivileged [large-pages, mlock]
sirocco: allocation strategy: hole-based extension
```

This is what "different paths under different privileges" looks like in practice:
the same binary extends through a hole as an ordinary user and switches to
`SetFileValidData` the moment the privilege is available. `--privileged` asks for
the privileges explicitly before the probe; without it, the process uses whatever
the token already holds.

## 13. Metadata fidelity

**Hard links.** The scan records the file id and link count — free from `stat` on
Linux, `NtQueryInformationFile(FileStandardInformation)` plus `FileIdInfo` on
Windows. The first path is copied normally; every other path with the same id
becomes a `CreateHardLinkW` / `link(2)`, with no data movement. `--links` is on
by default. Siblings can be in flight concurrently, so a failed link is retried
briefly and then falls back to a copy; correctness never depends on winning that
race.

**Times and modes.** Windows writes attributes and mtime in one
`SetFileInformationByHandle(FileBasicInfo)` call. Linux writes the mode through
the handle (`fchmod`), so the executable bit survives — a real gap in an earlier
version, now locked down by `permissions_are_preserved`.

**Extended attributes and ACLs.** `--xattrs` copies attributes one by one through
`fgetxattr` / `fsetxattr`. POSIX ACLs are extended attributes
(`system.posix_acl_access`, `system.posix_acl_default`), so they reuse that
channel instead of dragging in libacl. On Windows, `--acl` copies the owner,
group and DACL with `GetSecurityInfo` / `SetSecurityInfo`; setting the owner
needs `SeRestorePrivilege` and degrades to a DACL-only write without it.

ACLs are opt-in on both platforms. Copying a security descriptor across
filesystems fails often enough — unmapped SIDs, unsupported ACE types — that it
should be a decision rather than a default.

## 14. Doing more than one thing at a time

### 14.1 Sparse sources are not read

`--sparse` has two layers. On the source side, `FSCTL_QUERY_ALLOCATED_RANGES`
or `SEEK_DATA`/`SEEK_HOLE` yields the allocated ranges, and a chunk that falls
entirely inside an unallocated region is **never read**: the writer receives a
`Hole` message and punches the destination instead. Such a chunk does not even
occupy a buffer. On the destination side, any remaining chunk whose content is
all zeroes is punched as before.

Measured on a 64 MiB file of which 3 MiB was allocated:

```
data : 3.00 MiB written, 3.00 MiB read, 0 B verified, 63.9 MiB punched
```

One consequence is easy to miss: once holes have been punched, the destination's
sparse flag must **not** be cleared — that would materialise the holes as zero
clusters and undo the entire exercise. The preallocation path clears the flag
(§9); the sparse path must not.

### 14.2 Splitting across processes

`--procs=N` does not split per file — that would only make several processes
contend for the same directory lock. The split is at subtree granularity: the
parent scans, cuts the traversal into N contiguous ranges by byte count, creates
every directory up front, and hands each worker a manifest (`--files-from`) to
run independently.

```
$ sirocco copy src dst --procs=4
sirocco: split into 4 processes (20000 files)
sirocco split summary (4 processes)
  files        : 20000 copied, 0 failed
  elapsed      : 7s
```

On the same 20 000-file tree: 14.5 s single-process, **7 s** with four. The gain
comes from bypassing the directory lock, not from more threads — `robocopy
/MT:16` needs 10.5 s on the same tree.

### 14.3 Cross-process progress without a channel

Every intuitive design has a cost: a pipe costs a syscall per update, a socket
costs a thread, shared memory plus a publisher thread still costs a thread. This
path runs hundreds of thousands of times per second.

The zero-cost design is to **put the counters themselves in shared memory**.
`Counters` is a `#[repr(C)]` struct of `AtomicU64`s; the child builds its `Stats`
directly on the mapped region, so the `fetch_add` calls the pipeline was already
doing land in pages the parent can read.

- The parent creates one mapping per worker (`<tmp>/stats-<i>.shm`) and passes it
  as `--shm=<path>`.
- The child calls `Stats::from_shared(ptr)`; every counter is then visible.
- The parent reads those cache lines four times a second and sums them.

No extra thread, no IPC, no syscall in the hot path. A `magic` field sits first
in the struct so a stale or unrelated mapping is rejected rather than read.

### 14.4 Feeders adapt

The right number of feeder threads depends on the workload and the filesystem,
and no static formula knows it: a handful of large files needs one (a single file
fills the queue), while a directory of 40 000 tiny files is limited by the
directory lock, where more threads only make it worse.

So the feeder count is a **permit count** (`FeedGate`: a `Mutex<usize>` and a
`Condvar`), and a thread that cannot get a permit parks instead of spinning. A
controller samples throughput every 600 ms — bytes for large files, file count
for small ones — and climbs:

```
rate improved  → remember this value
rate dropped ≥8 % → return to the best value and reverse direction
```

It stops after twelve adjustments or at 85 % progress, so the tail of a run does
not oscillate. Measured on 20 000 × 4 KiB files:

| Feeder policy | Elapsed |
|---|---|
| Fixed at 2 | 15.7 s |
| **Adaptive** | **12.3 s / 13.7 s** |

## 15. The desktop application

The GUI is an iced front end over the same engine, and it is deliberately thin.
There are exactly two channels between the UI and the engine, and neither costs a
thread:

1. **Counters.** The GUI allocates an `Arc<Stats>` and passes it to
   `session::run_with(cfg, src, dst, Some(stats))`. The engine's existing
   `fetch_add` calls land in that memory; the UI reads a snapshot every 90 ms.
   This is the same struct the `--procs` path shares across processes, used across
   threads instead.
2. **File events.** `EngineConfig.events: Option<Sender<Event>>` carries
   `Started`, `Done`, `Skipped`, `Failed` and `Finished`. The UI drains the
   channel in the same tick, so it needs no receiver thread.

Three gauges were added to `Counters` for the pipeline view: `readers_active`,
`writers_active` (incremented on submit, decremented on completion) and `queued`.

### 15.1 The pipeline view answers "where is the bottleneck"

The first version drew reader dots, a queue bar and writer dots with a flowing
highlight. It had no legend, it looked busy when idle, and it answered no actual
question.

It is now three occupancy bars and a verdict:

| Condition | Verdict |
|---|---|
| Writers saturated, queue backing up | `destination-bound` |
| Readers saturated, queue empty | `source-bound` |
| Read, write and queue all low, bytes still moving | `metadata-bound` — scanning or directory creation is the constraint |
| Pipeline empty, bytes still moving | `small files · kernel path` |

The last row matters most. Small files go through `CopyFile2` /
`copy_file_range`, so the chunk pipeline is genuinely empty. Three empty bars
would read as a fault; saying "this path does not use the pipeline" is the
honest version.

### 15.2 Events leave through one door

An early version emitted `Done` only from the kernel-copy fast path, so files
that went through the pipeline never left the IN FLIGHT list, which then stopped
updating at its 64-row cap. The fix is structural: the event is emitted from
`complete_file()`, the single function every file passes through — full copy,
append, block delta, index hit, or an externally completed file. The failure
reason lives in `FileCtx.err`, and `Shared::fail(&file, msg)` writes it both to
the file and to the summary list, so the row shows why and the report still has
the complete list.

### 15.3 Four layout traps

1. **Scrollbars overlay content** rather than taking width, so a right-aligned
   file size loses its last eight pixels — `2.24 KiB` renders as `2.24 K`. The
   fix is 10 px of right padding inside the scroll region.
2. **`Button` does not centre its child**; it lays it out at the padding origin.
   A full-width button needs `container(...).center_x(Fill)` or the label sticks
   to the left edge.
3. **`Space` has zero width inside a `Row`**, so horizontal gaps need an explicit
   width. The symptom was a heading rendering as `THROUGHPUTlast 60 s`.
4. **`Counters.queued` is the sum of both queues** while `queue_depth` is
   per-queue, so the capacity shown beside the meter has to be doubled — otherwise
   a healthy run displays `128 / 64`.

### 15.4 Colour is chosen by contrast, not by taste

The palette is three text levels measured against their background: body text
about 15:1, secondary 8:1, labels 5:1 — all above WCAG AA for their sizes. The
first attempt had labels at 2.46:1 and borders at 1.24:1, which on a dark
background is not a hierarchy, it is an absence of one.

Colour is reserved for meaning: one accent, and semantics for running, done and
failed. Nothing is coloured for decoration.

### 15.5 What the GUI deliberately does not do

The GUI exposes the engine's options, but three families stay on the command
line because a window is the wrong instrument for them:

- **Multi-process splitting.** It re-executes the binary with a manifest; it is
  batch territory, and a progress bar over four processes is not more informative
  than the parent's summary.
- **Destructive modes without a preview.** `delete` has no button. A delete
  button in a GUI is a disaster waiting for a mis-click; `--dry-run` exists for
  that purpose.
- **Verbosity.** `-v` / `--json` are output switches for scripts, and the GUI's
  output is the window.

Everything else — I/O shape, scheduling, privileges, fidelity, filters, the block
index, delta tuning — is reachable from the rail, grouped so that the common case
is visible and the rest is one click away.

## 16. Measurements

Machine: i7-11800H (8C16T), cloud-synced NVMe (write bandwidth ≈ 160 MB/s, which
is the binding constraint on this machine).

Baseline (Python, buffered + `fsync`): 159 MiB/s write, 1293 MiB/s cached read.

| Scenario | Result |
|---|---|
| Full copy, IOCP + direct I/O + sparse preallocation | **159 MiB/s** — the device's write ceiling |
| Full copy, blocking fallback backend | 138 MiB/s |
| Full copy, `--no-direct` (writes into the page cache) | 723 MiB/s — a cache number, not a durability one |
| Preallocation done the naive way | 67.8 MiB/s |
| 20 000 × 4 KiB files, user-space pipeline | 33 s |
| 20 000 × 4 KiB files, kernel path | 17 s (single-threaded `robocopy`: 17 s) |
| 20 000 × 4 KiB files, 4 processes | 7 s (`robocopy /MT:16`: 10.5 s) |
| 8 files / 78 MiB with `--verify=hash` | 0 failures, every block verified |
| 200 files / 200 MiB | 0 failures |

The conclusion is not "Sirocco is faster than X". It is that on this machine the
engine sits on the device's ceiling, so the interesting differences show up in
the *protocol* costs — small files, incremental sync, verification — rather than
in bulk throughput. A cloud-synced volume cannot demonstrate absolute NVMe
performance, but the correctness conclusions (byte-identical trees, verified
blocks, no leaks) do not depend on the medium.

## 17. Known limits

These are design constraints, not a backlog.

- **No CoW filesystem optimisation** (btrfs / XFS reflink). A reflink copy is
  instant and free, which would make most of this engine pointless on those
  filesystems. Supporting it would mean the engine's best case is "do nothing",
  and the honest answer there is to use `cp --reflink`.
- **No pack-small-files-into-a-stream mode** (§6). For a local copy it doubles
  the I/O.
- **No user-space compression for local copies** (§7). The cost model says it
  loses; NTFS transparent compression is the only compression applied, and only
  when the measurement supports it.
- **ACLs are opt-in and thinly tested** (§13). The `--acl` path is wired on both
  platforms, but only permission bits are covered by regression tests; POSIX ACL
  entries and Windows DACL inheritance are not.
- **Delta will not engage between two equally fast volumes** (§5.1). That is the
  model working as designed, not a defect.
- **The index trusts timestamps** (§8). A file rewritten externally with its
  mtime restored will not be noticed; `--index=verify` re-reads the destination
  to check.
- **ARM64 is untested on hardware.** The code takes syscall numbers from
  `libc::SYS_*` and has a table-driven CRC32C path for CPUs without the
  instruction, but no ARM machine has run the suite.

## 18. Code map

```
crates/
  sirocco-sys/          platform layer
    src/win/ffi.rs      hand-written Win32 bindings
    src/win/iocp.rs     IOCP reactor + OVERLAPPED slab
    src/win/mod.rs      RawFile / volume info / topology / privileges / FSCTL
    src/lnx/ffi.rs      io_uring + FIEMAP constants and structures
    src/lnx/uring.rs    raw io_uring implementation
    src/lnx/mod.rs      O_DIRECT / sysfs / affinity / ioprio
    src/blocking.rs     portable fallback (one handle per thread)
    src/{align,buf,slab,clock,caps,affinity,interrupt,shm}.rs
  sirocco-core/         algorithms
    src/delta.rs        incremental decision (§5)
    src/entropy.rs      measured compressibility (§7)
    src/lz.rs           LZ4 block codec
    src/hash.rs         XXH64 / CRC32C
    src/queue.rs        bounded lock-free MPMC
    src/stats.rs        counters, snapshot, rate estimator
    src/plan.rs         job description types
    src/pattern.rs      glob matching
  sirocco-engine/       orchestration
    src/session.rs      decision + scheduling + summary
    src/pipeline.rs     readers, writers, feeders
    src/scan.rs         parallel directory walk
    src/journal.rs      resume log
    src/config.rs       every knob, and normalisation
  sirocco-cli/          argument parsing, reporting, bench/info/verify
  sirocco-gui/          iced application (§15)
tests/
  e2e.py                end-to-end suite, hashes both trees independently
  gui_shot.py           launches the GUI and captures its client area
```
