// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{any::Any, sync::Arc, time::Duration};

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
        while let Ok(mut any) = mutex.borrow().await {
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

#[test]
fn borrow_dyn_multi_thread() {
    let mutex = Arc::new(BorrowMutex::<16, dyn Any + Send>::new());

    let f1_mutex = mutex.clone();
    let f1 = move || {
        futures::executor::block_on(async move {
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
                    _ = f1_mutex.wait_to_lend().fuse() => {
                        f1_mutex.lend(&mut test as &mut (dyn Any + Send)).unwrap().await
                    }
                }
            }
        })
    };

    let f2_mutex = mutex.clone();
    let f2 = move || {
        futures::executor::block_on(async move {
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
                    _ = f2_mutex.wait_to_lend().fuse() => {
                        f2_mutex.lend(&mut test as &mut (dyn Any + Send)).unwrap().await
                    }
                }
            }
        })
    };

    let tborrower_mutex = mutex.clone();
    let tborrower = std::thread::spawn(|| {
        futures::executor::block_on(async move {
            while let Ok(mut any) = tborrower_mutex.borrow().await {
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
        })
    });

    std::thread::spawn(f1).join().unwrap();
    std::thread::spawn(f2).join().unwrap();
    futures::executor::block_on(async move {
        mutex.terminate().await;
    });
    tborrower.join().unwrap();
}

#[test]
fn borrow_dyn_multi_thread_multi_borrow() {
    let mutex = Arc::new(BorrowMutex::<16, dyn Any + Send>::new());

    let f1_mutex = mutex.clone();
    let f1 = move || {
        futures::executor::block_on(async move {
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
                    _ = f1_mutex.wait_to_lend().fuse() => {
                        f1_mutex.lend(&mut test as &mut (dyn Any + Send)).unwrap().await
                    }
                }
            }
        })
    };

    let f2_mutex = mutex.clone();
    let f2 = move || {
        futures::executor::block_on(async move {
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
                    _ = f2_mutex.wait_to_lend().fuse() => {
                        f2_mutex.lend(&mut test as &mut (dyn Any + Send)).unwrap().await
                    }
                }
            }
        })
    };

    let mut tborrowers = Vec::new();
    for n in 0..3 {
        let n = n + 3;
        let tborrower_mutex = mutex.clone();
        let tborrower = std::thread::spawn(move || {
            futures::executor::block_on(async move {
                while let Ok(mut any) = tborrower_mutex.borrow().await {
                    if let Some(test) = any.downcast_mut::<TestObject>() {
                        test.counter += 1;
                        println!("t{n}: counter: {}", test.counter);
                    } else if let Some(test) = any.downcast_mut::<AnotherTestObject>() {
                        test.another_counter += 1;
                        test.is_even = !test.is_even;
                        println!("t{n}: another_counter: {}", test.another_counter);
                        assert_eq!(test.is_even, test.another_counter % 2 == 0);
                    }
                    drop(any);
                    Delay::new(Duration::from_millis(200)).await;
                }
            })
        });
        tborrowers.push(tborrower);
    }

    std::thread::spawn(f1).join().unwrap();
    std::thread::spawn(f2).join().unwrap();
    futures::executor::block_on(async move {
        mutex.terminate().await;
    });
    for t in tborrowers {
        t.join().unwrap();
    }
}
