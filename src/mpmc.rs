// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

// Not all of the methods are used
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct MPMC<const S: usize, T> {
    prod_head: AtomicUsize,
    prod_tail: AtomicUsize,
    cons_head: AtomicUsize,
    cons_tail: AtomicUsize,
    ring: [UnsafeCell<MaybeUninit<T>>; S],
}

impl<const S: usize, T> MPMC<S, T> {
    pub fn new() -> Self {
        if !S.is_power_of_two() || S < 2 {
            panic!("size must be a power of 2 (and be greater or equal 2)");
        }

        Self {
            prod_head: AtomicUsize::new(0),
            prod_tail: AtomicUsize::new(0),
            cons_head: AtomicUsize::new(0),
            cons_tail: AtomicUsize::new(0),
            // uninitialized memory won't be accessed
            #[allow(clippy::uninit_assumed_init)]
            ring: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }

    pub fn capacity(&self) -> usize {
        S - 1
    }

    pub fn len(&self) -> usize {
        let prod_tail = self.prod_tail.load(Ordering::Acquire);
        let cons_tail = self.cons_tail.load(Ordering::Acquire);
        //println!("len, prod_tail={prod_tail}, cons_tail={cons_tail}");
        // in case of intermediate update the calculated length won't ever be >= capacity

        (prod_tail - cons_tail) & self.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    pub fn push(&self, val: T) -> Result<&UnsafeCell<T>, T> {
        let mut head = self.prod_head.load(Ordering::Acquire);

        // reserve an index for the element
        loop {
            let tail = self.cons_tail.load(Ordering::Acquire);

            if self.capacity().wrapping_add(tail.wrapping_sub(head)) == 0 {
                return Err(val);
            }

            match self.prod_head.compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(new_head) => head = new_head,
            }
        }

        // memcpy
        let slot = &self.ring[head & (self.capacity())];
        unsafe { (*slot.get()).write(val) };

        // mark the enqueue completion
        loop {
            match self.prod_tail.compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => {
                    // other push() is still in progress and hasn't updated the tail yet.
                    // we can't update it ourselves now because the other push might not
                    // have written the entry yet, and also its future write to tail may
                    // effectively move the tail backwards.
                    // so just wait with a busy-loop
                    std::thread::yield_now();
                }
            }
        }

        // return a pointer to the enqueued element. the caller can dereference
        // this with unsafe if they know what they're doing
        Ok(unsafe { core::intrinsics::transmute(slot) })
    }

    pub fn pop(&self) -> Option<T> {
        let mut data: MaybeUninit<T> = MaybeUninit::uninit();
        let mut head = self.cons_head.load(Ordering::Acquire);

        // get an index to dequeue from
        loop {
            let tail = self.prod_tail.load(Ordering::Acquire);
            if head == tail {
                return None;
            }

            match self.cons_head.compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(new_head) => head = new_head,
            }
        }

        // memcpy
        let slot = &self.ring[head & self.capacity()];
        data.write(unsafe { (*slot.get()).assume_init_read() });

        // mark the dequeue completion
        loop {
            match self.cons_tail.compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => {
                    // other pop() is still in progress and hasn't updated the tail yet.
                    // we can't update it ourselves now because the other pop might not
                    // have read its entry yet, and also its future write to tail may
                    // effectively move the tail backwards.
                    // so just wait with a busy-loop
                    std::thread::yield_now();
                }
            }
        }

        Some(unsafe { data.assume_init() })
    }

    pub fn peek(&self) -> Option<&UnsafeCell<T>> {
        let prod_tail = self.prod_tail.load(Ordering::Acquire);
        let cons_tail = self.cons_tail.load(Ordering::Acquire);
        //println!("peek, prod_tail={prod_tail}, cons_tail={cons_tail}");
        if prod_tail == cons_tail {
            return None;
        }

        let slot = &self.ring[cons_tail & self.capacity()];
        Some(unsafe { core::intrinsics::transmute(slot) })
    }
}

impl<const S: usize, T> Default for MPMC<S, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const S: usize, T> core::fmt::Debug for MPMC<S, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "MPMC {{ capacity: {capacity}, len: {len} }}",
            capacity = self.capacity(),
            len = self.len()
        ))
    }
}

unsafe impl<const S: usize, T> Sync for MPMC<S, T> {}
unsafe impl<const S: usize, T> Send for MPMC<S, T> {}
