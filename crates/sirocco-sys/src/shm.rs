//! A small shared-memory segment backed by a file.
//!
//! Used to publish progress counters from a split worker to its parent without a
//! pipe, a socket or a publisher thread. Backing it with a regular file instead of
//! a named object keeps one implementation for both platforms and leaves nothing
//! behind when the run ends.

use std::io;
use std::path::Path;

/// A named shared-memory segment, used to publish progress counters from a
/// worker process to its parent.
pub struct Shm {
    ptr: *mut u8,
    len: usize,
    #[cfg(windows)]
    mapping: *mut core::ffi::c_void,
    #[cfg(windows)]
    file: std::fs::File,
    #[cfg(unix)]
    fd: i32,
}

// SAFETY: the segment is plain memory shared by design; callers synchronise
// through atomics.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Shm {
    /// Create (or truncate) a segment of `len` bytes.
    pub fn create(
        path: &Path,
        len: usize,
    ) -> io::Result<Self> {
        imp::create(path, len)
    }

    /// Attach to an existing segment.
    pub fn open(path: &Path) -> io::Result<Self> {
        imp::open(path)
    }

    /// Size of the segment in bytes.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True for a zero-length segment, which is never useful but keeps `len`
    /// company.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Typed view of the segment.
    ///
    /// # Safety
    /// `T` must be plain-old-data whose size does not exceed [`Shm::len`], and
    /// concurrent access must be synchronised by the caller.
    #[inline]
    #[must_use]
    pub unsafe fn as_ptr<T>(&self) -> *mut T {
        debug_assert!(std::mem::size_of::<T>() <= self.len);
        self.ptr.cast()
    }
}

#[cfg(windows)]
mod imp {
    use super::Path;
    use super::Shm;
    use super::io;
    use crate::win::ffi::CloseHandle;
    use crate::win::ffi::CreateFileMappingW;
    use crate::win::ffi::HANDLE;
    use crate::win::ffi::MapViewOfFile;
    use crate::win::ffi::PAGE_READWRITE;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;

    fn wide(p: &Path) -> Vec<u16> {
        std::ffi::OsStr::new(p)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn create(
        path: &Path,
        len: usize,
    ) -> io::Result<Shm> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len as u64)?;
        map(file, len)
    }

    pub fn open(path: &Path) -> io::Result<Shm> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let len = file.metadata()?.len() as usize;
        map(file, len)
    }

    fn map(
        file: std::fs::File,
        len: usize,
    ) -> io::Result<Shm> {
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle() as HANDLE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        // FILE_MAP_ALL_ACCESS: the counters are written through this view, so a
        // read-only mapping would fault on the first `fetch_add`.
        let ptr = unsafe { MapViewOfFile(mapping, 0x000F_001F, 0, 0, len) };
        if ptr.is_null() {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(mapping) };
            return Err(e);
        }
        let _ = wide(Path::new(""));
        Ok(Shm {
            ptr: ptr.cast::<u8>(),
            len,
            mapping,
            file,
        })
    }
}

#[cfg(windows)]
impl Drop for Shm {
    fn drop(&mut self) {
        use crate::win::ffi::CloseHandle;
        use crate::win::ffi::UnmapViewOfFile;
        unsafe {
            if !self.ptr.is_null() {
                UnmapViewOfFile(self.ptr.cast());
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
            }
        }
        // `file` closes itself.
        let _ = &self.file;
    }
}

#[cfg(unix)]
mod imp {
    use super::Path;
    use super::Shm;
    use super::io;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::IntoRawFd;

    pub fn create(
        path: &Path,
        len: usize,
    ) -> io::Result<Shm> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len as u64)?;
        let fd = file.into_raw_fd();
        map(fd, len)
    }

    pub fn open(path: &Path) -> io::Result<Shm> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, std::ptr::addr_of_mut!(st)) } < 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        map(fd, st.st_size as usize)
    }

    fn map(
        fd: i32,
        len: usize,
    ) -> io::Result<Shm> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        Ok(Shm {
            ptr: ptr.cast::<u8>(),
            len,
            fd,
        })
    }
}

#[cfg(unix)]
impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                libc::munmap(self.ptr.cast(), self.len);
            }
            if self.fd >= 0 {
                libc::close(self.fd);
            }
        }
    }
}
