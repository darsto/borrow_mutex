// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{pin::pin, sync::Arc, task::Context, time::Duration};

use futures::{Future, FutureExt};
use futures_timer::Delay;

use borrow_mutex::BorrowMutex;

#[derive(Debug)]
struct TestObject;

async fn start_lending(mutex: Arc<BorrowMutex<16, TestObject>>) {
    let mut test = TestObject;
    loop {
        mutex.wait_to_lend().await;
        mutex.lend(&mut test).unwrap().await;
    }
}

#[test]
fn borrow_basic_immediate_drop() {
    let mutex = Arc::new(BorrowMutex::<16, TestObject>::new());
    let t1_mutex = mutex.clone();

    {
        let mut normal_borrow = pin!(mutex.borrow());
        let _ = normal_borrow
            .as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()));
        let mut obj = TestObject;
        let immediate_lend_drop = mutex.lend(&mut obj).unwrap();
        drop(immediate_lend_drop);
    }

    let _t1 = std::thread::spawn(move || {
        futures::executor::block_on(async move {
            start_lending(t1_mutex).await;
        });
    });

    std::thread::sleep(Duration::from_millis(300));

    let t2 = async {
        let normal_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.borrow().fuse() => {
                Ok(())
            }
        };
        assert!(normal_borrow.is_ok());
        println!("normal_borrow ok");

        let immediate_drop = mutex.borrow();
        drop(immediate_drop);
        println!("immediate_drop ok");

        let normal_drop = mutex.borrow().await.unwrap();
        drop(normal_drop);
        println!("normal_drop ok");

        let normal_borrow = mutex.borrow().await.unwrap();
        let forever_pending_borrow_res = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            borrow = mutex.borrow().fuse() => {
                Ok(borrow)
            }
        };
        assert!(forever_pending_borrow_res.is_err());
        drop(normal_borrow);
        println!("forever_pending_borrow ok");

        let another_normal_borrow = futures::select! {
            _ = Delay::new(Duration::from_millis(300)).fuse() => {
                Err(())
            }
            _ = mutex.borrow().fuse() => {
                Ok(())
            }
        };
        assert!(another_normal_borrow.is_ok());
        println!("another_normal_borrow ok");
    };

    futures::executor::block_on(t2);
}
