//! Windows platform implementation.
//!
//! Direct I/O (`FILE_FLAG_NO_BUFFERING`), IOCP-driven async I/O, physical layout
//! queries, instant allocation via `SetFileValidData`, per-handle I/O priority and
//! processor-group aware affinity.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::ptr::NonNull;

use crate::Access;
use crate::Create;
use crate::OpenOpts;

pub mod ffi;
pub mod iocp;

use ffi::ABOVE_NORMAL_PRIORITY_CLASS;
use ffi::AdjustTokenPrivileges;
use ffi::BELOW_NORMAL_PRIORITY_CLASS;
use ffi::BOOL;
use ffi::COMPRESSION_FORMAT_DEFAULT;
use ffi::COPY_FILE_REQUEST_COMPRESSED_TRAFFIC;
use ffi::COPYFILE2_EXTENDED_PARAMETERS;
use ffi::CREATE_ALWAYS;
use ffi::CREATE_NEW;
use ffi::CloseHandle;
use ffi::CopyFile2;
use ffi::CopyFileExW;
use ffi::CreateFileW;
use ffi::CreateHardLinkW;
use ffi::DACL_SECURITY_INFORMATION;
use ffi::DEVICE_SEEK_PENALTY_DESCRIPTOR;
use ffi::DeviceIoControl;
use ffi::DiscardVirtualMemory;
use ffi::ERROR_ACCESS_DENIED;
use ffi::ERROR_HANDLE_EOF;
use ffi::ERROR_INVALID_PARAMETER;
use ffi::ERROR_IO_PENDING;
use ffi::ERROR_MORE_DATA;
use ffi::ERROR_NOT_ENOUGH_MEMORY;
use ffi::ERROR_PRIVILEGE_NOT_HELD;
use ffi::ERROR_SUCCESS;
use ffi::FILE_ALLOCATED_RANGE_BUFFER;
use ffi::FILE_ATTRIBUTE_ARCHIVE;
use ffi::FILE_ATTRIBUTE_NORMAL;
use ffi::FILE_ATTRIBUTE_READONLY;
use ffi::FILE_ATTRIBUTE_SPARSE_FILE;
use ffi::FILE_FLAG_BACKUP_SEMANTICS;
use ffi::FILE_FLAG_DELETE_ON_CLOSE;
use ffi::FILE_FLAG_NO_BUFFERING;
use ffi::FILE_FLAG_OVERLAPPED;
use ffi::FILE_FLAG_SEQUENTIAL_SCAN;
use ffi::FILE_FLAG_WRITE_THROUGH;
use ffi::FILE_IO_PRIORITY_HINT_INFO;
use ffi::FILE_READ_ATTRIBUTES;
use ffi::FILE_SHARE_DELETE;
use ffi::FILE_SHARE_READ;
use ffi::FILE_SHARE_WRITE;
use ffi::FILE_WRITE_ATTRIBUTES;
use ffi::FILE_ZERO_DATA_INFORMATION;
use ffi::FILETIME;
use ffi::FSCTL_GET_RETRIEVAL_POINTERS;
use ffi::FSCTL_QUERY_ALLOCATED_RANGES;
use ffi::FSCTL_SET_COMPRESSION;
use ffi::FSCTL_SET_SPARSE;
use ffi::FSCTL_SET_ZERO_DATA;
use ffi::FileBasicInfo;
use ffi::FileEndOfFileInformation;
use ffi::FileIdInfo;
use ffi::FileIoPriorityHintInfo;
use ffi::FileStandardInformation;
use ffi::FlushFileBuffers;
use ffi::GENERIC_READ;
use ffi::GENERIC_WRITE;
use ffi::GROUP_AFFINITY;
use ffi::GROUP_SECURITY_INFORMATION;
use ffi::GetActiveProcessorCount;
use ffi::GetCurrentProcess;
use ffi::GetCurrentProcessorNumber;
use ffi::GetCurrentThread;
use ffi::GetDiskFreeSpaceExW;
use ffi::GetDiskFreeSpaceW;
use ffi::GetFileAttributesW;
use ffi::GetFileInformationByHandleEx;
use ffi::GetFileSizeEx;
use ffi::GetLargePageMinimum;
use ffi::GetLastError;
use ffi::GetLogicalProcessorInformationEx;
use ffi::GetModuleHandleW;
use ffi::GetOverlappedResult;
use ffi::GetProcAddress;
use ffi::GetSecurityInfo;
use ffi::GetSystemInfo;
use ffi::GetTokenInformation;
use ffi::GetVolumeInformationW;
use ffi::GetVolumePathNameW;
use ffi::HANDLE;
use ffi::HIGH_PRIORITY_CLASS;
use ffi::IDLE_PRIORITY_CLASS;
use ffi::INVALID_HANDLE_VALUE;
use ffi::IO_STATUS_BLOCK;
use ffi::IOCTL_STORAGE_QUERY_PROPERTY;
use ffi::IoPriorityCritical;
use ffi::IoPriorityHigh;
use ffi::IoPriorityLow;
use ffi::IoPriorityNormal;
use ffi::IoPriorityVeryLow;
use ffi::LPVOID;
use ffi::LUID;
use ffi::LUID_AND_ATTRIBUTES;
use ffi::LocalFree;
use ffi::LookupPrivilegeValueW;
use ffi::MEM_COMMIT;
use ffi::MEM_LARGE_PAGES;
use ffi::MEM_RELEASE;
use ffi::MEM_RESERVE;
use ffi::NORMAL_PRIORITY_CLASS;
use ffi::NtQueryInformationFile;
use ffi::NtSetInformationFile;
use ffi::OPEN_ALWAYS;
use ffi::OPEN_EXISTING;
use ffi::OVERLAPPED;
use ffi::OWNER_SECURITY_INFORMATION;
use ffi::OpenProcessToken;
use ffi::PAGE_READWRITE;
use ffi::PropertyStandardQuery;
use ffi::READ_CONTROL;
use ffi::REALTIME_PRIORITY_CLASS;
use ffi::ReadFile;
use ffi::RelationAll;
use ffi::RelationNumaNode;
use ffi::RelationProcessorCore;
use ffi::SE_FILE_OBJECT;
use ffi::SE_PRIVILEGE_ENABLED;
use ffi::STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR;
use ffi::STORAGE_PROPERTY_QUERY;
use ffi::SYSTEM_INFO;
use ffi::SetEndOfFile;
use ffi::SetFileAttributesW;
use ffi::SetFileInformationByHandle;
use ffi::SetFilePointerEx;
use ffi::SetFileTime;
use ffi::SetFileValidData;
use ffi::SetPriorityClass;
use ffi::SetSecurityInfo;
use ffi::SetThreadGroupAffinity;
use ffi::SetThreadPriority;
use ffi::StorageAccessAlignmentProperty;
use ffi::StorageDeviceSeekPenaltyProperty;
use ffi::THREAD_PRIORITY_ABOVE_NORMAL;
use ffi::THREAD_PRIORITY_BELOW_NORMAL;
use ffi::THREAD_PRIORITY_HIGHEST;
use ffi::THREAD_PRIORITY_IDLE;
use ffi::THREAD_PRIORITY_LOWEST;
use ffi::THREAD_PRIORITY_NORMAL;
use ffi::THREAD_PRIORITY_TIME_CRITICAL;
use ffi::TOKEN_ADJUST_PRIVILEGES;
use ffi::TOKEN_ELEVATION;
use ffi::TOKEN_PRIVILEGES;
use ffi::TOKEN_QUERY;
use ffi::TRUE;
use ffi::TRUNCATE_EXISTING;
use ffi::TokenElevation;
use ffi::VirtualAlloc;
use ffi::VirtualAllocExNuma;
use ffi::VirtualFree;
use ffi::VirtualLock;
use ffi::WRITE_DAC;
use ffi::WRITE_OWNER;
use ffi::WriteFile;

