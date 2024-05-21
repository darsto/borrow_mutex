// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk
#![doc = include_str!("../README.md")]

use core::future::Future;
use core::pin::Pin;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use core::task::{Context, Poll};
use std::cell::UnsafeCell;
use std::marker::PhantomPinned;
use std::mem::ManuallyDrop;

use atomic_waker::{AtomicWaker, AtomicWakerState};
use thiserror::Error;

#[doc(hidden)]
pub mod mpmc;
use mpmc::MPMC;
#[doc(hidden)]
mod atomic_waker;

/// Async Mutex which does not require wrapping the target structure.
/// A `&mut T` can be lended to the mutex at any given time.
///
/// This lets any other side borrow the `&mut T`. The  is borrow-able only
/// while the lender awaits, and the lending side can await until someone wants
/// to borrow. The semantics enforce at most one side has a mutable reference
/// at any given time.
///
/// This lets us share any mutable object between distinct async contexts
/// without [`Arc`]<[`Mutex`]> over the object in question and without relying
/// on any kind of internal mutability. It's mostly aimed at single-threaded
/// executors where internal mutability is an unnecessary complication.
/// Still, the [`BorrowMutex`] is Send+Sync and can be safely used from
/// any number of threads.
///
/// [`Arc`]: std::sync::Arc
/// [`Mutex`]: std::sync::Mutex
pub struct BorrowMutex<const MAX_BORROWERS: usize, T: ?Sized> {
    inner_ref: UnsafeCell<Option<*mut T>>,
    lend_waiter: AtomicWaker,
    lend_waiter_state: AtomicWakerState,
    lender_present: AtomicBool,
    lender_lended: AtomicBool,
    terminated: AtomicBool,
    borrowers: MPMC<MAX_BORROWERS, BorrowMutexRef>,
}

impl<const M: usize, T: ?Sized> BorrowMutex<M, T> {
    pub const MAX_BORROWERS: usize = M;

    /// Create a new empty [`BorrowMutex`]
    pub const fn new() -> Self {
        Self {
            inner_ref: UnsafeCell::new(None),
            lend_waiter: AtomicWaker::new(None),
            lend_waiter_state: AtomicWakerState::new(0),
            lender_present: AtomicBool::new(false),
            lender_lended: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            borrowers: MPMC::new(),
        }
    }

