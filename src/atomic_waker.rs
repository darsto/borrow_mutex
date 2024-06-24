//! This is inspired by atomic_waker.rs from futures-core-0.3.30.
//! The AtomicWaker struct and its methods were replaced by regular
//! functions which take its `Option<Waker>` and the state variable
//! as separate parameters. This lets us pack the desired structure
//! with additional data without any padding bytes.
//! On top of that, this AtomicWaker will return Poll::Ready(())
//! if wake() was called anytime before, even if this is was the
//! first executed poll

use core::cell::UnsafeCell;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use core::task::Poll;
use core::task::Waker;

pub type AtomicWaker = UnsafeCell<Option<Waker>>;
pub type AtomicWakerState = AtomicU8;

// neither polling nor waiting
const IDLING: u8 = 0;
// poll_const() in progress and currently replacing the waker
const REGISTERING: u8 = 0b001;
// wake() in progress and currently accessing the waker
const WAKING: u8 = 0b010;
// wake() in pending (but wake() is not necessarily in progress)
const AWOKEN: u8 = 0b100;

pub fn poll_const(atomic_waker: &AtomicWaker, state: &AtomicWakerState, waker: &Waker) -> Poll<()> {
    match state.fetch_or(REGISTERING, Acquire) {
        IDLING => {
            // Store the new waker, but avoid storing if the old waker
            // is good enough - meaning it would already wake enough
            //
            // SAFETY: We've set the REGISTERING bit, so we're the only ones accessing
            // the waker now
            match unsafe { &mut *atomic_waker.get() } {
                Some(old_waker) if old_waker.will_wake(waker) => (),
                new_waker => *new_waker = Some(waker.clone()),
            }

            let prev = state.swap(IDLING, AcqRel);
            // wake() could have been called just before setting the actual
            // waker, which it couldn' read due to our REGISTERING bit.
            // If so, just return ready
            if prev & WAKING != 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
        prev if prev & REGISTERING != 0 => {
            // another poll_const() is underway. There's no good way to handle
            // it and it's not the expected functionality, so just return
            // Poll::Pending. Once the other poll is finished it will unset
            // the REGISTERING flag
            Poll::Pending
        }
        prev if prev & AWOKEN != 0 => {
            debug_assert!(prev == AWOKEN || prev == WAKING | AWOKEN);
            state.store(IDLING, Release);
            Poll::Ready(())
        }
        prev => {
            debug_assert!(prev == WAKING);
            // We're about to be awoken, but we haven't necessarily stored
            // our waker yet and we won't be polled again. We have to return
            // Ready now, but also make sure to now return Ready multiple
            // times from a single wake - for that reason we have a wait
            while state.load(Relaxed) & AWOKEN != 0 {
                core::hint::spin_loop();
            }
            Poll::Ready(())
        }
    }
}

pub fn wake(atomic_waker: &AtomicWaker, state: &AtomicWakerState) {
    // Release ordering is only used to effectively synchronize any caller's
    // access to memory this waker is meant to share
    match state.fetch_or(WAKING, AcqRel) {
        IDLING => {
            // SAFETY: we've set the WAKING bit, so we're the only ones
            // accessing the waker now
            let waker = unsafe { (*atomic_waker.get()).take() };
            // transition to the next state. Any poll triggered before
            // is currently blocked and just waits for us.
            state.store(AWOKEN, Release);
            if let Some(waker) = waker {
                // wake the waker - it might be a spurious wake, but it
                // won't cause any harm
                waker.wake();
            }
        }
        _ => {
            // the waker is already awake or the registering is underway
            // and it will react to the WAKING bit we've set.
            // Nothing else to do
        }
    }
}
