//! Fixed-size buffer pool backed by one large aligned (optionally huge-page)
//! mapping.
//!
//! Every buffer is the same size and shares the same base alignment, so a buffer
//! handed to the I/O engine always satisfies direct-I/O alignment without any
//! per-operation fixups.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::align::AlignedBlock;

/// A fixed number of equal-sized, equally-aligned buffers carved out of one
/// mapping. Handing one out never allocates.
pub struct BufferPool {
    block: AlignedBlock,
    buf_size: usize,
    count: u32,
    free: Mutex<Vec<u32>>,
    out: AtomicUsize,
    base: usize,
}

impl BufferPool {
    /// `buf_size` is rounded up to `align`; `count` buffers are preallocated.
    ///
    /// # Errors
    ///
    /// Propagates the allocation failure from [`AlignedBlock`].
    pub fn new(
        buf_size: usize,
        count: usize,
        align: usize,
        huge: bool,
    ) -> std::io::Result<Arc<Self>> {
        Self::new_on_node(buf_size, count, align, huge, None)
    }

    /// Same, but allocate the backing mapping on a specific NUMA node so the I/O
    /// buffers live next to the device and the threads that touch them.
    ///
    /// # Errors
    ///
    /// Propagates the allocation failure from [`AlignedBlock`].
    pub fn new_on_node(
        buf_size: usize,
        count: usize,
        align: usize,
        huge: bool,
        node: Option<u16>,
    ) -> std::io::Result<Arc<Self>> {
        let buf_size = crate::align::round_up(buf_size.max(4096), align.max(4096));
        let count = count.max(1);
        let total = buf_size * count;
        let mut block = AlignedBlock::new_on_node(total, huge, node)?;
        block.prefault();
        // Pin the pool when we are allowed to: a copy of a few hundred GiB will
        // otherwise find its own buffers evicted, and every first touch of a
        // returned buffer is a soft fault inside the submission path.
        let _ = block.lock_pages();
        let base = block.as_ptr() as usize;
        let free = (0..count as u32).rev().collect();
        Ok(Arc::new(Self {
            block,
            buf_size,
            count: count as u32,
            free: Mutex::new(free),
            out: AtomicUsize::new(0),
            base,
        }))
    }

    /// Size of one buffer, after rounding.
    #[inline]
    pub const fn buf_size(&self) -> usize {
        self.buf_size
    }

    /// Number of buffers in the pool.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.count as usize
    }

    /// Buffers currently handed out and not yet returned.
    #[inline]
    pub fn available(&self) -> usize {
        self.count as usize - self.out.load(Ordering::Relaxed)
    }

    /// True when the pool is backed by large pages.
    #[inline]
    pub const fn is_huge(&self) -> bool {
        self.block.is_huge()
    }

    /// Non-blocking acquire.
    #[inline]
    pub fn try_take(self: &Arc<Self>) -> Option<Buf> {
        let idx = self.free.lock().unwrap().pop()?;
        self.out.fetch_add(1, Ordering::Relaxed);
        Some(Buf {
            pool: self.clone(),
            idx,
        })
    }

    /// Acquire, spinning briefly before yielding. Returns `None` only if the pool
    /// is empty and `give_up_after` spins elapsed.
    pub fn take(
        self: &Arc<Self>,
        give_up_after: u32,
    ) -> Option<Buf> {
        for _ in 0..give_up_after {
            if let Some(b) = self.try_take() {
                return Some(b);
            }
            std::hint::spin_loop();
        }
        for _ in 0..64 {
            if let Some(b) = self.try_take() {
                return Some(b);
            }
            std::thread::yield_now();
        }
        None
    }

    /// Base address of buffer `i` (used to register fixed buffers with `io_uring`).
    #[inline]
    pub fn buf_ptr(
        &self,
        i: usize,
    ) -> *mut u8 {
        debug_assert!(i < self.count as usize);
        unsafe { (self.base as *mut u8).add(i * self.buf_size) }
    }

    #[inline]
    fn put(
        &self,
        idx: u32,
    ) {
        self.out.fetch_sub(1, Ordering::Relaxed);
        self.free.lock().unwrap().push(idx);
    }
}

/// RAII handle to one pooled buffer. Returns itself to the pool on drop.
pub struct Buf {
    pool: Arc<BufferPool>,
    idx: u32,
}

impl Buf {
    /// Start of this buffer's storage.
    #[inline]
    #[must_use]
    pub fn ptr(&self) -> *mut u8 {
        unsafe { (self.pool.base as *mut u8).add(self.idx as usize * self.pool.buf_size) }
    }

    /// Usable length, which is always the pool's buffer size.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.pool.buf_size
    }

    /// Always false: pooled buffers are never zero length.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Slot number inside the pool, which is also what a fixed-buffer
    /// registration uses as its index.
    #[inline]
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.idx
    }

    /// The whole buffer as an immutable slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr(), self.len()) }
    }

    /// The whole buffer as a mutable slice.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.len()) }
    }

    /// Zero a byte range inside the buffer (used to pad the file tail).
    #[inline]
    pub fn zero_range(
        &mut self,
        from: usize,
        to: usize,
    ) {
        let to = to.min(self.len());
        if from < to {
            unsafe { std::ptr::write_bytes(self.ptr().add(from), 0, to - from) };
        }
    }

    /// Borrow a sub-slice without bounds-check overhead at the call site.
    #[inline]
    #[must_use]
    pub fn slice(
        &self,
        from: usize,
        to: usize,
    ) -> &[u8] {
        debug_assert!(to <= self.len() && from <= to);
        unsafe { std::slice::from_raw_parts(self.ptr().add(from), to - from) }
    }
}

impl Drop for Buf {
    #[inline]
    fn drop(&mut self) {
        self.pool.put(self.idx);
    }
}

unsafe impl Send for Buf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_roundtrip() {
        let p = BufferPool::new(4096, 4, 4096, false).unwrap();
        assert_eq!(p.available(), 4);
        let a = p.try_take().unwrap();
        assert_eq!(a.ptr() as usize % 4096, 0);
        let b = p.try_take().unwrap();
        assert_ne!(a.ptr(), b.ptr());
        drop(a);
        assert_eq!(p.available(), 3);
        let c = p.try_take().unwrap();
        assert_eq!(p.available(), 2);
        drop(b);
        drop(c);
        assert_eq!(p.available(), 4);
    }
}
