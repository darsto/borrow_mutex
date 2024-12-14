// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk
#![allow(unknown_lints)]
#![deny(clippy::enum_glob_use)]
#![allow(clippy::missing_transmute_annotations)]
#![allow(clippy::doc_lazy_continuation)]
#![doc = include_str!("../README.md")]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(any(feature = "std", panic = "abort")))]
compile_error!("no_std version of this crate requires panic = abort to ensure safety.");

use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, size_of, ManuallyDrop};
use core::pin::Pin;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicU8, Ordering};
use core::task::{Context, Poll};

use atomic_waker::{AtomicWaker, AtomicWakerState};

#[doc(hidden)]
pub mod mpmc;
use mpmc::{MPMCRef, MPMC};
#[doc(hidden)]
mod atomic_waker;

/// Async Mutex which does not require wrapping the target structure.
/// A `&mut T` can be lended to the mutex at any given time.
///
/// This lets any other side borrow the `&mut T`. The mutable ref is borrow-able
/// only while the lender awaits, and the lending side can await until someone
/// wants to borrow. The semantics enforce at most one side has a mutable reference
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
#[repr(C)]
pub struct BorrowMutex<const MAX_BORROWERS: usize, T: ?Sized> {
    inner_ref: UnsafeCell<Option<NonNull<T>>>,
    lend_waiter: AtomicWaker,
    lend_waiter_state: AtomicWakerState,
    state: AtomicU8,
    borrowers: MPMC<MAX_BORROWERS, BorrowRef>,
}

#[repr(u8)]
#[derive(Debug)]
enum LendState {
    None = 0,
    Starting,
    Lending,
    Terminating,
}

impl<const M: usize, T: ?Sized> BorrowMutex<M, T> {
    pub const MAX_BORROWERS: usize = M;

    /// Create a new empty [`BorrowMutex`].
    pub const fn new() -> Self {
        Self {
            inner_ref: UnsafeCell::new(None),
            lend_waiter: AtomicWaker::new(None),
            lend_waiter_state: AtomicWakerState::new(0),
            state: AtomicU8::new(LendState::None as u8),
            borrowers: MPMC::new(),
        }
    }

