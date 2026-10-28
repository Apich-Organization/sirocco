//! Message types that travel between the pipeline stages.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use sirocco_sys::buf::Buf;

/// Shared state for one destination file. Every chunk of the file points here.
pub struct FileCtx {
    /// Unique, monotonically increasing id — used as the key of the per-thread
    /// handle caches (a `PathBuf` would work too but hashing a path per chunk is
    /// pure overhead).
    pub id: u64,
    /// Source path, for error messages.
    pub src: PathBuf,
    /// Destination path; also the handle-cache key base.
    pub dst: PathBuf,
    /// Logical size of the file.
    pub size: u64,
    /// Modification time to restore on the destination.
    pub mtime: Option<SystemTime>,
    /// Chunks not yet written. The file is finished when this hits zero.
    pub pending: AtomicU32,
    /// Logical bytes handed to a writer so far, for the delta's "saved" figure.
    pub written: AtomicU64,
    /// Set by any thread that hits an error on this file.
    pub failed: AtomicBool,
    /// Why the file failed, for the per-file event. The summary keeps the full
    /// list; this is the one line a UI shows next to the file name.
    err: Mutex<Option<String>>,
    /// Destination was created (and preallocated) by the scheduler.
    pub preallocated: bool,
    /// Destination should be sparse (holes punched for zero chunks).
    pub sparse: bool,
    /// The destination was extended through a hole purely to avoid NTFS
    /// zero-filling the tail; the flag should be cleared once it is written.
    pub clear_sparse_at_end: bool,
    /// Skip the mtime restore (e.g. when the destination is a special file).
    pub skip_times: bool,
    /// Per-block source hashes, collected while copying so the persistent index
    /// can be written without a second pass over the data. `None` when indexing
    /// is off.
    pub hashes: Option<std::sync::Mutex<Vec<u64>>>,
    /// Source-relative path, so the completion event can name the file the way
    /// the feed displays it without a second lookup.
    pub rel: PathBuf,
    /// Modification time the destination will end up with, in ns. Recorded in the
    /// index so the next run can tell whether the destination still matches.
    pub mtime_ns: u128,
    /// Small file: written with buffered I/O at its exact length, with no
    /// preallocation and no truncation. Saves three metadata round trips per file,
    /// which dominates the cost when the average file is a few kilobytes.
    pub small: bool,
    /// Permission bits (Unix) or attributes (Windows) to apply at the end.
    pub mode: u32,
    /// The destination existed and was longer than the source: it must be
    /// truncated even when the size happens to be sector aligned.
    pub force_trunc: bool,
}

impl FileCtx {
    /// Mark the file failed, keeping the reason for the completion event.
    pub fn fail_with(
        &self,
        msg: String,
    ) {
        if let Ok(mut g) = self.err.lock()
            && g.is_none()
        {
            *g = Some(msg);
        }
        self.failed.store(true, Ordering::Relaxed);
    }

    /// Mark the file failed without a specific reason.
    pub fn fail(&self) {
        self.fail_with("copy failed".to_string());
    }

    /// The reason recorded by [`FileCtx::fail_with`], if any.
    pub fn error(&self) -> Option<String> {
        self.err.lock().ok().and_then(|g| g.clone())
    }

    /// Build the shared state for one destination file.
    #[must_use]
    pub const fn new(
        id: u64,
        src: PathBuf,
        dst: PathBuf,
        rel: PathBuf,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> Self {
        Self {
            id,
            src,
            dst,
            rel,
            size,
            mtime,
            pending: AtomicU32::new(0),
            written: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            err: Mutex::new(None),
            preallocated: false,
            sparse: false,
            clear_sparse_at_end: false,
            skip_times: false,
            hashes: None,
            mtime_ns: 0,
            small: false,
            mode: 0,
            force_trunc: false,
        }
    }
}

/// One unit of work: read `rlen` bytes from `src_off`, write `wlen` bytes to
/// `dst_off`.
#[derive(Clone)]
pub struct Chunk {
    /// The file this chunk belongs to.
    pub file: Arc<FileCtx>,
    /// Offset to read from.
    pub src_off: u64,
    /// Read length, padded up to the device alignment.
    pub rlen: u64,
    /// Offset to write to.
    pub dst_off: u64,
    /// Write length, padded up to the device alignment.
    pub wlen: u64,
    /// Logical bytes actually contained in the file.
    pub len: u64,
    /// The chunk touches EOF: the source must be read with a buffered handle and
    /// the tail of the buffer zero-filled.
    pub tail: bool,
    /// Index of this chunk within the file, used to place its hash in the index.
    pub block: u32,
    /// Read and write with buffered handles, using the exact length. Set for small
    /// files, where direct I/O's alignment rules cost more than they save.
    pub buffered: bool,
    /// The writer creates the destination. Only set for single-chunk files, where
    /// one create cannot race with another chunk's truncate; it saves the
    /// scheduler an open/close of every small file.
    pub create: bool,
}

impl Chunk {
    /// Split `len` bytes at `src_off` / `dst_off`, padding the lengths up to the
    /// device alignment unless the chunk ends at EOF.
    #[inline]
    pub fn new(
        file: Arc<FileCtx>,
        src_off: u64,
        dst_off: u64,
        len: u64,
        align: u64,
    ) -> Self {
        let tail = src_off + len >= file.size;
        let rlen = if tail {
            len
        } else {
            sirocco_sys::align_up(len, align)
        };
        let wlen = sirocco_sys::align_up(len, align);
        Self {
            file,
            src_off,
            rlen,
            dst_off,
            wlen,
            len,
            tail,
            block: 0,
            buffered: false,
            create: false,
        }
    }

    /// A chunk written at its exact length with buffered handles: no padding, no
    /// truncation, no alignment constraints.
    pub const fn exact(
        file: Arc<FileCtx>,
        off: u64,
        len: u64,
    ) -> Self {
        Self {
            file,
            src_off: off,
            rlen: len,
            dst_off: off,
            wlen: len,
            len,
            tail: true,
            block: 0,
            buffered: true,
            create: true,
        }
    }
}

/// A chunk whose data is already in memory, ready to be written.
pub struct Ready {
    /// Which chunk the data belongs to.
    pub chunk: Chunk,
    /// The buffer holding it; returns to the pool when the writer drops it.
    pub buf: Buf,
    /// Hash of the logical bytes, computed by the reader when verification is on.
    pub hash: u64,
    /// The buffer holds only zeros — the writer may punch a hole instead.
    pub all_zero: bool,
}

/// What a scheduler sends to a reader.
pub enum ReadMsg {
    /// Read this chunk.
    Chunk(Chunk),
    /// The producer is done; exit once in-flight work drains.
    Shutdown,
}

/// What a reader (or the sparse path) sends to a writer.
pub enum WriteMsg {
    /// Write this buffer at this offset.
    Ready(Ready),
    /// A region the source does not allocate: punch it instead of copying bytes.
    /// No buffer is involved, so it never touches the reader threads.
    Hole(Chunk),
    /// The readers are done; exit once in-flight work drains.
    Shutdown,
}