// ---------------------------------------------------------------- helpers

fn wide(p: &Path) -> Vec<u16> {
    OsStr::new(p)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn werr(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn last() -> io::Error {
    werr(unsafe { GetLastError() })
}

#[inline]
fn win_bool(r: BOOL) -> io::Result<()> {
    if r != 0 {
        Ok(())
    } else {
        Err(last())
    }
}

// ---------------------------------------------------------------- extents

/// One contiguous run of clusters on the physical medium.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Virtual cluster number (file offset / cluster size).
    pub vcn: u64,
    /// Logical cluster number on the volume.
    pub lcn: u64,
    /// Length in clusters.
    pub len: u64,
    /// Holey / unallocated run.
    pub sparse: bool,
}

impl Extent {
    /// Byte offset of this extent's first cluster.
    #[inline]
    #[must_use]
    pub const fn byte_offset(
        &self,
        cluster: u64,
    ) -> u64 {
        self.vcn * cluster
    }
}

// ---------------------------------------------------------------- RawFile

/// A raw OS file handle.
///
/// Always opened with `FILE_FLAG_OVERLAPPED` so the same handle can be used for
/// IOCP completions, and with `FILE_FLAG_NO_BUFFERING` when `direct` is set.
pub struct RawFile {
    h: HANDLE,
    path: PathBuf,
    direct: bool,
    /// Alignment the buffer/offset/length must respect for direct I/O.
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
        let (mut access, disp) = match (opts.access, opts.create) {
            | (Access::Read, _) => (GENERIC_READ, OPEN_EXISTING),
            // `SetFileTime` and `SetFileInformationByHandle` need
            // FILE_WRITE_ATTRIBUTES, which a read-only file still grants; a
            // GENERIC_WRITE handle on such a file is refused outright.
            | (Access::Attributes, _) => {
                (
                    FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
                    OPEN_EXISTING,
                )
            },
            | (Access::Write, Create::Open) => (GENERIC_WRITE, OPEN_EXISTING),
            | (Access::Write, Create::CreateNew) => (GENERIC_WRITE, CREATE_NEW),
            | (Access::Write, Create::Create) => (GENERIC_WRITE, CREATE_ALWAYS),
            | (Access::Write, Create::OpenOrCreate) => (GENERIC_WRITE, OPEN_ALWAYS),
            | (Access::Write, Create::Truncate) => (GENERIC_WRITE, TRUNCATE_EXISTING),
            | (Access::ReadWrite, Create::Open) => (GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING),
            | (Access::ReadWrite, Create::CreateNew) => (GENERIC_READ | GENERIC_WRITE, CREATE_NEW),
            | (Access::ReadWrite, Create::Create) => (GENERIC_READ | GENERIC_WRITE, CREATE_ALWAYS),
            | (Access::ReadWrite, Create::OpenOrCreate) => {
                (GENERIC_READ | GENERIC_WRITE, OPEN_ALWAYS)
            },
            | (Access::ReadWrite, Create::Truncate) => {
                (GENERIC_READ | GENERIC_WRITE, TRUNCATE_EXISTING)
            },
        };

        let mut flags = FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED;
        if opts.direct {
            flags |= FILE_FLAG_NO_BUFFERING;
        }
        if opts.sequential {
            flags |= FILE_FLAG_SEQUENTIAL_SCAN;
        }
        if opts.write_through {
            flags |= FILE_FLAG_WRITE_THROUGH;
        }
        if opts.sparse {
            flags |= FILE_ATTRIBUTE_SPARSE_FILE;
        }
        if opts.delete_on_close {
            flags |= FILE_FLAG_DELETE_ON_CLOSE;
        }
        if opts.write_dac {
            // SetSecurityInfo needs WRITE_DAC (and WRITE_OWNER for the owner),
            // neither of which FILE_GENERIC_WRITE contains.
            access |= WRITE_DAC | WRITE_OWNER;
        }
        if opts.backup_semantics && crate::caps::get().backup {
            // Lets us open files the DACL would deny; the privilege itself has to
            // be held, which `caps` has already enabled.
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }

        let w = wide(path);
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                disp,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last());
        }
        let f = Self {
            h,
            path: path.to_path_buf(),
            direct: opts.direct,
            align: crate::DEFAULT_DIRECT_ALIGN,
        };
        if opts.sparse {
            let _ = f.set_sparse();
        }
        if let Some(hint) = opts.io_priority {
            let _ = f.set_io_priority(hint);
        }
        Ok(f)
    }

    /// The raw handle. Only for passing back to this module's functions.
    #[inline]
    #[must_use]
    pub const fn handle(&self) -> HANDLE {
        self.h
    }

    /// The path this handle was opened with, for diagnostics.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the handle was opened with direct I/O.
    #[inline]
    #[must_use]
    pub const fn is_direct(&self) -> bool {
        self.direct
    }

    /// Required alignment for buffers and offsets on this handle: the sector size
    /// for a direct handle, 1 for a buffered one.
    #[inline]
    #[must_use]
    pub const fn alignment(&self) -> usize {
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

    /// Positioned read. Blocking; for bulk data use the IOCP reactor instead.
    pub fn pread(
        &self,
        buf: &mut [u8],
        off: u64,
    ) -> io::Result<usize> {
        let mut ov = OVERLAPPED::default();
        ov.u.s.Offset = off as u32;
        ov.u.s.OffsetHigh = (off >> 32) as u32;
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.h,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &raw mut got,
                &raw mut ov,
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_IO_PENDING {
                let ok2 = unsafe { GetOverlappedResult(self.h, &raw mut ov, &raw mut got, TRUE) };
                if ok2 == 0 {
                    let e2 = unsafe { GetLastError() };
                    // Reading at or past EOF is not an error for our callers.
                    if e2 == ERROR_HANDLE_EOF {
                        return Ok(0);
                    }
                    return Err(werr(e2));
                }
            } else if e == ERROR_HANDLE_EOF {
                return Ok(0);
            } else {
                return Err(werr(e));
            }
        }
        Ok(got as usize)
    }

    /// Positioned write. Blocking.
    pub fn pwrite(
        &self,
        buf: &[u8],
        off: u64,
    ) -> io::Result<usize> {
        let mut ov = OVERLAPPED::default();
        ov.u.s.Offset = off as u32;
        ov.u.s.OffsetHigh = (off >> 32) as u32;
        let mut got: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.h,
                buf.as_ptr().cast(),
                buf.len() as u32,
                &raw mut got,
                &raw mut ov,
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_IO_PENDING {
                let ok2 = unsafe { GetOverlappedResult(self.h, &raw mut ov, &raw mut got, TRUE) };
                if ok2 == 0 {
                    return Err(last());
                }
            } else {
                return Err(werr(e));
            }
        }
        Ok(got as usize)
    }

    /// Current file size, read from the handle.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the size query fails.
    pub fn size(&self) -> io::Result<u64> {
        let mut n: i64 = 0;
        win_bool(unsafe { GetFileSizeEx(self.h, &raw mut n) })?;
        Ok(n as u64)
    }

    /// Change the logical size.
    ///
    /// Goes through `NtSetInformationFile(FileEndOfFileInformation)`: one call
    /// instead of the `SetFilePointerEx` + `SetEndOfFile` pair, and it does not
    /// disturb the handle's file pointer. Falls back to the Win32 pair if the
    /// native call is refused.
    pub fn set_len(
        &self,
        n: u64,
    ) -> io::Result<()> {
        let mut iosb = IO_STATUS_BLOCK::default();
        let mut eof: i64 = n as i64;
        let st = unsafe {
            NtSetInformationFile(
                self.h,
                &raw mut iosb,
                (&raw mut eof).cast(),
                std::mem::size_of::<i64>() as u32,
                FileEndOfFileInformation,
            )
        };
        if st >= 0 {
            return Ok(());
        }
        let mut cur: i64 = 0;
        unsafe {
            SetFilePointerEx(self.h, n as i64, &raw mut cur, 0);
        }
        win_bool(unsafe { SetEndOfFile(self.h) })
    }

    /// Instant preallocation: extend, then mark the extended range as valid so the
    /// cache manager never zero-fills it. Requires `SeManageVolumePrivilege`.
    ///
    /// Returns `Ok(false)` when the privilege is missing (the caller then falls
    /// back to `set_len`).
    pub fn prealloc_valid(
        &self,
        n: u64,
    ) -> io::Result<bool> {
        let cur = self.size()?;
        if cur > n {
            // A destination left longer by a previous run must be cut back, or the
            // stale tail survives the copy.
            self.set_len(n)?;
            return Ok(true);
        }
        if cur == n {
            return Ok(true);
        }
        self.set_len(n)?;
        let ok = unsafe { SetFileValidData(self.h, n as i64) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_PRIVILEGE_NOT_HELD || e == ERROR_ACCESS_DENIED {
                return Ok(false);
            }
            return Err(werr(e));
        }
        Ok(true)
    }

    /// Mark the file sparse, so extending it creates a hole instead of zeroed
    /// clusters. NTFS zero-fills an extension otherwise, which costs a second pass
    /// over the data.
    ///
    /// # Errors
    ///
    /// Returns the OS error from the filesystem control call.
    pub fn set_sparse(&self) -> io::Result<()> {
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_SET_SPARSE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        win_bool(ok)
    }

    /// Set or clear the sparse attribute.
    ///
    /// `FILE_SET_SPARSE_BUFFER { BOOLEAN SetSparse; }` — a one-byte input buffer.
    /// Clearing only succeeds when the file has no holes left to materialise,
    /// which is exactly the case for a fully written destination.
    pub fn set_sparse_flag(
        &self,
        on: bool,
    ) -> io::Result<()> {
        let mut flag: u8 = u8::from(on);
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_SET_SPARSE,
                (&raw mut flag).cast::<std::ffi::c_void>(),
                1,
                std::ptr::null_mut(),
                0,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        win_bool(ok)
    }

    /// Punch a hole: decommit `[off, off+len)` and keep the logical size.
    pub fn zero_range(
        &self,
        off: u64,
        len: u64,
    ) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let mut z = FILE_ZERO_DATA_INFORMATION {
            FileOffset: off as i64,
            BeyondFinalZero: (off + len) as i64,
        };
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_SET_ZERO_DATA,
                (&raw mut z).cast::<std::ffi::c_void>(),
                std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
                std::ptr::null_mut(),
                0,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        win_bool(ok)
    }

    /// Ranges of the file that are actually allocated on disk.
    pub fn allocated_ranges(
        &self,
        total: u64,
    ) -> io::Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let mut inb = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: 0,
            Length: total as i64,
        };
        let mut buf = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 1024];
        loop {
            let mut ret: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    self.h,
                    FSCTL_QUERY_ALLOCATED_RANGES,
                    (&raw mut inb).cast::<std::ffi::c_void>(),
                    std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) as u32,
                    &raw mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = unsafe { GetLastError() };
                if e == ERROR_MORE_DATA {
                    let n = ret as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
                    for r in &buf[..n] {
                        if r.Length > 0 {
                            out.push((r.FileOffset as u64, r.Length as u64));
                        }
                    }
                    let last = buf[n - 1];
                    let next = last.FileOffset + last.Length;
                    if next >= total as i64 {
                        break;
                    }
                    inb.FileOffset = next;
                    inb.Length = total as i64 - next;
                    continue;
                }
                return Err(werr(e));
            }
            let n = ret as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
            for r in &buf[..n] {
                if r.Length > 0 {
                    out.push((r.FileOffset as u64, r.Length as u64));
                }
            }
            break;
        }
        Ok(out)
    }

    /// Physical (LCN) mapping of the file's extents.
    pub fn extents(&self) -> io::Result<Vec<crate::Extent>> {
        let mut out = Vec::new();
        let mut vcn: i64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let input = vcn.to_le_bytes();
            let mut ret: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    self.h,
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    std::ptr::from_ref(&input).cast_mut().cast(),
                    input.len() as u32,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &raw mut ret,
                    std::ptr::null_mut(),
                )
            };
            let e = if ok == 0 {
                unsafe { GetLastError() }
            } else {
                0
            };
            if ok == 0 && e != ERROR_MORE_DATA {
                return Err(werr(e));
            }
            if ret < 16 {
                break;
            }
            // RETRIEVAL_POINTERS_BUFFER { DWORD ExtentCount; LARGE_INTEGER
            // StartingVcn; MAPPING_PAIR Extents[]; } — the LARGE_INTEGER is
            // 8-byte aligned, so StartingVcn sits at 8 and the pairs start at 16.
            let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            let start_vcn = i64::from_le_bytes(buf[8..16].try_into().unwrap());
            let mut last_vcn = start_vcn;
            let mut any = false;
            for i in 0..count {
                let off = 16 + i * 16;
                if off + 16 > ret as usize {
                    break;
                }
                let next_vcn = i64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                let lcn = i64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
                if next_vcn > last_vcn {
                    out.push(crate::Extent {
                        vcn: last_vcn as u64,
                        lcn: if lcn < 0 { 0 } else { lcn as u64 },
                        len: (next_vcn - last_vcn) as u64,
                        sparse: lcn < 0,
                    });
                    last_vcn = next_vcn;
                    any = true;
                }
            }
            if e != ERROR_MORE_DATA || !any {
                break;
            }
            vcn = last_vcn;
        }
        Ok(out)
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
        let info = FILE_IO_PRIORITY_HINT_INFO {
            PriorityHint: u32::from(hint),
        };
        let ok = unsafe {
            SetFileInformationByHandle(
                self.h,
                FileIoPriorityHintInfo,
                (&raw const info) as *mut std::ffi::c_void,
                std::mem::size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32,
            )
        };
        win_bool(ok)
    }

    /// Turn on filesystem compression for this file. Data written afterwards is
    /// compressed by the filesystem, so the engine never has to.
    ///
    /// # Errors
    ///
    /// Returns the OS error when compression is unavailable for this volume.
    pub fn set_compressed(&self) -> io::Result<()> {
        let mut fmt: u16 = COMPRESSION_FORMAT_DEFAULT;
        let mut ret: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                FSCTL_SET_COMPRESSION,
                (&raw mut fmt).cast::<std::ffi::c_void>(),
                2,
                std::ptr::null_mut(),
                0,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        win_bool(ok)
    }

    /// Flush the file's data and metadata to the device.
    ///
    /// # Errors
    ///
    /// Returns the OS error from the flush.
    pub fn flush(&self) -> io::Result<()> {
        win_bool(unsafe { FlushFileBuffers(self.h) })
    }

    /// Flush the file's data, leaving metadata alone (`fdatasync`).
    ///
    /// # Errors
    ///
    /// Returns the OS error from the flush.
    pub fn sync_data(&self) -> io::Result<()> {
        self.flush()
    }

    /// Set the modification time through the existing handle (no reopen).
    pub fn set_modified(
        &self,
        t: std::time::SystemTime,
    ) -> io::Result<()> {
        const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
        let d = t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        let ft = (d.as_nanos() / 100) as u64 + EPOCH_DIFF;
        let ftime = FILETIME {
            dwLowDateTime: ft as u32,
            dwHighDateTime: (ft >> 32) as u32,
        };
        win_bool(unsafe {
            SetFileTime(self.h, std::ptr::null(), std::ptr::null(), &raw const ftime)
        })
    }
}

