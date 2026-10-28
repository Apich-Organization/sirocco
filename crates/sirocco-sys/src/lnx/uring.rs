//! Hand-rolled `io_uring` reactor.
//!
//! No liburing. We mmap the SQ/CQ rings ourselves, write SQEs directly into the
//! shared array and batch submissions with a single `io_uring_enter`. Fixed
//! buffers (`IORING_REGISTER_BUFFERS`) are used when the engine registers the
//! buffer pool, which is the single biggest win: without it every operation pays
//! for `get_user_pages` on a 1 MiB buffer.

// --- lint exceptions for this module ----------------------------------------
//
// Same reasoning as `lnx/mod.rs`: this is the ring layout as the kernel defines
// it, plus the `syscall` wrappers. `too_many_arguments` is on the list because
// `submit` takes one argument per field of an SQE — grouping them into a struct
// would mean building the struct on every request, on the hottest path there is.
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::use_self)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::ptr;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use crate::Completion;
use crate::OpenOpts;
use crate::RawFile;
use crate::Reactor;

use super::ffi::*;

struct Ring {
    fd: i32,
    // submission queue
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sq_array: *mut u32,
    sqes: *mut io_uring_sqe,
    // completion queue
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const io_uring_cqe,
    // mappings
    sq_map: *mut libc::c_void,
    sq_map_len: usize,
    cq_map: *mut libc::c_void,
    cq_map_len: usize,
    sqes_map: *mut libc::c_void,
    sqes_map_len: usize,
    single_mmap: bool,
    sqpoll: bool,
    pending_sq: u32,
}

unsafe impl Send for Ring {}