    #[inline]
    fn as_ptr(&self) -> BorrowMutexRef<'_, T> {
        BorrowMutexRef(self as *const _ as *const BorrowMutex<0, T>, PhantomData)
    }

    /// Retrieve a Future that resolves when a reference is lended.
    /// Specifically, it resolves to [`Result`]<[`BorrowGuardArmed`], [`Error`]>.
    ///
    /// The error can be:
    ///  - the max number of concurrent borrowers was reached - [`MAX_BORROWERS`].
    ///  - the mutex was terminated via [`BorrowMutex::terminate()`] call.
    ///
    /// The borrowers are lended to in FIFO order but they get put to the
    /// waiting queue only on their first poll.
    ///
    /// A borrow guard which was lended to must be dropped to allow further
    /// borrows to happen.
    ///
    /// Also see [`BorrowMutex::lend()`].
    ///
    /// [`MAX_BORROWERS`]: Self::MAX_BORROWERS
    /// [`Error`]: enum@crate::Error
    pub fn request_borrow<'g, 'm: 'g>(&'m self) -> BorrowGuardUnarmed<'g, T> {
        BorrowGuardUnarmed {
            mutex: self.as_ptr(),
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
    pub fn wait_to_lend<'g, 'm: 'g>(&'m self) -> LendWaiter<'g, T> {
        LendWaiter {
            mutex: self.as_ptr(),
        }
    }

    /// Lend a mutable reference to the first waiting borrower (FIFO order).
    /// This can be called even if there are no borrowers at the time (and will
    /// immediately return None). It is recommended to first call
    /// [`BorrowMutex::wait_to_lend()`].
    ///
    /// On successful lend, a [`LendGuard`] future is returned which:
    ///  - can be immediately dropped, which makes the whole operation invisible
    ///    to the borrower - it will keep waiting for another lender
    ///  - or, can be polled, and then dropped only once it resolves (to nothing).
    ///
    /// The first poll will awake the borrower. Dropping the [`LendGuard`] before
    /// it resolves (which implies - before the borrowing side drops its guard)
    /// will abort the process. Forgetting it via [`core::mem::forget()`] or
    /// similar cannot be detected at runtime, and may allow **Undefined Behavior**.
    /// The API should be marked unsafe, but it's not. Hopefully this broken
    /// behavior could be prohibited in future rust versions with some
    /// `#[must_drop]` annotation.
    ///
    /// # Note
    ///
    /// Only one value may be lended to the mutex at a time. Trying to lend
    /// while a previous [`LendGuard`] still exists will abort the entire process.
    ///
    /// Trying to lend after a mutex was terminated ([`BorrowMutex::terminate()`])
    /// will also abort the process.
    pub fn lend<'g, 'm: 'g>(&'m self, value: &'g mut T) -> Option<LendGuard<'g, T>> {
        if let Err(prev) = self.state.compare_exchange(
            LendState::None as u8,
            LendState::Starting as u8,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            // we could return None, but this would be ill-advised to try to
            // handle it. The rest of error handling uses abort(), so do the
            // same here for consistency.
            if prev == LendState::Terminating as u8 {
                abort("BorrowMutex lended to while terminated");
            } else {
                abort("multiple distinct references lended to a BorrowMutex");
            }
        }

        // SAFETY: We're the only ones writing this value and [`BorrowMutex::lender_lended`]
        // atomic makes sure no one is currently reading it
        unsafe { *self.inner_ref.get() = Some(NonNull::from(value)) };
        self.state
            .store(LendState::Lending as u8, Ordering::Release);

        // SAFETY: We're the only lend-er, and the object in MPMC gets only
        // de-queued and invalidated by the lender.
        let borrow = self.borrowers.peek()?;
        let borrow = unsafe { &*borrow.get() };
        Some(LendGuard {
            mutex: self.as_ptr(),
            borrow,
            _marker: PhantomPinned,
        })
    }

    /// Mark the mutex as terminated, meaning any borrows requests (pending or
    /// to-be-made) will return [`Error::Terminated`].
    ///
    /// # Note
    ///
    /// This is an async function as the existing borrowers need to be awoken
    /// one by one to drop their guard, which then allows this function to
    /// proceed. This only applies to existing borrowers - Any borrows made
    /// while this function is executing will immediately return an error on
    /// the first poll.
    ///
    /// Calling this function while a value is lended will abort the process.
    pub async fn terminate(&self) {
        if let Err(prev) = self.state.compare_exchange(
            LendState::None as u8,
            LendState::Terminating as u8,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            if prev == LendState::Terminating as u8 {
                return;
            } else {
                // we can't gracefully proceed now, so abort the entire process
                abort("BorrowMutex terminated while a reference is lended");
            }
        }

        // Make sure self.terminated is immediately visible for concurrent
        // borrowers as well as lenders
        atomic::fence(Ordering::SeqCst);

        while let Some(borrow) = self.borrowers.peek() {
            // SAFETY: we're the only "lend-er", and the object in MPMC will be
            // de-queued and invalidated only by us. There are no other mutable
            // references to this object, so soundness properties are preserved.
            let borrow = unsafe { &*borrow.get() };
            let lend_guard = LendGuard {
                mutex: self.as_ptr(),
                borrow,
                _marker: PhantomPinned,
            };

            // self.inner_ref remains null but our [`self.terminated`] shall
            // prevent any access to it
            lend_guard.await;
            // dropping the lend_guard shall pop the borrow-er from the queue
        }
    }

    /// Equivalent of core::mem::offset_of!(.., borrowers) that works
    /// in rust version pre-1.77.
    const fn borrowers_offset() -> usize {
        let offset = size_of::<UnsafeCell<Option<NonNull<T>>>>()
            + size_of::<AtomicWaker>()
            + size_of::<AtomicWakerState>()
            + size_of::<AtomicU8>();

        let align = align_of::<MPMC<M, BorrowRef>>();
        (offset + align - 1) & !(align - 1)
    }
}

