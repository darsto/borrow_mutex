// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::time::Duration;

use borrow_mutex::BorrowMutex;
use futures::{join, select, FutureExt};
use smol::Timer;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[test]
fn borrow_single_threaded() {
    std::env::set_var("SMOL_THREADS", "2");

    let mutex = BorrowMutex::<16, TestObject>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };

        loop {
            if test.counter >= 20 {
                break;
            }

            let mut timer = Timer::after(Duration::from_millis(200)).fuse();
            let mut lender = mutex.wait_for_borrow().fuse();

            select! {
                _ = timer => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    println!("t1: counter: {}", test.counter);
                }
                _ = lender => {
                    mutex.lend(&mut test).unwrap().await
                }
            }
        }

        mutex.terminate();
        if let Some(lender) = mutex.lend(&mut test) {
            lender.await;
        };
    };

    let t2 = async {
        loop {
            let Some(test) = mutex.borrow_mut() else {
                break;
            };

            let test = test.await;
            test.counter += 1;
            println!("t2: counter: {}", test.counter);
            Timer::after(Duration::from_millis(200)).await;
        }
    };

    futures::executor::block_on(async {
        join!(t1, t2);
    });
}
