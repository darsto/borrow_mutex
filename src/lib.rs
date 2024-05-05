// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk
#![doc = include_str!("../README.md")]

use core::future::Future;
use core::pin::Pin;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use core::task::{Context, Poll};
use std::marker::PhantomPinned;
use std::mem::ManuallyDrop;

use thiserror::Error;

pub mod atomic_waiter;
use atomic_waiter::AtomicWaiter;
pub mod mpmc;
use mpmc::MPMC;

/// Async Mutex which does not require wrapping the target structure.
/// Instead a &mut T can be lended to the mutex at any given timeslice.
///
/// This lets any other side borrow this &mut T. The data is borrow-able only
/// while the lender awaits, and the lending side can await until someone wants
/// to borrow. The semantics enforce at most one side has a mutable reference
/// at any given time.
///
/// This lets us share any mutable object between distinct async contexts
/// without Arc<Mutex> over the object in question and without relying on any
/// kind of internal mutability. It's mostly aimed at single-threaded executors
/// where internal mutability is an unnecessary complication. Nevertheless,
/// the Mutex is Send+Sync and can be safely used from any number of threads.
///
/// The API is fully safe and doesn't cause UB under any circumstances, but
/// it's not able to enforce all the semantics at compile time. I.e. if a
/// lending side of a transaction drops the lending Future before it's
/// resolved (before the borrowing side stops using it), the process will
/// immediately abort (...after printing an error message).
pub struct BorrowMutex<const MAX_BORROWERS: usize, T> {
    inner_ref: AtomicPtr<T>,
    lend_waiter: AtomicWaiter,
    terminated: AtomicBool,
    borrowers: MPMC<MAX_BORROWERS, BorrowMutexRef>,
}

unsafe impl<const M: usize, T> Send for BorrowMutex<M, T> {}
unsafe impl<const M: usize, T> Sync for BorrowMutex<M, T> {}

impl<const M: usize, T> BorrowMutex<M, T> {
    pub const MAX_BORROWERS: usize = M;

    pub fn new() -> Self {
        Self {
            inner_ref: AtomicPtr::new(null_mut()),
            lend_waiter: AtomicWaiter::new(),
            terminated: AtomicBool::new(false),
            borrowers: MPMC::new(),
        }
    }

    /// Retrieve a Future that resolves when a reference is lended.
    /// See [`Self::lend()`]
    pub fn request_borrow<'g, 'm: 'g>(&'m self) -> BorrowMutexGuardUnarmed<'g, M, T> {
        BorrowMutexGuardUnarmed {
            mutex: self,
            inner: AtomicPtr::new(null_mut()),
            terminated: AtomicBool::new(false),
        }
    }

    /// Retrieve a Future that resolves as soon as any borrow request is pending
    pub fn wait_to_lend<'g, 'm: 'g>(&'m self) -> BorrowMutexLender<'g, M, T> {
        BorrowMutexLender { mutex: self }
    }

    /// Lend a mutable reference to the first borrower (FIFO order).
    /// This can be called even if there are no borrowers at the time (and will
    /// immediately return None), but since it holds a mutable reference and
    /// prevents its further use, it's recommended to first await the lender
    /// ([`BorrowMutexLender`], obtained with [`Self::wait_to_lend`]).
    pub fn lend<'g, 'm: 'g>(&'m self, value: &'g mut T) -> Option<BorrowMutexLendGuard<'g, M, T>> {
        if self.terminated.load(Ordering::Acquire) {
            eprintln!("BorrowMutex lended to while terminated");
            // we could return None, but this would be ill-advised to try to
            // handle it. The rest of error handling uses abort(), so do the
            // same here for consistency.
            std::process::abort();
        }

        let borrow = self.borrowers.peek()?;

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

        // SAFETY: The above check ensures that we're the only lend-er, and the
        // object in MPMC is only de-queued and invalidated by the lender. There
        // are no other mutable references to this object, so soundness properties
        // are preserved.
        let borrow = unsafe { &*borrow.get() };
        Some(BorrowMutexLendGuard {
            mutex: self,
            borrow,
            _marker: PhantomPinned,
        })
    }

    /// Mark the mutex as terminated, meaning any borrows requests (pending or
    /// to-be-made) will return [`BorrowMutexError::Terminated`].
    pub async fn terminate(&self) {
        if self.terminated.swap(true, Ordering::AcqRel) {
            // already terminated
            return;
        }

        if !self.inner_ref.load(Ordering::Acquire).is_null() {
            eprintln!("BorrowMutex terminated while a reference is lended");
            // we can't gracefully proceed now, so abort the entire process
            std::process::abort();
        }

        while let Some(borrow) = self.borrowers.peek() {
            // SAFETY: we're the only "lend-er", and the object in MPMC will be
            // de-queued and invalidated only by us. There are no other mutable
            // references to this object, so soundness properties are preserved.
            let borrow = unsafe { &*borrow.get() };
            let lend_guard = BorrowMutexLendGuard {
                mutex: self,
                borrow,
                _marker: PhantomPinned,
            };

            // self.inner_ref remains null but our [`self.terminated`] shall
            // prevent any access to it
            lend_guard.await;
            // dropping the lend_guard shall pop the borrow-er from the queue
        }
    }
}

