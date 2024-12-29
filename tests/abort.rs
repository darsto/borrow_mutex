// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{
    future::Future,
    sync::{
        mpsc::{self, *},
        OnceLock,
    },
    task::{Context, Poll},
    time::Duration,
};

use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

// wait for a borrower, lend, then drop the lend guard prematurely
fn tc_lend_guard_drop() {
    let mutex = BorrowMutex::<16, TestObject>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };
        mutex.wait_to_lend().await;
        let mut lend = core::pin::pin!(mutex.lend(&mut test).unwrap());
        let poll = lend
            .as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()));
        assert_eq!(poll, Poll::Pending);
        // drop and cause an abort
    };

    let t2 = async {
        Delay::new(Duration::from_millis(100)).await;
        if let Ok(mut test) = mutex.try_borrow().await {
            test.counter += 1;
        }
    };

    futures::executor::block_on(async {
        futures::join!(t1, t2);
    });
}

// lend without any borrower waiting, get a borrower, then drop the lend guard prematurely
fn tc_early_lend_guard_drop() {
    let mutex = BorrowMutex::<16, TestObject>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };
        // lend without any borrower
        let mut lend = core::pin::pin!(mutex.lend(&mut test).unwrap());
        let poll = lend
            .as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()));
        assert_eq!(poll, Poll::Pending);
        Delay::new(Duration::from_millis(200)).await;
        let poll = lend
            .as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()));
        assert_eq!(poll, Poll::Pending);
        // drop and cause an abort
    };

    let t2 = async {
        Delay::new(Duration::from_millis(100)).await;
        if let Ok(mut test) = mutex.try_borrow().await {
            Delay::new(Duration::from_millis(100)).await;
            test.counter += 1;
        }
    };

    futures::executor::block_on(async {
        futures::join!(t1, t2);
    });
}

struct TestResult {
    aborted: bool,
}

static TEST_TX: OnceLock<Sender<TestResult>> = OnceLock::new();
fn abort_fn() -> ! {
    let tx = TEST_TX.get().unwrap();
    tx.send(TestResult { aborted: true }).unwrap();
    loop {
        std::thread::park();
    }
}

fn run_test_case(test_rx: &mut Receiver<TestResult>, f: fn()) {
    std::thread::spawn(move || {
        f();

        let tx = TEST_TX.get().unwrap();
        tx.send(TestResult { aborted: false }).unwrap();
    });

    let res = test_rx.recv().unwrap();
    assert!(res.aborted);
}

#[test]
fn test_lend_guard_drop() {
    let (tx, mut rx): (Sender<TestResult>, Receiver<TestResult>) = mpsc::channel();
    TEST_TX.set(tx).unwrap();
    unsafe {
        borrow_mutex::set_abort_fn(abort_fn);
    }

    run_test_case(&mut rx, tc_lend_guard_drop);
    run_test_case(&mut rx, tc_early_lend_guard_drop);
}
