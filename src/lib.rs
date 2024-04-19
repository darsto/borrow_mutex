// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::future::Future;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::task::{Context, Poll};

use futures::Stream;

pub mod atomic_waiter;
use atomic_waiter::AtomicWaiter;

pub mod mpmc;
use mpmc::MPMC;

pub const BORROW_MUTEX_MAX_BORROWERS: usize = 16;

// Async Mutex for `&mut T`. It does not require wrapping the target structure
// with the Mutex, only its mut reference.
//
// This lets others obtain this mutable reference. The data is borrow-able only
// while we await, and the borrowing itself is a future which doesn't resolve
// until we await. The semantics enforce that only one side has a mutable
// reference at any given time.
//
// This lets us share any mutable object between distinct async contexts
// without Arc<Mutex> over the object in question and without relying on any
// kind of internal mutability. It's mostly aimed at single-threaded executors
// where internal mutability is an unnecessary complication, but the Mutex is
// Send+Sync and can be safely used from any number of threads.
//
// The API is fully safe, but it's not able to enforce all the semantics at
// compile time. I.e. if a lending side of a transaction drops the lending
// Future before it's resolved (before the borrowing side stops using it),
// the process will immediately abort.
pub struct BorrowMutex<'m, T: 'm> {
    inner_ref: AtomicPtr<T>,
    lend_waiter: AtomicWaiter,
    borrowers: MPMC<BORROW_MUTEX_MAX_BORROWERS, BorrowMutexRef<'m, T>>,
}

unsafe impl<'m, T> Sync for BorrowMutex<'m, T> {}
unsafe impl<'m, T> Send for BorrowMutex<'m, T> {}

impl<'m, T> std::fmt::Debug for BorrowMutex<'m, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "BorrowMutex {{ available_for_borrow: {} }}",
            !self.inner_ref.load(Ordering::Relaxed).is_null()
        ))
    }
}
pub struct BorrowMutexRef<'m, T: 'm> {
    mutex: &'m BorrowMutex<'m, T>,
    inner_ref: AtomicPtr<T>,
    borrow_waiter: AtomicWaiter,
    guard_present: AtomicBool,
    safe_to_drop: AtomicBool,
}

pub struct BorrowMutexGuard<'m, T: 'm> {
    inner: &'m BorrowMutexRef<'m, T>,
}

// await until the reference is lended
impl<'m, T> Future for BorrowMutexGuard<'m, T> {
    type Output = &'m mut T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.inner.borrow_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        let inner = self.inner.inner_ref.load(Ordering::Acquire);
        if inner.is_null() {
            return Poll::Pending;
        }

        Poll::Ready(unsafe { &mut *inner })
    }
}

impl<'m, T> BorrowMutexGuard<'m, T> {
    pub fn get(&self) -> Option<&mut T> {
        let inner = self.inner.inner_ref.load(Ordering::Acquire);
        if inner.is_null() {
            return None;
        }

        Some(unsafe { &mut *inner })
    }
}

impl<'m, T: 'm> Drop for BorrowMutexGuard<'m, T> {
    fn drop(&mut self) {
        self.inner.guard_present.store(false, Ordering::Release);
        self.inner.mutex.lend_waiter.wake();
        self.inner.safe_to_drop.store(true, Ordering::Release)
    }
}

impl<'m, T> BorrowMutex<'m, T> {
    pub fn new() -> Self {
        Self {
            inner_ref: AtomicPtr::new(null_mut()),
            lend_waiter: AtomicWaiter::new(),
            borrowers: MPMC::new(),
        }
    }

    pub fn borrow_mut(&'m self) -> Option<BorrowMutexGuard<'m, T>> {
        let Ok(inner) = self.borrowers.push(BorrowMutexRef {
            mutex: self,
            inner_ref: AtomicPtr::new(null_mut()),
            borrow_waiter: AtomicWaiter::new(),
            guard_present: AtomicBool::new(true),
            safe_to_drop: AtomicBool::new(false),
        }) else {
            // too many borrows
            return None;
        };
        // BorrowMutexGuard will turn ready when any LendGuard sees us, so
        // awake it now if there is one
        self.lend_waiter.wake();

        Some(BorrowMutexGuard {
            inner: unsafe { &*inner.get() },
        })
    }

    pub fn wait_for_borrow(&'m self) -> BorrowMutexLender<'m, T> {
        BorrowMutexLender { mutex: self }
    }
}

impl<'m, T> Default for BorrowMutex<'m, T> {
    fn default() -> Self {
        Self::new()
    }
}

// await until there is someone wanting to borrow
impl<'m, T> Future for BorrowMutex<'m, T> {
    type Output = BorrowMutexLender<'m, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.lend_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        if self.borrowers.is_empty() {
            // spurious awake
            return Poll::Pending;
        }

        Poll::Ready(BorrowMutexLender {
            mutex: unsafe { std::mem::transmute(Pin::into_inner(self.into_ref())) },
        })
    }
}