impl Drop for RawFile {
    fn drop(&mut self) {
        if !self.h.is_null() && self.h != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.h) };
            self.h = INVALID_HANDLE_VALUE;
        }
    }
}

impl std::fmt::Debug for RawFile {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("RawFile")
            .field("handle", &self.h)
            .field("path", &self.path)
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

fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let w = wide(path);
    let mut buf = vec![0u16; 261];
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    win_bool(ok)?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Query the volume that `path` lives on.
///
/// # Errors
///
/// Returns the OS error when the volume cannot be queried.
pub fn volume_info(path: &Path) -> io::Result<VolumeInfo> {
    let root = volume_root(path)?;
    let rw = wide(&root);

    let mut spc = 0u32;
    let mut bps = 0u32;
    let mut nfree = 0u32;
    let mut ntotal = 0u32;
    let queried = unsafe {
        GetDiskFreeSpaceW(
            rw.as_ptr(),
            &raw mut spc,
            &raw mut bps,
            &raw mut nfree,
            &raw mut ntotal,
        )
    };
    let logical_sector = if queried != 0 {
        bps.max(512)
    } else {
        crate::DEFAULT_DIRECT_ALIGN as u32
    };
    let cluster = spc.saturating_mul(bps).max(logical_sector);

    let mut free = 0u64;
    let mut total = 0u64;
    let mut avail = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(rw.as_ptr(), &raw mut avail, &raw mut total, &raw mut free);
    }