unsafe impl<const M: usize, T: ?Sized + Send> Send for BorrowMutex<M, T> {}
unsafe impl<const M: usize, T: ?Sized + Send> Sync for BorrowMutex<M, T> {}

impl<const M: usize, T: ?Sized> Default for BorrowMutex<M, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const M: usize, T: ?Sized> core::fmt::Debug for BorrowMutex<M, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "BorrowMutex {{ lender_state: {:?} }}",
            self.state.load(Ordering::Relaxed)
        ))
    }
}

/// Reference to BorrowMutex, but without the `MAX_BORROWERS` const generic.
/// For internal use only.
struct BorrowMutexRef<'a, T: ?Sized>(*const BorrowMutex<0, T>, PhantomData<&'a T>);

impl<T: ?Sized> BorrowMutexRef<'_, T> {
    #[inline]
    fn borrowers(&self) -> MPMCRef<'_, BorrowRef> {
        unsafe {
            MPMCRef::from_ptr(
                self.0
                    .cast::<u8>()
                    .add(BorrowMutex::<0, T>::borrowers_offset())
                    as *const MPMC<0, BorrowRef>,
            )
        }
    }
}

impl<T: ?Sized> core::ops::Deref for BorrowMutexRef<'_, T> {
    type Target = BorrowMutex<0, T>;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0 }
    }
}

impl<T: ?Sized> Clone for BorrowMutexRef<'_, T> {
    fn clone(&self) -> Self {
        BorrowMutexRef(self.0, PhantomData)
    }
}

/// Common part between [`LendGuard`] and [`BorrowGuardUnarmed`]/[`BorrowGuardArmed`].
/// This is kept inside the [`BorrowMutex`]'s MPMC until it can be safely
/// dropped.
struct BorrowRef {
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
pub struct BorrowGuardUnarmed<'g, T: ?Sized> {
    mutex: BorrowMutexRef<'g, T>,
    inner: AtomicPtr<BorrowRef>,
    terminated: AtomicBool,
}

/// Possible borrow errors
#[derive(Debug)]
pub enum Error {
    TooManyBorrows,
    Terminated,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use Error as E;
        let msg = match self {
            E::TooManyBorrows => "Too many borrow requests that are still pending",
            E::Terminated => "The mutex was terminated and won't be ever lend-ed to again",
        };
        f.write_str(msg)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

// await until the reference is lended
impl<'g, T: 'g + ?Sized> Future for BorrowGuardUnarmed<'g, T> {
    type Output = Result<BorrowGuardArmed<'g, T>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.mutex.state.load(Ordering::Acquire) == LendState::Terminating as u8 {
            return Poll::Ready(Err(Error::Terminated));
        }

        if self.terminated.load(Ordering::Relaxed) {
            return Poll::Ready(Err(Error::Terminated));
        }

        if self.inner.load(Ordering::Relaxed).is_null() {
            // we're polled for the first time; try to register a real borrower
            let Ok(inner) = self
                .mutex
                .borrowers() //
                .push(BorrowRef {
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

        // The mutex could've been terminated just after we've pushed to the ring
        atomic::fence(Ordering::SeqCst);
        if self.mutex.state.load(Ordering::Relaxed) == LendState::Terminating as u8 {
            return Poll::Ready(Err(Error::Terminated));
        }

        // SAFETY: The object is kept inside the BorrowMutex until the lend guard
        // drops, which can only happen after this borrow guard is dropped. There
        // are no other mutable references to this object, so soundness is preserved
        let inner = unsafe { &*self.inner.load(Ordering::Relaxed) };
        if atomic_waker::poll_const(&inner.borrow_waker, &inner.borrow_waker_state, cx.waker())
            == Poll::Pending
        {
            return Poll::Pending;
        }

        // The borrow_waker turns ready only when wake() is called.
        // There are no spurious wakeups, and this is the only ready poll we get.
        let lend_state = self.mutex.state.load(Ordering::Acquire);
        assert_eq!(lend_state, LendState::Lending as u8);
        // SAFETY: The object remains valid until we unset [`BorrowMutexRef::guard_present`]
        // (which happens at [`BorrowGuardArmed::drop`]).
        let inner_ref = unsafe { *self.mutex.inner_ref.get() }.unwrap();
        self.terminated.store(true, Ordering::Relaxed);
        // SAFETY: Rust binds those references to &self lifetime and doesn't let
        // us return them from the function, but we know self.mutex lives long
        // enough - the returned guard has the same lifetime
        let (lend_waiter, lend_waiter_state) = unsafe {
            (
                std::mem::transmute(&self.mutex.lend_waiter),
                std::mem::transmute(&self.mutex.lend_waiter_state),
            )
        };
        Poll::Ready(Ok(BorrowGuardArmed {
            inner_ref,
            lend_waiter,
            lend_waiter_state,
            inner,
        }))
    }
}

impl<T: ?Sized> Drop for BorrowGuardUnarmed<'_, T> {
    fn drop(&mut self) {
        if !self.terminated.load(Ordering::Relaxed) {
            let ref_ptr = self.inner.load(Ordering::Relaxed);
            if !ref_ptr.is_null() {
                // SAFETY: The object is kept inside the BorrowMutex until the lend guard
                // drops, which can only happen after guard_present is set to false, (which
                // we're just about to do). There are no other mutable references to this
                // object, so soundness is preserved
                unsafe { &*ref_ptr }
                    .guard_present
                    .store(false, Ordering::Relaxed);
                // self.inner is no longer valid
                atomic_waker::wake(&self.mutex.lend_waiter, &self.mutex.lend_waiter_state);
            }
        }
    }
}

