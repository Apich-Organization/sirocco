//! Sirocco platform layer.
//!
//! Everything here is deliberately raw: no async runtime, no buffered wrapper, no
//! portability shims that hide the cost. Each OS gets a native path:
//!
//! * Windows — `CreateFileW` with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`,
//!   `ReadFile`/`WriteFile` on an IOCP, `FSCTL_GET_RETRIEVAL_POINTERS` for physical
//!   layout, `SetFileValidData` for instant allocation.
//! * Linux — `open(O_DIRECT)` and a hand-rolled `io_uring` (SQ/CQ mmap, registered
//!   buffers/files, optional SQPOLL/IOPOLL), `FIEMAP`, `fallocate`, `ioprio_set`.
//!
//! The rest of the system only sees [`RawFile`], [`VolumeInfo`], [`Reactor`] and
//! [`Topology`].

// --- lint exceptions, with reasons ------------------------------------------
//
// This crate *is* the unsafe layer. Every other crate in the workspace is safe
// code, so the `unsafe_code` warning is silenced here and nowhere else.
#![allow(unsafe_code)]
#![allow(clippy::missing_safety_doc)]
// Offset arithmetic and byte slicing are the program here, not an oversight.
// A copy engine computes `offset + length` millions of times per second; turning
// each one into `checked_add(..).unwrap_or(..)` would both slow the hot path and
// make the pointer arithmetic unreadable. The invariants that keep these sound
// are stated at each `unsafe` block instead.
#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::indexing_slicing)]
// The same argument for the FFI edge: `try_into().unwrap()` on a fixed-size
// slice is infallible, and a `Result` would only add branches on paths that
// cannot fail.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
// A helper used once is still the right shape when it names a concept
// (`bind_pages_to_node`, `last()`); inlining them would make the call sites
// longer without making them clearer.
#![allow(clippy::single_call_fn)]
// Every fallible function in this crate returns the OS error unchanged. Writing
// "Returns the OS error" above each of them would be 22 copies of the same
// sentence, so the crate says it once, here.
#![allow(clippy::missing_errors_doc)]
// The panics that remain are index and `unwrap` panics inside FFI plumbing,
// each guarded by the length check immediately above it. The invariant is
// stated at the site; a `# Panics` section per function would not add to it.
#![allow(clippy::missing_panics_doc)]
// The remaining exceptions are about shape, not correctness: the topology probe
// and the affinity planner are long because the platform queries are long; a
// capability set is a bag of booleans by nature; FFI casts are deliberate.
#![allow(clippy::too_many_lines)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::non_send_fields_in_send_ty)]

use std::io;
use std::path::Path;

/// The platform backend this build uses.
pub mod affinity;
pub mod align;
pub mod blocking;
pub mod buf;
pub mod caps;
pub mod clock;
pub mod interrupt;
pub mod shm;
pub mod slab;

#[cfg(target_os = "linux")]
pub mod lnx;
#[cfg(target_os = "windows")]
pub mod win;

#[cfg(target_os = "linux")]
mod plat {
    pub use crate::lnx::*;
}
#[cfg(target_os = "windows")]
mod plat {
    pub use crate::win::*;
}

pub use plat::CpuInfo;
pub use plat::DeviceInfo;
pub use plat::Extent;
pub use plat::FileId;
pub use plat::RawFile;
pub use plat::Topology;
pub use plat::VolumeInfo;
pub use plat::copy_acl;
pub use plat::copy_file_fast;
pub use plat::copy_xattrs;
pub use plat::current_cpu;
pub use plat::dio_alignment;
pub use plat::file_identity;
pub use plat::file_order_key;
pub use plat::hard_link;
pub use plat::is_rotational;
pub use plat::numa_node_of;
pub use plat::physical_extents;
pub use plat::pin_current_thread;
pub use plat::set_mode;
pub use plat::set_privileges;
pub use plat::set_process_priority;
pub use plat::set_thread_mempolicy;
pub use plat::set_thread_priority;
pub use plat::volume_info;
pub use plat::xattrs_supported;

/// A single asynchronous I/O completion.
#[derive(Clone, Copy, Debug)]
pub struct Completion {
    /// Caller supplied tag, round-tripped untouched.
    pub user: u64,
    /// Bytes transferred on success, or a negative OS error code on failure.
    pub res: i64,
}

impl Completion {
    /// True when the operation succeeded.
    #[inline]
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.res >= 0
    }

    /// Bytes transferred; zero on failure.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.res.max(0) as usize
    }

    /// OS error code (positive), valid only when [`Completion::is_ok`] is false.
    #[inline]
    #[must_use]
    pub const fn err_code(&self) -> u32 {
        (-self.res) as u32
    }
}