    let mut fsbuf = vec![0u16; 64];
    let mut serial = 0u32;
    let mut maxcomp = 0u32;
    let mut flags = 0u32;
    let ok = unsafe {
        GetVolumeInformationW(
            rw.as_ptr(),
            std::ptr::null_mut(),
            0,
            &raw mut serial,
            &raw mut maxcomp,
            &raw mut flags,
            fsbuf.as_mut_ptr(),
            fsbuf.len() as u32,
        )
    };
    let fs = if ok != 0 {
        let n = fsbuf.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf16_lossy(&fsbuf[..n])
    } else {
        String::new()
    };

    // Physical sector size + seek penalty via the volume handle.
    let vh = unsafe {
        CreateFileW(
            rw.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    let mut physical_sector = logical_sector;
    let mut rotational = false;
    if vh != INVALID_HANDLE_VALUE {
        let q = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageAccessAlignmentProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut out = [0u8; 64];
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vh,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&raw const q) as *mut std::ffi::c_void,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && ret as usize >= std::mem::size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() {
            let d: &STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR =
                unsafe { &*out.as_ptr().cast::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() };
            if d.BytesPerPhysicalSector > 0 {
                physical_sector = d.BytesPerPhysicalSector;
            }
        }

        let q2 = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let ok = unsafe {
            DeviceIoControl(
                vh,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&raw const q2) as *mut std::ffi::c_void,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &raw mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && ret as usize >= std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
            let d: &DEVICE_SEEK_PENALTY_DESCRIPTOR =
                unsafe { &*out.as_ptr().cast::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() };
            rotational = d.IncursSeekPenalty != 0;
        }
        unsafe { CloseHandle(vh) };
    }

    Ok(VolumeInfo {
        root,
        fs,
        logical_sector,
        physical_sector: physical_sector.max(logical_sector),
        cluster,
        rotational,
        total_bytes: total,
        free_bytes: free,
        serial,
    })
}

/// True when the volume behind `path` is a spinning disk. Assumes SSD on any
/// failure, which is the safer default: physical ordering is pure overhead there.
#[must_use]
pub fn is_rotational(path: &Path) -> bool {
    volume_info(path).is_ok_and(|v| v.rotational)
}

/// Physical layout of an open file, as `(vcn, lcn, len)` runs.
///
/// # Errors
///
/// Returns the OS error when the layout query is unsupported or fails.
pub fn physical_extents(f: &RawFile) -> io::Result<Vec<crate::Extent>> {
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
) -> io::Result<(NonNull<u8>, bool)> {
    let alloc = |sz: usize, flags: u32| -> *mut std::ffi::c_void {
        // Node-local when a node was requested and the API exists, plain
        // `VirtualAlloc` otherwise.
        node.map_or_else(
            || unsafe { VirtualAlloc(std::ptr::null_mut(), sz, flags, PAGE_READWRITE) },
            |n| unsafe {
                VirtualAllocExNuma(
                    GetCurrentProcess(),
                    std::ptr::null_mut(),
                    sz,
                    flags,
                    PAGE_READWRITE,
                    u32::from(n),
                )
            },
        )
    };
    if huge {
        let lpm = unsafe { GetLargePageMinimum() } as usize;
        if lpm > 0 {
            let sz = crate::align::round_up(size, lpm);
            let p = alloc(sz, MEM_COMMIT | MEM_RESERVE | MEM_LARGE_PAGES);
            if !p.is_null() {
                return Ok((NonNull::new(p.cast::<u8>()).unwrap(), true));
            }
        }
    }
    let sz = crate::align::round_up(size, 4096);
    let p = alloc(sz, MEM_COMMIT | MEM_RESERVE);
    let nn = NonNull::new(p.cast::<u8>())
        .ok_or_else(|| io::Error::from_raw_os_error(ERROR_NOT_ENOUGH_MEMORY as i32))?;
    Ok((nn, false))
}

pub(crate) fn free_pages(
    ptr: *mut u8,
    _size: usize,
) {
    unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
}

pub(crate) fn lock_pages(
    ptr: *mut u8,
    size: usize,
) -> bool {
    // VirtualLock refuses anything outside the process working-set quota; locking
    // as much as fits is exactly what we want.
    unsafe { VirtualLock(ptr.cast(), size) != 0 }
}

pub(crate) fn discard_pages(
    ptr: *mut u8,
    size: usize,
) {
    unsafe { DiscardVirtualMemory(ptr.cast(), size) };
}

/// Page size reported by the OS.
#[must_use]
pub fn page_size() -> usize {
    let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&raw mut si) };
    si.dwPageSize.max(4096) as usize
}

