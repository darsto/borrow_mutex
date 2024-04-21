// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::time::Duration;

use futures::FutureExt;
use smol::Timer;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[test]
fn borrow_basic_single_thread() {
    let mutex = BorrowMutex::<16, TestObject>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };
        loop {
            if test.counter >= 20 {
                break;
            }
            let mut timer = Timer::after(Duration::from_millis(200)).fuse();
            futures::select! {
                _ = timer => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    println!("t1: counter: {}", test.counter);
                }
                _ = mutex.wait_to_lend().fuse() => {
                    mutex.lend(&mut test).unwrap().await
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
            Timer::after(Duration::from_millis(200)).await;
        }
    };

    futures::executor::block_on(async {
        futures::join!(t1, t2);
    });
}