impl Ring {
    fn new(
        entries: u32,
        iopoll: bool,
        sqpoll: bool,
        sq_cpu: Option<u32>,
    ) -> io::Result<Ring> {
        let mut p: io_uring_params = unsafe { std::mem::zeroed() };
        if iopoll {
            p.flags |= IORING_SETUP_IOPOLL;
        }
        if sqpoll {
            p.flags |= IORING_SETUP_SQPOLL;
            if let Some(c) = sq_cpu {
                p.flags |= IORING_SETUP_SQ_AFF;
                p.sq_thread_cpu = c;
            }
            p.sq_thread_idle = 2000;
        }
        // These cut a syscall out of the fast path, but they are 5.19+; on an
        // older LTS kernel `io_uring_setup` rejects the whole call with EINVAL and
        // the engine would silently fall back to the portable thread pool. Try
        // with the flags, then without.
        let tuning = IORING_SETUP_COOP_TASKRUN | IORING_SETUP_SINGLE_ISSUER;
        let base_flags = p.flags;
        let entries = entries.next_power_of_two().clamp(8, 32768);
        let mut fd = -1;
        for attempt in 0..2 {
            p.flags = base_flags | if attempt == 0 { tuning } else { 0 };
            fd = unsafe {
                syscall(
                    SYS_IO_URING_SETUP,
                    entries as libc::c_long,
                    std::ptr::from_mut(&mut p),
                )
            } as i32;
            if fd >= 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINVAL) {
                return Err(e);
            }
            // EINVAL: retry without the tuning flags (or without IOPOLL/SQPOLL).
            p.flags = 0;
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let sq_ring_sz = (p.sq_off.array + p.sq_entries * 4) as usize;
        let cq_ring_sz =
            (p.cq_off.cqes + p.cq_entries * std::mem::size_of::<io_uring_cqe>() as u32) as usize;
        let sqes_sz = (p.sq_entries as usize) * std::mem::size_of::<io_uring_sqe>();

        let single = p.features & IORING_FEAT_SINGLE_MMAP != 0;
        let map_len = if single {
            sq_ring_sz.max(cq_ring_sz)
        } else {
            sq_ring_sz
        };

        let sq_map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                IORING_OFF_SQ_RING,
            )
        };
        if sq_map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let cq_map = if single {
            sq_map
        } else {
            let m = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    cq_ring_sz,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_POPULATE,
                    fd,
                    IORING_OFF_CQ_RING,
                )
            };
            if m == libc::MAP_FAILED {
                let e = io::Error::last_os_error();
                unsafe {
                    libc::munmap(sq_map, map_len);
                    libc::close(fd);
                }
                return Err(e);
            }
            m
        };
        let sqes_map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                sqes_sz,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                IORING_OFF_SQES,
            )
        };
        if sqes_map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe {
                libc::munmap(sq_map, map_len);
                if !single {
                    libc::munmap(cq_map, cq_ring_sz);
                }
                libc::close(fd);
            }
            return Err(e);
        }

        let b =
            |base: *mut libc::c_void, off: u32| unsafe { (base as *const u8).add(off as usize) };

        Ok(Ring {
            fd,
            sq_head: b(sq_map, p.sq_off.head).cast(),
            sq_tail: b(sq_map, p.sq_off.tail).cast(),
            sq_mask: unsafe { *(b(sq_map, p.sq_off.ring_mask) as *const u32) },
            sq_entries: p.sq_entries,
            sq_array: b(sq_map, p.sq_off.array) as *mut u32,
            sqes: sqes_map as *mut io_uring_sqe,
            cq_head: b(cq_map, p.cq_off.head).cast(),
            cq_tail: b(cq_map, p.cq_off.tail).cast(),
            cq_mask: unsafe { *(b(cq_map, p.cq_off.ring_mask) as *const u32) },
            cqes: b(cq_map, p.cq_off.cqes).cast(),
            sq_map,
            sq_map_len: map_len,
            cq_map,
            cq_map_len: cq_ring_sz,
            sqes_map,
            sqes_map_len: sqes_sz,
            single_mmap: single,
            sqpoll,
            pending_sq: 0,
        })
    }

    /// Reserve an SQE slot. The caller fills it, then calls [`Ring::publish`].
    ///
    /// Publishing `sq_tail` before the SQE is written would let an SQPOLL kernel
    /// thread read a zeroed SQE (opcode 0 = NOP, `user_data` 0) and post a bogus
    /// completion. Benign with a single issuing thread, fatal the moment SQPOLL is
    /// enabled, so the order is fixed here rather than left as a trap.
    #[inline]
    fn sqe(&mut self) -> Option<*mut io_uring_sqe> {
        let tail = unsafe { (*self.sq_tail).load(Ordering::Acquire) };
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        if tail.wrapping_sub(head) >= self.sq_entries {
            return None;
        }
        let idx = tail & self.sq_mask;
        unsafe {
            *self.sq_array.add(idx as usize) = idx;
            let s = self.sqes.add(idx as usize);
            ptr::write_bytes(s as *mut u8, 0, std::mem::size_of::<io_uring_sqe>());
            Some(s)
        }
    }

    #[inline]
    fn publish(&mut self) {
        let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };
        unsafe { (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release) };
    }

    /// Push everything queued to the kernel.
    fn submit(
        &mut self,
        min_complete: u32,
    ) -> io::Result<()> {
        if self.pending_sq == 0 {
            if min_complete == 0 {
                return Ok(());
            }
            let r = unsafe {
                syscall(
                    SYS_IO_URING_ENTER,
                    self.fd as libc::c_long,
                    0,
                    min_complete as libc::c_long,
                    IORING_ENTER_GETEVENTS as libc::c_long,
                    ptr::null::<libc::c_void>(),
                    0,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }

        let mut guard = 0;
        while self.pending_sq > 0 {
            let to_submit = self.pending_sq;
            let flags = if self.sqpoll {
                // The SQ thread polls the ring; we only need to wake it for events.
                IORING_ENTER_GETEVENTS | IORING_ENTER_SQ_WAKEUP
            } else {
                IORING_ENTER_GETEVENTS
            };
            let r = unsafe {
                syscall(
                    SYS_IO_URING_ENTER,
                    self.fd as libc::c_long,
                    to_submit as libc::c_long,
                    min_complete as libc::c_long,
                    flags as libc::c_long,
                    ptr::null::<libc::c_void>(),
                    0,
                )
            };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    // Bounded, or a signal storm would loop here forever.
                    guard += 1;
                    if guard > 1024 {
                        return Err(e);
                    }
                    continue;
                }
                return Err(e);
            }
            self.pending_sq = self.pending_sq.saturating_sub(r as u32);
            guard += 1;
            if guard > 1024 {
                break;
            }
        }
        Ok(())
    }

    #[inline]
    fn peek(&mut self) -> Option<io_uring_cqe> {
        let head = unsafe { (*self.cq_head).load(Ordering::Acquire) };
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        if head == tail {
            return None;
        }
        let cqe = unsafe { ptr::read(self.cqes.add((head & self.cq_mask) as usize)) };
        unsafe { (*self.cq_head).store(head.wrapping_add(1), Ordering::Release) };
        Some(cqe)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.sq_map, self.sq_map_len);
            if !self.single_mmap {
                libc::munmap(self.cq_map, self.cq_map_len);
            }
            libc::munmap(self.sqes_map, self.sqes_map_len);
            libc::close(self.fd);
        }
    }
}

