//! Raw `io_uring` and `FIEMAP` definitions.
//!
//! The `io_uring` ABI is stable and versioned through
//! `io_uring_params.features`, so binding it directly (rather than going through
//! liburing's C shim) is both faster and lets us pick up new flags without
//! waiting for a wrapper crate.

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]
#![allow(clippy::upper_case_acronyms)]
// A transcription of the system headers: the identifier *is* the
// documentation, and a doc comment per constant would only repeat it.
#![allow(missing_docs)]

// ---------------------------------------------------------------- syscalls
//
// Taken from `libc`, never hardcoded: the numbers differ per architecture
// (ioprio_set is 251 on x86_64 but 30 on aarch64, mbind is 237 vs 235, statx is
// 332 vs 291). Hardcoding them silently turns the feature off — or calls an
// entirely different syscall — on anything that is not x86_64.
pub const SYS_IO_URING_SETUP: libc::c_long = libc::SYS_io_uring_setup;
pub const SYS_IO_URING_ENTER: libc::c_long = libc::SYS_io_uring_enter;
pub const SYS_IO_URING_REGISTER: libc::c_long = libc::SYS_io_uring_register;
pub const SYS_IOPRIO_SET: libc::c_long = libc::SYS_ioprio_set;
pub const SYS_IOPRIO_GET: libc::c_long = libc::SYS_ioprio_get;

unsafe extern "C" {
    pub fn syscall(
        num: libc::c_long,
        ...
    ) -> libc::c_long;
}

// ---------------------------------------------------------------- ring offsets
pub const IORING_OFF_SQ_RING: libc::off_t = 0;
pub const IORING_OFF_CQ_RING: libc::off_t = 0x800_0000;
pub const IORING_OFF_SQES: libc::off_t = 0x1000_0000;

// ---------------------------------------------------------------- setup flags
pub const IORING_SETUP_IOPOLL: u32 = 1 << 0;
pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;
pub const IORING_SETUP_SQ_AFF: u32 = 1 << 2;
pub const IORING_SETUP_CQSIZE: u32 = 1 << 3;
pub const IORING_SETUP_CLAMP: u32 = 1 << 4;
pub const IORING_SETUP_ATTACH_WQ: u32 = 1 << 5;
pub const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;
pub const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
pub const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;

pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub const IORING_FEAT_NODROP: u32 = 1 << 1;
pub const IORING_FEAT_SUBMIT_STABLE: u32 = 1 << 2;
pub const IORING_FEAT_RW_CUR_POS: u32 = 1 << 3;
pub const IORING_FEAT_FAST_POLL: u32 = 1 << 5;

// ---------------------------------------------------------------- enter flags
pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
pub const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;

// ---------------------------------------------------------------- opcodes
pub const IORING_OP_READ_FIXED: u8 = 4;
pub const IORING_OP_WRITE_FIXED: u8 = 5;
pub const IORING_OP_READ: u8 = 22;
pub const IORING_OP_WRITE: u8 = 23;
pub const IORING_OP_FSYNC: u8 = 3;
pub const IORING_OP_ASYNC_CANCEL: u8 = 14;

/// `sqe.flags` for `IORING_OP_ASYNC_CANCEL`.
pub const IORING_ASYNC_CANCEL_ALL: u32 = 1 << 0;
pub const IORING_ASYNC_CANCEL_ANY: u32 = 1 << 2;

// ---------------------------------------------------------------- register opcodes
pub const IORING_REGISTER_BUFFERS: u32 = 0;
pub const IORING_UNREGISTER_BUFFERS: u32 = 1;
pub const IORING_REGISTER_FILES: u32 = 2;
pub const IORING_UNREGISTER_FILES: u32 = 3;
pub const IORING_REGISTER_FILES_UPDATE: u32 = 9;

// ---------------------------------------------------------------- structures
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct io_sqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct io_cqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct io_uring_params {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: io_sqring_offsets,
    pub cq_off: io_cqring_offsets,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct io_uring_sqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub rw_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub splice_fd_in: i32,
    pub addr2: u64,
    pub pad2: u64,
}

const _: () = assert!(std::mem::size_of::<io_uring_sqe>() == 64);

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct io_uring_cqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

const _: () = assert!(std::mem::size_of::<io_uring_cqe>() == 16);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct iovec {
    pub iov_base: *mut libc::c_void,
    pub iov_len: usize,
}

// ---------------------------------------------------------------- FIEMAP
pub const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct fiemap_extent {
    pub fe_logical: u64,
    pub fe_physical: u64,
    pub fe_length: u64,
    pub fe_reserved64: [u64; 2],
    pub fe_flags: u32,
    pub fe_reserved: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<fiemap_extent>() == 56);

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct fiemap {
    pub fm_start: u64,
    pub fm_length: u64,
    pub fm_flags: u32,
    pub fm_mapped_extents: u32,
    pub fm_extent_count: u32,
    pub fm_reserved: u32,
    pub fm_extents: [fiemap_extent; 0],
}

const _: () = assert!(std::mem::size_of::<fiemap>() == 32);

pub const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
pub const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
pub const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x0000_0100;
pub const FIEMAP_EXTENT_SHARED: u32 = 0x0000_0400;

pub const FIEMAP_FLAG_SYNC: u32 = 0x0000_0001;
pub const FIEMAP_FLAG_XATTR: u32 = 0x0000_0002;

// ---------------------------------------------------------------- ioprio
pub const IOPRIO_WHO_PROCESS: i32 = 1;
pub const IOPRIO_CLASS_NONE: u16 = 0;
pub const IOPRIO_CLASS_RT: u16 = 1;
pub const IOPRIO_CLASS_BE: u16 = 2;
pub const IOPRIO_CLASS_IDLE: u16 = 3;
pub const IOPRIO_PRIO_SHIFT: u16 = 13;

#[inline]
pub const fn ioprio_value(
    class: u16,
    data: u16,
) -> i32 {
    ((class as i32) << IOPRIO_PRIO_SHIFT) | (data as i32)
}

// ---------------------------------------------------------------- misc
pub const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
pub const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
pub const FALLOC_FL_ZERO_RANGE: i32 = 0x10;

pub const SEEK_DATA: i32 = 3;
pub const SEEK_HOLE: i32 = 4;
