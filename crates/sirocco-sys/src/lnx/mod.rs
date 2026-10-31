//! Linux platform implementation.
//!
//! `O_DIRECT` for cache bypass, `io_uring` for completion-driven I/O, `FIEMAP` for
//! physical layout, `fallocate` for instant allocation, `ioprio_set` for I/O
//! scheduling class, and cgroup-aware CPU/NUMA placement from sysfs.

// --- lint exceptions for this module ----------------------------------------
//
// This file is a transcription of the Linux ABI: `statx` timestamps, `FIEMAP`
// extents, `ioprio` classes, byte offsets inside an mmap'd ring. The lints below
// fire on the shape of that code rather than on a defect in it, and they are
// allowed here — at module scope, with this note — instead of being papered over
// line by line with casts that hide the ABI.
//
// * `cast_lossless` — `i32 -> i64` for a syscall argument is a sign extension
//   into a register. `i64::from(x)` reads as a numeric conversion and obscures
//   that this is the kernel's calling convention.
// * `ptr_as_ptr` / `borrow_as_ptr` — `mmap`, `ioctl` and `syscall` take
//   `*mut c_void`; converting a `*const u8` or a `&mut stat` to that is the call
//   site, not a shortcut around the type system.
// * `must_use_candidate` / `missing_const_for_fn` — accessors on a raw handle.
//   Whether reading a field is worth a `#[must_use]`, or is `const`-callable, is
//   a detail of today's implementation and not part of the contract.
// * `wildcard_imports` — `use ffi::*` is the point: the module below *is* the ABI.
// * `use_self` — this file is one type with a hundred methods; spelling the type
//   is clearer than making the reader resolve `Self`.
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::use_self)]
#![allow(clippy::wildcard_imports)]

use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use crate::Access;
use crate::Create;
use crate::OpenOpts;

pub mod ffi;
pub mod uring;

use ffi::*;

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn eio() -> io::Error {
    io::Error::last_os_error()
}

// ---------------------------------------------------------------- extents

/// One physically contiguous run of blocks, as `FIEMAP` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Virtual block (file offset / block size).
    pub vcn: u64,
    /// Physical block on the device.
    pub lcn: u64,
    /// Length in blocks.
    pub len: u64,
    /// True when the run is an unallocated hole rather than real blocks.
    pub sparse: bool,
}

impl Extent {
    /// File offset of block `vcn`, in bytes, at the given block size.
    #[inline]
    pub fn byte_offset(
        &self,
        block: u64,
    ) -> u64 {
        self.vcn * block
    }
}

// ---------------------------------------------------------------- RawFile

/// An open file, owned by the thread that opened it.
///
/// Holds the descriptor plus the alignment its direct-I/O buffers must satisfy.
/// There is one per thread rather than one shared handle: concurrent reads
/// through a shared `O_DIRECT` descriptor return short or zero-length results on
/// several filesystems, which is not something a retry loop can fix.
pub struct RawFile {
    fd: OwnedFd,
    path: PathBuf,
    direct: bool,
    align: usize,
}

unsafe impl Send for RawFile {}
unsafe impl Sync for RawFile {}

