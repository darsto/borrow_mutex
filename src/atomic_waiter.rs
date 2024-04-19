use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;
use futures::Stream;

/// Thin wrapper over [`futures::task::AtomicWaker`]. This represents a
/// Send + Sync Future that can be completed by calling its wake() method.
/// It's possible to wake() before anything poll()-s, and this will result
/// in the first subsequent poll() immediately return success.
/// Every successfull poll() resets the "awoken" status.
pub struct AtomicWaiter {
    /// The task was awoken since the last poll.
    /// This is useful to check if the task was awoken before anyone
    /// awaited it - in that case awaiting should return immediately
    awoken: AtomicBool,
    waker: AtomicWaker,
}

unsafe impl Sync for AtomicWaiter {}
unsafe impl Send for AtomicWaiter {}

impl std::fmt::Debug for AtomicWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "AsyncCbStream {{ ready: {} }}",
            self.awoken.load(Ordering::Relaxed)
        ))
    }
}

impl AtomicWaiter {
    pub fn new() -> Self {
        Self {
            awoken: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    pub fn wake(&self) {
        let was_awake = self.awoken.swap(true, Ordering::AcqRel);
        if !was_awake {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            } else {
                // nothing to do -> any subsequent poll will immediately return success
            }
        }
    }

    pub fn poll_const(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.awoken.swap(false, Ordering::AcqRel) {
            Poll::Ready(())
        } else {
            self.waker.register(cx.waker());
            // we might have been "awoken" just before setting the waker
            // and we won't be awoken again, so check again now
            if self.awoken.swap(false, Ordering::AcqRel) {
                return Poll::Ready(());
            }

            Poll::Pending
        }
    }
}

impl Default for AtomicWaiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for AtomicWaiter {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_const(cx)
    }
}

impl Stream for AtomicWaiter {
    type Item = ();
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll(cx).map(Some)
    }
}