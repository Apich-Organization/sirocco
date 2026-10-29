//! The reader/writer pipeline.
//!
//! ```text
//!  scheduler ──Chunk──▶ read queue ──▶ [reader × R] ──Ready──▶ write queue ──▶ [writer × W] ──▶ disk
//!                                          │                                     │
//!                                     buffer pool ◀──────────────────────────────┘
//! ```
//!
//! Each reader and each writer owns a reactor (an IOCP on Windows, an `io_uring` on
//! Linux) and therefore its own file handles: an IOCP handle belongs to exactly
//! one port, and per-thread handles are cheaper than serialising submissions.
//!
//! Reader threads are pinned to one set of physical cores and writer threads to a
//! disjoint set, so the two halves of the pipeline never share an SMT sibling.
// Every `unsafe` in this file is a call into the reactor (`read`, `write`,
// `poll`), whose contract is that the buffer outlives the operation. The slab
// above it is what guarantees that, and each block says so.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use sirocco_core::plan::VerifyMode;
use sirocco_core::queue::BoundedQueue;
use sirocco_core::stats::Stats;
use sirocco_sys::Access;
use sirocco_sys::Create;
use sirocco_sys::OpenOpts;
use sirocco_sys::RawFile;
use sirocco_sys::Reactor;
use sirocco_sys::Topology;
use sirocco_sys::affinity::AffinityPlan;
use sirocco_sys::buf::Buf;
use sirocco_sys::buf::BufferPool;
use sirocco_sys::slab::Slab;

use crate::config::EngineConfig;
use crate::index::BlockIndex;
use crate::index::IndexStore;
use crate::types::Chunk;
use crate::types::FileCtx;
use crate::types::ReadMsg;
use crate::types::Ready;
use crate::types::WriteMsg;

/// State shared by every stage.
pub struct Shared {
    /// The run's configuration, shared rather than copied per thread.
    pub cfg: Arc<EngineConfig>,
    /// Buffers, sized so no thread ever waits for one.
    pub pool: Arc<BufferPool>,
    /// Progress counters, written directly by every thread.
    pub stats: Arc<Stats>,
    /// Scheduler → readers.
    pub read_q: Arc<BoundedQueue<ReadMsg>>,
    /// Readers → writers.
    pub write_q: Arc<BoundedQueue<WriteMsg>>,
    /// Human-readable failures, in the order they happened.
    pub errors: Arc<Mutex<Vec<String>>>,
    /// The process-wide stop flag; see [`sirocco_sys::interrupt`].
    pub abort: &'static AtomicBool,
    /// CPU and NUMA layout, for the affinity planner.
    pub topo: Arc<Topology>,
    /// Root of the destination tree, used to compute journal keys.
    pub dst_root: std::path::PathBuf,
    /// Optional resume journal; recorded once a file is fully written.
    pub journal: Option<Arc<crate::journal::Journal>>,
    /// Persistent block index, written when a file completes.
    pub index: Arc<IndexStore>,
    /// Physical sector size (or 4096): the granularity of every request.
    pub align: u64,
    /// Readers still running; the last one to exit closes the write queue.
    pub readers_alive: AtomicUsize,
    /// Writers still running. A push into the write queue must give up when this
    /// hits zero, or the feeder blocks forever on a queue nobody drains.
    pub writers_alive: AtomicUsize,
    /// Destinations that completed successfully. `move` deletes a source only if
    /// its destination is in here — a size check is not enough, because the
    /// destination's size is established *before* any byte is written.
    pub completed: Mutex<std::collections::HashSet<std::path::PathBuf>>,
    /// Destinations that will never be good. The hard-link path links later
    /// siblings to the first copy, so it has to distinguish "not finished yet"
    /// from "will never finish".
    pub failed: Mutex<std::collections::HashSet<std::path::PathBuf>>,
}