impl RawFile {
    /// Open a file with the given flags, returning a handle this thread owns.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `CreateFileW` / `open(2)`.
    pub fn open(
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<Self> {
        let mut flags = match opts.access {
            | Access::Read | Access::Attributes => libc::O_RDONLY,
            | Access::Write => libc::O_WRONLY,
            | Access::ReadWrite => libc::O_RDWR,
        };
        flags |= libc::O_CLOEXEC;
        if opts.direct {
            flags |= libc::O_DIRECT;
        }
        if opts.write_through {
            flags |= libc::O_DSYNC;
        }
        match opts.create {
            | Create::Open => {},
            | Create::CreateNew => flags |= libc::O_CREAT | libc::O_EXCL,
            | Create::Create => flags |= libc::O_CREAT | libc::O_TRUNC,
            | Create::OpenOrCreate => flags |= libc::O_CREAT,
            | Create::Truncate => flags |= libc::O_TRUNC,
        }
        // O_NOATIME only works for the owner (or CAP_FOWNER); retry without it.
        let with_noatime = opts.access == Access::Read && !opts.create.has_create();
        let mut fd = -1;
        let c = cstr(path)?;
        if with_noatime {
            fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_NOATIME, 0o666 as libc::c_uint) };
        }
        if fd < 0 {
            fd = unsafe { libc::open(c.as_ptr(), flags, 0o666 as libc::c_uint) };
        }
        if fd < 0 {
            return Err(eio());
        }
        let f = Self {
            // SAFETY: `fd` is a valid, uniquely owned descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: path.to_path_buf(),
            direct: opts.direct,
            align: crate::DEFAULT_DIRECT_ALIGN,
        };
        if opts.sequential {
            unsafe {
                libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            }
        }
        if let Some(hint) = opts.io_priority {
            let _ = f.set_io_priority(hint);
        }
        Ok(f)
    }

    /// The raw descriptor, for the reactor and for direct `libc` calls.
    #[inline]
    pub fn handle(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The path this handle was opened with, for diagnostics.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the handle was opened with `O_DIRECT`.
    #[inline]
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    /// Buffer alignment this handle requires: the device sector size for direct
    /// I/O, `1` when the page cache is in the way and nothing has to be aligned.
    #[inline]
    pub fn alignment(&self) -> usize {
        if self.direct { self.align } else { 1 }
    }

    /// Override the alignment the caller must satisfy. Never below 512.
    #[inline]
    pub fn set_alignment(
        &mut self,
        a: usize,
    ) {
        self.align = a.max(512);
    }

    /// Read at `off` until `buf` is full or the file ends.
    ///
    /// `EINTR` is retried; the returned length is short only at end of file.
    ///
    /// # Errors
    ///
    /// Returns the OS error for any failure other than an interrupted call.
    pub fn pread(
        &self,
        buf: &mut [u8],
        off: u64,
    ) -> io::Result<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let r = unsafe {
                libc::pread(
                    self.handle(),
                    buf[done..].as_mut_ptr().cast(),
                    buf.len() - done,
                    (off + done as u64) as libc::off_t,
                )
            };
            if r < 0 {
                let e = eio();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if r == 0 {
                break;
            }
            done += r as usize;
        }
        Ok(done)
    }

    /// Write `buf` at `off`, looping until every byte is written.
    ///
    /// A short `pwrite` is not an error — the kernel returns whatever fits in the
    /// request queue — so the remainder is resubmitted.
    ///
    /// # Errors
    ///
    /// Returns the OS error for any failure other than an interrupted call.
    pub fn pwrite(
        &self,
        buf: &[u8],
        off: u64,
    ) -> io::Result<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let r = unsafe {
                libc::pwrite(
                    self.handle(),
                    buf[done..].as_ptr().cast(),
                    buf.len() - done,
                    (off + done as u64) as libc::off_t,
                )
            };
            if r < 0 {
                let e = eio();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if r == 0 {
                break;
            }
            done += r as usize;
        }
        Ok(done)
    }

    /// Current file size, read from the handle.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the size query fails.
    pub fn size(&self) -> io::Result<u64> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.handle(), &mut st) } < 0 {
            return Err(eio());
        }
        Ok(st.st_size as u64)
    }

    /// Set the file length: extend with a hole, or truncate the tail.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `ftruncate`.
    pub fn set_len(
        &self,
        n: u64,
    ) -> io::Result<()> {
        if unsafe { libc::ftruncate(self.handle(), n as libc::off_t) } < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// Preallocate blocks without changing the size. Returns `Ok(false)` when the
    /// filesystem does not support `fallocate`.
    pub fn prealloc_valid(
        &self,
        n: u64,
    ) -> io::Result<bool> {
        // fallocate only grows; a longer destination has to be cut back or the
        // stale tail survives the copy.
        if self.size()? > n {
            self.set_len(n)?;
        }
        let r = unsafe { libc::fallocate(self.handle(), 0, 0, n as libc::off_t) };
        if r == 0 {
            return Ok(true);
        }
        let e = eio();
        if e.raw_os_error() == Some(libc::EOPNOTSUPP) || e.raw_os_error() == Some(libc::ENOSYS) {
            return Ok(false);
        }
        Err(e)
    }

    /// Mark the file sparse.
    ///
    /// Nothing to do: files are sparse by construction on every modern Linux
    /// filesystem. The method exists so that the caller does not have to know
    /// that.
    ///
    /// # Errors
    ///
    /// Never fails; the `Result` keeps the signature identical to the Windows
    /// implementation.
    pub fn set_sparse(&self) -> io::Result<()> {
        // Files are sparse by default on every modern Linux filesystem.
        Ok(())
    }

    /// Linux files are sparse by construction; there is no attribute to toggle.
    pub fn set_sparse_flag(
        &self,
        _on: bool,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Punch a hole over `[off, off + len)`, which reads back as zeroes without
    /// occupying blocks.
    ///
    /// Used for regions the source does not allocate: copying the zeroes would
    /// cost a write per byte and leave the destination larger than the source.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the filesystem refuses the punch.
    pub fn zero_range(
        &self,
        off: u64,
        len: u64,
    ) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let r = unsafe {
            libc::fallocate(
                self.handle(),
                FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                off as libc::off_t,
                len as libc::off_t,
            )
        };
        if r < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// The allocated runs of the file, as `(offset, length)` pairs.
    ///
    /// Drives `--sparse`: the gaps between these runs are read from the source
    /// not at all and punched in the destination instead.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the filesystem cannot answer the query at all;
    /// `EINVAL` and `ENXIO` are handled internally.
    pub fn allocated_ranges(
        &self,
        total: u64,
    ) -> io::Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let mut pos = 0u64;
        while pos < total {
            let d = unsafe { libc::lseek(self.handle(), pos as libc::off_t, SEEK_DATA) };
            if d < 0 {
                let e = eio();
                // ENXIO means "no more data", which is a complete answer. Anything
                // else (EINVAL on tmpfs/overlayfs, EBADF, ...) means the filesystem
                // does not support the query, and returning a partial list here
                // would make the caller treat the rest of the file as holes and
                // punch real data away.
                if e.raw_os_error() == Some(libc::ENXIO) {
                    break;
                }
                return Err(e);
            }
            let h = unsafe { libc::lseek(self.handle(), d, SEEK_HOLE) };
            if h < 0 {
                let e = eio();
                if e.raw_os_error() == Some(libc::ENXIO) {
                    out.push((d as u64, total - d as u64));
                    break;
                }
                return Err(e);
            }
            if h <= d {
                // A filesystem reporting an empty run would spin forever, and a
                // partial answer would make the caller treat the rest of the file as
                // holes and punch real data away.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SEEK_DATA/SEEK_HOLE returned an empty range",
                ));
            }
            out.push((d as u64, (h - d) as u64));
            pos = h as u64;
        }
        Ok(out)
    }

    /// Physical extent map, in filesystem blocks.
    ///
    /// Loops until `FIEMAP_EXTENT_LAST`: a fragmented file easily has more than the
    /// 256 extents one call returns, and a truncated map would silently disable the
    /// caller's physical ordering for the tail of the file.
    pub fn extents(&self) -> io::Result<Vec<Extent>> {
        // One FIEMAP call returns at most this many extents; the loop below asks
        // again from where the previous call stopped.
        const N: usize = 256;
        let size = self.size()?;
        if size == 0 {
            return Ok(Vec::new());
        }
        // `st_blksize` is the unit FIEMAP reports in on this filesystem; using a
        // hardcoded 4096 against a 512-byte-cluster volume makes every lookup miss.
        let bs = self.block_size().max(512);
        let mut buf =
            vec![0u8; std::mem::size_of::<fiemap>() + N * std::mem::size_of::<fiemap_extent>()];
        let mut out: Vec<Extent> = Vec::new();
        let mut start: u64 = 0;
        for _ in 0..4096 {
            let fm = buf.as_mut_ptr() as *mut fiemap;
            unsafe {
                (*fm).fm_start = start;
                (*fm).fm_length = size.saturating_sub(start);
                (*fm).fm_flags = FIEMAP_FLAG_SYNC;
                (*fm).fm_mapped_extents = 0;
                (*fm).fm_extent_count = N as u32;
            }
            if unsafe { libc::ioctl(self.handle(), FS_IOC_FIEMAP, buf.as_mut_ptr()) } < 0 {
                let e = eio();
                // EOPNOTSUPP / ENOTTY: the filesystem has no physical map. That is
                // not an error for the caller, it just means "no ordering hints".
                return match e.raw_os_error() {
                    | Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL) => Ok(out),
                    | _ => Err(e),
                };
            }
            let mapped = unsafe { (*fm).fm_mapped_extents } as usize;
            if mapped == 0 {
                break;
            }
            let base =
                unsafe { buf.as_ptr().add(std::mem::size_of::<fiemap>()) } as *const fiemap_extent;
            let mut last_end = start;
            let mut saw_last = false;
            for i in 0..mapped {
                let e = unsafe { *base.add(i) };
                if e.fe_length == 0 {
                    continue;
                }
                out.push(Extent {
                    vcn: e.fe_logical / bs,
                    lcn: e.fe_physical / bs,
                    len: e.fe_length.div_ceil(bs),
                    sparse: e.fe_flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC) != 0,
                });
                last_end = e.fe_logical + e.fe_length;
                if e.fe_flags & FIEMAP_EXTENT_LAST != 0 {
                    saw_last = true;
                    break;
                }
            }
            if saw_last || last_end <= start || last_end >= size {
                break;
            }
            start = last_end;
        }
        Ok(out)
    }

    /// Filesystem block size, as FIEMAP reports offsets in.
    fn block_size(&self) -> u64 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.handle(), &mut st) } == 0 {
            (st.st_blksize as u64).max(512)
        } else {
            4096
        }
    }

    /// Set the I/O priority hint on this handle (0 = very low, 4 = critical).
    ///
    /// # Errors
    ///
    /// Returns the OS error when the hint could not be applied.
    pub fn set_io_priority(
        &self,
        hint: u8,
    ) -> io::Result<()> {
        // Map 0..=4 (verylow..critical) onto the best-effort class.
        let data = match hint {
            | 0 => 7u16,
            | 1 => 6,
            | 2 => 4,
            | 3 => 2,
            | _ => 0,
        };
        let v = ioprio_value(IOPRIO_CLASS_BE, data);
        let r = unsafe { syscall(SYS_IOPRIO_SET, IOPRIO_WHO_PROCESS, 0 as libc::c_int, v) };
        if r < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// Turn on filesystem compression for this file. Data written afterwards is
    /// compressed by the filesystem, so the engine never has to.
    ///
    /// # Errors
    ///
    /// Returns the OS error when compression is unavailable for this volume.
    pub fn set_compressed(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "per-file transparent compression is not available on this platform",
        ))
    }

    /// Flush the file's data and metadata to the device.
    ///
    /// # Errors
    ///
    /// Returns the OS error from the flush.
    pub fn flush(&self) -> io::Result<()> {
        if unsafe { libc::fsync(self.handle()) } < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// Flush the file's data, leaving metadata alone (`fdatasync`).
    ///
    /// # Errors
    ///
    /// Returns the OS error from the flush.
    pub fn sync_data(&self) -> io::Result<()> {
        if unsafe { libc::fdatasync(self.handle()) } < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// Set the modification time through the existing fd (no reopen).
    pub fn set_modified(
        &self,
        t: std::time::SystemTime,
    ) -> io::Result<()> {
        let d = t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        let mut ts: [libc::timespec; 2] = unsafe { std::mem::zeroed() };
        // Leave atime alone.
        ts[0].tv_nsec = libc::UTIME_OMIT;
        ts[1].tv_sec = d.as_secs() as libc::time_t;
        ts[1].tv_nsec = d.subsec_nanos() as _;
        if unsafe { libc::futimens(self.handle(), ts.as_ptr()) } < 0 {
            return Err(eio());
        }
        Ok(())
    }

    /// Drop the page cache for a range (meaningless with `O_DIRECT`, useful when
    /// the engine falls back to buffered I/O).
    pub fn drop_cache(
        &self,
        off: u64,
        len: u64,
    ) {
        unsafe {
            libc::posix_fadvise(
                self.handle(),
                off as libc::off_t,
                len as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            );
        }
    }
}

impl Create {
    fn has_create(self) -> bool {
        !matches!(self, Create::Open)
    }
}

impl Drop for RawFile {
    fn drop(&mut self) {
        // OwnedFd closes automatically.
    }
}

impl std::fmt::Debug for RawFile {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("RawFile")
            .field("path", &self.path)
            .field("fd", &self.handle())
            .field("direct", &self.direct)
            .field("align", &self.align)
            .finish()
    }
}

