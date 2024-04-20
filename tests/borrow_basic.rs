// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::sync::OnceLock;
use std::time::Duration;

use borrow_mutex::BorrowMutex;
use futures::{select, FutureExt};
use smol::Timer;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

static MUTEX: OnceLock<BorrowMutex<16, TestObject>> = OnceLock::new();

#[test]
fn borrow_single_threaded() {
    std::env::set_var("SMOL_THREADS", "2");

    MUTEX.set(BorrowMutex::new()).unwrap();

    let t1 = smol::spawn(async {
        let mutex = MUTEX.get().unwrap();
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
    });

    //let t2_mutex = mutex.clone();
    let t2 = smol::spawn(async {
        let mutex = MUTEX.get().unwrap();
        println!("t2_mutex: {mutex:?}");

        loop {
            let Some(test) = mutex.borrow_mut() else {
                break;
            };

            let test = test.await;
            test.counter += 1;
            println!("t2: counter: {}", test.counter);
            Timer::after(Duration::from_millis(200)).await;
        }
    });

    smol::block_on(async {
        t1.await;
        t2.await;
    });
}