/// Completion-driven I/O backend.
///
/// Implementations are owned by exactly one thread. A handle may only be
/// registered with one reactor at a time — that is an IOCP constraint, and we keep
/// the contract identical on `io_uring` so the engine can be written once.
pub trait Reactor: Send {
    /// Open a file bound to this reactor.
    ///
    /// Handles are per-reactor by construction: an IOCP handle can only be
    /// associated with one completion port, and giving each I/O thread its own
    /// handle removes the need to serialise submissions.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `CreateFileW` / `open(2)`.
    fn open(
        &mut self,
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<RawFile>;

    /// Queue a read of `len` bytes at `off` into `buf`.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the request could not be submitted, for example
    /// because the backend queue is full.
    ///
    /// # Safety
    /// `buf` must stay valid for `len` bytes until the completion is reaped, and
    /// must satisfy [`RawFile::alignment`] when the file uses direct I/O.
    unsafe fn read(
        &mut self,
        f: &RawFile,
        buf: *mut u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()>;

    /// Queue a write of `len` bytes from `buf` at `off`.
    ///
    /// # Errors
    ///
    /// Same as [`Reactor::read`].
    ///
    /// # Safety
    /// Same contract as [`Reactor::read`].
    unsafe fn write(
        &mut self,
        f: &RawFile,
        buf: *const u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()>;

    /// Append completions to `out`.
    ///
    /// Blocks until at least one is available (or nothing is in flight), then also
    /// takes whatever else is already queued and returns. It deliberately does
    /// **not** wait for all `want`: doing so drains the device queue every round
    /// trip, so the pipeline stalls on its slowest request instead of refilling as
    /// soon as anything lands. Callers must loop.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the wait itself fails.
    fn poll(
        &mut self,
        want: usize,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize>;

    /// Operations submitted but not yet reaped.
    fn in_flight(&self) -> usize;

    /// Register a set of fixed buffers with the backend.
    ///
    /// `bufs` is `(address, length)` pairs. `io_uring` turns this into
    /// `IORING_REGISTER_BUFFERS`, which removes the per-operation page pinning;
    /// IOCP has no equivalent and ignores it.
    ///
    /// # Errors
    ///
    /// Returns the registration failure from the backend.
    fn register_buffers(
        &mut self,
        bufs: &[(usize, usize)],
    ) -> io::Result<()> {
        let _ = bufs;
        Ok(())
    }

    /// Backend name (`iocp`, `io_uring`, `blocking`).
    fn name(&self) -> &'static str;

    /// Ask the backend to abandon in-flight operations. Called when the user
    /// interrupts; completions still arrive, so buffers are released normally.
    fn cancel(&mut self) {}

    /// Reap everything currently pending.
    ///
    /// # Errors
    ///
    /// Same as [`Reactor::poll`].
    fn drain(
        &mut self,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let mut total = 0;
        while self.in_flight() > 0 {
            let n = self.poll(1, out)?;
            if n == 0 {
                break;
            }
            total += n;
        }
        Ok(total)
    }
}

/// Which reactor implementation to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReactorKind {
    /// Platform native: IOCP on Windows, `io_uring` on Linux.
    #[default]
    Native,
    /// Thread pool + pread/pwrite. Portable, slower, always available.
    Blocking,
}

/// How a file is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Access {
    /// Read-only.
    #[default]
    Read,
    /// Write-only.
    Write,
    /// Both. Needed for `FSCTL_SET_COMPRESSION` and for read-back verification.
    ReadWrite,
    /// Metadata only: enough to set timestamps, mode bits and (on Windows) the
    /// security descriptor, and not enough to write data.
    ///
    /// A destination that was copied from a read-only source is read-only itself,
    /// so `Write` cannot reopen it — which is exactly what the metadata pass after
    /// a kernel-side copy has to do.
    Attributes,
}

/// Creation / truncation semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Create {
    /// Must already exist.
    #[default]
    Open,
    /// Fail if it exists.
    CreateNew,
    /// Create or truncate.
    Create,
    /// Create if missing, keep contents otherwise.
    OpenOrCreate,
    /// Truncate an existing file.
    Truncate,
}

/// Open flags. Direct I/O, cache bypass and I/O priority are all decided here so
/// there is exactly one place that touches `CreateFileW` / `open(2)`.
#[derive(Clone, Copy, Debug)]
pub struct OpenOpts {
    /// Requested access mode.
    pub access: Access,
    /// What to do about an existing file.
    pub create: Create,
    /// `FILE_FLAG_NO_BUFFERING` / `O_DIRECT`.
    pub direct: bool,
    /// Sequential-scan hint (Windows) — tells the cache manager to read ahead.
    pub sequential: bool,
    /// `FILE_FLAG_WRITE_THROUGH` / `O_DSYNC`.
    pub write_through: bool,
    /// Mark the file sparse at creation.
    pub sparse: bool,
    /// Windows `FILE_FLAG_DELETE_ON_CLOSE`.
    pub delete_on_close: bool,
    /// I/O priority hint 0..=4 (verylow..critical).
    pub io_priority: Option<u8>,
    /// Open with backup/restore intent so a restrictive DACL does not deny access.
    /// Only takes effect when the process actually holds the privilege.
    pub backup_semantics: bool,
    /// Request `WRITE_DAC`/`WRITE_OWNER` so the security descriptor can be set on
    /// this handle. Windows only; ignored elsewhere.
    pub write_dac: bool,
}

impl Default for OpenOpts {
    fn default() -> Self {
        Self {
            access: Access::Read,
            create: Create::Open,
            direct: false,
            sequential: false,
            write_through: false,
            sparse: false,
            delete_on_close: false,
            io_priority: None,
            backup_semantics: false,
            write_dac: false,
        }
    }
}

impl OpenOpts {
    /// Read-only, existing file, buffered.
    #[must_use]
    pub fn read() -> Self {
        Self::default()
    }