// ---------------------------------------------------------------- volume info

/// Geometry and identity of the volume a path lives on. Every alignment and
/// sparse decision the engine makes comes from here.
#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// Mount point / drive root the path resolved to.
    pub root: PathBuf,
    /// Filesystem name (`NTFS`, `btrfs`, `ext4`, ...).
    pub fs: String,
    /// Logical sector size: the alignment reads must satisfy.
    pub logical_sector: u32,
    /// Physical sector size: the alignment writes want.
    pub physical_sector: u32,
    /// Allocation unit, used to map a byte offset to a physical cluster.
    pub cluster: u32,
    /// True for spinning media, where physical ordering pays off.
    pub rotational: bool,
    /// Volume capacity.
    pub total_bytes: u64,
    /// Free space at the time of the query.
    pub free_bytes: u64,
    /// Volume serial number, used to detect a same-volume move.
    pub serial: u32,
}

/// Older name for [`VolumeInfo`].
pub type DeviceInfo = VolumeInfo;

fn sysfs_read_u32(p: &Path) -> Option<u32> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Resolve the block device backing `path` via `/sys/dev/block/M:m`.
fn block_device_name(dev: u64) -> Option<String> {
    let major = ((dev >> 8) & 0xfff) as u32;
    let minor = ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32;
    let link = format!("/sys/dev/block/{major}:{minor}");
    let target = std::fs::read_link(&link).ok()?;
    let s = target.to_string_lossy().to_string();
    // .../block/sda/sda1 -> sda
    let mut parts = s.split("/block/");
    let rest = parts.nth(1)?;
    Some(rest.split('/').next()?.to_string())
}