impl<const M: usize, T> Default for BorrowMutex<M, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const M: usize, T> core::fmt::Debug for BorrowMutex<M, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "BorrowMutex {{ available_for_borrow: {} }}",
            !self.inner_ref.load(Ordering::Relaxed).is_null()
        ))
    }
}

/// Common part between lend guard and borrow guard.
/// This is kept inside the BorrowMutex MPMC until it can be safely
/// dropped.
///
/// TODO: This is currently 40bytes on x86_64, but could be 24bytes if we
/// organized the fields
struct BorrowMutexRef {
    borrow_waiter: AtomicWaiter,
    ref_acquired: AtomicBool,
    guard_present: AtomicBool,
}

/// TODO: doc
pub struct BorrowMutexGuardUnarmed<'g, const M: usize, T> {
    mutex: &'g BorrowMutex<M, T>,
    inner: AtomicPtr<BorrowMutexRef>,
    terminated: AtomicBool,
}

/// Possible borrow errors
#[derive(Debug, Error)]
pub enum BorrowMutexError {
    #[error("Too many borrow requests that are still pending")]
    TooManyBorrows,
    #[error("The mutex was terminated and won't be ever lend-ed to again")]
    Terminated,
}

// await until the reference is lended
impl<'g, const M: usize, T: 'g> Future for BorrowMutexGuardUnarmed<'g, M, T> {
    type Output = Result<BorrowMutexGuardArmed<'g, T>, BorrowMutexError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mutex.terminated.load(Ordering::Acquire) {
            if self.inner.load(Ordering::Relaxed).is_null() {
                // we haven't managed yet to register a real borrower, so
                // no need to drop it in the Drop trait
                self.terminated.store(true, Ordering::Relaxed)
            }
            return Poll::Ready(Err(BorrowMutexError::Terminated));
        }

        if self.terminated.load(Ordering::Relaxed) {
            return Poll::Ready(Err(BorrowMutexError::Terminated));
        }

        if self.inner.load(Ordering::Relaxed).is_null() {
            // we're polled for the first time; try to register a real borrower
            let Ok(inner) = self
                .mutex
                .borrowers //
                .push(BorrowMutexRef {
                    borrow_waiter: AtomicWaiter::new(),
                    ref_acquired: AtomicBool::new(false),
                    guard_present: AtomicBool::new(true),
                })
            else {
                return Poll::Ready(Err(BorrowMutexError::TooManyBorrows));
            };

            // borrow guard will turn ready when any lend guard sees us, so
            // try to awake any sleeping one
            self.mutex.lend_waiter.wake();
            self.inner.store(inner.get(), Ordering::Relaxed);
        }

        // SAFETY: The object is kept inside the BorrowMutex until the lend guard
        // drops, which can only happen after this borrow guard is dropped. There
        // are no other mutable references to this object, so soundness is preserved
        let inner = unsafe { &mut *self.inner.load(Ordering::Relaxed) };
        if inner.borrow_waiter.poll_const(cx) == Poll::Pending {
            return Poll::Pending;
        }

        // The borrow_waiter turns ready only when wake() is called.
        // There are no spurious wakeups, and this is the only ready poll we get.
        let inner_ref = self.mutex.inner_ref.load(Ordering::Acquire);
        assert!(!inner_ref.is_null());
        self.terminated.store(true, Ordering::Relaxed);
        Poll::Ready(Ok(BorrowMutexGuardArmed {
            inner_ref: AtomicPtr::new(inner_ref),
            lend_waiter: &self.mutex.lend_waiter,
            inner: AtomicPtr::new(inner),
        }))
    }
}

