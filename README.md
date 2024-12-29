[![crates.io](https://img.shields.io/crates/v/borrow_mutex)][crates.io]
[![libs.rs](https://img.shields.io/badge/libs.rs-borrow_mutex-orange)][libs.rs]
[![documentation](https://img.shields.io/docsrs/borrow_mutex)][documentation]
[![rust-version](https://img.shields.io/static/v1?label=Rust&message=1.65.0)][rust-version]
[![license](https://img.shields.io/crates/l/borrow_mutex)][license]

[crates.io]: https://crates.io/crates/borrow_mutex
[libs.rs]: https://lib.rs/crates/borrow_mutex
[documentation]: https://docs.rs/borrow_mutex
[rust-version]: https://www.rust-lang.org
[license]: https://github.com/darsto/borrow_mutex/blob/master/LICENSE

# BorrowMutex

**Early version!** We have nontrivial tests, including Miri, but bugs are still expected.

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

The most common use case is having a state handled entirely in its own
async context, but occasionally having to be accessed from the outside -
another async context.

Since the shared data doesn't have to be wrapped inside an [`Arc`],
it doesn't have to be allocated on the heap. In fact, BorrowMutex does not
perform any allocations whatsoever. The
[`tests/borrow_basic.rs`](https://github.com/darsto/borrow_mutex/blob/master/tests/borrow_basic.rs)
presents a simple example where *everything* is stored on the stack.

## Safety

The API is unsound when futures are forgotten ([`core::mem::forget()`]).
For convenience, none of the API is marked unsafe.

See [`BorrowMutex::lend`] for details.

Hopefully the unsound code could be prohibited in future rust versions
with additional compiler annotations.

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
    while let Ok(mut test) = mutex.try_borrow().await {
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

    mutex.terminate().await.unwrap();
};

futures::executor::block_on(async {
    futures::join!(f1, f2);
});
```

Both futures should print interchangeably. See `tests/borrow_basic.rs` for
a full working example.

# What if Drop is not called?

Unfortunately, **Undefined Behavior**. With [`core::mem::forget()`] or similar
called on [`LendGuard`] we can make the borrow checker believe the lended
`&mut T` is no longer used, while in fact, it is:

```rust,should_panic
# use borrow_mutex::BorrowMutex;
# use futures::Future;
# use futures::task::Context;
# use futures::task::Poll;
# use core::pin::pin;
struct TestStruct {
    counter: usize,
}
let mutex = BorrowMutex::<16, TestStruct>::new();
let mut test = TestStruct { counter: 1 };

let mut test_borrow = pin!(mutex.try_borrow());
let _ = test_borrow
    .as_mut()
    .poll(&mut Context::from_waker(&futures::task::noop_waker()));

let mut t1 = Box::pin(async {
    mutex.lend(&mut test).unwrap().await;
});

let _ = t1
    .as_mut()
    .poll(&mut Context::from_waker(&futures::task::noop_waker()));
std::mem::forget(t1);
// the compiler thinks `test` is no longer borrowed, but in fact it is

let Poll::Ready(Ok(mut test_borrow)) = test_borrow
    .as_mut()
    .poll(&mut Context::from_waker(&futures::task::noop_waker()))
else {
    panic!();
};

// now we get two mutable references, this is strictly UB
test_borrow.counter = 2;
test.counter = 6;
assert_eq!(test_borrow.counter, 2); // this fails
```

Similar Rust libraries make their API unsafe exactly because of this reason -
it's the caller's responsibility to not call [`core::mem::forget()`] or similar
([async-scoped](https://docs.rs/async-scoped/0.9.0/async_scoped/struct.Scope.html#method.scope))

However, this Undefined Behavior is really difficult to trigger in regular
code. It's hardly useful to call [`core::mem::forget()`] on a future.

[`Arc`]: std::sync::Arc
[`Mutex`]: std::sync::Mutex
