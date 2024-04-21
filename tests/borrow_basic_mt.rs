// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use smol::Timer;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[test]
fn borrow_basic_double_thread() {
    std::env::set_var("SMOL_THREADS", "2");

    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());

    let t1_mutex = mutex.clone();
    let t1 = smol::spawn(async move {
        let mut test = TestObject { counter: 1 };

        eprintln!("t1 thread: {:?}", std::thread::current());

        loop {
            if test.counter >= 20 {
                break;
            }

            let mut timer = Timer::after(Duration::from_millis(100)).fuse();
            let mut lender = t1_mutex.wait_for_borrow().fuse();

            futures::select! {
                _ = timer => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    eprintln!("t1: counter: {}", test.counter);
                }
                _ = lender => {
                    t1_mutex.lend(&mut test).unwrap().await;
                }
            }
        }

        t1_mutex.terminate();
        if let Some(lender) = t1_mutex.lend(&mut test) {
            lender.await;
        };
    });

    let t2_mutex = mutex.clone();
    let t2 = smol::spawn(async move {
        eprintln!("t2 thread: {:?}", std::thread::current());

        loop {
            let Some(test) = t2_mutex.borrow_mut() else {
                break;
            };

            {
                let mut test = test.await;
                test.counter += 1;
                eprintln!("t2: counter: {}", test.counter);
            }
            Timer::after(Duration::from_millis(200)).await;
        }
    });

    smol::block_on(async {
        t1.await;
        t2.await;
    });
}