/// Query the volume that `path` lives on.
///
/// # Errors
///
/// Returns the OS error when the volume cannot be queried.
pub fn volume_info(path: &Path) -> io::Result<VolumeInfo> {
    let c = cstr(path)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } < 0 {
        return Err(eio());
    }
    let bs = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    let total = bs * st.f_blocks as u64;
    let free = bs * st.f_bavail as u64;

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut stat) } < 0 {
        return Err(eio());
    }
    let dev = stat.st_dev as u64;
    let mut logical_sector = 512u32;
    let mut physical_sector = 512u32;
    let mut rotational = false;
    if let Some(name) = block_device_name(dev) {
        let q = PathBuf::from(format!("/sys/block/{name}/queue"));
        if let Some(v) = sysfs_read_u32(&q.join("logical_block_size")) {
            logical_sector = v;
        }
        if let Some(v) = sysfs_read_u32(&q.join("physical_block_size")) {
            physical_sector = v;
        }
        if let Some(v) = sysfs_read_u32(&q.join("rotational")) {
            rotational = v == 1;
        }
    }

    // Filesystem type name from the magic number.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c.as_ptr(), &mut sfs) };
    let fs = match sfs.f_type as i64 {
        | 0xEF53 => "ext4".to_string(),
        | 0x5846_5342 => "xfs".to_string(),
        | 0x9123_683E => "btrfs".to_string(),
        | 0x0102_1994 => "tmpfs".to_string(),
        | 0x2FC1_2FC1 => "zfs".to_string(),
        | 0x6969 => "nfs".to_string(),
        | 0x6573_5546 => "fuse".to_string(),
        | 0x794C_7630 => "overlayfs".to_string(),
        | _ => format!("0x{:x}", sfs.f_type),
    };

    Ok(VolumeInfo {
        root: PathBuf::from("/"),
        fs,
        logical_sector,
        physical_sector,
        cluster: logical_sector,
        rotational,
        total_bytes: total,
        free_bytes: free,
        serial: dev as u32,
    })
}

/// True when the volume behind `path` is a spinning disk. Assumes SSD on any
/// failure, which is the safer default: physical ordering is pure overhead there.
pub fn is_rotational(path: &Path) -> bool {
    volume_info(path).is_ok_and(|v| v.rotational)
}

/// Physical layout of the open file, from `FIEMAP`.
///
/// # Errors
///
/// Returns the OS error when the filesystem has no extent map.
pub fn physical_extents(f: &RawFile) -> io::Result<Vec<Extent>> {
    f.extents()
}

// ---------------------------------------------------------------- identity

/// Identity of a file within its volume, plus its hard link count.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileId {
    /// Volume serial the index is relative to.
    pub volume: u64,
    /// Per-volume file index (MFT record number / inode).
    pub index: u64,
}

/// Re-exported so callers do not need a platform import for timestamps.
pub type FileTimes = std::fs::FileTimes;

// ---------------------------------------------------------------- memory

pub(crate) fn alloc_pages(
    size: usize,
    huge: bool,
    node: Option<u16>,
) -> io::Result<(std::ptr::NonNull<u8>, bool)> {
    let page = page_size();
    let sz = crate::align::round_up(size, page);
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    if huge {
        flags |= libc::MAP_HUGETLB;
    }
    let mut p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            sz,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    let got_huge = if p == libc::MAP_FAILED && huge {
        flags &= !libc::MAP_HUGETLB;
        p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sz,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        // The huge-page mapping failed, so this is the plain-page fallback.
        false
    } else {
        huge
    };
    if p == libc::MAP_FAILED {
        return Err(eio());
    }
    if !got_huge {
        unsafe {
            libc::madvise(p, sz, libc::MADV_HUGEPAGE);
        }
    }
    let nn = std::ptr::NonNull::new(p as *mut u8)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOMEM))?;
    if let Some(n) = node {
        // Best effort: an unprivileged process may lack MPOL_MF_MOVE rights.
        let _ = bind_pages_to_node(nn.as_ptr(), sz, n);
    }
    Ok((nn, got_huge))
}

pub(crate) fn free_pages(
    ptr: *mut u8,
    size: usize,
) {
    unsafe {
        libc::munmap(ptr.cast(), crate::align::round_up(size, page_size()));
    }
}

pub(crate) fn lock_pages(
    ptr: *mut u8,
    size: usize,
) -> bool {
    // MAP_LOCKED needs CAP_IPC_LOCK; mlock degrades to the RLIMIT_MEMLOCK quota.
    unsafe { libc::mlock(ptr.cast(), size) == 0 }
}

pub(crate) fn discard_pages(
    ptr: *mut u8,
    size: usize,
) {
    unsafe {
        libc::madvise(ptr.cast(), size, libc::MADV_DONTNEED);
    }
}

/// The system page size, or 4096 when `sysconf` cannot answer.
pub fn page_size() -> usize {
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as usize } else { 4096 }
}

// ---------------------------------------------------------------- topology

/// One logical CPU, as the topology probe sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// Package id on Linux, processor group on Windows.
    pub group: u16,
    /// Bit position within the group; `id` is the number to use.
    pub index: u8,
    /// Absolute logical CPU number.
    pub id: u16,
    /// NUMA node that owns this CPU.
    pub node: u16,
    /// Physical core index; CPUs sharing it are SMT siblings.
    pub core: u16,
}

