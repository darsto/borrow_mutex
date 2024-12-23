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
fn borrow_basic_double_thread_smol() {
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
            futures::select! {
                _ = Timer::after(Duration::from_millis(100)).fuse() => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    eprintln!("t1: counter: {}", test.counter);
                }
                _ = t1_mutex.wait_to_lend().fuse() => {
                    t1_mutex.lend(&mut test).unwrap().await;
                }
            }
        }

        t1_mutex.terminate().await;
    });

    let t2_mutex = mutex.clone();
    let t2 = smol::spawn(async move {
        eprintln!("t2 thread: {:?}", std::thread::current());

        while let Ok(mut test) = t2_mutex.borrow().await {
            test.counter += 1;
            eprintln!("t2: counter: {}", test.counter);
            drop(test);
            Timer::after(Duration::from_millis(200)).await;
        }
    });

    smol::block_on(async {
        t1.await;
        t2.await;
    });
}
