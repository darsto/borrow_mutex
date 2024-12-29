// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[test]
fn borrow_basic_double_thread() {
    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());

    let t1_mutex = mutex.clone();
    let t1 = std::thread::spawn(|| {
        futures::executor::block_on(async move {
            let mut test = TestObject { counter: 1 };

            eprintln!("t1 thread: {:?}", std::thread::current());

            loop {
                if test.counter >= 20 {
                    break;
                }
                futures::select! {
                    _ = Delay::new(Duration::from_millis(100)).fuse() => {
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

            t1_mutex.terminate().await.unwrap();
        })
    });

    let t2_mutex = mutex.clone();
    let t2 = std::thread::spawn(|| {
        futures::executor::block_on(async move {
            eprintln!("t2 thread: {:?}", std::thread::current());

            while let Ok(mut test) = t2_mutex.try_borrow().await {
                test.counter += 1;
                eprintln!("t2: counter: {}", test.counter);
                drop(test);
                Delay::new(Duration::from_millis(200)).await;
            }
        })
    });

    t1.join().unwrap();
    t2.join().unwrap();
}

#[test]
fn borrow_basic_multi_borrow() {
    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());

    let t1_mutex = mutex.clone();
    let t1 = std::thread::spawn(|| {
        futures::executor::block_on(async move {
            let mut test = TestObject { counter: 1 };

            eprintln!("t1 thread: {:?}", std::thread::current());

            loop {
                if test.counter >= 20 {
                    break;
                }
                futures::select! {
                    _ = Delay::new(Duration::from_millis(100)).fuse() => {
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

            t1_mutex.terminate().await.unwrap();
        })
    });

    for n in 0..3 {
        let n = n + 3;
        let t_mutex = mutex.clone();
        let t: std::thread::JoinHandle<()> = std::thread::spawn(move || {
            futures::executor::block_on(async move {
                eprintln!("t{n} thread: {:?}", std::thread::current());

                while let Ok(mut test) = t_mutex.try_borrow().await {
                    test.counter += 1;
                    eprintln!("t{n}: counter: {}", test.counter);
                    drop(test);
                    Delay::new(Duration::from_millis(50)).await;
                }
            })
        });
        t.join().unwrap();
    }

    t1.join().unwrap();
}
