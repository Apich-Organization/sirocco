//! Bounded MPMC queue (Vyukov's algorithm).
//!
//! The pipeline needs to hand chunks from N reader threads to M writer threads
//! with hard backpressure — an unbounded channel would let the readers outrun the
//! writers and blow up the buffer pool. This is a fixed-capacity lock-free ring:
//! `push` fails fast when full so the producer can block on the pool instead.

// --- `unsafe` is allowed here, and only here -------------------------------
// The Vyukov MPMC ring stores `UnsafeCell` slots and publishes them with
// explicit acquire/release ordering.
#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[repr(align(64))]
struct Padded<T>(T);

struct Slot<T> {
    seq: AtomicUsize,
    val: UnsafeCell<MaybeUninit<T>>,
}

/// A bounded multi-producer, multi-consumer queue (Vyukov's ring).
///
/// Push and pop never block and never allocate: a full queue hands the value
/// back, and an empty one returns `None`. The pipeline relies on that — a
/// blocking queue would tie a scheduler thread to a stalled writer.
pub struct BoundedQueue<T> {
    slots: Box<[Slot<T>]>,
    mask: usize,
    head: Padded<AtomicUsize>,
    tail: Padded<AtomicUsize>,
}

unsafe impl<T: Send> Send for BoundedQueue<T> {}
unsafe impl<T: Send> Sync for BoundedQueue<T> {}

impl<T> BoundedQueue<T> {
    /// `capacity` is rounded up to a power of two.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        let mut slots = Vec::with_capacity(cap);
        for i in 0..cap {
            slots.push(Slot {
                seq: AtomicUsize::new(i),
                val: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }
        Self {
            slots: slots.into_boxed_slice(),
            mask: cap - 1,
            head: Padded(AtomicUsize::new(0)),
            tail: Padded(AtomicUsize::new(0)),
        }
    }

    /// Number of slots. Always a power of two.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Approximate number of queued items. Only meaningful as a gauge: another
    /// thread may push or pop between the two loads.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        let t = self.tail.0.load(Ordering::Relaxed);
        let h = self.head.0.load(Ordering::Relaxed);
        t.saturating_sub(h)
    }

    /// True when the queue looks empty. Same caveat as [`Self::len`].
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Push, returning the value back if the queue is full.
    ///
    /// # Errors
    ///
    /// Returns the value itself when the queue is full, so the caller can retry
    /// without cloning it.
    pub fn push(
        &self,
        v: T,
    ) -> Result<(), T> {
        let mut pos = self.tail.0.load(Ordering::Relaxed);
        loop {
            let slot = unsafe { self.slots.get_unchecked(pos & self.mask) };
            let seq = slot.seq.load(Ordering::Acquire);
            let dif = seq.wrapping_sub(pos) as isize;
            match dif.cmp(&0) {
                | std::cmp::Ordering::Equal => {
                    match self.tail.0.compare_exchange_weak(
                        pos,
                        pos.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        | Ok(_) => {
                            unsafe { (*slot.val.get()).write(v) };
                            slot.seq.store(pos.wrapping_add(1), Ordering::Release);
                            return Ok(());
                        },
                        | Err(p) => pos = p,
                    }
                },
                // Full: the slot still belongs to the previous lap.
                | std::cmp::Ordering::Less => return Err(v),
                // Another producer took the slot; re-read and retry.
                | std::cmp::Ordering::Greater => pos = self.tail.0.load(Ordering::Relaxed),
            }
        }
    }

    /// Pop, returning `None` when empty.
    pub fn pop(&self) -> Option<T> {
        let mut pos = self.head.0.load(Ordering::Relaxed);
        loop {
            let slot = unsafe { self.slots.get_unchecked(pos & self.mask) };
            let seq = slot.seq.load(Ordering::Acquire);
            let dif = seq.wrapping_sub(pos.wrapping_add(1)) as isize;
            match dif.cmp(&0) {
                | std::cmp::Ordering::Equal => {
                    match self.head.0.compare_exchange_weak(
                        pos,
                        pos.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        | Ok(_) => {
                            let v = unsafe { (*slot.val.get()).assume_init_read() };
                            slot.seq
                                .store(pos.wrapping_add(self.mask + 1), Ordering::Release);
                            return Some(v);
                        },
                        | Err(p) => pos = p,
                    }
                },
                // Empty: the producer has not published this lap yet.
                | std::cmp::Ordering::Less => return None,
                // Another consumer took the slot; re-read and retry.
                | std::cmp::Ordering::Greater => pos = self.head.0.load(Ordering::Relaxed),
            }
        }
    }

    /// Spin briefly, then yield, until an item is available or `give_up` elapses.
    pub fn pop_wait(
        &self,
        give_up_after: u32,
    ) -> Option<T> {
        for i in 0..give_up_after {
            if let Some(v) = self.pop() {
                return Some(v);
            }
            if i < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        self.pop()
    }
}

impl<T> Drop for BoundedQueue<T> {
    fn drop(&mut self) {
        if std::mem::needs_drop::<T>() {
            while self.pop().is_some() {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn single_thread_order() {
        let q = BoundedQueue::new(4);
        assert!(q.is_empty());
        for i in 0..3 {
            q.push(i).unwrap();
        }
        assert_eq!(q.len(), 3);
        assert_eq!(q.pop(), Some(0));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn full_queue_returns_value() {
        let q = BoundedQueue::new(2);
        q.push(1).unwrap();
        q.push(2).unwrap();
        assert_eq!(q.push(3), Err(3));
        assert_eq!(q.pop(), Some(1));
        q.push(3).unwrap();
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
    }

    #[test]
    fn mpmc_stress() {
        let q = Arc::new(BoundedQueue::new(64));
        let n_prod = 4;
        let n_cons = 4;
        let per = 20_000;
        let total = n_prod * per;
        let mut handles = Vec::new();
        for p in 0..n_prod {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..per {
                    let v = p * per + i;
                    loop {
                        if q.push(v).is_ok() {
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            }));
        }
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        for _ in 0..n_cons {
            let q = q.clone();
            let seen = seen.clone();
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::new();
                while local.len() < total / n_cons {
                    if let Some(v) = q.pop_wait(1000) {
                        local.push(v);
                    }
                }
                seen.lock().unwrap().extend(local);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut all = seen.lock().unwrap().clone();
        all.sort_unstable();
        assert_eq!(all.len(), total);
        assert_eq!(all[0], 0);
        assert_eq!(*all.last().unwrap(), total - 1);
    }
}