impl<'m, const M: usize, T> Drop for BorrowMutexGuardUnarmed<'m, M, T> {
    fn drop(&mut self) {
        if !self.terminated.load(Ordering::Relaxed) {
            // SAFETY: The object is kept inside the BorrowMutex until the lend guard
            // drops, which can only happen after guard_present is set to false, (which
            // we're just about to do). There are no other mutable references to this
            // object, so soundness is preserved
            unsafe { &*self.inner.load(Ordering::Relaxed) }
                .guard_present
                .store(false, Ordering::Release);
            // self.inner is no longer valid
            self.mutex.lend_waiter.wake();
        }
    }
}

/// SAFETY: This is merely a pointer to a state within BorrowMutex that consists
/// of only atomics.
unsafe impl<'m, const M: usize, T> Send for BorrowMutexGuardUnarmed<'m, M, T> {}

/// TODO: doc
pub struct BorrowMutexGuardArmed<'g, T> {
    inner_ref: AtomicPtr<T>,
    lend_waiter: &'g AtomicWaiter,
    inner: AtomicPtr<BorrowMutexRef>,
}

impl<'g, T> BorrowMutexGuardArmed<'g, T> {
    /// Equivalent of the std::sync::MutexGuard::map() API which is currently
    /// unstable. This makes a guard for a component of the borrowed data.
    /// TODO: hide this behind a feature flag?
    pub fn map<U, F>(orig: Self, f: F) -> BorrowMutexGuardArmed<'g, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let inner_ref = AtomicPtr::new(f(unsafe { &mut *orig.inner_ref.load(Ordering::Relaxed) }));
        let orig = ManuallyDrop::new(orig);
        BorrowMutexGuardArmed {
            inner_ref,
            lend_waiter: orig.lend_waiter,
            inner: AtomicPtr::new(orig.inner.load(Ordering::Relaxed)),
        }
    }
}

impl<'g, T> core::ops::Deref for BorrowMutexGuardArmed<'g, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        let inner_ref = self.inner_ref.load(Ordering::Acquire);
        // SAFETY: The mutable reference was captured via [`Self::lend()`] which
        // effectively doesn't return until the lend guard is dropped, which doesn't
        // happen until this borrow guard is dropped.
        // If the lending future is dropped without awaiting the whole process will
        // abort - before any undefined behavior creeps in.
        unsafe { &mut *inner_ref }
    }
}

impl<'g, T> core::ops::DerefMut for BorrowMutexGuardArmed<'g, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let inner_ref = self.inner_ref.load(Ordering::Acquire);
        // SAFETY: The mutable reference was captured via [`Self::lend()`] which
        // effectively doesn't return until the lend guard is dropped, which doesn't
        // happen until this borrow guard is dropped.
        // If the lending future is dropped without awaiting the whole process will
        // abort - before any undefined behavior creeps in.
        unsafe { &mut *inner_ref }
    }
}

impl<'m, T> Drop for BorrowMutexGuardArmed<'m, T> {
    fn drop(&mut self) {
        // SAFETY: The object is kept inside the BorrowMutex until the lend guard
        // drops, which can only happen after guard_present is set to false, (which
        // we're just about to do). There are no other mutable references to this
        // object, so soundness is preserved
        unsafe { &*self.inner.load(Ordering::Relaxed) }
            .guard_present
            .store(false, Ordering::Release);
        // self.inner is no longer valid
        self.lend_waiter.wake();
    }
}