    /// Write-only, created or truncated, buffered.
    #[must_use]
    pub fn write() -> Self {
        Self {
            access: Access::Write,
            create: Create::Create,
            ..Self::default()
        }
    }

    /// Enable or disable direct (unbuffered) I/O.
    #[must_use]
    pub const fn direct(
        mut self,
        on: bool,
    ) -> Self {
        self.direct = on;
        self
    }

    /// Set the sequential-scan hint.
    #[must_use]
    pub const fn sequential(
        mut self,
        on: bool,
    ) -> Self {
        self.sequential = on;
        self
    }
}

/// Privilege names, spelled the way each OS expects them.
///
/// Windows has to be asked to enable these on the process token; on Linux the
/// equivalent capabilities are granted at exec time and this is a probe.
pub mod privilege_names {
    /// `SeManageVolumePrivilege`: enables `SetFileValidData` (instant allocation).
    pub const MANAGE_VOLUME: &str = crate::plat::SE_MANAGE_VOLUME;
    /// `SeLockMemoryPrivilege`: enables large pages for the buffer pool.
    pub const LOCK_MEMORY: &str = crate::plat::SE_LOCK_MEMORY;
    /// `SeBackupPrivilege`: read files a DACL would otherwise deny.
    pub const BACKUP: &str = crate::plat::SE_BACKUP;
    /// `SeRestorePrivilege`: write owner/group on the destination.
    pub const RESTORE: &str = crate::plat::SE_RESTORE;
}

/// Map a human name (`verylow`..`critical`) to the platform I/O priority hint.
#[must_use]
pub fn io_priority_from_name(name: &str) -> Option<u8> {
    plat::io_priority_hint(name)
}

/// Map a human name to the platform process priority class.
#[must_use]
pub fn process_priority_from_name(name: &str) -> Option<u32> {
    plat::process_priority_class(name)
}

/// Map a human name to the platform thread priority level.
#[must_use]
pub fn thread_priority_from_name(name: &str) -> Option<i32> {
    plat::thread_priority_level(name)
}

/// Default alignment used for direct I/O when the device reports nothing better.
pub const DEFAULT_DIRECT_ALIGN: usize = 4096;

/// Round `v` up to the next multiple of `a`. `a` must be a power of two.
#[inline]
#[must_use]
pub fn align_up(
    v: u64,
    a: u64,
) -> u64 {
    debug_assert!(a.is_power_of_two());
    (v + a - 1) & !(a - 1)
}

/// Round `v` down to the previous multiple of `a`. `a` must be a power of two.
#[inline]
#[must_use]
pub fn align_down(
    v: u64,
    a: u64,
) -> u64 {
    debug_assert!(a.is_power_of_two());
    v & !(a - 1)
}

/// True when `v` is already a multiple of `a`.
#[inline]
#[must_use]
pub const fn is_aligned(
    v: u64,
    a: u64,
) -> bool {
    v & (a - 1) == 0
}

/// Build a reactor of the requested kind, falling back to the portable backend
/// when the native one is unavailable.
///
/// # Errors
///
/// Returns the failure from the fallback backend when even that cannot start.
pub fn make_reactor(
    kind: ReactorKind,
    depth: usize,
    blocking_threads: usize,
) -> io::Result<Box<dyn Reactor>> {
    match kind {
        | ReactorKind::Native => {
            match plat::native_reactor(depth) {
                | Ok(r) => Ok(r),
                | Err(e) => {
                    eprintln!("sirocco: native reactor unavailable ({e}); using blocking backend");
                    Ok(Box::new(blocking::BlockingReactor::new(
                        blocking_threads.max(1),
                    )))
                },
            }
        },
        | ReactorKind::Blocking => {
            Ok(Box::new(blocking::BlockingReactor::new(
                blocking_threads.max(1),
            )))
        },
    }
}

/// Convenience: does this path exist and is it a directory?
#[must_use]
pub fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_dir())
}
