//! A fixed-capacity slab with a free list.
//!
//! Used for two things where allocation on the I/O hot path is unacceptable:
//! `OVERLAPPED` structures (which the kernel needs at a *stable* address, and
//! whose address must map back to an index on completion) and per-op state
//! records. Capacity is fixed at construction so the backing array never moves.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

/// A slot table for in-flight operations.
///
/// The reactor hands out a slot index per submitted operation and puts it back
/// when the completion arrives. Indices rather than pointers, so a slot can be
/// validated instead of trusted.
pub struct Slab<T> {
    data: Box<[Slot<T>]>,
    free: Vec<u32>,
    live: usize,
    cap: u32,
}

struct Slot<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> Slab<T> {
    /// Create a slab with room for `cap` live values.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        let cap = cap.max(1);
        let mut data = Vec::with_capacity(cap);
        for _ in 0..cap {
            data.push(Slot(UnsafeCell::new(MaybeUninit::uninit())));
        }
        let free = (0..cap as u32).rev().collect();
        Self {
            data: data.into_boxed_slice(),
            free,
            live: 0,
            cap: cap as u32,
        }
    }

    /// Number of slots, live or free.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap as usize
    }

    /// Slots currently holding a value.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.live
    }

    /// True when no slot is in use.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Put `v` in a free slot. `None` means the slab is full, which the caller
    /// treats as "the backend queue is exhausted".
    #[inline]
    pub fn alloc(
        &mut self,
        v: T,
    ) -> Option<u32> {
        let idx = self.free.pop()?;
        unsafe {
            (*self.data[idx as usize].0.get()).write(v);
        }
        self.live += 1;
        Some(idx)
    }

    /// # Safety
    /// `idx` must come from [`Slab::alloc`] and not have been freed.
    #[inline]
    #[must_use]
    pub unsafe fn get(
        &self,
        idx: u32,
    ) -> &T {
        debug_assert!((idx as usize) < self.data.len());
        unsafe { (*self.data[idx as usize].0.get()).assume_init_ref() }
    }

    /// # Safety
    /// `idx` must come from [`Slab::alloc`] and not have been freed.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    #[must_use]
    pub unsafe fn get_mut(
        &self,
        idx: u32,
    ) -> &mut T {
        debug_assert!((idx as usize) < self.data.len());
        unsafe { (*self.data[idx as usize].0.get()).assume_init_mut() }
    }

    /// # Safety
    /// `idx` must come from [`Slab::alloc`] and not have been freed. The value is
    /// dropped and the slot returned to the free list.
    #[inline]
    pub unsafe fn free(
        &mut self,
        idx: u32,
    ) {
        debug_assert!((idx as usize) < self.data.len());
        unsafe {
            (*self.data[idx as usize].0.get()).assume_init_drop();
        }
        self.free.push(idx);
        self.live -= 1;
    }

    /// Move the value out of the slot and return the slot to the free list.
    ///
    /// # Safety
    /// `idx` must come from [`Slab::alloc`] and not have been freed.
    #[inline]
    pub unsafe fn take(
        &mut self,
        idx: u32,
    ) -> T {
        debug_assert!((idx as usize) < self.data.len());
        let v = unsafe { (*self.data[idx as usize].0.get()).assume_init_read() };
        self.free.push(idx);
        self.live -= 1;
        v
    }

    /// Recover the slot index from a pointer previously obtained via
    /// [`Slab::get_mut`]. Relies on the backing array being contiguous, which is
    /// why the storage is a `Box<[Slot<T>]>` and never grows.
    #[inline]
    pub fn index_of(
        &self,
        p: *const T,
    ) -> Option<u32> {
        let base = self.data.as_ptr() as usize;
        let end = base + std::mem::size_of::<Slot<T>>() * self.data.len();
        let a = p as usize;
        if a < base || a >= end {
            return None;
        }
        let off = a - base;
        if !off.is_multiple_of(std::mem::size_of::<Slot<T>>()) {
            return None;
        }
        Some((off / std::mem::size_of::<Slot<T>>()) as u32)
    }
}

unsafe impl<T: Send> Send for Slab<T> {}

impl<T> Drop for Slab<T> {
    fn drop(&mut self) {
        if std::mem::needs_drop::<T>() {
            for i in 0..self.cap {
                if !self.free.contains(&i) {
                    unsafe {
                        (*self.data[i as usize].0.get()).assume_init_drop();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_roundtrip() {
        let mut s: Slab<u64> = Slab::with_capacity(4);
        let a = s.alloc(11).unwrap();
        let b = s.alloc(22).unwrap();
        assert_eq!(s.len(), 2);
        unsafe {
            assert_eq!(*s.get(a), 11);
            assert_eq!(*s.get(b), 22);
        }
        assert_eq!(s.index_of(std::ptr::from_ref(unsafe { s.get(a) })), Some(a));
        unsafe { s.free(a) };
        assert_eq!(s.len(), 1);
        let c = s.alloc(33).unwrap();
        assert_eq!(c, a);
        unsafe { assert_eq!(*s.get(c), 33) };
    }
}
