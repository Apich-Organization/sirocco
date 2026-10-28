//! IOCP-backed reactor.
//!
//! One completion port per thread, one file handle per (file, thread) — a handle
//! can only be associated with a single port, and per-thread handles are cheaper
//! than the synchronisation needed to share them.
//!
//! Submissions are posted straight to the kernel; completions are reaped in
//! batches with `GetQueuedCompletionStatusEx`, which avoids one syscall per
//! operation.

use std::io;
use std::path::Path;

use crate::Completion;
use crate::OpenOpts;
use crate::RawFile;
use crate::Reactor;
use crate::slab::Slab;

use super::ffi::CancelIoEx;
use super::ffi::CloseHandle;
use super::ffi::CreateIoCompletionPort;
use super::ffi::ERROR_IO_PENDING;
use super::ffi::ERROR_WAIT_TIMEOUT;
use super::ffi::FALSE;
use super::ffi::GetLastError;
use super::ffi::GetQueuedCompletionStatusEx;
use super::ffi::HANDLE;
use super::ffi::INFINITE;
use super::ffi::INVALID_HANDLE_VALUE;
use super::ffi::LPCVOID;
use super::ffi::LPVOID;
use super::ffi::OVERLAPPED;
use super::ffi::OVERLAPPED_ENTRY;
use super::ffi::ReadFile;
use super::ffi::RtlNtStatusToDosError;
use super::ffi::WriteFile;

#[repr(C)]
struct OvEx {
    ov: OVERLAPPED,
    user: u64,
    h: HANDLE,
    op: u32,
    _pad: u32,
}

/// IOCP-backed reactor. One per I/O thread, as IOCP requires.
pub struct IocpReactor {
    port: HANDLE,
    slab: Slab<OvEx>,
    entries: Vec<OVERLAPPED_ENTRY>,
    in_flight: usize,
}

// The port handle is owned by the reactor and only touched from its own thread.
unsafe impl Send for IocpReactor {}

impl IocpReactor {
    /// Create a completion port with room for `depth` outstanding operations.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the port cannot be created.
    pub fn new(depth: usize) -> io::Result<Self> {
        let depth = depth.clamp(8, 1 << 16);
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Reap in batches as large as the ring is deep; a fixed 64 forced
        // ceil(overlap/64) syscalls per drain once overlap went past that.
        let batch = depth.clamp(64, 1024);
        let entries = vec![
            OVERLAPPED_ENTRY {
                lpCompletionKey: 0,
                lpOverlapped: std::ptr::null_mut(),
                Internal: 0,
                dwNumberOfBytesTransferred: 0,
            };
            batch
        ];
        Ok(Self {
            port,
            slab: Slab::with_capacity(depth),
            entries,
            in_flight: 0,
        })
    }

    #[inline]
    fn submit(
        &mut self,
        f: &RawFile,
        buf: *mut u8,
        len: usize,
        off: u64,
        user: u64,
        write: bool,
    ) -> io::Result<()> {
        let idx = self
            .slab
            .alloc(OvEx {
                ov: OVERLAPPED::default(),
                user,
                h: f.handle(),
                op: u32::from(write),
                _pad: 0,
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "iocp: no free overlapped slot (depth exhausted)",
                )
            })?;

        let e = unsafe { self.slab.get_mut(idx) };
        e.ov.u.s.Offset = off as u32;
        e.ov.u.s.OffsetHigh = (off >> 32) as u32;
        e.ov.hEvent = std::ptr::null_mut();
        let ovp: *mut OVERLAPPED = &raw mut e.ov;

        let mut got: u32 = 0;
        let ok = if write {
            unsafe { WriteFile(f.handle(), buf as LPCVOID, len as u32, &raw mut got, ovp) }
        } else {
            unsafe { ReadFile(f.handle(), buf as LPVOID, len as u32, &raw mut got, ovp) }
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                unsafe { self.slab.free(idx) };
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        // Whether it completed inline or is pending, a packet is queued because
        // the handle is associated with this port.
        self.in_flight += 1;
        Ok(())
    }
}

