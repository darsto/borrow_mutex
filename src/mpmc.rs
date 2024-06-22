// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

// Not all of the methods are used
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{align_of, size_of, MaybeUninit};
use core::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
pub struct MPMC<const S: usize, T> {
    prod_head: AtomicUsize,
    prod_tail: AtomicUsize,
    cons_head: AtomicUsize,
    cons_tail: AtomicUsize,
    size: usize,
    ring: [UnsafeCell<MaybeUninit<T>>; S],
}

pub struct MPMCRef<'a, T>(*const MPMC<0, T>, PhantomData<&'a T>);

impl<const S: usize, T> MPMC<S, T> {
    pub const fn new() -> Self {
        if !S.is_power_of_two() || S < 2 {
            panic!("size must be a power of 2 (and be greater or equal 2)");
        }

        Self {
            prod_head: AtomicUsize::new(0),
            prod_tail: AtomicUsize::new(0),
            cons_head: AtomicUsize::new(0),
            cons_tail: AtomicUsize::new(0),
            size: S,
            // uninitialized memory won't be accessed
            #[allow(clippy::uninit_assumed_init)]
            ring: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }

    #[inline]
    pub fn as_ptr(&self) -> MPMCRef<'_, T> {
        unsafe { MPMCRef::from_ptr(self) }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        S - 1
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.as_ptr().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_ptr().is_empty()
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.as_ptr().is_full()
    }

    #[inline]
    pub fn push(&self, val: T) -> Result<&UnsafeCell<T>, T> {
        self.as_ptr().push(val)
    }

    #[inline]
    pub fn pop(&self) -> Option<T> {
        self.as_ptr().pop()
    }

    #[inline]
    pub fn peek(&self) -> Option<&UnsafeCell<T>> {
        self.as_ptr().peek()
    }

    /// Equivalent of core::mem::offset_of!(.., ring) that works
    /// in rust version pre-1.77.
    const fn ring_offset() -> usize {
        let offset = size_of::<AtomicUsize>() * 4 + size_of::<usize>();

        let align = align_of::<[UnsafeCell<MaybeUninit<T>>; 2]>();
        (offset + align - 1) & !(align - 1)
    }
}

impl<'a, T> MPMCRef<'a, T> {
    pub unsafe fn from_ptr<const S: usize>(ptr: *const MPMC<S, T>) -> MPMCRef<'a, T> {
        MPMCRef(ptr as *const _ as *const MPMC<0, T>, PhantomData)
    }

    #[inline]
    fn ring(&self) -> &[UnsafeCell<MaybeUninit<T>>] {
        let ring_ptr = unsafe { self.0.cast::<u8>().add(MPMC::<0, T>::ring_offset()) }
            as *const UnsafeCell<MaybeUninit<T>>;
        unsafe { core::slice::from_raw_parts(ring_ptr, self.size) }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.size - 1
    }

    #[inline]
    pub fn len(&self) -> usize {
        let prod_tail = self.prod_tail.load(Ordering::Acquire);
        let cons_tail = self.cons_tail.load(Ordering::Acquire);
        (prod_tail - cons_tail) & self.capacity()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    pub fn push(&self, val: T) -> Result<&'a UnsafeCell<T>, T> {
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
        let slot = &self.ring()[head & (self.capacity())];
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
                    core::hint::spin_loop();
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
        let slot = &self.ring()[head & self.capacity()];
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
                    core::hint::spin_loop();
                }
            }
        }

        Some(unsafe { data.assume_init() })
    }

    #[inline]
    pub fn peek(&self) -> Option<&'a UnsafeCell<T>> {
        let prod_tail = self.prod_tail.load(Ordering::Acquire);
        let cons_tail = self.cons_tail.load(Ordering::Acquire);
        if prod_tail == cons_tail {
            return None;
        }

        let slot = &self.ring()[cons_tail & self.capacity()];
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

impl<'a, T> core::ops::Deref for MPMCRef<'a, T> {
    type Target = MPMC<0, T>;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0 }
    }
}

impl<'a, T> Clone for MPMCRef<'a, T> {
    fn clone(&self) -> Self {
        MPMCRef(self.0, PhantomData)
    }
}

#[cfg(test)]
mod tests {
    use super::MPMC;

    #[test]
    fn validate_ring_field_offset() {
        assert_eq!(
            MPMC::<0, usize>::ring_offset(),
            core::mem::offset_of!(MPMC<0, usize>, ring)
        );

        assert_eq!(
            MPMC::<0, &dyn core::any::Any>::ring_offset(),
            core::mem::offset_of!(MPMC<0, &dyn core::any::Any>, ring)
        );
    }
}