unsafe impl<T: ?Sized + Send> Send for BorrowGuardUnarmed<'_, T> {}

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
    inner_ref: NonNull<T>,
    lend_waiter: &'g AtomicWaker,
    lend_waiter_state: &'g AtomicWakerState,
    inner: &'g BorrowRef,
}

impl<'g, T: ?Sized> BorrowGuardArmed<'g, T> {
    /// Equivalent of the std::sync::MutexGuard::map() API which is currently
    /// unstable. This makes a guard for a component of the borrowed data.
    pub fn map<U: ?Sized, F>(orig: Self, f: F) -> BorrowGuardArmed<'g, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let inner_ref = f(unsafe { &mut *orig.inner_ref.as_ptr() });
        let orig = ManuallyDrop::new(orig);
        BorrowGuardArmed {
            inner_ref: NonNull::from(inner_ref),
            lend_waiter: orig.lend_waiter,
            lend_waiter_state: orig.lend_waiter_state,
            inner: orig.inner,
        }
    }
}

impl<T: ?Sized> core::ops::Deref for BorrowGuardArmed<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { self.inner_ref.as_ref() }
    }
}

impl<T: ?Sized> core::ops::DerefMut for BorrowGuardArmed<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.inner_ref.as_mut() }
    }
}

impl<T: ?Sized> Drop for BorrowGuardArmed<'_, T> {
    fn drop(&mut self) {
        self.inner.guard_present.store(false, Ordering::Release);
        // self.inner is no longer valid
        atomic_waker::wake(self.lend_waiter, self.lend_waiter_state);
    }
}

unsafe impl<T: ?Sized + Send> Send for BorrowGuardArmed<'_, T> {}
unsafe impl<T: ?Sized + Sync> Sync for BorrowGuardArmed<'_, T> {}

/// A Future of the [`BorrowMutex`] which resolves as soon as any borrow request
/// is pending.
///
/// This structure is created by [`BorrowMutex::wait_to_lend()`].
pub struct LendWaiter<'m, T: ?Sized> {
    mutex: BorrowMutexRef<'m, T>,
}