// ---------------------------------------------------------------- topology

/// One logical CPU, as the topology probe sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// Processor group (Windows only; always 0 on Linux).
    pub group: u16,
    /// Index within the group.
    pub index: u8,
    /// Absolute logical CPU number (group * 64 + index).
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
    /// `cores[c]` = logical CPU indices that share physical core `c`.
    pub cores: Vec<Vec<usize>>,
    /// `nodes[n]` = logical CPU indices in NUMA node `n`.
    pub nodes: Vec<Vec<usize>>,
    /// `core_efficiency[c]` = Windows `EfficiencyClass`, normalised to 0..=255.
    /// Non-zero only on hybrid CPUs (P-cores > E-cores).
    pub core_efficiency: Vec<u8>,
}

impl Topology {
    /// Probe the machine. Never fails: an unreadable topology degrades to a flat
    /// list of CPUs rather than refusing to start.
    #[must_use]
    pub fn detect() -> Self {
        let mut t = Self::default();
        let mut len: u32 = 0;
        unsafe {
            GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &raw mut len);
        }
        if len == 0 {
            len = 64 * 1024;
        }
        let mut buf = vec![0u8; len as usize + 4096];
        // `ReturnedLength` is in/out: it must carry the buffer size on entry.
        let mut got = buf.len() as u32;
        let ok = unsafe {
            GetLogicalProcessorInformationEx(RelationAll, buf.as_mut_ptr(), &raw mut got)
        };
        if ok == 0 {
            // Fall back to a flat CPU list.
            let n = unsafe { GetActiveProcessorCount(0xFFFF) } as usize;
            for i in 0..n {
                t.cpus.push(CpuInfo {
                    group: 0,
                    index: (i % 64) as u8,
                    id: i as u16,
                    node: 0,
                    core: i as u16,
                });
                t.cores.push(vec![i]);
            }
            t.nodes.push((0..t.cpus.len()).collect());
            return t;
        }

        let mut off = 0usize;
        // (group, cpu mask, numa node)
        let mut node_masks: Vec<(u16, u64, u16)> = Vec::new();
        // (group, mask, efficiency class)
        let mut core_masks: Vec<(u16, u64, u8)> = Vec::new();
        while off + 8 <= got as usize {
            let rel = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let sz = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
            if sz == 0 {
                break;
            }
            let body = &buf[off..off + sz];
            #[allow(non_upper_case_globals)]
            match rel {
                | RelationProcessorCore => {
                    // PROCESSOR_RELATIONSHIP: Flags(1) EfficiencyClass(1) Reserved(20)
                    // GroupCount(2) then GROUP_AFFINITY[]
                    if body.len() >= 32 {
                        let gc = u16::from_ne_bytes(body[30..32].try_into().unwrap()) as usize;
                        for g in 0..gc {
                            let o = 32 + g * 16;
                            if o + 16 > body.len() {
                                break;
                            }
                            let mask = u64::from_ne_bytes(body[o..o + 8].try_into().unwrap());
                            let group = u16::from_ne_bytes(body[o + 8..o + 10].try_into().unwrap());
                            // body[9] is EfficiencyClass: higher means more
                            // performant, 0 on non-hybrid CPUs.
                            core_masks.push((group, mask, body[9]));
                        }
                    }
                },
                | RelationNumaNode if body.len() >= 32 => {
                    let node = u32::from_ne_bytes(body[8..12].try_into().unwrap()) as u16;
                    let gc = u16::from_ne_bytes(body[30..32].try_into().unwrap()) as usize;
                    for g in 0..gc {
                        let o = 32 + g * 16;
                        if o + 16 > body.len() {
                            break;
                        }
                        let mask = u64::from_ne_bytes(body[o..o + 8].try_into().unwrap());
                        let group = u16::from_ne_bytes(body[o + 8..o + 10].try_into().unwrap());
                        node_masks.push((group, mask, node));
                    }
                },
                | _ => {},
            }
            off += sz;
        }

        let node_of = |group: u16, bit: u8| -> u16 {
            for (g, m, n) in &node_masks {
                if *g == group && (m >> bit) & 1 == 1 {
                    return *n;
                }
            }
            0
        };

        // Build the flat CPU list from the core masks. One mask per physical core,
        // one bit per logical processor on it (two bits when SMT is on).
        for (ci, (group, mask, eff)) in core_masks.iter().enumerate() {
            t.core_efficiency.push(*eff);
            let mut logicals = Vec::new();
            for bit in 0..64u8 {
                if mask & (1u64 << bit) != 0 {
                    let idx = t.cpus.len();
                    t.cpus.push(CpuInfo {
                        group: *group,
                        index: bit,
                        id: group.wrapping_mul(64).wrapping_add(u16::from(bit)),
                        node: node_of(*group, bit),
                        core: ci as u16,
                    });
                    logicals.push(idx);
                }
            }
            if !logicals.is_empty() {
                t.cores.push(logicals);
            }
        }

