# BorrowMutex

**Very initial version!** Use with caution

[`BorrowMutex`] is an async Mutex which does not require wrapping the target
structure. Instead, a `&mut T` can be lended to the mutex at any given time.

This lets any other side borrow the `&mut T`. The mutable ref is borrow-able
only while the lender awaits, and the lending side can await until someone
wants to borrow. The semantics enforce at most one side has a mutable reference
at any given time.

This lets us share any mutable object between distinct async contexts
without [`Arc`]<[`Mutex`]> over the object in question and without relying
on any kind of internal mutability. It's mostly aimed at single-threaded
executors where internal mutability is an unnecessary complication.
Still, the [`BorrowMutex`] is Send+Sync and can be safely used from
any number of threads.

Since the shared data doesn't have to be wrapped inside an [`Arc`],
it doesn't have to be allocated on the heap. In fact, BorrowMutex does not
perform any allocations whatsoever. The
[`tests/borrow_basic.rs`](https://github.com/darsto/borrow_mutex/blob/master/tests/borrow_basic.rs)
presents a simple example where *everything* is stored on the stack.

The API is fully safe and doesn't cause UB under any circumstances, but
it's not able to enforce all the semantics at compile time. I.e. if a
lending side of a transaction drops the lending Future before it's
resolved (before the borrowing side stops using it), the process will
immediately abort (and print an error message).

The mutex is also sound with any [`core::mem::forget()`].

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
        // either sleep 200ms or lend if needed in the meantime
        futures::select! {
            _ = smol::Timer::after(std::time::Duration::from_millis(200)).fuse() => {
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

Both futures should print interchangeably. See `tests/borrow_basic.rs` for
a full working example.

# What if Drop is not called?

**Is BorrowMutex really sound, under all conditions?**
It should be. I was not able to trigger any undefined behavior with
safe code.

The object returned from [`BorrowMutex::lend()`] (that is a [`LendGuard`])
has a Drop impl that must be called before the &mut value can be usable again.
If the reference is still borrowed on the borrower side, the program immediately
aborts (panic is not sufficient). But what about [`core::mem::forget()`]? The
Guard could be technically forgotten, and the &mut reference could be reused on
the lender side while it's still used on the borrowstd::sync::er side. That
would be unsound, and cause immediate undefined behavior. Similar Rust libraries
make their API unsafe exactly because of this reason - it's the caller's
responsibility to not call [`core::mem::forget()`] or similar
([async-scoped](https://docs.rs/async-scoped/0.9.0/async_scoped/struct.Scope.html#method.scope))

[`BorrowMutex`] doesn't have any unsafe APIs. [`core::mem::forget()`] can be
called on the [`LendGuard`] and is perfectly sound. That's because
the borrower doesn't obtain the &mut reference until the [`LendGuard`]
is polled. We have two scenarios:
- With [`LendGuard`] dropped without ever polling it, the [`BorrowMutex`] is
hardly usable and will abort on the next [`BorrowMutex::lend()`] call
(multiple lended values), but no undefined behavior can be observed.
- To poll the Guard once and drop it later it needs to be manually pinned first.
This can be implicitly via .await (which also polls to completion/cancellation,
so is out of this consideration) or explicitly pinned with [`core::pin::pin!()`].
The [`core::pin::Pin<&mut LendGuard>`] can be forgotten this way - but this
still Drops the original LendGuard and there's no way to prevent that with
only safe code. The [`LendGuard`] is !Unpin exactly for this reason.

To expand on that second case, see a similar discussion at
<https://github.com/imxrt-rs/imxrt-hal/issues/137> and a
[code snippet linked inside](https://play.rust-lang.org/?version=stable&mode=debug&edition=2021&gist=79e34e7c3e968f8f6680a7cd08d1ffc4)

[`Arc`]: std::sync::Arc
[`Mutex`]: std::sync::Mutex