/// `user_data` for the cancel request itself, so its completion can be skipped.
const CANCEL_SENTINEL: u64 = u64::MAX;

/// Completion-driven I/O over a single `io_uring` instance.
///
/// One of these belongs to one thread. The ring is not internally synchronised:
/// submissions go straight into the shared submission queue, and only the owning
/// thread touches it, which is what keeps the per-request cost at a handful of
/// stores instead of a lock.
pub struct UringReactor {
    ring: Ring,
    in_flight: usize,
    fixed: HashMap<usize, u16>,
}

impl UringReactor {
    /// A reactor with the given queue depth and default flags.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the ring cannot be created.
    pub fn new(depth: usize) -> io::Result<Self> {
        Self::with_flags(depth, false, false, None)
    }

    /// A reactor with explicit `IORING_SETUP_*` behaviour.
    ///
    /// `iopoll` spins on the completion queue instead of parking, which pays off
    /// only on a device that completes in microseconds; `sqpoll` moves submission
    /// to a kernel thread so the caller never enters the kernel to submit, and
    /// `sq_cpu` pins that thread.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the ring cannot be created with those flags.
    pub fn with_flags(
        depth: usize,
        iopoll: bool,
        sqpoll: bool,
        sq_cpu: Option<u32>,
    ) -> io::Result<Self> {
        let ring = Ring::new(depth as u32, iopoll, sqpoll, sq_cpu)?;
        Ok(Self {
            ring,
            in_flight: 0,
            fixed: HashMap::new(),
        })
    }

    #[inline]
    fn push(
        &mut self,
        opcode: u8,
        fd: i32,
        buf: usize,
        len: usize,
        off: u64,
        user: u64,
        fixed: Option<u16>,
    ) -> io::Result<()> {
        loop {
            if let Some(s) = self.ring.sqe() {
                unsafe {
                    (*s).opcode = if fixed.is_some() {
                        if opcode == IORING_OP_READ {
                            IORING_OP_READ_FIXED
                        } else {
                            IORING_OP_WRITE_FIXED
                        }
                    } else {
                        opcode
                    };
                    (*s).fd = fd;
                    (*s).addr = buf as u64;
                    (*s).len = len as u32;
                    (*s).off = off;
                    (*s).user_data = user;
                    if let Some(i) = fixed {
                        (*s).buf_index = i;
                    }
                }
                self.ring.publish();
                self.ring.pending_sq += 1;
                self.in_flight += 1;
                return Ok(());
            }
            // Ring full: flush what we have and try again.
            self.ring.submit(0)?;
            if self.ring.pending_sq > 0 {
                std::thread::yield_now();
            }
        }
    }
}

impl Reactor for UringReactor {
    fn open(
        &mut self,
        path: &Path,
        opts: &OpenOpts,
    ) -> io::Result<RawFile> {
        RawFile::open(path, opts)
    }

