// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use futures::{Future, FutureExt};
use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject;

struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn waiter_and_lend_guard_receive_borrow_notification() {
    let mutex = BorrowMutex::<16, usize>::new();
    let waiter_wakes = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let lender_wakes = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waiter_waker = Waker::from(waiter_wakes.clone());
    let lender_waker = Waker::from(lender_wakes.clone());
    let mut waiter_cx = Context::from_waker(&waiter_waker);
    let mut lender_cx = Context::from_waker(&lender_waker);
    let mut waiter = pin!(mutex.wait_to_lend());
    assert!(waiter.as_mut().poll(&mut waiter_cx).is_pending());

    let mut value = 0;
    let mut lender = pin!(mutex.lend(&mut value).unwrap());
    assert!(lender.as_mut().poll(&mut lender_cx).is_pending());

    let mut borrower = pin!(mutex.try_borrow());
    let borrower_waker = futures::task::noop_waker();
    let mut borrower_cx = Context::from_waker(&borrower_waker);
    assert!(borrower.as_mut().poll(&mut borrower_cx).is_pending());
    assert!(waiter_wakes.0.load(Ordering::Relaxed) > 0);
    assert!(lender_wakes.0.load(Ordering::Relaxed) > 0);
    assert_eq!(waiter.as_mut().poll(&mut waiter_cx), Poll::Ready(()));

    assert!(lender.as_mut().poll(&mut lender_cx).is_pending());
    let Poll::Ready(Ok(guard)) = borrower.as_mut().poll(&mut borrower_cx) else {
        panic!("borrower did not acquire the reference");
    };
    drop(guard);
    assert_eq!(lender.as_mut().poll(&mut lender_cx), Poll::Ready(()));
}

async fn start_lending(mutex: Arc<BorrowMutex<16, TestObject>>) {
    let mut test = TestObject;
    loop {
        mutex.wait_to_lend().await;
        mutex.lend(&mut test).unwrap().await;
    }
}

#[test]
fn borrow_basic_immediate_drop() {
    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());
    let t1_mutex = mutex.clone();

    {
        let mut normal_borrow = pin!(mutex.try_borrow());
        let _ = normal_borrow
            .as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()));
        let mut obj = TestObject;
        let immediate_lend_drop = mutex.lend(&mut obj).unwrap();
        drop(immediate_lend_drop);
    }

    let _t1 = std::thread::spawn(move || {
        futures::executor::block_on(async move {
            start_lending(t1_mutex).await;
        });
    });

    std::thread::sleep(Duration::from_millis(300));

    let t2 = async {
        let normal_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.try_borrow().fuse() => {
                Ok(())
            }
        };
        assert!(normal_borrow.is_ok());
        println!("normal_borrow ok");

        let immediate_drop = mutex.try_borrow();
        drop(immediate_drop);
        println!("immediate_drop ok");

        let normal_drop = mutex.try_borrow().await.unwrap();
        drop(normal_drop);
        println!("normal_drop ok");

        let normal_borrow = mutex.try_borrow().await.unwrap();
        let forever_pending_borrow_res = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            borrow = mutex.try_borrow().fuse() => {
                Ok(borrow)
            }
        };
        assert!(forever_pending_borrow_res.is_err());
        drop(normal_borrow);
        println!("forever_pending_borrow ok");

        let another_normal_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.try_borrow().fuse() => {
                Ok(())
            }
        };
        assert!(another_normal_borrow.is_ok());
        println!("another_normal_borrow ok");
    };

    futures::executor::block_on(t2);
}

#[test]
fn borrow_basic_double_lend() {
    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());

    let _t1 = {
        let mutex = mutex.clone();
        std::thread::spawn(move || {
            futures::executor::block_on(async move {
                start_lending(mutex).await;
            });
        })
    };

    let _t2 = {
        let mutex = mutex.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            futures::executor::block_on(async move {
                assert!(mutex.lend(&mut TestObject {}).is_none());
            });
        })
    };

    std::thread::sleep(Duration::from_millis(300));

    let t3 = async {
        let normal_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.try_borrow().fuse() => {
                Ok(())
            }
        };
        assert!(normal_borrow.is_ok());
        println!("normal_borrow ok");

        let another_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.try_borrow().fuse() => {
                Ok(())
            }
        };
        assert!(another_borrow.is_ok());
        println!("another_borrow ok");
    };

    futures::executor::block_on(t3);
}