/// An armed guard could be used to obtain multiple immutable references, so it makes
/// some sense to impl Sync.
/// SAFETY: This is merely a pointer to a state within BorrowMutex that consists
/// of only atomics.
unsafe impl<'m, T> Sync for BorrowMutexGuardArmed<'m, T> {}

/// TODO: doc
pub struct BorrowMutexLender<'l, const M: usize, T> {
    mutex: &'l BorrowMutex<M, T>,
}

// await until there is someone wanting to borrow
impl<'m, const M: usize, T> Future for BorrowMutexLender<'m, M, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // in general case we want to poll the lend_waiter, but it's awoken
        // on both:
        // - dropping the BorrowGuard
        // - creating a new BorrowGuard
        // And the same lend_waiter is polled in LendGuard, which could have
        // consumed both of those wakes. Before we start endlessly polling now,
        // check if we're ready
        if !self.mutex.borrowers.is_empty() {
            return Poll::Ready(());
        }
        // LendGuard could have turned Ready without ever polling, so
        // also handle the spurious wakes here
        while self.mutex.lend_waiter.poll_const(cx) == Poll::Ready(()) {
            if !self.mutex.borrowers.is_empty() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

impl<'g, 'm: 'g, const M: usize, T> BorrowMutexLender<'m, M, T> {
    /// Alias to [`BorrowMutex::lend`] with exact same semantics.
    pub fn lend(&'m self, value: &'g mut T) -> Option<BorrowMutexLendGuard<'g, M, T>> {
        self.mutex.lend(value)
    }
}

/// TODO: doc
pub struct BorrowMutexLendGuard<'l, const M: usize, T> {
    mutex: &'l BorrowMutex<M, T>,
    borrow: &'l BorrowMutexRef,
    /// Once polled, the LendGuard must have its Drop impl called, so
    /// effectively forbid core::mem::forget() by making the guard !Unpin
    /// See README.md "What if Drop is not called?"
    _marker: PhantomPinned,
}

// await until the (first available) borrower acquires and then drops the BorrowMutexGuard
impl<'m, const M: usize, T> Future for BorrowMutexLendGuard<'m, M, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.borrow.ref_acquired.swap(true, Ordering::Acquire) {
            // first time polling this LendGuard, so wake the Borrower
            self.borrow.borrow_waiter.wake();

            // the BorrowGuard could have been already dropped and won't wake us
            // again, so check now
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        while self.mutex.lend_waiter.poll_const(cx) == Poll::Ready(()) {
            // lend_waiter could have been awoken due to a new BorrowGuard,
            // but we're pending until our BorrowGuard is dropped
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}

impl<'l, const M: usize, T> Drop for BorrowMutexLendGuard<'l, M, T> {
    fn drop(&mut self) {
        if self.borrow.guard_present.load(Ordering::Acquire) {
            eprintln!("LendGuard dropped while the reference is still borrowed");
            // the mutable reference is about to be re-gained in the lending context while
            // it's still used in the borrowed context. We can't let that happen, and we can't
            // even panic here as this may invalidate the referenced object. If the borrow is
            // happening independently of this panic (i.e. on another thread) it would be UB.
            // So abort the entire process here.
            std::process::abort();
        }
        assert!(self.borrow.ref_acquired.load(Ordering::Acquire));
        self.mutex.inner_ref.store(null_mut(), Ordering::Release);

        let _ = self.mutex.borrowers.pop().unwrap();
        // self.borrow should be no longer accessed (it's still valid memory, noone
        // else has any other reference to the same region, but it doesn't contain
        // a valid object anymore)
    }
}

/// SAFETY: This is merely a pointer to a state within BorrowMutex that consists
/// of only atomics.
unsafe impl<'l, const M: usize, T> Send for BorrowMutexLendGuard<'l, M, T> {}
