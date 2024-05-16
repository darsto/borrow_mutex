// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use core::sync::atomic::{AtomicBool, Ordering};
use std::cell::UnsafeCell;

pub struct AtomicFatPtr<T: ?Sized> {
    inner: UnsafeCell<Option<*mut T>>,
    locked: AtomicBool,
}

unsafe impl<T: ?Sized> Sync for AtomicFatPtr<T> {}
unsafe impl<T: ?Sized> Send for AtomicFatPtr<T> {}

impl<T: ?Sized> core::fmt::Debug for AtomicFatPtr<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "AtomicFatPtr {{ ptr: {:#x?}, locked: {} }}",
            self.inner,
            self.locked.load(Ordering::Relaxed)
        ))
    }
}

impl<T: ?Sized> AtomicFatPtr<T> {
    pub fn new(value: Option<*mut T>) -> Self {
        Self {
            inner: UnsafeCell::new(value),
            locked: AtomicBool::new(false),
        }
    }

    pub fn load(&self, _ordering: Ordering) -> Option<*mut T> {
        self.lock();
        let ret = unsafe { *self.inner.get() };
        self.locked.store(false, Ordering::SeqCst);
        ret
    }

    pub fn store(&self, value: Option<*mut T>, _ordering: Ordering) {
        self.lock();
        unsafe { *self.inner.get() = value };
        self.locked.store(false, Ordering::SeqCst);
    }

    pub fn swap(&self, value: Option<*mut T>, _ordering: Ordering) -> Option<*mut T> {
        self.lock();
        let prev = unsafe { *self.inner.get() };
        unsafe { *self.inner.get() = value };
        self.locked.store(false, Ordering::SeqCst);
        prev
    }

    fn lock(&self) {
        loop {
            match self.locked.compare_exchange_weak(
                false,
                true,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => {
                    std::thread::yield_now();
                }
            }
        }
    }
}