/// The CPU and NUMA layout of this machine, probed once.
#[derive(Clone, Debug, Default)]
pub struct Topology {
    /// Every logical CPU.
    pub cpus: Vec<CpuInfo>,
    /// Physical cores, each as the indices of its logical CPUs in [`cpus`].
    ///
    /// [`cpus`]: Topology::cpus
    pub cores: Vec<Vec<usize>>,
    /// NUMA nodes, each as the indices of its CPUs in [`cpus`].
    ///
    /// [`cpus`]: Topology::cpus
    pub nodes: Vec<Vec<usize>>,
    /// Relative core capability from `cpu_capacity` (0..=255). Non-zero only on
    /// hybrid CPUs where the kernel publishes asymmetric capacities.
    pub core_efficiency: Vec<u8>,
}

fn parse_cpu_list(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            | Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) {
                    for c in a..=b {
                        out.push(c);
                    }
                }
            },
            | None => {
                if let Ok(v) = part.parse::<u16>() {
                    out.push(v);
                }
            },
        }
    }
    out
}

impl Topology {
    /// Probe the CPU and NUMA layout of this machine.
    ///
    /// Only CPUs in the current affinity mask are reported: a job confined to
    /// four cores by cgroup or `taskset` must not plan for the other sixty.
    pub fn detect() -> Topology {
        let mut t = Topology::default();
        // Which CPUs may we actually run on?
        let mut allowed = Vec::new();
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                for c in 0..libc::CPU_SETSIZE as usize {
                    if libc::CPU_ISSET(c, &set) {
                        allowed.push(c as u16);
                    }
                }
            }
        }
        if allowed.is_empty() {
            let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            allowed = (0..n as u16).collect();
        }

        // NUMA node -> cpus
        let mut node_cpus: Vec<(u16, Vec<u16>)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/devices/system/node") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(num) = name
                    .strip_prefix("node")
                    .and_then(|s| s.parse::<u16>().ok())
                    && let Ok(list) = std::fs::read_to_string(e.path().join("cpulist"))
                {
                    node_cpus.push((num, parse_cpu_list(&list)));
                }
            }
        }
        node_cpus.sort_by_key(|(n, _)| *n);

        let mut core_key: Vec<(u16, u16)> = Vec::new(); // (package, core)
        for &cpu in &allowed {
            let base = PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}/topology"));
            let pkg = sysfs_read_u32(&base.join("physical_package_id")).unwrap_or(0) as u16;
            let core = sysfs_read_u32(&base.join("core_id")).unwrap_or(cpu as u32) as u16;
            let node = node_cpus
                .iter()
                .find(|(_, cpus)| cpus.contains(&cpu))
                .map_or(0, |(n, _)| *n);
            let key = (pkg, core);
            let ci = match core_key.iter().position(|k| *k == key) {
                | Some(i) => i,
                | None => {
                    core_key.push(key);
                    core_key.len() - 1
                },
            };
            t.cpus.push(CpuInfo {
                group: pkg,
                index: (cpu % 64) as u8,
                id: cpu,
                node,
                core: ci as u16,
            });
        }
        t.cores = vec![Vec::new(); core_key.len()];
        for (i, c) in t.cpus.iter().enumerate() {
            t.cores[c.core as usize].push(i);
        }
        t.cores.retain(|v| !v.is_empty());

        // Hybrid CPUs (Intel P/E, ARM big.LITTLE) publish `cpu_capacity`.
        let caps: Vec<u32> = t
            .cores
            .iter()
            .map(|c| {
                let cpu = t.cpus[c[0]].id;
                sysfs_read_u32(&PathBuf::from(format!(
                    "/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"
                )))
                .unwrap_or(0)
            })
            .collect();
        let max = caps.iter().copied().max().unwrap_or(0);
        t.core_efficiency = if max > 0 {
            caps.into_iter().map(|c| ((c * 255) / max) as u8).collect()
        } else {
            vec![0u8; t.cores.len()]
        };
        let max_node = t.cpus.iter().map(|c| c.node).max().unwrap_or(0) as usize;
        t.nodes = vec![Vec::new(); max_node + 1];
        for (i, c) in t.cpus.iter().enumerate() {
            t.nodes[c.node as usize].push(i);
        }
        t.nodes.retain(|v| !v.is_empty());
        t
    }

    /// Number of logical CPUs in the affinity mask.
    pub fn logical_count(&self) -> usize {
        self.cpus.len()
    }

    /// Number of physical cores.
    pub fn physical_count(&self) -> usize {
        self.cores.len().max(1)
    }

    /// One logical CPU index per physical core, taken in core order.
    ///
    /// This is what the placement planner hands to reader and writer threads: it
    /// must not put two of them on SMT siblings, which share an execution unit
    /// and would halve each other's throughput.
    pub fn one_per_core(&self) -> Vec<usize> {
        self.cores.iter().map(|c| c[0]).collect()
    }
}

/// Which logical CPU the calling thread is running on right now.
pub fn current_cpu() -> u32 {
    unsafe { libc::sched_getcpu().max(0) as u32 }
}

/// Pin the calling thread to one logical CPU.
///
/// # Errors
///
/// Returns the OS error when the affinity mask could not be set.
pub fn pin_current_thread(
    t: &Topology,
    cpu: usize,
) -> io::Result<()> {
    let c = t
        .cpus
        .get(cpu)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        // Recover the absolute CPU id from the (group, index) pair.
        libc::CPU_SET(c.id as usize, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) < 0 {
            return Err(eio());
        }
    }
    Ok(())
}

/// Set the calling thread's priority.
///
/// # Errors
///
/// Returns the OS error when the priority could not be changed.
pub fn set_thread_priority(level: i32) -> io::Result<()> {
    let nice = match level {
        | i32::MIN..=-10 => -10,
        | -9..=-1 => level,
        | 0 => 0,
        | 1 => 5,
        | 2 => 10,
        | _ => 15,
    };
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } < 0 {
        return Err(eio());
    }
    Ok(())
}

