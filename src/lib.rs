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

/// Async Mutex for `&mut T`. It does not require wrapping the target structure
/// with the Mutex, only its mut reference.
///
/// This lets others obtain this mutable reference. The data is borrow-able only
/// while we await, and the borrowing itself is a future which doesn't resolve
/// until we await. The semantics enforce that only one side has a mutable
/// reference at any given time.
///
/// This lets us share any mutable object between distinct async contexts
/// without Arc<Mutex> over the object in question and without relying on any
/// kind of internal mutability. It's mostly aimed at single-threaded executors
/// where internal mutability is an unnecessary complication, but the Mutex is
/// Send+Sync and can be safely used from any number of threads.
///
/// The API is fully safe, but it's not able to enforce all the semantics at
/// compile time. I.e. if a lending side of a transaction drops the lending
/// Future before it's resolved (before the borrowing side stops using it),
/// the process will immediately abort.
pub struct BorrowMutex<const MAX_BORROWERS: usize, T> {
    inner_ref: AtomicPtr<T>,
    lend_waiter: AtomicWaiter,
    terminated: AtomicBool,
    borrowers: MPMC<MAX_BORROWERS, BorrowMutexRef>,
}

unsafe impl<const M: usize, T> Sync for BorrowMutex<M, T> {}
unsafe impl<const M: usize, T> Send for BorrowMutex<M, T> {}

impl<const M: usize, T> std::fmt::Debug for BorrowMutex<M, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "BorrowMutex {{ available_for_borrow: {} }}",
            !self.inner_ref.load(Ordering::Relaxed).is_null()
        ))
    }
}

// TODO: This is currently 40bytes on x86_64, but could be 24bytes if we
// organized the fields
struct BorrowMutexRef {
    borrow_waiter: AtomicWaiter,
    ref_acquired: AtomicBool,
    guard_present: AtomicBool,
}

pub struct BorrowMutexGuard<'g, const M: usize, T: 'g> {
    mutex: *const BorrowMutex<M, T>,
    inner: &'g BorrowMutexRef,
}

// await until the reference is lended
impl<'g, const M: usize, T: 'g,> Future for BorrowMutexGuard<'g, M, T> {
    type Output = &'g mut T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.inner.borrow_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        // The borrow_waiter turns ready only when wake() is called,
        // there are no spurious wakeups.
        let inner_ref = unsafe { &*self.mutex }.inner_ref.load(Ordering::Acquire);
        Poll::Ready(unsafe { &mut *inner_ref })
    }
}

impl<'m, const M: usize, T: 'm> Drop for BorrowMutexGuard<'m, M, T> {
    fn drop(&mut self) {
        let mutex = unsafe { &*self.mutex};
        self.inner.guard_present.store(false, Ordering::Release);
        // self.inner must be no longer accessed

        mutex.lend_waiter.wake();
    }

}

unsafe impl<'m, const M: usize, T: 'm> Sync for BorrowMutexGuard<'m, M, T> {}
unsafe impl<'m, const M: usize, T: 'm> Send for BorrowMutexGuard<'m, M, T> {}

impl<const M: usize, T> BorrowMutex<M, T> {
    pub fn new() -> Self {
        Self {
            inner_ref: AtomicPtr::new(null_mut()),
            lend_waiter: AtomicWaiter::new(),
            terminated: AtomicBool::new(false),
            borrowers: MPMC::new(),
        }
    }