impl Shared {
    /// Assemble the shared state for one run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<EngineConfig>,
        align: u64,
        topo: Arc<Topology>,
        dst_root: std::path::PathBuf,
        journal: Option<Arc<crate::journal::Journal>>,
        index: Arc<IndexStore>,
        node: Option<u16>,
        stats: Arc<Stats>,
    ) -> io::Result<Arc<Self>> {
        // Allocate the pool on the device's NUMA node: the buffers are touched by
        // the device and by the I/O threads, both of which live there.
        let pool = BufferPool::new_on_node(
            cfg.buffer_size,
            cfg.buffer_count(),
            align.max(4096) as usize,
            cfg.huge_pages,
            node,
        )?;
        Ok(Arc::new(Self {
            stats,
            pool,
            read_q: Arc::new(BoundedQueue::new(cfg.queue_depth)),
            write_q: Arc::new(BoundedQueue::new(cfg.queue_depth)),
            errors: Arc::new(Mutex::new(Vec::new())),
            abort: sirocco_sys::interrupt::flag(),
            readers_alive: AtomicUsize::new(cfg.readers),
            writers_alive: AtomicUsize::new(cfg.writers),
            completed: Mutex::new(std::collections::HashSet::new()),
            failed: Mutex::new(std::collections::HashSet::new()),
            topo,
            dst_root,
            journal,
            index,
            align,
            cfg,
        }))
    }

    /// Record a failure for the summary.
    pub fn error(
        &self,
        msg: String,
    ) {
        self.stats.errors.fetch_add(1, Ordering::Relaxed);
        let mut g = self.errors.lock().unwrap();
        if g.len() < 10_000 {
            g.push(msg);
        }
    }

    /// Queue a chunk, blocking while the queue is full.
    pub fn push_chunk(
        &self,
        c: Chunk,
    ) {
        let mut msg = ReadMsg::Chunk(c);
        loop {
            match self.read_q.push(msg) {
                | Ok(()) => return,
                | Err(m) => {
                    msg = m;
                    // Bail out if the readers are gone; otherwise the scheduler
                    // would spin here forever.
                    if self.abort.load(Ordering::Relaxed)
                        || self.readers_alive.load(Ordering::Acquire) == 0
                    {
                        return;
                    }
                    std::thread::yield_now();
                },
            }
        }
    }

    /// Queue pre-read data for the writer stage (used by the delta and append
    /// paths, which produce buffers without going through the read queue).
    /// Returns `false` when the message was dropped because the writers are gone.
    ///
    /// The caller has to settle the file's `pending` count in that case; the
    /// buffer goes back to the pool either way, but a chunk nobody will write is
    /// still a chunk the file is waiting for.
    pub fn push_ready(
        &self,
        r: Ready,
    ) -> bool {
        let mut msg = WriteMsg::Ready(r);
        loop {
            match self.write_q.push(msg) {
                | Ok(()) => return true,
                | Err(m) => {
                    msg = m;
                    if self.abort.load(Ordering::Relaxed)
                        || self.writers_alive.load(Ordering::Acquire) == 0
                    {
                        return false;
                    }
                    std::thread::yield_now();
                },
            }
        }
    }

    /// Record that a destination will never be good.
    ///
    /// The hard-link path links later siblings to the first copy, so it has to be
    /// able to tell "not finished yet" from "will never finish".
    pub fn mark_failed(
        &self,
        dst: &std::path::Path,
    ) {
        if let Ok(mut set) = self.failed.lock() {
            set.insert(dst.to_path_buf());
        }
    }

    /// True when this destination is known to have failed.
    #[must_use]
    pub fn is_failed(
        &self,
        dst: &std::path::Path,
    ) -> bool {
        self.failed.lock().is_ok_and(|s| s.contains(dst))
    }

    /// Record a destination as finished. Only `move` consumes this, but it is the
    /// only signal that distinguishes "written" from "reserved at full size".
    pub fn mark_complete(
        &self,
        dst: &std::path::Path,
    ) {
        if let Ok(mut set) = self.completed.lock() {
            set.insert(dst.to_path_buf());
        }
    }

    /// Was this destination finished successfully?
    pub fn is_complete(
        &self,
        dst: &std::path::Path,
    ) -> bool {
        self.completed.lock().is_ok_and(|s| s.contains(dst))
    }

    /// Queue a region to punch rather than copy. Goes straight to the writer:
    /// there is nothing to read.
    pub fn push_hole(
        &self,
        c: Chunk,
    ) {
        let mut msg = WriteMsg::Hole(c);
        loop {
            match self.write_q.push(msg) {
                | Ok(()) => return,
                | Err(m) => {
                    msg = m;
                    if self.abort.load(Ordering::Relaxed)
                        || self.writers_alive.load(Ordering::Acquire) == 0
                    {
                        return;
                    }
                    std::thread::yield_now();
                },
            }
        }
    }

    /// Snapshot of the queue depths, for the UI.
    pub fn update_queue_gauge(&self) {
        self.stats.queued.store(
            (self.read_q.len() + self.write_q.len()) as u64,
            Ordering::Relaxed,
        );
    }

    /// No more chunks will be produced.
    ///
    /// The shutdown markers must actually reach every reader, so this blocks while
    /// the queue is full — a dropped marker would leave a reader spinning forever.
    pub fn close(&self) {
        for _ in 0..self.cfg.readers {
            let mut msg = ReadMsg::Shutdown;
            loop {
                match self.read_q.push(msg) {
                    | Ok(()) => break,
                    | Err(m) => {
                        msg = m;
                        if self.abort.load(Ordering::Relaxed)
                            || self.readers_alive.load(Ordering::Acquire) == 0
                        {
                            return;
                        }
                        std::thread::yield_now();
                    },
                }
            }
        }
    }
}

/// Per-thread cache of open handles.
///
/// Keyed by `(file id, buffered?)`. Cleared whenever the reactor goes idle, which
/// is the only moment at which no in-flight operation still references a handle.
struct HandleCache {
    /// `Box`ed so the address of a handle stays valid while the vector grows —
    /// the blocking reactor holds raw pointers to entries for the duration of a
    /// queued job. That is the whole reason `vec_box` is allowed here.
    #[allow(clippy::vec_box)]
    files: Vec<Box<RawFile>>,
    map: HashMap<u64, usize>,
}

const CACHE_MAX: usize = 96;

// Handle-cache key layout: `id << 3 | kind`. The kind matters because the same
// file is opened several times with different flags — a buffered handle must never
// be confused with a direct one, or an unaligned tail read would hit
// ERROR_INVALID_PARAMETER / EINVAL.
const K_SRC: u64 = 0;
const K_SRC_BUF: u64 = 1;
const K_DST: u64 = 2;
const K_DST_BUF: u64 = 5;
const K_VER: u64 = 3;
const K_VER_BUF: u64 = 4;

#[inline]
const fn key(
    id: u64,
    kind: u64,
) -> u64 {
    (id << 3) | kind
}

impl HandleCache {
    fn new() -> Self {
        Self {
            files: Vec::with_capacity(CACHE_MAX + 8),
            map: HashMap::with_capacity(CACHE_MAX + 8),
        }
    }

    #[inline]
    fn get(
        &self,
        key: u64,
    ) -> Option<&RawFile> {
        self.map.get(&key).map(|&i| self.files[i].as_ref())
    }

    fn clear(&mut self) {
        self.files.clear();
        self.map.clear();
    }

    /// The cache is full and a new handle is needed.
    ///
    /// The caller must stop submitting until the reactor drains: only then is it
    /// safe to close the cached handles, because an in-flight operation still
    /// references one. Without this the cache grows to one handle per file ever
    /// touched and the process runs out of file descriptors.
    #[inline]
    const fn full(&self) -> bool {
        self.files.len() >= CACHE_MAX
    }