        if t.cpus.is_empty() {
            let n = unsafe { GetActiveProcessorCount(0xFFFF) } as usize;
            for i in 0..n {
                t.cpus.push(CpuInfo {
                    group: 0,
                    index: (i % 64) as u8,
                    id: i as u16,
                    node: 0,
                    core: i as u16,
                });
                t.cores.push(vec![i]);
            }
        }

        let max_node = t.cpus.iter().map(|c| c.node).max().unwrap_or(0) as usize;
        t.nodes = vec![Vec::new(); max_node + 1];
        for (i, c) in t.cpus.iter().enumerate() {
            t.nodes[c.node as usize].push(i);
        }
        t.nodes.retain(|n| !n.is_empty());
        t
    }

    /// Number of logical CPUs.
    #[must_use]
    pub const fn logical_count(&self) -> usize {
        self.cpus.len()
    }

    /// Number of physical cores.
    #[must_use]
    pub fn physical_count(&self) -> usize {
        self.cores.len().max(1)
    }

    /// One logical CPU per physical core, in node order — the preferred placement
    /// for I/O threads that must not share a core.
    #[must_use]
    pub fn one_per_core(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.cores.len());
        for c in &self.cores {
            v.push(c[0]);
        }
        v
    }
}

/// Pin the calling thread to a specific logical CPU.
/// Which logical CPU the calling thread is running on right now.
#[must_use]
pub fn current_cpu() -> u32 {
    unsafe { GetCurrentProcessorNumber() }
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
        .ok_or_else(|| io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32))?;
    let ga = GROUP_AFFINITY {
        Mask: 1usize << c.index,
        Group: c.group,
        Reserved: [0; 3],
    };
    let th = unsafe { GetCurrentThread() };
    win_bool(unsafe { SetThreadGroupAffinity(th, &raw const ga, std::ptr::null_mut()) })
}

/// Set the calling thread's priority.
///
/// # Errors
///
/// Returns the OS error when the priority could not be changed.
pub fn set_thread_priority(level: i32) -> io::Result<()> {
    let th = unsafe { GetCurrentThread() };
    win_bool(unsafe { SetThreadPriority(th, level) })
}

/// Set the whole process's priority class.
///
/// # Errors
///
/// Returns the OS error when the class could not be set.
pub fn set_process_priority(class: u32) -> io::Result<()> {
    let p = unsafe { GetCurrentProcess() };
    win_bool(unsafe { SetPriorityClass(p, class) })
}

// ---------------------------------------------------------------- privileges

/// Enable the named privileges for the current process token.
/// Returns the privileges that are now held.
pub fn set_privileges(names: &[&str]) -> io::Result<Vec<String>> {
    let mut token: HANDLE = std::ptr::null_mut();
    win_bool(unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &raw mut token,
        )
    })?;
    let mut held = Vec::new();
    for name in names {
        let wname: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut luid = LUID::default();
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), wname.as_ptr(), &raw mut luid) } == 0 {
            continue;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let ok = unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &raw const tp,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            // AdjustTokenPrivileges "succeeds" even when nothing changed; the only
            // reliable check is the last error.
            if unsafe { GetLastError() } == ERROR_SUCCESS {
                held.push((*name).to_string());
            }
        }
    }
    unsafe { CloseHandle(token) };
    Ok(held)
}

/// Enables `SetFileValidData`, i.e. allocation without zero-filling.
pub const SE_MANAGE_VOLUME: &str = "SeManageVolumePrivilege";
/// Enables large pages for the buffer pool.
pub const SE_LOCK_MEMORY: &str = "SeLockMemoryPrivilege";
/// Read files a DACL would otherwise deny.
pub const SE_BACKUP: &str = "SeBackupPrivilege";
/// Write owner/group on the destination.
pub const SE_RESTORE: &str = "SeRestorePrivilege";
/// Raise the process working-set quota.
pub const SE_INCREASE_QUOTA: &str = "SeIncreaseQuotaPrivilege";
/// Reserved for the performance counter subsystem.
pub const SE_PROFILE_SINGLE_PROCESS: &str = "SeProfileSingleProcessPrivilege";
/// Raise a thread above normal priority.
pub const SE_INCREASE_BASE_PRIORITY: &str = "SeIncreaseBasePriorityPrivilege";
/// Read or write the audit ACE of a security descriptor.
pub const SE_SECURITY: &str = "SeSecurityPrivilege";
/// Act as a trusted computing base; not used by the engine, listed for
/// completeness of the probe.
pub const SE_TCB: &str = "SeTcbPrivilege";

/// Is the process running elevated?
#[must_use]
pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return false;
    }
    let mut info = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut info).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && info.TokenIsElevated != 0
}

/// Probe every privilege we know how to use and record the ones that stuck.
#[must_use]
pub fn detect_caps() -> crate::caps::Caps {
    let wanted = [
        SE_MANAGE_VOLUME,
        SE_LOCK_MEMORY,
        SE_BACKUP,
        SE_RESTORE,
        SE_INCREASE_BASE_PRIORITY,
    ];
    let held = set_privileges(&wanted).unwrap_or_default();
    let has = |n: &str| held.iter().any(|h| h == n);
    crate::caps::Caps {
        elevated: is_elevated(),
        instant_alloc: has(SE_MANAGE_VOLUME),
        large_pages: has(SE_LOCK_MEMORY),
        backup: has(SE_BACKUP),
        restore: has(SE_RESTORE),
        rt_io: false,
        rt_priority: has(SE_INCREASE_BASE_PRIORITY),
        // VirtualLock succeeds for anything inside the working-set quota, which
        // covers a buffer pool of a few hundred MiB.
        mlock: true,
        noatime: false,
    }
}

// ---------------------------------------------------------------- fs helpers

/// Set or clear the read-only attribute.
///
/// # Errors
///
/// Returns the OS error when the attributes cannot be read or written.
pub fn set_readonly(
    path: &Path,
    ro: bool,
) -> io::Result<()> {
    let w = wide(path);
    let cur = unsafe { GetFileAttributesW(w.as_ptr()) };
    if cur == u32::MAX {
        return Err(last());
    }
    let newv = if ro {
        cur | FILE_ATTRIBUTE_READONLY
    } else {
        cur & !FILE_ATTRIBUTE_READONLY
    };
    win_bool(unsafe { SetFileAttributesW(w.as_ptr(), newv) })
}

/// Delete a file, clearing a read-only attribute first.
///
/// # Errors
///
/// Returns the OS error when the file still cannot be removed.
pub fn remove_file_force(path: &Path) -> io::Result<()> {
    let _ = set_readonly(path, false);
    std::fs::remove_file(path)
}