/// Set the whole process's priority class.
///
/// # Errors
///
/// Returns the OS error when the class could not be set.
pub fn set_process_priority(class: u32) -> io::Result<()> {
    // On Linux the "class" maps to a nice value.
    let nice = match class {
        | 0x40 => 19,   // idle
        | 0x4000 => 10, // below normal
        | 0x8000 => -5, // above normal
        | 0x80 => -10,  // high
        | 0x100 => -20, // realtime (needs CAP_SYS_NICE)
        // 0x20 is "normal", which is also the fallback for an unknown class.
        | _ => 0,
    };
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } < 0 {
        return Err(eio());
    }
    Ok(())
}

// ---------------------------------------------------------------- privileges

/// Administrator: mount, quota and most namespace operations.
pub const CAP_SYS_ADMIN: i32 = 21;
/// Raise the I/O and CPU scheduling class of a process.
pub const CAP_SYS_NICE: i32 = 23;
/// Lock pages, which is what `MAP_HUGETLB` and `mlock` need.
pub const CAP_IPC_LOCK: i32 = 14;
/// Read a file whose permissions deny it, for backup tools.
pub const CAP_DAC_READ_SEARCH: i32 = 2;
/// Set ownership and timestamps on files that are not ours.
pub const CAP_FOWNER: i32 = 3;
/// Raw block device access.
pub const CAP_SYS_RAWIO: i32 = 17;

/// Report which of the requested capabilities are effective. Linux has no
/// "enable" step — capabilities come from the process credentials — so this is a
/// probe, not a mutation.
pub fn set_privileges(names: &[&str]) -> io::Result<Vec<String>> {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut cap_eff: u64 = 0;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            cap_eff = u64::from_str_radix(rest.trim(), 16).unwrap_or(0);
        }
    }
    let mut held = Vec::new();
    for n in names {
        let bit = match *n {
            | "SeManageVolumePrivilege" | "SeIncreaseQuotaPrivilege" => CAP_SYS_ADMIN,
            | "SeLockMemoryPrivilege" => CAP_IPC_LOCK,
            | "SeBackupPrivilege" => CAP_DAC_READ_SEARCH,
            | "SeRestorePrivilege" => CAP_FOWNER,
            | "SeProfileSingleProcessPrivilege" => CAP_SYS_NICE,
            | _ => continue,
        };
        if cap_eff & (1u64 << bit) != 0 {
            held.push((*n).to_string());
        }
    }
    Ok(held)
}

/// Enables `SetFileValidData`, i.e. allocation without zero-filling.
pub const SE_MANAGE_VOLUME: &str = "SeManageVolumePrivilege";
/// Enables large pages for the buffer pool.
pub const SE_LOCK_MEMORY: &str = "SeLockMemoryPrivilege";
/// Read files a DACL would otherwise deny.
pub const SE_BACKUP: &str = "SeBackupPrivilege";
/// Restore files a DACL would otherwise deny, including ownership.
pub const SE_RESTORE: &str = "SeRestorePrivilege";
/// Raise the process quota, used when reserving destination space.
pub const SE_INCREASE_QUOTA: &str = "SeIncreaseQuotaPrivilege";
/// Raise the I/O priority of the whole process.
pub const SE_PROFILE_SINGLE_PROCESS: &str = "SeProfileSingleProcessPrivilege";

/// Read `CapEff` from `/proc/self/status`.
fn effective_caps() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            return u64::from_str_radix(rest.trim(), 16).unwrap_or(0);
        }
    }
    0
}

/// Is the process running as root?
///
/// The GUI uses this to explain *why* a privilege request granted nothing: on
/// Linux the capabilities are attached to the credentials at exec time, so
/// there is no "enable" step to offer.
#[must_use]
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Re-launching with elevated rights is a Windows concept.
///
/// On Linux the equivalent is starting the binary under `sudo`, which the
/// process cannot do to itself without a terminal; the GUI says so instead of
/// pretending.
///
/// # Errors
///
/// Always [`io::ErrorKind::Unsupported`].
pub fn relaunch_elevated(_args: &[String]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "start the binary under sudo to run it with elevated rights",
    ))
}

/// Which privileged operations this process can actually perform.
///
/// The answer is a probe, not a promise: `fallocate` is unprivileged but the
/// filesystem may still refuse it, and a capability can be dropped by the
/// container after the process started.
pub fn detect_caps() -> crate::caps::Caps {
    let eff = effective_caps();
    let has = |bit: i32| eff & (1u64 << bit) != 0;
    let uid = unsafe { libc::geteuid() };
    crate::caps::Caps {
        elevated: uid == 0,
        // fallocate(2) is unprivileged on every current filesystem; the runtime
        // probe still decides per volume.
        instant_alloc: true,
        large_pages: has(CAP_IPC_LOCK),
        backup: has(CAP_DAC_READ_SEARCH),
        restore: has(CAP_FOWNER),
        rt_io: has(CAP_SYS_ADMIN),
        rt_priority: has(CAP_SYS_NICE),
        mlock: has(CAP_IPC_LOCK),
        // O_NOATIME only needs ownership, and we retry without it on EPERM.
        noatime: true,
    }
}

// ---------------------------------------------------------------- fs helpers

/// Make a file read-only or writable by clearing or setting the write bits.
///
/// # Errors
///
/// Returns the OS error when the metadata or the mode could not be changed.
pub fn set_readonly(
    path: &Path,
    ro: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::metadata(path)?;
    let mut perm = md.permissions();
    let mode = perm.mode();
    perm.set_mode(if ro {
        mode & !0o222
    } else {
        mode | 0o200
    });
    std::fs::set_permissions(path, perm)
}

