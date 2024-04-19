// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use borrow_mutex::BorrowMutex;
use futures::{select, FutureExt};
use smol::Timer;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[test]
fn borrow_single_threaded() {
    std::env::set_var("SMOL_THREADS", "1");

    let mutex = Arc::new(BorrowMutex::<TestObject>::new());

    let t1_mutex = mutex.clone();
    let t1 = smol::spawn(async move {
        let mut test = TestObject { counter: 1 };

        println!("t1_mutex: {t1_mutex:?}");

        loop {
            if test.counter >= 20 {
                break;
            }

            let mut timer = Timer::after(Duration::from_millis(200)).fuse();
            let mut lender = t1_mutex.wait_for_borrow().fuse();

            select! {
                _ = timer => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    println!("t1: counter: {}", test.counter);
                }
                _ = lender => {
                    t1_mutex.lend(&mut test).unwrap().await
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
        println!("t2_mutex: {t2_mutex:?}");

        loop {
            let Some(test) = t2_mutex.borrow_mut() else {
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
