# BorrowMutex

**Very initial version!** Use with caution

Async Mutex which does not require wrapping the target structure.
Instead a &mut T can be lended to the mutex at any given timeslice.

This lets any other side borrow this &mut T. The data is borrow-able only
while the lender awaits, and the lending side can await until someone wants
to borrow. The semantics enforce at most one side has a mutable reference
at any given time.

This lets us share any mutable object between distinct async contexts
without Arc<Mutex> over the object in question and without relying on any
kind of internal mutability. It's mostly aimed at single-threaded executors
where internal mutability is an unnecessary complication. Nevertheless,
the Mutex is Send+Sync and can be safely used from any number of threads.

Since the shared data doesn't have to be wrapped inside an Arc<Mutex>, it
doesn't have to be allocated on the heap. In fact, BorrowMutex does not
perform any allocations whatsoever. The `tests/borrow_basic.rs` presents a
simple example where *everything* is stored on the stack.

The API is fully safe and doesn't cause UB under any circumstances, but
it's not able to enforce all the semantics at compile time. I.e. if a
lending side of a transaction drops the lending Future before it's
resolved (before the borrowing side stops using it), the process will
immediately abort (...after printing an error message).

## Example

```rust
use borrow_mutex::BorrowMutex;
use futures::FutureExt;

struct TestObject {
    counter: usize,
}

let mutex = BorrowMutex::<16, TestObject>::new();

let f1 = async {
    // try to borrow, await, and repeat until we get an Err.
    // The Err can be either:
    // - the mutex has too many concurrent borrowers (in this example we
    //   have just 1, and the max was 16)
    // - the mutex was terminated - i.e. because the lending side knows it
    //   won't lend anymore
    // We eventually expect the latter here
    while let Ok(mut test) = mutex.request_borrow().await {
        test.counter += 1; // mutate the object!
        println!("f1: counter: {}", test.counter);
        drop(test);
        // `test` is dropped, and so the mutex.lend().await on the
        // other side returns and can use the object freely again.
        // we'll request another borrow in 100ms
        smol::Timer::after(std::time::Duration::from_millis(100)).await;
    }
};

let f2 = async {
    let mut test = TestObject { counter: 1 };
    // local object we'll be sharing

    loop {
        if test.counter >= 20 {
            break;
        }
        let mut timer = smol::Timer::after(std::time::Duration::from_millis(200)).fuse();
        // either sleep 200ms or lend if needed in the meantime
        futures::select! {
            _ = timer => {
                if test.counter < 10 {
                    test.counter += 1;
                }
                println!("f2: counter: {}", test.counter);
            }
            _ = mutex.wait_to_lend().fuse() => {
                // there's someone waiting to borrow, lend
                mutex.lend(&mut test).unwrap().await
            }
        }
    }

    mutex.terminate().await;
};

futures::executor::block_on(async {
    futures::join!(f1, f2);
});
```

Both futures should print interchangeably. See `tests/borrow_basic.rs` for a full
working example.