impl<'m, T> Stream for BorrowMutex<'m, T> {
    type Item = BorrowMutexLender<'m, T>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll(cx).map(Some)
    }
}

pub struct BorrowMutexLender<'m, T: 'm> {
    mutex: &'m BorrowMutex<'m, T>,
}

impl<'m, T> BorrowMutexLender<'m, T> {
    /// Lend a mutable reference to the first borrower (FIFO order).
    /// This can be called even if there are no borrowers at the time (and will
    /// immediately return None), but since it holds a mutable reference and
    /// prevents its further use, it's recommended to first await the lender
    /// ([`BorrowMutexLender`]).
    pub fn lend(&mut self, value: &'m mut T) -> Option<BorrowMutexLendGuard<'m, T>> {
        let prev = self.mutex.inner_ref.swap(value, Ordering::AcqRel);
        if !prev.is_null() {
            eprintln!("multiple distinct references lended to a BorrowMutex");
            // the previous inner_ref was replaced and only the newly lend-ed
            // value can be borrowed from now on (on another thread), this won't
            // cause any undefined behavior yet, but we can't reasonably proceed.
            // Panicking here would be same as dropping the value while
            // borrowed (see the destructor of [`BorrowMutexLendGuard`]), and we
            // can't let that happen, so abort the entire process now.
            std::process::abort();
        }

        let Some(borrow) = self.mutex.borrowers.peek() else {
            return None;
        };
        let borrow = unsafe { &*borrow.get() };
        Some(BorrowMutexLendGuard {
            mutex: self.mutex,
            borrow,
        })
    }
}

// await until someone's BorrowMutexGuard turns ready
impl<'m, T> Future for BorrowMutexLender<'m, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.mutex.lend_waiter.poll_const(cx)
    }
}

pub struct BorrowMutexLendGuard<'m, T: 'm> {
    mutex: &'m BorrowMutex<'m, T>,
    borrow: &'m BorrowMutexRef<'m, T>,
}

// await until the (first available) borrower acquires and then drops the BorrowMutexGuard
impl<'m, T> Future for BorrowMutexLendGuard<'m, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner_ref = self.mutex.inner_ref.swap(null_mut(), Ordering::AcqRel);
        if !inner_ref.is_null() {
            // first time polling this LendGuard, so wake the Borrower
            self.borrow.inner_ref.store(inner_ref, Ordering::Release);
            self.borrow.borrow_waiter.wake();

            // the BorrowGuard could have been already dropped and won't wake us
            // again, so check now
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        if self.mutex.lend_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        if self.borrow.guard_present.load(Ordering::Acquire) {
            // spurious awake
            return Poll::Pending;
        }

        Poll::Ready(())
    }
}

impl<'a, T: 'a> Drop for BorrowMutexLendGuard<'a, T> {
    fn drop(&mut self) {
        if !self.borrow.safe_to_drop.load(Ordering::Acquire) {
            eprintln!("LendGuard dropped while the value is still borrowed");
            // the mutable reference is about to be re-gained in the lending context while
            // it's still used in the borrowed context. We can't let that happen, and we can't
            // even panic here as this may invalidate the referenced object. If the borrow is
            // happening independently of this panic (i.e. on another thread) it would be UB.
            // So abort the entire process here.
            std::process::abort();
        }
        let inner_ref = self.borrow.inner_ref.swap(null_mut(), Ordering::Release);
        assert!(!inner_ref.is_null());
        self.mutex.inner_ref.store(inner_ref, Ordering::Release);

        let _ = self.mutex.borrowers.pop().unwrap();
        // self.borrow should be no longer accessed (it's still valid memory, noone
        // else has any other reference to the same region, but it doesn't contain
        // a valid object anymore)
    }
}