    /// Retrieve a Future that resolves when a reference is lended.
    ///
    /// The borrowers are lended to in FIFO order. A borrower which
    /// was lended to must be dropped in order for further borrows to
    /// happen.
    ///
    /// There is a limit to the number of concurrent borrowers -
    /// [`MAX_BORROWERS`] - but this function never fails. If the limit
    /// is reached, the returned [`BorrowGuardUnarmed`] will resolve to
    /// error on its first poll.
    ///
    /// See [`BorrowMutex::lend()`]
    ///
    /// [`MAX_BORROWERS`]: Self::MAX_BORROWERS
    pub fn request_borrow<'g, 'm: 'g>(&'m self) -> BorrowGuardUnarmed<'g, M, T> {
        BorrowGuardUnarmed {
            mutex: self,
            inner: AtomicPtr::new(null_mut()),
            terminated: AtomicBool::new(false),
        }
    }

    /// Retrieve a Future that resolves as soon as any borrow request is pending.
    ///
    /// # Note
    ///
    /// This can be called concurrently from multiple async contexts but it's
    /// hardly useful this way. See [`BorrowMutex::lend()`] for its limitations.
    pub fn wait_to_lend<'g, 'm: 'g>(&'m self) -> Lender<'g, M, T> {
        Lender { mutex: self }
    }

    /// Lend a mutable reference to the first borrower (FIFO order).
    /// This can be called even if there are no borrowers at the time (and will
    /// immediately return None), but since it holds a mutable reference and
    /// prevents its further use, it's recommended to first call
    /// [`BorrowMutex::wait_to_lend()`].
    ///
    /// # Note
    ///
    /// Only one value may be lended to the mutex at a time. Trying to lend
    /// while a previous [`LendGuard`] still exists will abort the entire process.
    pub fn lend<'g, 'm: 'g>(&'m self, value: &'g mut T) -> Option<LendGuard<'g, M, T>> {
        if self.lender_present.swap(true, Ordering::AcqRel) {
            eprintln!("multiple distinct references lended to a BorrowMutex");
            // we could return None, but this would be ill-advised to try to
            // handle it. The rest of error handling uses abort(), so do the
            // same here for consistency.
            std::process::abort();
        }

        if self.terminated.load(Ordering::Acquire) {
            eprintln!("BorrowMutex lended to while terminated");
            // we could return None, but this would be ill-advised to try to
            // handle it. The rest of error handling uses abort(), so do the
            // same here for consistency.
            std::process::abort();
        }

        // SAFETY: We're the only ones writing this value and [`BorrowMutex::lender_lended`]
        // atomic makes sure no one is currently reading it
        unsafe { *self.inner_ref.get() = Some(value) };
        let was_lended = self.lender_lended.swap(true, Ordering::AcqRel);
        assert!(!was_lended);

        // SAFETY: We're the only lend-er, and the object in MPMC gets only
        // de-queued and invalidated by the lender.
        let borrow = self.borrowers.peek()?;
        let borrow = unsafe { &*borrow.get() };
        Some(LendGuard {
            mutex: self,
            borrow,
            _marker: PhantomPinned,
        })
    }

    /// Mark the mutex as terminated, meaning any borrows requests (pending or
    /// to-be-made) will return [`Error::Terminated`].
    pub async fn terminate(&self) {
        if self.terminated.swap(true, Ordering::AcqRel) {
            // already terminated
            return;
        }

        if self.lender_present.load(Ordering::Acquire) {
            eprintln!("BorrowMutex terminated while a reference is lended");
            // we can't gracefully proceed now, so abort the entire process
            std::process::abort();
        }

        while let Some(borrow) = self.borrowers.peek() {
            // SAFETY: we're the only "lend-er", and the object in MPMC will be
            // de-queued and invalidated only by us. There are no other mutable
            // references to this object, so soundness properties are preserved.
            let borrow = unsafe { &*borrow.get() };
            let lend_guard = LendGuard {
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

unsafe impl<const M: usize, T: ?Sized> Send for BorrowMutex<M, T> {}
unsafe impl<const M: usize, T: ?Sized> Sync for BorrowMutex<M, T> {}

impl<const M: usize, T: ?Sized> Default for BorrowMutex<M, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const M: usize, T: ?Sized> core::fmt::Debug for BorrowMutex<M, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "BorrowMutex {{ available_for_borrow: {} }}",
            self.lender_lended.load(Ordering::Relaxed)
        ))
    }
}

/// Common part between [`LendGuard`] and [`BorrowGuardUnarmed`]/[`BorrowGuardArmed`].
/// This is kept inside the [`BorrowMutex`]'s MPMC until it can be safely
/// dropped.
///
/// TODO: This is currently 40bytes on x86_64, but could be 24bytes if we
/// organized the fields
struct BorrowMutexRef {
    borrow_waker: AtomicWaker,
    borrow_waker_state: AtomicWakerState,
    ref_acquired: AtomicBool,
    guard_present: AtomicBool,
}

/// An RAII implementation of a "scoped lock" of a mutex. This is an unarmed
/// variant which is still awaiting a value to be lended.
///
/// This structure is created by the [`BorrowMutex::request_borrow`] method.
///
/// It doesn't provide any access to the underlying structure yet, and has to
/// be polled to completion first. It should resolve into an armed
/// [`BorrowGuardArmed`] variant.
///
/// Borrows are resolved in FIFO order, but they need to be polled at
/// least once to be properly "registered" and considered for lending to.
/// The lending happens via [`BorrowMutex::wait_to_lend()`] and
/// [`BorrowMutex::lend()`]).
///
/// If an [`BorrowGuardUnarmed`] is dropped (falls out of scope) before
/// resolving to [`BorrowGuardArmed`], it effectively cancels the borrow
/// request, letting other potential borrowers borrow the lended value.
pub struct BorrowGuardUnarmed<'g, const M: usize, T: ?Sized> {
    mutex: &'g BorrowMutex<M, T>,
    inner: AtomicPtr<BorrowMutexRef>,
    terminated: AtomicBool,
}

/// Possible borrow errors
#[derive(Debug, Error)]
pub enum Error {
    #[error("Too many borrow requests that are still pending")]
    TooManyBorrows,
    #[error("The mutex was terminated and won't be ever lend-ed to again")]
    Terminated,
}