    unsafe fn read(
        &mut self,
        f: &RawFile,
        buf: *mut u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        let fixed = self.fixed.get(&(buf as usize)).copied();
        self.push(
            IORING_OP_READ,
            f.handle(),
            buf as usize,
            len,
            off,
            user,
            fixed,
        )
    }

    unsafe fn write(
        &mut self,
        f: &RawFile,
        buf: *const u8,
        len: usize,
        off: u64,
        user: u64,
    ) -> io::Result<()> {
        let fixed = self.fixed.get(&(buf as usize)).copied();
        self.push(
            IORING_OP_WRITE,
            f.handle(),
            buf as usize,
            len,
            off,
            user,
            fixed,
        )
    }

    fn poll(
        &mut self,
        want: usize,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let mut total = 0usize;
        // Reap whatever is already there.
        while total < want {
            match self.ring.peek() {
                | Some(c) => {
                    if c.user_data == CANCEL_SENTINEL {
                        // Our own cancel request; it does not belong to any chunk.
                        continue;
                    }
                    self.in_flight = self.in_flight.saturating_sub(1);
                    out.push(Completion {
                        user: c.user_data,
                        res: c.res as i64,
                    });
                    total += 1;
                },
                | None => break,
            }
        }
        if total >= want || self.in_flight == 0 {
            return Ok(total);
        }
        // Nothing pending in the CQ but operations are in flight: flush and wait.
        self.ring.submit((want - total) as u32)?;
        while total < want {
            match self.ring.peek() {
                | Some(c) => {
                    if c.user_data == CANCEL_SENTINEL {
                        // Our own cancel request; it does not belong to any chunk.
                        continue;
                    }
                    self.in_flight = self.in_flight.saturating_sub(1);
                    out.push(Completion {
                        user: c.user_data,
                        res: c.res as i64,
                    });
                    total += 1;
                },
                | None => break,
            }
        }
        Ok(total)
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn name(&self) -> &'static str {
        "io_uring"
    }

    fn cancel(&mut self) {
        // One SQE cancels every request on the ring. Each cancelled operation still
        // posts a completion (-ECANCELED), so `in_flight` drains through the normal
        // path and no buffer is handed back to the pool while the kernel is still
        // writing into it — which is what zeroing `in_flight` here used to do.
        if self.in_flight == 0 {
            return;
        }
        if let Some(s) = self.ring.sqe() {
            unsafe {
                (*s).opcode = IORING_OP_ASYNC_CANCEL;
                (*s).fd = -1;
                (*s).addr = 0;
                (*s).len = 0;
                (*s).rw_flags = IORING_ASYNC_CANCEL_ANY;
                (*s).user_data = CANCEL_SENTINEL;
            }
            self.ring.publish();
            self.ring.pending_sq += 1;
            let _ = self.ring.submit(0);
        }
    }

    fn register_buffers(
        &mut self,
        bufs: &[(usize, usize)],
    ) -> io::Result<()> {
        let mut iov: Vec<iovec> = bufs
            .iter()
            .map(|(p, l)| {
                iovec {
                    iov_base: *p as *mut libc::c_void,
                    iov_len: *l,
                }
            })
            .collect();
        let r = unsafe {
            syscall(
                SYS_IO_URING_REGISTER,
                self.ring.fd as libc::c_long,
                IORING_REGISTER_BUFFERS as libc::c_long,
                iov.as_mut_ptr(),
                iov.len() as libc::c_long,
            )
        };
        if r < 0 {
            // Usually ENOMEM from RLIMIT_MEMLOCK: the pool is bigger than the
            // process may pin. Every operation then silently falls back to plain
            // READ/WRITE and pays get_user_pages each time, so say so.
            return Err(io::Error::last_os_error());
        }
        for (i, (p, _)) in bufs.iter().enumerate() {
            self.fixed.insert(*p, i as u16);
        }
        Ok(())
    }
}