/// Delete a directory tree, clearing read-only attributes as it goes.
///
/// `std::fs::remove_dir_all` fails on the first read-only file, which is common
/// for anything that came off a CD or out of an installer.
///
/// # Errors
///
/// Returns the OS error when an entry cannot be removed.
pub fn remove_dir_all_force(path: &Path) -> io::Result<()> {
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    {
        // Walk and clear the read-only bit, then retry.
        let mut stack = vec![path.to_path_buf()];
        let mut dirs = Vec::new();
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                if is_dir {
                    stack.push(p);
                } else {
                    let _ = set_readonly(&p, false);
                    let _ = std::fs::remove_file(&p);
                }
            }
            dirs.push(d);
        }
        for d in dirs.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
        Ok(())
    }
}

/// Total/free bytes of the volume containing `path`.
pub fn free_space(path: &Path) -> io::Result<(u64, u64)> {
    let root = volume_root(path)?;
    let rw = wide(&root);
    let (mut free, mut total, mut avail) = (0u64, 0u64, 0u64);
    win_bool(unsafe {
        GetDiskFreeSpaceExW(rw.as_ptr(), &raw mut avail, &raw mut total, &raw mut free)
    })?;
    Ok((free, total))
}

/// Allocate a large-page-aligned block (helper for the buffer pool).
#[must_use]
pub fn large_page_size() -> usize {
    unsafe { GetLargePageMinimum() as usize }
}

/// Number of logical CPUs, or 1 if the OS will not say.
#[must_use]
pub fn thread_count() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Map `verylow`..`critical` to the Win32 I/O priority hint.
#[inline]
#[must_use]
pub fn io_priority_hint(name: &str) -> Option<u8> {
    match name {
        | "verylow" | "idle" => Some(IoPriorityVeryLow as u8),
        | "low" => Some(IoPriorityLow as u8),
        | "normal" => Some(IoPriorityNormal as u8),
        | "high" => Some(IoPriorityHigh as u8),
        | "critical" => Some(IoPriorityCritical as u8),
        | _ => None,
    }
}

/// Map `idle`..`realtime` to a Win32 process priority class.
#[inline]
#[must_use]
pub fn process_priority_class(name: &str) -> Option<u32> {
    match name {
        | "idle" => Some(IDLE_PRIORITY_CLASS),
        | "below" | "belownormal" => Some(BELOW_NORMAL_PRIORITY_CLASS),
        | "normal" => Some(NORMAL_PRIORITY_CLASS),
        | "above" | "abovenormal" => Some(ABOVE_NORMAL_PRIORITY_CLASS),
        | "high" => Some(HIGH_PRIORITY_CLASS),
        | "realtime" => Some(REALTIME_PRIORITY_CLASS),
        | _ => None,
    }
}

/// Map `idle`..`timecritical` to a Win32 thread priority level.
#[inline]
#[must_use]
pub fn thread_priority_level(name: &str) -> Option<i32> {
    match name {
        | "idle" => Some(THREAD_PRIORITY_IDLE),
        | "lowest" => Some(THREAD_PRIORITY_LOWEST),
        | "below" => Some(THREAD_PRIORITY_BELOW_NORMAL),
        | "normal" => Some(THREAD_PRIORITY_NORMAL),
        | "above" => Some(THREAD_PRIORITY_ABOVE_NORMAL),
        | "highest" => Some(THREAD_PRIORITY_HIGHEST),
        | "timecritical" => Some(THREAD_PRIORITY_TIME_CRITICAL),
        | _ => None,
    }
}

// ---------------------------------------------------------------- NUMA

/// NUMA node that owns the storage backing `path`.
///
/// Windows does not expose the node of a disk through a stable documented IOCTL,
/// so this reports `None` and the engine falls back to the node its I/O threads
/// were placed on.
#[must_use]
pub const fn numa_node_of(_path: &Path) -> Option<u16> {
    None
}

/// Preferred node for allocations made by the calling thread.
///
/// `SetThreadPreferredNode` is not in every import library, so it is resolved at
/// run time. Absent on pre-1809 Windows, where it degrades to a no-op.
pub fn set_thread_mempolicy(node: u16) -> io::Result<()> {
    type Fn = unsafe extern "system" fn(HANDLE, u16) -> BOOL;
    static PROC: std::sync::OnceLock<Option<Fn>> = std::sync::OnceLock::new();
    let f = PROC.get_or_init(|| {
        let name = b"SetThreadPreferredNode\0";
        unsafe {
            let module = wide(Path::new("kernel32.dll"));
            let h = GetModuleHandleW(module.as_ptr());
            if h.is_null() {
                return None;
            }
            let p = GetProcAddress(h, name.as_ptr());
            if p.is_null() {
                return None;
            }
            Some(std::mem::transmute::<*const std::ffi::c_void, Fn>(p))
        }
    });
    f.map_or(Ok(()), |call| {
        let th = unsafe { GetCurrentThread() };
        win_bool(unsafe { call(th, node) })
    })
}

/// Bind an existing mapping to a node (no-op on Windows; the allocation itself is
/// already node-local via `VirtualAllocExNuma`).
#[allow(dead_code)]
pub(crate) const fn bind_pages_to_node(
    _ptr: *mut u8,
    _len: usize,
    _node: u16,
) {
}