// await until the reference is lended
impl<'g, const M: usize, T: 'g + ?Sized> Future for BorrowGuardUnarmed<'g, M, T> {
    type Output = Result<BorrowGuardArmed<'g, T>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mutex.terminated.load(Ordering::Acquire) {
            if self.inner.load(Ordering::Relaxed).is_null() {
                // we haven't managed yet to register a real borrower, so
                // no need to drop it in the Drop trait
                self.terminated.store(true, Ordering::Relaxed)
            }
            return Poll::Ready(Err(Error::Terminated));
        }

        if self.terminated.load(Ordering::Relaxed) {
            return Poll::Ready(Err(Error::Terminated));
        }

        if self.inner.load(Ordering::Relaxed).is_null() {
            // we're polled for the first time; try to register a real borrower
            let Ok(inner) = self
                .mutex
                .borrowers //
                .push(BorrowMutexRef {
                    borrow_waker: AtomicWaker::new(None),
                    borrow_waker_state: AtomicWakerState::new(0),
                    ref_acquired: AtomicBool::new(false),
                    guard_present: AtomicBool::new(true),
                })
            else {
                return Poll::Ready(Err(Error::TooManyBorrows));
            };

            // borrow guard will turn ready when any lend guard sees us, so
            // try to awake any sleeping one
            atomic_waker::wake(&self.mutex.lend_waiter, &self.mutex.lend_waiter_state);
            self.inner.store(inner.get(), Ordering::Relaxed);
        }

        // SAFETY: The object is kept inside the BorrowMutex until the lend guard
        // drops, which can only happen after this borrow guard is dropped. There
        // are no other mutable references to this object, so soundness is preserved
        let inner = unsafe { &mut *self.inner.load(Ordering::Relaxed) };
        if atomic_waker::poll_const(&inner.borrow_waker, &inner.borrow_waker_state, cx.waker())
            == Poll::Pending
        {
            return Poll::Pending;
        }

        // The borrow_waker turns ready only when wake() is called.
        // There are no spurious wakeups, and this is the only ready poll we get.
        assert!(self.mutex.lender_lended.load(Ordering::Acquire));
        // SAFETY: The object remains valid until we unset [`BorrowMutexRef::guard_present`]
        // (which happens at [`BorrowGuardArmed::drop`]).
        let inner_ref = unsafe { *self.mutex.inner_ref.get() }.unwrap();
        self.terminated.store(true, Ordering::Relaxed);
        Poll::Ready(Ok(BorrowGuardArmed {
            inner_ref,
            lend_waiter: &self.mutex.lend_waiter,
            lend_waiter_state: &self.mutex.lend_waiter_state,
            inner: AtomicPtr::new(inner),
        }))
    }
}

impl<'m, const M: usize, T: ?Sized> Drop for BorrowGuardUnarmed<'m, M, T> {
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
            atomic_waker::wake(&self.mutex.lend_waiter, &self.mutex.lend_waiter_state);
        }
    }
}

unsafe impl<'m, const M: usize, T: ?Sized> Send for BorrowGuardUnarmed<'m, M, T> {}

/// An RAII implementation of a "scoped lock" of a mutex. This is an armed
/// variant which provides access to the lended value via its [`Deref`] and
/// [`DerefMut`] implementations.
///
/// This structure is created by polling [`BorrowGuardUnarmed`] to completion.
///
/// When [`BorrowGuardArmed`] is dropped (falls out of scope) the associated
/// future on the lender side ([`LendGuard`]) is completed and can be dropped,
/// which enables further lends to the [`BorrowMutex`].
///
/// [`Deref`]: core::ops::Deref
/// [`DerefMut`]: core::ops::DerefMut
pub struct BorrowGuardArmed<'g, T: ?Sized> {
    inner_ref: *mut T,
    lend_waiter: &'g AtomicWaker,
    lend_waiter_state: &'g AtomicWakerState,
    inner: AtomicPtr<BorrowMutexRef>,
}

impl<'g, T: ?Sized> BorrowGuardArmed<'g, T> {
    /// Equivalent of the std::sync::MutexGuard::map() API which is currently
    /// unstable. This makes a guard for a component of the borrowed data.
    /// TODO: hide this behind a feature flag?
    pub fn map<U, F>(orig: Self, f: F) -> BorrowGuardArmed<'g, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let inner_ref = f(unsafe { &mut *orig.inner_ref });
        let orig = ManuallyDrop::new(orig);
        BorrowGuardArmed {
            inner_ref,
            lend_waiter: orig.lend_waiter,
            lend_waiter_state: orig.lend_waiter_state,
            inner: AtomicPtr::new(orig.inner.load(Ordering::Relaxed)),
        }
    }
}

impl<'g, T: ?Sized> core::ops::Deref for BorrowGuardArmed<'g, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner_ref }
    }
}

