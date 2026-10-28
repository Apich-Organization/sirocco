//! Aligned / huge-page backed memory.
//!
//! Direct I/O demands buffer, offset and length alignment to the device sector
//! size (logical for reads, physical for writes on most devices). Rather than
//! hoping `Vec<u8>` happens to be aligned we allocate exactly what we want, and
//! when the process holds `SeLockMemoryPrivilege` / `CAP_IPC_LOCK` we back the
//! buffer with large pages so the TLB stops being a factor at multi-GB/s.

use std::io;
use std::ptr::NonNull;

/// A page-aligned allocation. Freed on drop.
pub struct AlignedBlock {
    ptr: NonNull<u8>,
    size: usize,
    huge: bool,
}

// The block owns a raw mapping with no aliasing guarantees, which is exactly what
// the I/O engine needs; sharing it across threads is sound because every access
// is either exclusive (one in-flight op per buffer) or read-only.
unsafe impl Send for AlignedBlock {}
unsafe impl Sync for AlignedBlock {}

impl AlignedBlock {
    /// Allocate `size` bytes (rounded up to a page / large page).
    ///
    /// `huge` requests 2 MiB pages; it silently degrades to normal pages when the
    /// privilege or the OS support is missing.
    ///
    /// # Errors
    ///
    /// Propagates the allocator failure from `crate::plat::alloc_pages`.
    pub fn new(
        size: usize,
        huge: bool,
    ) -> io::Result<Self> {
        Self::new_on_node(size, huge, None)
    }

    /// Allocate on a specific NUMA node. Falls back to a plain allocation when
    /// the node-local request fails.
    ///
    /// # Errors
    ///
    /// Propagates the allocator failure when even the node-less fallback fails.
    pub fn new_on_node(
        size: usize,
        huge: bool,
        node: Option<u16>,
    ) -> io::Result<Self> {
        let size = size.max(4096);
        let (ptr, huge) = match crate::plat::alloc_pages(size, huge, node) {
            | Ok(v) => v,
            | Err(_) if node.is_some() => crate::plat::alloc_pages(size, huge, None)?,
            | Err(e) => return Err(e),
        };
        Ok(Self { ptr, size, huge })
    }

    /// Base address of the block.
    #[inline]
    #[must_use]
    pub const fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The whole block as an immutable slice.
    #[inline]
    #[must_use]
    pub const fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    /// The whole block as a mutable slice.
    #[inline]
    pub const fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.size) }
    }

    /// Size of the block in bytes, after rounding.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.size
    }

    /// Always false in practice; present because `len` exists.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// True when the block is actually backed by large pages.
    #[inline]
    #[must_use]
    pub const fn is_huge(&self) -> bool {
        self.huge
    }

    /// Force the pages to be resident (pre-fault) so the first I/O does not pay
    /// for page faults inside the completion path.
    pub fn prefault(&mut self) {
        // Write one byte per page: cheap (one store per 4 KiB) and guarantees the
        // PTEs exist. For large pages the whole page is faulted by the first store.
        let step = if self.huge { 2 << 20 } else { 4096 };
        let mut i = 0;
        while i < self.size {
            unsafe { self.ptr.as_ptr().add(i).write_volatile(0) };
            i += step;
        }
    }

    /// Pin the pages so they cannot be paged out. Best effort: the OS limits how
    /// much a process may lock, and a partial lock is still worth having.
    #[must_use]
    pub fn lock_pages(&self) -> bool {
        crate::plat::lock_pages(self.ptr.as_ptr(), self.size)
    }

    /// Hint the kernel that we are done with the contents (drops them from cache
    /// on Linux when possible).
    pub fn discard(&mut self) {
        crate::plat::discard_pages(self.ptr.as_ptr(), self.size);
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        crate::plat::free_pages(self.ptr.as_ptr(), self.size);
    }
}

impl std::fmt::Debug for AlignedBlock {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("AlignedBlock")
            .field("ptr", &format_args!("{:p}", self.ptr.as_ptr()))
            .field("size", &self.size)
            .field("huge", &self.huge)
            .finish()
    }
}

/// Round `n` up to the next multiple of `a` (a must be a power of two).
#[inline]
#[must_use]
pub const fn round_up(
    n: usize,
    a: usize,
) -> usize {
    (n + a - 1) & !(a - 1)
}