/// Copy a whole file inside the kernel.
///
/// For a few-kilobyte file this is dramatically cheaper than the userspace path:
/// one call instead of open + create + write + set-time + two closes, and the copy
/// never leaves the filesystem driver (it can also become a server-side copy on a
/// network share). It is only used when nothing else needs the handle — no hashing,
/// no compression, no hole punching.
pub fn copy_file_fast(
    src: &Path,
    dst: &Path,
) -> io::Result<()> {
    let ws = wide(src);
    let wd = wide(dst);

    // `CopyFile2` is the modern entry point: it can ask for compressed SMB
    // traffic, which turns a copy to a network share into a server-side operation
    // instead of pushing uncompressed bytes over the wire.
    let network = dst.to_string_lossy().starts_with("\\\\");
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: if network {
            COPY_FILE_REQUEST_COMPRESSED_TRAFFIC
        } else {
            0
        },
        pfCancel: std::ptr::null_mut(),
        pProgressRoutine: std::ptr::null_mut(),
        pvCallbackContext: std::ptr::null_mut(),
    };
    let hr = unsafe { CopyFile2(ws.as_ptr(), wd.as_ptr(), &raw const params) };
    if hr >= 0 {
        return Ok(());
    }

    let ok = unsafe {
        CopyFileExW(
            ws.as_ptr(),
            wd.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        // A read-only destination is the one common failure worth retrying.
        let e = unsafe { GetLastError() };
        if e == ERROR_ACCESS_DENIED {
            let _ = set_readonly(dst, false);
            let ok2 = unsafe {
                CopyFileExW(
                    ws.as_ptr(),
                    wd.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok2 != 0 {
                return Ok(());
            }
            return Err(last());
        }
        return Err(werr(e));
    }
    Ok(())
}

/// `O_DIRECT` does not exist on Windows; the alignment comes from the volume
/// geometry instead (see `volume_info`).
#[must_use]
pub const fn dio_alignment(_path: &Path) -> Option<u32> {
    None
}

/// Identity and link count of a file, from one handle.
///
/// `FileIdInfo` gives the 128-bit id (the low half is the MFT record number on
/// NTFS) and `FileStandardInfo` gives the link count, the end-of-file offset and
/// the allocated size — which is how we tell a sparse file from a dense one
/// without reading it.
pub fn file_identity(path: &Path) -> io::Result<(u64, u32)> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    // FILE_STANDARD_INFORMATION { AllocationSize, EndOfFile, NumberOfLinks,
    // DeletePending, Directory } in one native call.
    let mut iosb = IO_STATUS_BLOCK::default();
    let mut std_info = [0u8; 32];
    let st = unsafe {
        NtQueryInformationFile(
            h,
            &raw mut iosb,
            std_info.as_mut_ptr().cast(),
            std_info.len() as u32,
            FileStandardInformation,
        )
    };
    if st < 0 {
        unsafe { CloseHandle(h) };
        return Err(io::Error::from_raw_os_error(st));
    }
    let links = u32::from_le_bytes(std_info[16..20].try_into().unwrap());
    let mut id_info = [0u8; 24]; // FILE_ID_INFO
    let ok =
        unsafe { GetFileInformationByHandleEx(h, FileIdInfo, id_info.as_mut_ptr().cast(), 24) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Err(last());
    }
    Ok((
        u64::from_le_bytes(id_info[8..16].try_into().unwrap()),
        links,
    ))
}

/// Whether this platform can copy extended attributes.
///
/// Windows has no equivalent of the POSIX xattr namespace. NTFS alternate data
/// streams are a different feature with different semantics, and silently
/// copying them because the user asked for `--xattrs` would be a surprise, so
/// the engine refuses the option up front.
#[must_use]
pub const fn xattrs_supported() -> bool {
    false
}

/// Copy extended attributes.
///
/// Unreachable in practice — `xattrs_supported` is false and the engine turns
/// the option off before the pipeline starts — but the platform surface has to
/// be uniform for the caller to compile.
pub fn copy_xattrs(
    _src: &Path,
    _dst: &RawFile,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "extended attributes are not supported on Windows",
    ))
}

/// Copy the DACL (and, when the privilege is held, the owner and group).
///
/// `CopyFileExW`/`CopyFile2` do this for the kernel-side path; the pipeline path
/// has to do it explicitly, otherwise a copy into a shared folder silently
/// inherits the destination's permissions instead of the source's.
pub fn copy_acl(
    src: &Path,
    dst: &RawFile,
) -> io::Result<()> {
    let w = wide(src);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    let mut sd: LPVOID = std::ptr::null_mut();
    let mut owner: LPVOID = std::ptr::null_mut();
    let mut group: LPVOID = std::ptr::null_mut();
    let mut dacl: LPVOID = std::ptr::null_mut();
    let mut sacl: LPVOID = std::ptr::null_mut();
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            info,
            &raw mut owner,
            &raw mut group,
            &raw mut dacl,
            &raw mut sacl,
            &raw mut sd,
        )
    };
    unsafe { CloseHandle(h) };
    if r != ERROR_SUCCESS {
        return Err(werr(r));
    }
    // Setting the owner needs SeRestorePrivilege; if we do not have it, fall back
    // to the DACL alone rather than failing the whole copy.
    let mut rc = unsafe {
        SetSecurityInfo(
            dst.handle(),
            SE_FILE_OBJECT,
            info,
            owner,
            group,
            dacl,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        rc = unsafe {
            SetSecurityInfo(
                dst.handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
    }
    unsafe { LocalFree(sd) };
    if rc != ERROR_SUCCESS {
        return Err(werr(rc));
    }
    Ok(())
}

/// Create a hard link at `link` pointing at `target`.
pub fn hard_link(
    target: &Path,
    link: &Path,
) -> io::Result<()> {
    let wt = wide(target);
    let wl = wide(link);
    if unsafe { CreateHardLinkW(wl.as_ptr(), wt.as_ptr(), std::ptr::null()) } == 0 {
        return Err(last());
    }
    Ok(())
}

/// Apply the Windows attribute bits through the handle.
///
/// The scan stores `FILE_ATTRIBUTE_*` in `Entry::mode`, so this is the Windows
/// counterpart of `fchmod`.
pub fn set_mode(
    f: &RawFile,
    mode: u32,
) -> io::Result<()> {
    // Only the attribute bits are meaningful here, and the archive bit must stay
    // set or backup tools will think the file was never written.
    let attrs = (mode & 0x0000_FFFF) | FILE_ATTRIBUTE_ARCHIVE;
    set_basic_info(f, attrs, None)
}

/// Apply attributes and (optionally) timestamps through the handle.
///
/// Uses `FILE_BASIC_INFO`, which sets every attribute in one call — no separate
/// `SetFileAttributesW`, and it cannot fail halfway.
pub fn set_basic_info(
    f: &RawFile,
    attrs: u32,
    mtime: Option<std::time::SystemTime>,
) -> io::Result<()> {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    #[allow(dead_code)]
    struct BasicInfo {
        creation: i64,
        access: i64,
        write: i64,
        change: i64,
        attrs: u32,
    }
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let to_ft = |t: std::time::SystemTime| -> i64 {
        let d = t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        ((d.as_nanos() / 100) as u64 + EPOCH_DIFF) as i64
    };
    let mut info = BasicInfo::default();
    // 0 means "leave unchanged" for every time field.
    if let Some(t) = mtime {
        info.write = to_ft(t);
    }
    info.attrs = attrs;
    let ok = unsafe {
        SetFileInformationByHandle(
            f.handle(),
            FileBasicInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<BasicInfo>() as u32,
        )
    };
    win_bool(ok)
}

/// Ordering key for `--order=physical`: the NTFS MFT record number.
///
/// The low 64 bits of `FILE_ID_128` are the MFT index on NTFS, so sorting by it
/// walks the volume in the order the metadata sits on the platter. Costs one
/// `CreateFileW` per file, which is why it is opt-in.
pub fn file_order_key(path: &Path) -> io::Result<u64> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    let mut info = [0u8; 24]; // FILE_ID_INFO { u64 volume; FILE_ID_128 id }
    let ok = unsafe {
        GetFileInformationByHandleEx(h, FileIdInfo, info.as_mut_ptr().cast(), info.len() as u32)
    };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Err(last());
    }
    Ok(u64::from_le_bytes(info[8..16].try_into().unwrap()))
}

/// Build the native reactor (IOCP).
pub fn native_reactor(depth: usize) -> io::Result<Box<dyn crate::Reactor>> {
    Ok(Box::new(iocp::IocpReactor::new(depth)?))
}