// await until there is someone wanting to borrow
impl<T: ?Sized> Future for LendWaiter<'_, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // in general case we want to poll the lend_waiter, but it's awoken
        // on both:
        // - dropping the BorrowGuard
        // - creating a new BorrowGuard
        // And the same lend_waiter is polled in LendGuard, which could have
        // consumed both of those wakes. Before we start endlessly polling now,
        // check if we're ready
        if !self.mutex.borrowers().is_empty() {
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
            if !self.mutex.borrowers().is_empty() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

unsafe impl<T: ?Sized> Send for LendWaiter<'_, T> {}

/// An RAII implementation of a "scoped lock" of a lending side of a mutex.
/// This structure is created by [`BorrowMutex::lend()`], and is associated
/// with a certain borrower.
///
/// This structure can be dropped immediately (without any polling), or can
/// be polled to completion before being dropped. Trying to drop it
/// prematurely will cause the entire process to abort.
///
/// On the first poll of [`LendGuard`] the associated [`BorrowGuardUnarmed`]
/// will resolve (effectively - it will turned armed), and [`LendGuard`] will
/// resolve once the armed guard - [`BorrowGuardArmed`] - is dropped.
pub struct LendGuard<'l, T: ?Sized> {
    mutex: BorrowMutexRef<'l, T>,
    borrow: &'l BorrowRef,
    _marker: PhantomPinned,
}

// await until the (first available) borrower acquires and then drops the BorrowMutexGuard
impl<T: ?Sized> Future for LendGuard<'_, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.borrow.ref_acquired.swap(true, Ordering::Relaxed) {
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

impl<T: ?Sized> Drop for LendGuard<'_, T> {
    fn drop(&mut self) {
        let guard_present = self.borrow.guard_present.load(Ordering::Acquire);
        if self.borrow.ref_acquired.load(Ordering::Relaxed) && guard_present {
            // the mutable reference is about to be re-gained in the lending context while
            // it's still used in the borrowed context. We can't let that happen, and we can't
            // even panic here as this may invalidate the referenced object. If the borrow is
            // happening independently of this panic (i.e. on another thread) it would be UB.
            // So abort the entire process here.
            abort("LendGuard dropped while the reference is still borrowed");
        }
        unsafe { *self.mutex.inner_ref.get() = None };
        if self.borrow.ref_acquired.load(Ordering::Relaxed) {
            let _ = self.mutex.borrowers().pop().unwrap();
        }
        // self.borrow should be no longer accessed (it's still valid memory with
        // valid initialized data, but might be reused by someone else now)
        let _ = self.mutex.state.compare_exchange(
            LendState::Lending as u8,
            LendState::None as u8,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

unsafe impl<T: ?Sized + Send> Send for LendGuard<'_, T> {}

#[cfg(feature = "std")]
static ABORT_FN: AtomicPtr<fn() -> !> = AtomicPtr::new(std::process::abort as *mut _);

#[cfg(feature = "std")]
#[doc(hidden)]
pub unsafe fn set_abort_fn(f: fn() -> !) {
    ABORT_FN.store(f as *mut _, Ordering::Relaxed);
}

fn abort(msg: &str) -> ! {
    #[cfg(feature = "std")]
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(msg.as_bytes());
        let _ = std::io::stderr().write_all(b"\n");
        let _ = std::io::stderr().flush();
        // SAFETY: ABORT_FN is always set to a valid fn()
        let abort_fn =
            unsafe { *(&ABORT_FN.load(Ordering::Relaxed) as *const _ as *mut fn() -> !) };
        abort_fn();
    }
    #[cfg(not(feature = "std"))]
    {
        panic!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::BorrowMutex;

    #[test]
    fn validate_borrowers_field_offset() {
        assert_eq!(
            BorrowMutex::<0, usize>::borrowers_offset(),
            core::mem::offset_of!(BorrowMutex<0, usize>, borrowers)
        );

        assert_eq!(
            BorrowMutex::<0, &dyn core::any::Any>::borrowers_offset(),
            core::mem::offset_of!(BorrowMutex<0, &dyn core::any::Any>, borrowers)
        );
    }
}
