// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{
    sync::{
        mpsc::{self, *},
        OnceLock,
    },
    thread::ThreadId,
    time::Duration,
};

use futures::FutureExt;
use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

fn test_double_lend_abort() {
    // TODO fork and wait till the child terminates -> the above fn can set a local status field
    let mutex = BorrowMutex::<16, TestObject>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };
        loop {
            if test.counter >= 20 {
                break;
            }
            futures::select! {
                _ = Delay::new(Duration::from_millis(200)).fuse() => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    println!("t1: counter: {}", test.counter);
                }
                _ = mutex.wait_to_lend().fuse() => {
                    let l1 = mutex.lend(&mut test).unwrap();
                    let mut test2 = TestObject { counter: 1 };
                    let l2 = mutex.lend(&mut test2).unwrap();
                    l1.await;
                    l2.await;
                }
            }
        }

        mutex.terminate().await;
    };

    let t2 = async {
        while let Ok(mut test) = mutex.request_borrow().await {
            test.counter += 1;
            println!("t2: counter: {}", test.counter);
            drop(test);
            Delay::new(Duration::from_millis(100)).await;
        }
    };

    futures::executor::block_on(async {
        futures::join!(t1, t2);
    });
}

struct TestResult {
    id: ThreadId,
    aborted: bool,
}

static TEST_TX: OnceLock<Sender<TestResult>> = OnceLock::new();
fn abort_fn() -> ! {
    let tx = TEST_TX.get().unwrap();
    tx.send(TestResult {
        id: std::thread::current().id(),
        aborted: true,
    })
    .unwrap();
    loop {
        std::thread::park();
    }
}

fn test_case(f: fn()) -> ThreadId {
    let tid = std::thread::spawn(move || {
        f();

        let tx = TEST_TX.get().unwrap();
        tx.send(TestResult {
            id: std::thread::current().id(),
            aborted: false,
        })
        .unwrap();
    });

    tid.thread().id()
}

#[test]
fn abort_tests() {
    let mut failed = false;
    let (tx, rx): (Sender<TestResult>, Receiver<TestResult>) = mpsc::channel();
    TEST_TX.set(tx).unwrap();
    unsafe {
        borrow_mutex::set_abort_fn(abort_fn);
    }

    let mut outstanding_tests = vec![("test_double_lend_abort", test_case(test_double_lend_abort))];

    while !outstanding_tests.is_empty() {
        let res = rx.recv().unwrap();
        let pos = outstanding_tests.iter().position(|t| t.1 == res.id);
        let Some(pos) = pos else {
            continue;
        };
        let (test_name, _) = outstanding_tests.remove(pos);
        if res.aborted {
            println!("{test_name}: Aborted as expected");
        } else {
            println!("{test_name}: Did not abort! Failure");
            failed = true;
        }
    }

    if failed {
        std::process::exit(1);
    }
}