    pub fn borrow_mut<'g, 'm: 'g>(&'m self) -> Option<BorrowMutexGuard<'g, M, T>> {
        if self.terminated.load(Ordering::Acquire) {
            // TODO make this an error enum
            return None;
        }

        let Ok(inner) = self.borrowers.push(BorrowMutexRef {
            borrow_waiter: AtomicWaiter::new(),
            ref_acquired: AtomicBool::new(false),
            guard_present: AtomicBool::new(true),
        }) else {
            // too many borrows
            return None;
        };
        // BorrowMutexGuard will turn ready when any LendGuard sees us, so
        // awake any sleeping one if it exists
        self.lend_waiter.wake();

        Some(BorrowMutexGuard {
            mutex: self,
            inner: unsafe { &*inner.get() },
        })
    }

    pub fn wait_for_borrow<'l, 'm: 'l>(&'m self) -> BorrowMutexLender<'l, M, T> {
        BorrowMutexLender { mutex: self }
    }

    /// Lend a mutable reference to the first borrower (FIFO order).
    /// This can be called even if there are no borrowers at the time (and will
    /// immediately return None), but since it holds a mutable reference and
    /// prevents its further use, it's recommended to first await the lender
    /// ([`BorrowMutexLender`], obtained with [`Self::wait_for_borrow`]).
    pub fn lend<'l, 'm: 'l>(&'m self, value: &'l mut T) -> Option<BorrowMutexLendGuard<'l, M, T>> {
        let prev = self.inner_ref.swap(value, Ordering::AcqRel);
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

        let Some(borrow) = self.borrowers.peek() else {
            return None;
        };
        let borrow = unsafe { &*borrow.get() };
        Some(BorrowMutexLendGuard {
            mutex: self,
            borrow,
        })
    }

    pub fn terminate(&self) {
        self.terminated.store(true, Ordering::Release);
    }
}

impl<const M: usize, T> Default for BorrowMutex<M, T> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BorrowMutexLender<'l, const M: usize, T> {
    mutex: &'l BorrowMutex<M, T>,
}

// await until there is someone wanting to borrow
impl<'m, const M: usize, T> Future for BorrowMutexLender<'m, M, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mutex.lend_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        if self.mutex.borrowers.is_empty() {
            // spurious awake
            return Poll::Pending;
        }

        Poll::Ready(())
    }
}

impl<'m, const M: usize, T> Stream for BorrowMutexLender<'m, M, T> {
    type Item = ();
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll(cx).map(Some)
    }
}

pub struct BorrowMutexLendGuard<'l, const M: usize, T> {
    mutex: *const BorrowMutex<M, T>,
    borrow: &'l BorrowMutexRef,
}

// await until the (first available) borrower acquires and then drops the BorrowMutexGuard
impl<'m, const M: usize, T> Future for BorrowMutexLendGuard<'m, M, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mutex = unsafe { &*self.mutex };
        if !self.borrow.ref_acquired.swap(true, Ordering::Acquire) {
            // first time polling this LendGuard, so wake the Borrower
            self.borrow.borrow_waiter.wake();

            // the BorrowGuard could have been already dropped and won't wake us
            // again, so check now
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        if mutex.lend_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        if self.borrow.guard_present.load(Ordering::Acquire) {
            // spurious awake
            return Poll::Pending;
        }

        Poll::Ready(())
    }
}

impl<'l, const M: usize, T> Drop for BorrowMutexLendGuard<'l, M, T> {
    fn drop(&mut self) {
        let mutex = unsafe { &*self.mutex };

        if self.borrow.guard_present.load(Ordering::Acquire) {
            eprintln!("LendGuard dropped while the value is still borrowed");
            // the mutable reference is about to be re-gained in the lending context while
            // it's still used in the borrowed context. We can't let that happen, and we can't
            // even panic here as this may invalidate the referenced object. If the borrow is
            // happening independently of this panic (i.e. on another thread) it would be UB.
            // So abort the entire process here.
            std::process::abort();
        }
        assert!(self.borrow.ref_acquired.load(Ordering::Acquire));
        mutex.inner_ref.store(null_mut(), Ordering::Release);

        let _ = mutex.borrowers.pop().unwrap();
        // self.borrow should be no longer accessed (it's still valid memory, noone
        // else has any other reference to the same region, but it doesn't contain
        // a valid object anymore)
    }
}

unsafe impl<'l, const M: usize, T> Sync for BorrowMutexLendGuard<'l, M, T>  {}
unsafe impl<'l, const M: usize, T> Send for BorrowMutexLendGuard<'l, M, T> {}