impl<'g, T: ?Sized> core::ops::DerefMut for BorrowGuardArmed<'g, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inner_ref }
    }
}

impl<'m, T: ?Sized> Drop for BorrowGuardArmed<'m, T> {
    fn drop(&mut self) {
        // SAFETY: The object is kept inside the BorrowMutex until the lend guard
        // drops, which can only happen after guard_present is set to false, (which
        // we're just about to do). There are no other mutable references to this
        // object, so soundness is preserved
        unsafe { &*self.inner.load(Ordering::Relaxed) }
            .guard_present
            .store(false, Ordering::Release);
        // self.inner is no longer valid
        atomic_waker::wake(self.lend_waiter, self.lend_waiter_state);
    }
}

unsafe impl<'m, T: ?Sized> Send for BorrowGuardArmed<'m, T> {}
/// An armed guard could be used to obtain multiple immutable references, so it makes
/// some sense to impl Sync.
unsafe impl<'m, T: ?Sized> Sync for BorrowGuardArmed<'m, T> {}

/// A Lender of the [`BorrowMutex`] currently awaiting a [`BorrowGuardUnarmed`]
/// to lend to.
///
/// This structure is created by [`BorrowMutex::wait_to_lend()`].
pub struct Lender<'l, const M: usize, T: ?Sized> {
    mutex: &'l BorrowMutex<M, T>,
}

// await until there is someone wanting to borrow
impl<'m, const M: usize, T: ?Sized> Future for Lender<'m, M, T> {
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
        while atomic_waker::poll_const(
            &self.mutex.lend_waiter,
            &self.mutex.lend_waiter_state,
            cx.waker(),
        ) == Poll::Ready(())
        {
            if !self.mutex.borrowers.is_empty() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

impl<'g, 'm: 'g, const M: usize, T: ?Sized> Lender<'m, M, T> {
    /// Alias to [`BorrowMutex::lend`] with the exact same semantics.
    pub fn lend(&'m self, value: &'g mut T) -> Option<LendGuard<'g, M, T>> {
        self.mutex.lend(value)
    }
}

/// An RAII implementation of a "scoped lock" of a lending side of a mutex.
/// This structure is created by [`BorrowMutex::lend()`], and is associated
/// with a certain [`BorrowGuardUnarmed`].
///
/// This structure must be polled to completion before being dropped.
/// Trying to drop it prematurely will cause the entire process to abort.
///
/// On the first poll of [`LendGuard`] the associated [`BorrowGuardUnarmed`]
/// will resolve (effectively - it will turned armed), and [`LendGuard`] will
/// resolve once the armed guard - [`BorrowGuardArmed`] - is dropped.
pub struct LendGuard<'l, const M: usize, T: ?Sized> {
    mutex: &'l BorrowMutex<M, T>,
    borrow: &'l BorrowMutexRef,
    /// Once polled, the LendGuard must have its Drop impl called, so
    /// effectively forbid core::mem::forget() by making the guard !Unpin
    /// See README.md "What if Drop is not called?"
    _marker: PhantomPinned,
}

// await until the (first available) borrower acquires and then drops the BorrowMutexGuard
impl<'m, const M: usize, T: ?Sized> Future for LendGuard<'m, M, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.borrow.ref_acquired.swap(true, Ordering::AcqRel) {
            // first time polling this LendGuard, so wake the Borrower
            atomic_waker::wake(&self.borrow.borrow_waker, &self.borrow.borrow_waker_state);

            // the BorrowGuard could have been already dropped and won't wake us
            // again, so check now
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        while atomic_waker::poll_const(
            &self.mutex.lend_waiter,
            &self.mutex.lend_waiter_state,
            cx.waker(),
        ) == Poll::Ready(())
        {
            // lend_waiter could have been awoken due to a new BorrowGuard,
            // but we're pending until our BorrowGuard is dropped
            if !self.borrow.guard_present.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}

impl<'l, const M: usize, T: ?Sized> Drop for LendGuard<'l, M, T> {
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
        self.mutex.lender_lended.store(false, Ordering::Release);
        unsafe { *self.mutex.inner_ref.get() = None };
        let _ = self.mutex.borrowers.pop().unwrap();
        // self.borrow should be no longer accessed (it's still valid memory with
        // valid initialized data, but might be reused by someone else now)
        self.mutex.lender_present.store(false, Ordering::Release);
    }
}

unsafe impl<'l, const M: usize, T: ?Sized> Send for LendGuard<'l, M, T> {}
