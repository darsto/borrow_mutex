// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{any::Any, time::Duration};

use futures::FutureExt;
use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject {
    counter: usize,
}

#[derive(Debug)]
#[repr(C)]
struct AnotherTestObject {
    is_even: bool,
    another_counter: usize,
}

#[test]
fn borrow_dyn_single_thread() {
    let mutex = BorrowMutex::<16, dyn Any>::new();

    let t1 = async {
        let mut test = TestObject { counter: 1 };
        loop {
            if test.counter >= 20 {
                break;
            }
            futures::select! {
                _ = Delay::new(Duration::from_millis(100)).fuse() => {
                    if test.counter < 10 {
                        test.counter += 1;
                    }
                    println!("t1: counter: {}", test.counter);
                }
                _ = mutex.wait_to_lend().fuse() => {
                    mutex.lend(&mut test as &mut dyn Any).unwrap().await
                }
            }
        }
    };

    let t2 = async {
        let mut test = AnotherTestObject {
            is_even: false,
            another_counter: 1,
        };
        loop {
            if test.another_counter >= 20 {
                break;
            }
            futures::select! {
                _ = Delay::new(Duration::from_millis(100)).fuse() => {
                    if test.another_counter < 10 {
                        test.another_counter += 1;
                        test.is_even = !test.is_even;
                    }
                    println!("t2: another_counter: {}", test.another_counter);
                    assert_eq!(test.is_even, test.another_counter % 2 == 0);
                }
                _ = mutex.wait_to_lend().fuse() => {
                    mutex.lend(&mut test as &mut dyn Any).unwrap().await
                }
            }
        }
    };

    let tborrower = async {
        while let Ok(mut any) = mutex.request_borrow().await {
            if let Some(test) = any.downcast_mut::<TestObject>() {
                test.counter += 1;
                println!("t3: counter: {}", test.counter);
            } else if let Some(test) = any.downcast_mut::<AnotherTestObject>() {
                test.another_counter += 1;
                test.is_even = !test.is_even;
                println!("t3: another_counter: {}", test.another_counter);
                assert_eq!(test.is_even, test.another_counter % 2 == 0);
            }
            drop(any);
            Delay::new(Duration::from_millis(200)).await;
        }
    };

    futures::executor::block_on(async {
        futures::join!(tborrower, async {
            t1.await;
            t2.await;
            mutex.terminate().await;
        });
    });
}