impl Reactor for IocpReactor {
    fn open(
        &mut self,
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<RawFile> {
        let f = RawFile::open(path, opts)?;
        let p = unsafe { CreateIoCompletionPort(f.handle(), self.port, 0, 1) };
        if p.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(f)
    }

    unsafe fn read(
        &mut self,
        f: &RawFile,
        buf: *mut u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        self.submit(f, buf, len, off, user, false)
    }

    unsafe fn write(
        &mut self,
        f: &RawFile,
        buf: *const u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        self.submit(f, buf.cast_mut(), len, off, user, true)
    }

    fn poll(
        &mut self,
        want: usize,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        if self.in_flight == 0 {
            return Ok(0);
        }
        let mut total = 0usize;
        // Block for the first completion, then take whatever else is already
        // queued and return. Waiting for all `want` completions would drain the
        // device queue every round trip: the pipeline would stall on its slowest
        // request instead of refilling as soon as anything lands.
        while total < want {
            let batch = (want - total).min(self.entries.len()).max(1);
            let mut n: u32 = 0;
            let timeout = if total == 0 { INFINITE } else { 0 };
            let ok = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port,
                    self.entries.as_mut_ptr(),
                    batch as u32,
                    &raw mut n,
                    timeout,
                    FALSE,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if n == 0 {
                    // A zero timeout with nothing ready is the normal "drained"
                    // answer; anything else is a real failure.
                    if total > 0 || e.raw_os_error() == Some(ERROR_WAIT_TIMEOUT as i32) {
                        break;
                    }
                    return Err(e);
                }
            }
            for i in 0..n as usize {
                let ent = unsafe { std::ptr::read(&raw const self.entries[i]) };
                let ovp = ent.lpOverlapped;
                if ovp.is_null() {
                    // A wake-up packet: it still counts against `in_flight`, or the
                    // counter would drift and `poll` could block forever.
                    self.in_flight = self.in_flight.saturating_sub(1);
                    continue;
                }
                let Some(idx) = self.slab.index_of(ovp as *const OvEx) else {
                    continue;
                };
                let e = unsafe { self.slab.get(idx) };
                let user = e.user;
                let h = e.h;
                self.in_flight = self.in_flight.saturating_sub(1);

                let res: i64 = if ent.Internal == 0 {
                    i64::from(ent.dwNumberOfBytesTransferred)
                } else {
                    // `Internal` already holds the NTSTATUS; translating it costs
                    // one call instead of the two GetOverlappedResult would make.
                    let code = unsafe { RtlNtStatusToDosError(ent.Internal as i32) };
                    let _ = h;
                    -i64::from(code.max(1))
                };
                out.push(Completion { user, res });
                unsafe { self.slab.free(idx) };
                total += 1;
            }
            if (n as usize) < batch {
                // The queue is empty; nothing more is ready right now.
                break;
            }
        }
        Ok(total)
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn name(&self) -> &'static str {
        "iocp"
    }

    fn cancel(&mut self) {
        // A null handle means "every handle this process owns", which is exactly
        // what an interrupt wants. The cancelled operations still post
        // completions (with ERROR_OPERATION_ABORTED), so the slab is released and
        // the buffers go back to the pool through the normal path.
        unsafe { CancelIoEx(std::ptr::null_mut(), std::ptr::null_mut()) };
    }
}

impl Drop for IocpReactor {
    fn drop(&mut self) {
        if self.port.is_null() {
            return;
        }
        // Abandon anything still in flight and reap it before the slab goes away:
        // the driver still owns those OVERLAPPEDs, and freeing them under it is a
        // use-after-free that only shows up on an error path.
        if self.in_flight > 0 {
            unsafe { CancelIoEx(std::ptr::null_mut(), std::ptr::null_mut()) };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while self.in_flight > 0 && std::time::Instant::now() < deadline {
                let mut n: u32 = 0;
                let ok = unsafe {
                    GetQueuedCompletionStatusEx(
                        self.port,
                        self.entries.as_mut_ptr(),
                        self.entries.len() as u32,
                        &raw mut n,
                        50,
                        FALSE,
                    )
                };
                if ok == 0 && n == 0 {
                    // A 50 ms timeout is not the deadline; keep trying until the
                    // real deadline, or the driver may still own freed memory.
                    continue;
                }
                for i in 0..n as usize {
                    let ent = unsafe { std::ptr::read(&raw const self.entries[i]) };
                    if ent.lpOverlapped.is_null() {
                        self.in_flight = self.in_flight.saturating_sub(1);
                        continue;
                    }
                    if let Some(idx) = self.slab.index_of(ent.lpOverlapped as *const OvEx) {
                        unsafe { self.slab.free(idx) };
                        self.in_flight = self.in_flight.saturating_sub(1);
                    }
                }
            }
        }
        unsafe { CloseHandle(self.port) };
        self.port = std::ptr::null_mut();
    }
}