    /// Open (or fetch) a handle, returning its slot index.
    fn slot(
        &mut self,
        reactor: &mut dyn Reactor,
        key: u64,
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<usize> {
        if let Some(&i) = self.map.get(&key) {
            return Ok(i);
        }
        if self.full() && reactor.in_flight() == 0 {
            self.clear();
        }
        let f = match reactor.open(path, opts) {
            | Ok(f) => f,
            | Err(_) if opts.direct => {
                // Direct I/O is not universally supported (network shares, some
                // FUSE mounts). Fall back rather than fail the copy.
                let mut o = *opts;
                o.direct = false;
                reactor.open(path, &o)?
            },
            | Err(e) => return Err(e),
        };
        let idx = self.files.len();
        self.files.push(Box::new(f));
        self.map.insert(key, idx);
        Ok(idx)
    }
}

struct ReadOp {
    chunk: Chunk,
    buf: Buf,
}

struct WriteOp {
    ready: Ready,
    hash: u64,
}

struct VerifyOp {
    file: Arc<FileCtx>,
    hash: u64,
    len: u64,
    buf: Buf,
}

enum WOp {
    Write(WriteOp),
    Verify(VerifyOp),
}

fn opts_src(
    cfg: &EngineConfig,
    buffered: bool,
) -> OpenOpts {
    OpenOpts {
        access: Access::Read,
        create: Create::Open,
        direct: cfg.direct_io && !buffered,
        sequential: true,
        io_priority: cfg.io_priority,
        backup_semantics: true,
        ..Default::default()
    }
}

fn opts_dst(
    cfg: &EngineConfig,
    write: bool,
) -> OpenOpts {
    OpenOpts {
        access: if write {
            Access::Write
        } else {
            Access::Read
        },
        create: Create::Open,
        direct: cfg.direct_io,
        sequential: true,
        sparse: cfg.sparse && write,
        io_priority: cfg.io_priority,
        write_dac: cfg.preserve_acl && write,
        ..Default::default()
    }
}

/// Spawn the pipeline threads.
pub fn spawn(
    shared: Arc<Shared>,
    placement: AffinityPlan,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::with_capacity(shared.cfg.readers + shared.cfg.writers);
    let placement = if shared.cfg.pin_threads {
        placement
    } else {
        AffinityPlan::default()
    };
    for i in 0..shared.cfg.readers {
        let s = shared.clone();
        let cpu = placement.readers.get(i).copied();
        handles.push(
            std::thread::Builder::new()
                .name(format!("sirocco-rd-{i}"))
                .stack_size(512 * 1024)
                .spawn(move || reader_main(s, i, cpu))
                .expect("spawn reader"),
        );
    }
    for i in 0..shared.cfg.writers {
        let s = shared.clone();
        let cpu = placement.writers.get(i).copied();
        handles.push(
            std::thread::Builder::new()
                .name(format!("sirocco-wr-{i}"))
                .stack_size(512 * 1024)
                .spawn(move || writer_main(s, i, cpu))
                .expect("spawn writer"),
        );
    }
    handles
}

fn apply_affinity(
    shared: &Shared,
    target: Option<sirocco_sys::affinity::CpuTarget>,
    name: &str,
) {
    if let Some(t) = target {
        let _ = sirocco_sys::pin_current_thread(&shared.topo, t.cpu);
        if shared.cfg.verbose >= 3 {
            // Verify the placement actually took effect rather than assuming it.
            eprintln!(
                "sirocco: {name} pinned to cpu {} (running on {})",
                t.cpu,
                sirocco_sys::current_cpu()
            );
        }
        // Bind the thread's own allocations to the node it was placed on; the
        // submission path allocates (handle caches, op slots) and we would rather
        // those pages did not come from the far socket.
        let _ = sirocco_sys::set_thread_mempolicy(t.node);
    }
    if let Some(p) = shared.cfg.thread_priority {
        let _ = sirocco_sys::set_thread_priority(p);
    }
}

fn register_fixed(
    reactor: &mut dyn Reactor,
    pool: &BufferPool,
    verbose: u8,
) {
    let n = pool.capacity();
    let mut bufs = Vec::with_capacity(n);
    for i in 0..n {
        bufs.push((pool.buf_ptr(i) as usize, pool.buf_size()));
    }
    // A failure here is not fatal, but it silently costs a get_user_pages per
    // operation, so it is worth one line at -v.
    if let Err(e) = reactor.register_buffers(&bufs)
        && verbose >= 1
    {
        eprintln!("sirocco: fixed buffers not registered ({e}); using plain read/write");
    }
}

// ------------------------------------------------------------------ reader

fn reader_main(
    shared: Arc<Shared>,
    idx: usize,
    cpu: Option<sirocco_sys::affinity::CpuTarget>,
) {
    let cfg = &shared.cfg;
    apply_affinity(&shared, cpu, &format!("reader {idx}"));

    let mut reactor = match sirocco_sys::make_reactor(cfg.reactor, cfg.overlap * 2, cfg.overlap) {
        | Ok(r) => r,
        | Err(e) => {
            shared.error(format!("reader {idx}: cannot create reactor: {e}"));
            shared.readers_alive.fetch_sub(1, Ordering::AcqRel);
            return;
        },
    };
    register_fixed(reactor.as_mut(), &shared.pool, cfg.verbose);

    let mut cache = HandleCache::new();
    let mut slab: Slab<ReadOp> = Slab::with_capacity(cfg.overlap * 2 + 4);
    let mut comps: Vec<sirocco_sys::Completion> = Vec::with_capacity(cfg.overlap * 2);
    let mut shutting_down = false;

    let mut cancelled = false;
    loop {
        if shared.abort.load(Ordering::Relaxed) {
            if !cancelled {
                // Drop everything still in the device queue instead of waiting
                // for it: an interrupt should stop within milliseconds.
                reactor.cancel();
                cancelled = true;
            }
            shutting_down = true;
        }

        // A full cache can only be cleared once nothing is in flight, so drain
        // here rather than waiting for a miss that can never happen: gating the
        // fill loop on `!full()` without this is a deadlock.
        if cache.full() && reactor.in_flight() == 0 {
            cache.clear();
        }

        // --- fill the reactor
        while !shutting_down && reactor.in_flight() < cfg.overlap && !cache.full() {
            let Some(msg) = shared.read_q.pop() else {
                break;
            };
            let ReadMsg::Chunk(chunk) = msg else {
                shutting_down = true;
                break;
            };

            let Some(buf) = shared.pool.take(256) else {
                // Pool exhausted: put the chunk back and drain completions.
                // Dropping it here would leave the file's pending count short
                // of zero — never finalised, never reported.
                let mut back = ReadMsg::Chunk(chunk);
                loop {
                    match shared.read_q.push(back) {
                        | Ok(()) => break,
                        | Err(m) => {
                            back = m;
                            if shared.abort.load(Ordering::Relaxed)
                                || shared.writers_alive.load(Ordering::Acquire) == 0
                            {
                                break;
                            }
                            std::thread::yield_now();
                        },
                    }
                }
                break;
            };

            let k = key(
                chunk.file.id,
                if chunk.tail {
                    K_SRC_BUF
                } else {
                    K_SRC
                },
            );
            let opts = opts_src(cfg, chunk.tail);
            let slot = match cache.slot(reactor.as_mut(), k, &chunk.file.src, &opts) {
                | Ok(s) => s,
                | Err(e) => {
                    shared.fail(&chunk.file, format!("open: {e}"));
                    drop(buf);
                    complete_file(shared.as_ref(), &chunk.file, &cache);
                    continue;
                },
            };

            let Some(op_idx) = slab.alloc(ReadOp {
                chunk: chunk.clone(),
                buf,
            }) else {
                // Dropping the chunk would leave the file waiting for a block
                // nobody will ever read. Fail it and settle the counter.
                shared.error(format!(
                    "reader: out of operation slots for {}",
                    chunk.file.src.display()
                ));
                chunk.file.fail();
                complete_file(shared.as_ref(), &chunk.file, &cache);
                continue;
            };
            let op = unsafe { slab.get(op_idx) };
            let f = &cache.files[slot];
            let r = unsafe {
                reactor.read(
                    f,
                    op.buf.ptr(),
                    chunk.rlen as usize,
                    chunk.src_off,
                    u64::from(op_idx),
                )
            };
            if r.is_ok() {
                shared.stats.readers_active.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(e) = r {
                let op = unsafe { slab.take(op_idx) };
                shared.fail(&chunk.file, format!("read: {e}"));
                drop(op.buf);
                complete_file(shared.as_ref(), &chunk.file, &cache);
            }
        }

        if reactor.in_flight() == 0 {
            if shutting_down {
                break;
            }
            // Nothing in flight and nothing queued: the feeders are between files.
            // Yielding here would burn a whole core per I/O thread (16 of them on a
            // 8-core box); a short sleep costs at most 50 us of latency on a path
            // that is already waiting on a syscall.
            std::thread::sleep(std::time::Duration::from_micros(50));
            continue;
        }

        comps.clear();
        let want = reactor.in_flight().min(cfg.overlap);
        if let Err(e) = reactor.poll(want, &mut comps) {
            shared.error(format!("reader poll: {e}"));
            // The ops still in `slab` reference buffers the device may still be
            // reading into. Dropping them returns those buffers to the pool,
            // where another file would take them and be corrupted. Give up on
            // the whole reactor instead: cancel, then leak the slab and the
            // handle cache so nothing is recycled while the kernel owns it.
            reactor.cancel();
            std::mem::forget(slab);
            std::mem::forget(cache);
            return;
        }

        // `comps` is a reusable batch buffer: draining is how it is emptied
        // without reallocating on every completion round.
        #[allow(clippy::iter_with_drain)]
        for c in comps.drain(..) {
            shared.stats.readers_active.fetch_sub(1, Ordering::Relaxed);
            let op_idx = c.user as u32;
            let mut op = unsafe { slab.take(op_idx) };
            if c.res < 0 {
                shared.error(format!(
                    "read {} @{}: {}",
                    op.chunk.file.src.display(),
                    op.chunk.src_off,
                    os_err(c.err_code())
                ));
                shared.fail(&op.chunk.file, "short read".into());
                drop(op.buf);
                complete_file(shared.as_ref(), &op.chunk.file, &cache);
                continue;
            }
            if (c.bytes() as u64) < op.chunk.rlen {
                shared.fail(&op.chunk.file, "short read".into());
                shared.error(format!(
                    "short read {} @{}: {} of {} bytes",
                    op.chunk.file.src.display(),
                    op.chunk.src_off,
                    c.bytes(),
                    op.chunk.rlen
                ));
                drop(op.buf);
                complete_file(shared.as_ref(), &op.chunk.file, &cache);
                continue;
            }
            shared
                .stats
                .bytes_read
                .fetch_add(op.chunk.rlen, Ordering::Relaxed);

            // Zero the padding so a padded write cannot leak stale buffer data.
            if op.chunk.tail && op.chunk.wlen > op.chunk.len {
                op.buf
                    .zero_range(op.chunk.len as usize, op.chunk.wlen as usize);
            }

            // The hash is needed for verification, and also for the persistent
            // index — in both cases it is computed on data that is already in
            // memory, so it costs a fraction of the I/O it describes.
            let need_hash = cfg.verify != VerifyMode::None || op.chunk.file.hashes.is_some();
            let hash = if need_hash {
                cfg.hash.hash(op.buf.slice(0, op.chunk.len as usize))
            } else {
                0
            };
            let all_zero = cfg.sparse && is_zero(op.buf.slice(0, op.chunk.len as usize));

            if !shared.push_ready(Ready {
                chunk: op.chunk.clone(),
                buf: op.buf,
                hash,
                all_zero,
            }) {
                // No writer will ever take this chunk. Fail the file and settle
                // the op so the counter can reach zero.
                shared.error(format!(
                    "read {} @{}: writers stopped",
                    op.chunk.file.src.display(),
                    op.chunk.src_off
                ));
                op.chunk.file.fail();
                complete_file(shared.as_ref(), &op.chunk.file, &cache);
            }
        }
    }

    cache.clear();
    if shared.readers_alive.fetch_sub(1, Ordering::AcqRel) == 1 {
        for _ in 0..cfg.writers {
            let mut msg = WriteMsg::Shutdown;
            loop {
                match shared.write_q.push(msg) {
                    | Ok(()) => break,
                    | Err(m) => {
                        msg = m;
                        // On abort the writers are already leaving; blocking here
                        // would hang the join. The same is true when every writer
                        // has died for its own reasons: nobody will drain the
                        // queue, so waiting for room in it is waiting forever.
                        if shared.abort.load(Ordering::Relaxed)
                            || shared.writers_alive.load(Ordering::Acquire) == 0
                        {
                            return;
                        }
                        std::thread::yield_now();
                    },
                }
            }
        }
    }
}

// ------------------------------------------------------------------ writer

fn writer_main(
    shared: Arc<Shared>,
    idx: usize,
    cpu: Option<sirocco_sys::affinity::CpuTarget>,
) {
    let cfg = &shared.cfg;
    apply_affinity(&shared, cpu, &format!("writer {idx}"));

    let mut reactor = match sirocco_sys::make_reactor(cfg.reactor, cfg.overlap * 2, cfg.overlap) {
        | Ok(r) => r,
        | Err(e) => {
            shared.error(format!("writer {idx}: cannot create reactor: {e}"));
            shared.writers_alive.fetch_sub(1, Ordering::AcqRel);
            return;
        },
    };
    register_fixed(reactor.as_mut(), &shared.pool, cfg.verbose);

    let mut cache = HandleCache::new();
    let mut slab: Slab<WOp> = Slab::with_capacity(cfg.overlap * 4 + 8);
    let mut comps: Vec<sirocco_sys::Completion> = Vec::with_capacity(cfg.overlap * 4);
    let mut shutting_down = false;
    let verify_mode = cfg.verify;

    let mut cancelled = false;
    loop {
        if shared.abort.load(Ordering::Relaxed) {
            if !cancelled {
                reactor.cancel();
                cancelled = true;
            }
            shutting_down = true;
        }

        if cache.full() && reactor.in_flight() == 0 {
            cache.clear();
        }

        while !shutting_down && reactor.in_flight() < cfg.overlap && !cache.full() {
            let Some(msg) = shared.write_q.pop() else {
                break;
            };
            let ready = match msg {
                | WriteMsg::Shutdown => {
                    shutting_down = true;
                    break;
                },
                | WriteMsg::Hole(c) => {
                    // The source does not allocate this range, so neither should
                    // the destination. One metadata call replaces a read and a
                    // write of the whole chunk.
                    // The punch runs on a *buffered* handle, so it needs the
                    // buffered key: sharing `K_DST` with the direct handle would
                    // hand whichever opened first to the other path.
                    let k = key(c.file.id, K_DST_BUF);
                    let mut opts = opts_dst(cfg, true);
                    opts.direct = false;
                    match cache.slot(reactor.as_mut(), k, &c.file.dst, &opts) {
                        | Ok(slot) => {
                            let f = &cache.files[slot];
                            if let Err(e) = f.zero_range(c.dst_off, c.wlen) {
                                // The range keeps whatever the previous version
                                // left there, so the file is not correct.
                                shared.error(format!(
                                    "punch {} @{}: {e}",
                                    c.file.dst.display(),
                                    c.dst_off
                                ));
                                c.file.fail();
                                complete_file(shared.as_ref(), &c.file, &cache);
                                continue;
                            }
                            shared.stats.bytes_saved.fetch_add(c.len, Ordering::Relaxed);
                            // Not counted as written: the whole point is that these
                            // bytes never touch the device.
                            shared.stats.bytes_holes.fetch_add(c.len, Ordering::Relaxed);
                            c.file.written.fetch_add(c.len, Ordering::Relaxed);
                            complete_file(shared.as_ref(), &c.file, &cache);
                        },
                        | Err(e) => {
                            shared.fail(&c.file, format!("punch: {e}"));
                            complete_file(shared.as_ref(), &c.file, &cache);
                        },
                    }
                    continue;
                },
                | WriteMsg::Ready(r) => r,
            };

            let k = key(
                ready.chunk.file.id,
                if ready.chunk.buffered {
                    K_DST_BUF
                } else {
                    K_DST
                },
            );
            let mut opts = opts_dst(cfg, true);
            if ready.chunk.buffered {
                opts.direct = false;
            }
            if ready.chunk.create {
                // Single-chunk file: the writer owns creation, so the scheduler
                // never has to open it.
                opts.create = Create::Create;
            }
            let dst = ready.chunk.file.dst.clone();
            let opened = cache
                .slot(reactor.as_mut(), k, &dst, &opts)
                .or_else(|first| {
                    // A directory where the source has a file blocks the copy,
                    // and in mirror mode the destination is ours to correct.
                    // Leaving it turns one mismatch into a permanent failure that
                    // repeats on every run.
                    if !(shared.cfg.mode == sirocco_core::plan::Mode::Sync
                        && shared.cfg.delete_extra
                        && dst.is_dir())
                    {
                        return Err(first);
                    }
                    std::fs::remove_dir_all(&dst)?;
                    cache.slot(reactor.as_mut(), k, &dst, &opts)
                });
            let slot = match opened {
                | Ok(s) => s,
                | Err(e) => {
                    shared.fail(&ready.chunk.file, format!("open destination: {e}"));
                    drop(ready.buf);
                    complete_file(shared.as_ref(), &ready.chunk.file, &cache);
                    continue;
                },
            };

            // All-zero chunk into a sparse destination: punch a hole instead of
            // moving the bytes through the page cache.
            if ready.all_zero && ready.chunk.file.sparse {
                let f = &cache.files[slot];
                if let Err(e) = f.zero_range(ready.chunk.dst_off, ready.chunk.wlen) {
                    // Without hole support the range keeps the previous version's
                    // bytes, so this is a failed copy, not a saved one.
                    shared.error(format!(
                        "punch {} @{}: {e}",
                        ready.chunk.file.dst.display(),
                        ready.chunk.dst_off
                    ));
                    ready.chunk.file.fail();
                    drop(ready.buf);
                    complete_file(shared.as_ref(), &ready.chunk.file, &cache);
                    continue;
                }
                shared
                    .stats
                    .bytes_saved
                    .fetch_add(ready.chunk.wlen, Ordering::Relaxed);
                shared
                    .stats
                    .bytes_written
                    .fetch_add(ready.chunk.wlen, Ordering::Relaxed);
                ready
                    .chunk
                    .file
                    .written
                    .fetch_add(ready.chunk.len, Ordering::Relaxed);
                drop(ready.buf);
                complete_file(shared.as_ref(), &ready.chunk.file, &cache);
                continue;
            }

            let Some(op_idx) = slab.alloc(WOp::Write(WriteOp {
                hash: ready.hash,
                ready,
            })) else {
                shared.error("writer: out of operation slots".into());
                break;
            };
            let op = unsafe { slab.get(op_idx) };
            let WOp::Write(w) = op else { unreachable!() };
            let f = &cache.files[slot];
            let r = unsafe {
                reactor.write(
                    f,
                    w.ready.buf.ptr(),
                    w.ready.chunk.wlen as usize,
                    w.ready.chunk.dst_off,
                    u64::from(op_idx),
                )
            };
            if r.is_ok() {
                shared.stats.writers_active.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(e) = r {
                let WOp::Write(w) = (unsafe { slab.take(op_idx) }) else {
                    unreachable!()
                };
                shared.fail(&w.ready.chunk.file, format!("write: {e}"));
                drop(w.ready.buf);
                complete_file(shared.as_ref(), &w.ready.chunk.file, &cache);
            }
        }

        if reactor.in_flight() == 0 {
            if shutting_down {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
            continue;
        }

        comps.clear();
        let want = reactor.in_flight().min(cfg.overlap);
        if let Err(e) = reactor.poll(want, &mut comps) {
            shared.error(format!("writer poll: {e}"));
            // Same reasoning as the reader loop: a buffer still owned by the
            // device must not go back to the pool.
            reactor.cancel();
            std::mem::forget(slab);
            std::mem::forget(cache);
            return;
        }

        // `comps` is a reusable batch buffer: draining is how it is emptied
        // without reallocating on every completion round.
        #[allow(clippy::iter_with_drain)]
        for c in comps.drain(..) {
            shared.stats.writers_active.fetch_sub(1, Ordering::Relaxed);
            let op_idx = c.user as u32;
            match unsafe { slab.take(op_idx) } {
                | WOp::Write(w) => {
                    if c.res < 0 {
                        shared.error(format!(
                            "write {} @{}: {}",
                            w.ready.chunk.file.dst.display(),
                            w.ready.chunk.dst_off,
                            os_err(c.err_code())
                        ));
                        w.ready.chunk.file.fail();
                        drop(w.ready.buf);
                        complete_file(shared.as_ref(), &w.ready.chunk.file, &cache);
                        continue;
                    }
                    // A short write (ENOSPC usually reports success with a partial
                    // count) would otherwise be counted as done, and `finalize`
                    // would then extend the file to full length over a zero tail.
                    if (c.bytes() as u64) < w.ready.chunk.wlen {
                        shared.error(format!(
                            "short write {} @{}: {} of {} bytes",
                            w.ready.chunk.file.dst.display(),
                            w.ready.chunk.dst_off,
                            c.bytes(),
                            w.ready.chunk.wlen
                        ));
                        w.ready.chunk.file.fail();
                        drop(w.ready.buf);
                        complete_file(shared.as_ref(), &w.ready.chunk.file, &cache);
                        continue;
                    }
                    shared
                        .stats
                        .bytes_written
                        .fetch_add(w.ready.chunk.wlen, Ordering::Relaxed);
                    w.ready
                        .chunk
                        .file
                        .written
                        .fetch_add(w.ready.chunk.len, Ordering::Relaxed);
                    record_hash(&w.ready.chunk, w.hash);

                    if !verify_chunk(verify_mode, &w.ready.chunk) {
                        drop(w.ready.buf);
                        complete_file(shared.as_ref(), &w.ready.chunk.file, &cache);
                        continue;
                    }

                    // Re-read what we just wrote and compare hashes.
                    let chunk = w.ready.chunk.clone();
                    let hash = w.hash;
                    drop(w.ready.buf);
                    // Verification is a promise; block for the buffer instead of
                    // quietly downgrading to an unchecked write.
                    let Some(vbuf) = shared.pool.take(1 << 16) else {
                        chunk
                            .file
                            .fail_with("verify: no buffer available".to_string());
                        shared.error(format!(
                            "verify: no buffer available for {}",
                            chunk.file.dst.display()
                        ));
                        chunk.file.fail();
                        complete_file(shared.as_ref(), &chunk.file, &cache);
                        continue;
                    };
                    let mut vopts = opts_dst(cfg, false);
                    if chunk.tail {
                        vopts.direct = false;
                    }
                    let vkey = key(
                        chunk.file.id,
                        if chunk.tail {
                            K_VER_BUF
                        } else {
                            K_VER
                        },
                    );
                    let vslot = match cache.slot(reactor.as_mut(), vkey, &chunk.file.dst, &vopts) {
                        | Ok(s) => s,
                        | Err(e) => {
                            shared.fail(&chunk.file, format!("open for verify: {e}"));
                            drop(vbuf);
                            complete_file(shared.as_ref(), &chunk.file, &cache);
                            continue;
                        },
                    };
                    let Some(vidx) = slab.alloc(WOp::Verify(VerifyOp {
                        file: chunk.file.clone(),
                        hash,
                        len: chunk.len,
                        buf: vbuf,
                    })) else {
                        shared.error("verify: out of operation slots".into());
                        complete_file(shared.as_ref(), &chunk.file, &cache);
                        continue;
                    };
                    let vo = unsafe { slab.get(vidx) };
                    let WOp::Verify(vo) = vo else { unreachable!() };
                    let f = &cache.files[vslot];
                    let r = unsafe {
                        reactor.read(
                            f,
                            vo.buf.ptr(),
                            chunk.len as usize,
                            chunk.dst_off,
                            u64::from(vidx),
                        )
                    };
                    if r.is_ok() {
                        // Counted against the writer gauge too: it is an
                        // outstanding operation on this thread, and its completion
                        // decrements the same counter.
                        shared.stats.writers_active.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Err(e) = r {
                        let WOp::Verify(vo) = (unsafe { slab.take(vidx) }) else {
                            unreachable!()
                        };
                        shared.error(format!(
                            "verify read {} @{} len={} tail={} direct={}: {e}",
                            chunk.file.dst.display(),
                            chunk.dst_off,
                            chunk.len,
                            chunk.tail,
                            vopts.direct
                        ));
                        chunk.file.fail();
                        drop(vo.buf);
                        complete_file(shared.as_ref(), &chunk.file, &cache);
                    }
                },
                | WOp::Verify(v) => {
                    if c.res < 0 {
                        shared.error(format!(
                            "verify read {}: {}",
                            v.file.dst.display(),
                            os_err(c.err_code())
                        ));
                        v.file.fail();
                    } else {
                        let got = c.bytes() as u64;
                        if got == v.len {
                            let h = cfg.hash.hash(v.buf.slice(0, got as usize));
                            if h == v.hash {
                                shared
                                    .stats
                                    .bytes_verified
                                    .fetch_add(got, Ordering::Relaxed);
                            } else {
                                shared.error(format!(
                                    "verification mismatch: {} (chunk at {}: {h:016x} != {:016x})",
                                    v.file.dst.display(),
                                    v.len,
                                    v.hash
                                ));
                                v.file.fail();
                            }
                        } else {
                            shared.error(format!(
                                "verify short read {}: {got} of {} bytes",
                                v.file.dst.display(),
                                v.len
                            ));
                            v.file.fail();
                        }
                    }
                    drop(v.buf);
                    complete_file(shared.as_ref(), &v.file, &cache);
                },
            }
        }
    }

    cache.clear();
    shared.writers_alive.fetch_sub(1, Ordering::AcqRel);
}

/// Store a block hash into the file's index buffer, if it has one.
#[inline]
fn record_hash(
    chunk: &Chunk,
    hash: u64,
) {
    if let Some(h) = &chunk.file.hashes
        && let Ok(mut v) = h.lock()
        && let Some(slot) = v.get_mut(chunk.block as usize)
    {
        *slot = hash;
    }
}

/// A chunk finished: decrement the file's pending count and finalize on zero.
impl Shared {
    /// Record a per-file failure in both places that need it: the summary list
    /// (for the report) and the file itself (for the completion event).
    fn fail(
        &self,
        file: &FileCtx,
        msg: String,
    ) {
        self.error(msg.clone());
        file.fail_with(msg);
    }
}

fn complete_file(
    shared: &Shared,
    file: &Arc<FileCtx>,
    cache: &HandleCache,
) {
    if file.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    if file.failed.load(Ordering::Acquire) {
        shared.mark_failed(&file.dst);
        shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = &shared.cfg.events {
            tx.send(crate::config::Event::Failed {
                rel: file.rel.clone(),
                error: file.error().unwrap_or_else(|| "copy failed".to_string()),
            });
        }
        return;
    }
    if !finalize(shared, file, cache) {
        // `finalize` already counted the failure and recorded why; what it must
        // not do is let the listener hear `Done` for a file that is not finished.
        if let Some(tx) = &shared.cfg.events {
            tx.send(crate::config::Event::Failed {
                rel: file.rel.clone(),
                error: file
                    .error()
                    .unwrap_or_else(|| "finalize failed".to_string()),
            });
        }
        return;
    }
    if let Some(tx) = &shared.cfg.events {
        tx.send(crate::config::Event::Done {
            rel: file.rel.clone(),
            size: file.size,
            // Bytes that never had to be written (delta blocks that matched).
            saved: file
                .size
                .saturating_sub(file.written.load(Ordering::Relaxed)),
        });
    }
}

/// Two timestamps are "the same" if they agree within the tolerance the skip
/// decision uses: coarse filesystems only store whole seconds.
fn within_two_seconds(
    a: std::time::SystemTime,
    b: std::time::SystemTime,
) -> bool {
    let tol = std::time::Duration::from_secs(2);
    a.duration_since(b).map_or_else(
        |_| b.duration_since(a).is_ok_and(|d| d <= tol),
        |d| d <= tol,
    )
}

/// Which chunks a verify mode actually re-reads.
///
/// `Sample` is a promise of three blocks -- head, middle, tail -- not a full
/// pass. Treating it as a full pass made the cheap mode the expensive one.
fn verify_chunk(
    mode: VerifyMode,
    c: &Chunk,
) -> bool {
    match mode {
        | VerifyMode::Hash => true,
        | VerifyMode::Sample => {
            let mid = c.file.size / 2;
            let end = c.src_off + c.len;
            c.src_off == 0 || (c.src_off <= mid && end >= mid) || end >= c.file.size
        },
        // `size` and `size-mtime` are checked once, against the finished file.
        | VerifyMode::None | VerifyMode::Size | VerifyMode::SizeMtime => false,
    }
}

fn finalize(
    shared: &Shared,
    file: &FileCtx,
    cache: &HandleCache,
) -> bool {
    // Nothing below may claim the file is good until every fallible step has
    // run: `mark_complete` is what lets `Mode::Move` delete the source, and a
    // destination left long by the padded tail is not a copy.
    let k = key(file.id, K_DST);
    let need_trunc = file.force_trunc || (!file.small && !file.size.is_multiple_of(shared.align));
    let need_touch = shared.cfg.keep_times && file.mtime.is_some();
    let need_mode = shared.cfg.preserve_permissions && file.mode != 0;
    let need_handle =
        need_trunc || need_touch || need_mode || shared.cfg.sync_at_end || file.clear_sparse_at_end;
    if !need_handle {
        shared.mark_complete(&file.dst);
        write_index(shared, file);
        record_journal(shared, file);
        shared.stats.files_done.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    // Prefer the handle we already hold; fall back to opening a fresh one.
    let fresh;
    // Small files are written through a buffered handle, so the mtime update must
    // find that one too — otherwise every small file pays an extra open.
    let f: &RawFile = if let Some(h) = cache.get(k).or_else(|| cache.get(key(file.id, K_DST_BUF))) {
        h
    } else {
        let opts = OpenOpts {
            access: Access::Write,
            create: Create::Open,
            write_dac: shared.cfg.preserve_acl,
            ..Default::default()
        };
        match RawFile::open(&file.dst, &opts) {
            | Ok(h) => {
                fresh = h;
                &fresh
            },
            | Err(e) => {
                shared.error(format!("finalize {}: {e}", file.dst.display()));
                shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
                file.fail();
                return false;
            },
        }
    };

    if need_trunc && let Err(e) = f.set_len(file.size) {
        shared.error(format!("truncate {}: {e}", file.dst.display()));
        shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
        file.fail();
        return false;
    }
    if file.clear_sparse_at_end {
        // Undo the hole-based reservation now that every byte is in place. A
        // failure leaves the destination looking sparse to `du` and to backup
        // tools, which is worth a line in the report but not a failed file.
        if let Err(e) = f.set_sparse_flag(false) {
            shared.error(format!("clear sparse {}: {e}", file.dst.display()));
        }
    }
    if shared.cfg.sync_at_end
        && let Err(e) = f.flush()
    {
        shared.error(format!("flush {}: {e}", file.dst.display()));
    }
    if shared.cfg.preserve_acl {
        // Needs the source path, not just the handle: the security APIs take one.
        // Best effort by design — a DACL that cannot be mapped across volumes is
        // common and is why `--acl` is opt-in.
        if let Err(e) = sirocco_sys::copy_acl(&file.src, f) {
            shared.error(format!("copy acl {}: {e}", file.dst.display()));
        }
    }
    if shared.cfg.preserve_xattrs {
        // Unlike ACLs, `--xattrs` is an explicit request for the user's own
        // attributes, so a failure is worth a line in the report. The engine
        // already refused the option on a platform without them, once.
        if let Err(e) = sirocco_sys::copy_xattrs(&file.src, f) {
            shared.error(format!("copy xattrs {}: {e}", file.dst.display()));
        }
    }
    if need_mode && let Err(e) = sirocco_sys::set_mode(f, file.mode) {
        shared.error(format!("set mode {}: {e}", file.dst.display()));
    }
    // The timestamp is not decoration: the next run's skip decision is made
    // against it, and `move` deletes the source once this file is complete. A
    // destination with the wrong mtime is a wrong copy.
    if need_touch
        && let Some(t) = file.mtime
        && let Err(e) = f.set_modified(t)
    {
        shared.error(format!("set mtime {}: {e}", file.dst.display()));
        shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
        file.fail();
        return false;
    }
    // `size` and `size-mtime` verify nothing on the data path: neither mode reads
    // the destination, and the pipeline compares nothing for them. Check the
    // finished file instead -- last, after the timestamp is restored, or it would
    // be comparing against "now".
    if matches!(shared.cfg.verify, VerifyMode::Size | VerifyMode::SizeMtime) {
        let verdict = std::fs::metadata(&file.dst).map(|md| {
            let size_ok = md.len() == file.size;
            let time_ok = shared.cfg.verify != VerifyMode::SizeMtime
                || file
                    .mtime
                    .is_none_or(|t| md.modified().is_ok_and(|m| within_two_seconds(m, t)));
            size_ok && time_ok
        });
        match verdict {
            | Ok(true) => {},
            | Ok(false) => {
                shared.error(format!(
                    "verify {}: destination does not match the source",
                    file.dst.display()
                ));
                shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
                file.fail();
                return false;
            },
            | Err(e) => {
                shared.error(format!("verify {}: {e}", file.dst.display()));
                shared.stats.files_failed.fetch_add(1, Ordering::Relaxed);
                file.fail();
                return false;
            },
        }
    }
    shared.mark_complete(&file.dst);
    write_index(shared, file);
    record_journal(shared, file);
    shared.stats.files_done.fetch_add(1, Ordering::Relaxed);
    true
}

/// Persist the block hashes collected while copying.
fn write_index(
    shared: &Shared,
    file: &FileCtx,
) {
    let Some(h) = &file.hashes else { return };
    let block = shared.cfg.delta.block.max(1);
    let expected = file.size.div_ceil(block) as usize;
    let hashes = match h.lock() {
        | Ok(g) => g.clone(),
        | Err(_) => return,
    };
    // A zero hash means the block never reported; treat it as "index incomplete"
    // rather than writing something that would later be trusted.
    if hashes.len() != expected || hashes.contains(&0) {
        return;
    }
    let idx = BlockIndex::new(shared.cfg.hash, block, file.size, file.mtime_ns, hashes);
    if let Err(e) = shared.index.store(&file.dst, &idx)
        && shared.cfg.verbose >= 2
    {
        shared.error(format!("index {}: {e}", file.dst.display()));
    }
}

fn record_journal(
    shared: &Shared,
    file: &FileCtx,
) {
    let Some(j) = &shared.journal else { return };
    let rel = file
        .dst
        .strip_prefix(&shared.dst_root)
        .unwrap_or(&file.dst)
        .to_string_lossy()
        .replace('\\', "/");
    j.record(&rel, file.size, file.mtime_ns);
}

/// Finalize a file from outside a pipeline thread (used by the delta path).
pub fn complete_external(
    shared: &Shared,
    file: &Arc<FileCtx>,
) {
    let cache = HandleCache {
        files: Vec::new(),
        map: HashMap::new(),
    };
    complete_file(shared, file, &cache);
}

/// Zero-scan used by the sparse and delta paths.
#[inline]
#[must_use]
pub fn is_zero_pub(b: &[u8]) -> bool {
    is_zero(b)
}

#[inline]
fn is_zero(b: &[u8]) -> bool {
    // Word-at-a-time scan; ~8 GB/s, cheap relative to the write it can replace.
    let (pre, mid, post) = unsafe { b.align_to::<u64>() };
    pre.iter().chain(post.iter()).all(|&x| x == 0) && mid.iter().all(|&x| x == 0)
}

#[inline]
fn os_err(code: u32) -> String {
    io::Error::from_raw_os_error(code as i32).to_string()
}