/// Remove a file, whatever its permissions.
///
/// On Linux the directory's write bit decides this, so the force is nominal; it
/// exists to keep one name across the two platforms.
///
/// # Errors
///
/// Returns the OS error when the file cannot be removed.
pub fn remove_file_force(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// Remove a tree, whatever the permissions inside it.
///
/// # Errors
///
/// Returns the OS error when part of the tree cannot be removed.
pub fn remove_dir_all_force(path: &Path) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

/// Free and total bytes on the filesystem holding `path`.
///
/// # Errors
///
/// Returns the OS error when the volume information cannot be read.
pub fn free_space(path: &Path) -> io::Result<(u64, u64)> {
    let v = volume_info(path)?;
    Ok((v.free_bytes, v.total_bytes))
}

/// Size of a huge page, or the base page size when huge pages are unavailable.
pub fn large_page_size() -> usize {
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let base = if p > 0 { p as usize } else { 4096 };
    // 2 MiB is the default huge page size on x86_64 and arm64.
    base.max(2 << 20)
}

/// Logical CPUs available to this process, for sizing a thread pool.
pub fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Map `--io-priority` to an `ioprio_set` class, or `None` for an unknown name.
#[inline]
pub fn io_priority_hint(name: &str) -> Option<u8> {
    match name {
        | "verylow" | "idle" => Some(0),
        | "low" => Some(1),
        | "normal" => Some(2),
        | "high" => Some(3),
        | "critical" => Some(4),
        | _ => None,
    }
}

/// Map `--process-priority` to the Windows-style class constants the CLI parses,
/// or `None` for an unknown name.
#[inline]
pub fn process_priority_class(name: &str) -> Option<u32> {
    match name {
        | "idle" => Some(0x40),
        | "below" | "belownormal" => Some(0x4000),
        | "normal" => Some(0x20),
        | "above" | "abovenormal" => Some(0x8000),
        | "high" => Some(0x80),
        | "realtime" => Some(0x100),
        | _ => None,
    }
}

/// Map `--thread-priority` to the numeric level `set_thread_priority` expects, or
/// `None` for an unknown name.
#[inline]
pub fn thread_priority_level(name: &str) -> Option<i32> {
    match name {
        | "idle" => Some(-15),
        | "lowest" => Some(-2),
        | "below" => Some(-1),
        | "normal" => Some(0),
        | "above" => Some(1),
        | "highest" => Some(2),
        | "timecritical" => Some(15),
        | _ => None,
    }
}

// ---------------------------------------------------------------- NUMA

const MPOL_BIND: libc::c_int = 2;
const MPOL_MF_MOVE: libc::c_uint = 1 << 1;
// Per-architecture; see the note in `ffi.rs`.
const SYS_MBIND: libc::c_long = libc::SYS_mbind;
const SYS_SET_MEMPOLICY: libc::c_long = libc::SYS_set_mempolicy;

/// NUMA node that owns the storage backing `path`, from sysfs.
pub fn numa_node_of(path: &Path) -> Option<u16> {
    let c = cstr(path).ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut st) } < 0 {
        return None;
    }
    let name = block_device_name(st.st_dev as u64)?;
    let s = std::fs::read_to_string(format!("/sys/block/{name}/device/numa_node")).ok()?;
    let n: i32 = s.trim().parse().ok()?;
    if n < 0 {
        None
    } else {
        Some(n as u16)
    }
}

/// Restrict this thread's allocations to one node.
pub fn set_thread_mempolicy(node: u16) -> io::Result<()> {
    let mut mask: libc::c_ulong = 0;
    if node as usize >= std::mem::size_of::<libc::c_ulong>() * 8 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    mask |= 1 << node;
    let r = unsafe {
        syscall(
            SYS_SET_MEMPOLICY,
            MPOL_BIND as libc::c_long,
            std::ptr::from_ref(&mask),
            (std::mem::size_of::<libc::c_ulong>() * 8) as libc::c_long,
        )
    };
    if r < 0 {
        return Err(eio());
    }
    Ok(())
}

/// Bind an existing mapping to a node.
pub(crate) fn bind_pages_to_node(
    ptr: *mut u8,
    len: usize,
    node: u16,
) -> io::Result<()> {
    if node as usize >= std::mem::size_of::<libc::c_ulong>() * 8 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mask: libc::c_ulong = 1 << node;
    let r = unsafe {
        syscall(
            SYS_MBIND,
            ptr as libc::c_long,
            len as libc::c_long,
            MPOL_BIND as libc::c_long,
            std::ptr::from_ref(&mask),
            (std::mem::size_of::<libc::c_ulong>() * 8) as libc::c_long,
            MPOL_MF_MOVE as libc::c_long,
        )
    };
    if r < 0 {
        return Err(eio());
    }
    Ok(())
}

/// Copy a whole file with `copy_file_range`, which keeps the data in the kernel.
///
/// Returns an error for anything the syscall cannot handle (different
/// filesystems, unsupported filesystems, ...) so the caller can fall back to the
/// userspace pipeline.
pub fn copy_file_fast(
    src: &Path,
    dst: &Path,
) -> io::Result<()> {
    let ro = RawFile::open(
        src,
        &OpenOpts {
            access: Access::Read,
            create: Create::Open,
            sequential: true,
            ..Default::default()
        },
    )?;
    let wo = RawFile::open(
        dst,
        &OpenOpts {
            access: Access::Write,
            create: Create::Create,
            ..Default::default()
        },
    )?;
    let size = ro.size()?;
    let mut done: u64 = 0;
    while done < size {
        let want = (size - done).min(1 << 30) as usize;
        // A null offset pair makes the kernel advance both file positions.
        let r = unsafe {
            libc::copy_file_range(
                ro.handle(),
                std::ptr::null_mut(),
                wo.handle(),
                std::ptr::null_mut(),
                want,
                0,
            )
        };
        if r < 0 {
            // EXDEV / EINVAL / ENOSYS / EOPNOTSUPP all mean "not supported here".
            return Err(eio());
        }
        if r == 0 {
            break;
        }
        done += r as u64;
    }
    if done != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "copy_file_range stopped early",
        ));
    }
    // Data only. `copy_file_range` carries neither timestamps, permission bits
    // nor ACLs, and applying them here made `--no-preserve-times` and the absence
    // of `--acl` unenforceable: the engine's own metadata pass is the single place
    // that knows which of them the user asked for.
    Ok(())
}

const SYS_STATX: libc::c_long = libc::SYS_statx;
const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;
const STATX_DIOALIGN: u32 = 0x2000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Statx {
    mask: u32,
    blksize: u32,
    attributes: u64,
    nlink: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    spare0: u16,
    ino: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    atime: StatxTimestamp,
    btime: StatxTimestamp,
    ctime: StatxTimestamp,
    mtime: StatxTimestamp,
    rdev_major: u32,
    rdev_minor: u32,
    dev_major: u32,
    dev_minor: u32,
    mnt_id: u64,
    dio_mem_align: u32,
    dio_offset_align: u32,
    spare3: [u64; 12],
}

/// The alignment `O_DIRECT` actually requires for this path.
///
/// `statx(STATX_DIOALIGN)` reports it directly, which is better than assuming
/// 512 or 4096: some devices want 4 KiB on both sides, some only on the offset,
/// and a few want more. Using the exact value means we never issue an unaligned
/// direct I/O and never over-pad a transfer.
pub fn dio_alignment(path: &Path) -> Option<u32> {
    let c = cstr(path).ok()?;
    let mut stx = Statx {
        mask: 0,
        blksize: 0,
        attributes: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        mode: 0,
        spare0: 0,
        ino: 0,
        size: 0,
        blocks: 0,
        attributes_mask: 0,
        atime: StatxTimestamp::default(),
        btime: StatxTimestamp::default(),
        ctime: StatxTimestamp::default(),
        mtime: StatxTimestamp::default(),
        rdev_major: 0,
        rdev_minor: 0,
        dev_major: 0,
        dev_minor: 0,
        mnt_id: 0,
        dio_mem_align: 0,
        dio_offset_align: 0,
        spare3: [0; 12],
    };
    let r = unsafe {
        syscall(
            SYS_STATX,
            libc::AT_FDCWD as libc::c_long,
            c.as_ptr() as libc::c_long,
            AT_STATX_DONT_SYNC as libc::c_long,
            STATX_DIOALIGN as libc::c_long,
            std::ptr::from_mut(&mut stx) as libc::c_long,
        )
    };
    if r < 0 || stx.mask & STATX_DIOALIGN == 0 {
        return None;
    }
    let a = stx.dio_mem_align.max(stx.dio_offset_align);
    if a >= 512 { Some(a) } else { None }
}

/// Identity and link count of a file. Cheap: one `statx`.
pub fn file_identity(path: &Path) -> io::Result<(u64, u32)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)?;
    Ok((md.ino(), md.nlink() as u32))
}

/// Create a hard link at `link` pointing at `target`.
pub fn hard_link(
    target: &Path,
    link: &Path,
) -> io::Result<()> {
    std::fs::hard_link(target, link)
}

/// Apply the permission bits through the handle (no path re-resolution).
pub fn set_mode(
    f: &RawFile,
    mode: u32,
) -> io::Result<()> {
    if unsafe { libc::fchmod(f.handle(), mode as libc::mode_t) } < 0 {
        return Err(eio());
    }
    Ok(())
}

/// Copy the POSIX ACL from `src` to `dst`.
///
/// On Linux an ACL *is* an extended attribute (`system.posix_acl_access` and
/// `system.posix_acl_default`), so this is the xattr path restricted to those two
/// names — no libacl dependency, and it works through the same handle.
pub fn copy_acl(
    src: &Path,
    dst: &RawFile,
) -> io::Result<usize> {
    copy_xattrs_where(src, dst, Some("system.posix_acl_"))
}

/// Whether this platform can copy extended attributes.
///
/// The engine checks this once and refuses `--xattrs` up front on a platform
/// where the answer is no, rather than reporting one error per file.
#[must_use]
pub const fn xattrs_supported() -> bool {
    true
}

/// Copy every extended attribute from `src` to `dst`.
pub fn copy_xattrs(
    src: &Path,
    dst: &RawFile,
) -> io::Result<usize> {
    copy_xattrs_where(src, dst, None)
}

fn copy_xattrs_where(
    src: &Path,
    dst: &RawFile,
    only_prefix: Option<&str>,
) -> io::Result<usize> {
    let c = cstr(src)?;
    let mut names = vec![0u8; 4096];
    let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n < 0 {
        return Err(eio());
    }
    let n = n as usize;
    if n == 0 {
        return Ok(0);
    }
    let mut count = 0;
    let mut off = 0;
    while off < n {
        let end = names[off..]
            .iter()
            .position(|&b| b == 0)
            .map_or(n, |p| off + p);
        let name = &names[off..end];
        off = end + 1;
        if name.is_empty() {
            continue;
        }
        if let Some(p) = only_prefix
            && !name.starts_with(p.as_bytes())
        {
            continue;
        }
        let Ok(name_c) = std::ffi::CString::new(name) else {
            continue;
        };
        let mut val = vec![0u8; 65536];
        let vn = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
                name_c.as_ptr(),
                val.as_mut_ptr().cast(),
                val.len(),
            )
        };
        if vn < 0 {
            continue;
        }
        let r = unsafe {
            libc::fsetxattr(
                dst.handle(),
                name_c.as_ptr(),
                val.as_ptr().cast(),
                vn as usize,
                0,
            )
        };
        if r == 0 {
            count += 1;
        }
    }
    Ok(count)
}

/// Ordering key for `--order=physical`: the inode number.
pub fn file_order_key(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.ino())
}

/// A reactor for this platform: `io_uring`, with the requested queue depth.
///
/// # Errors
///
/// Returns the OS error when the ring cannot be set up, which is what happens on
/// kernels with `io_uring` disabled by sysctl or by seccomp policy.
pub fn native_reactor(depth: usize) -> io::Result<Box<dyn crate::Reactor>> {
    Ok(Box::new(uring::UringReactor::new(depth)?))
}
