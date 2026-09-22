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

[`BorrowMutex`] is an async Mutex which does not require wrapping the target
structure. Instead, a `&mut T` can be lended to the mutex at any time.

This lets any other side borrow the `&mut T`. The mutable ref is borrow-able
only while the lender awaits, and the lending side can await until someone
wants to borrow. The semantics enforce at most one side has a mutable reference
at any given time.

This lets us share any mutable object between distinct async contexts
without [`Arc`]<[`Mutex`]> over the object in question and without relying
on any kind of internal mutability.

The most common use case is having a state handled entirely in its own
async context, but occasionally having to be accessed from the outside -
another async context.

Since the shared data doesn't have to be wrapped inside an [`Arc`],
it doesn't have to be allocated on the heap. In fact, BorrowMutex does not
perform any allocations whatsoever. The
[`tests/borrow_basic.rs`](https://github.com/darsto/borrow_mutex/blob/master/tests/borrow_basic.rs)
presents a simple example where *everything* is stored on the stack.

## Example

```rust,ignore
let mutex = BorrowMutex::<16, i32>::new();

let lender = async {
    let mut value = 0;
    mutex.wait_to_lend().await;
    mutex.lend(&mut value).unwrap().await;
};

let borrower = async {
    *mutex.try_borrow().await.unwrap() += 1;
};
```

A more realistic example would be to futures::select! on the lender side
on mutex.wait_to_lend() and some conn.incoming_message().

### Benchmarks

Measured locally with:

```console
cargo bench --bench mutex -- --sample-size 10 --warm-up-time 1 --measurement-time 1
```

Criterion median time per batch; lower is better. Each user performs 1,000
operations per batch. The uncontended BorrowMutex workload polls
`wait_to_lend().now_or_never()` without a borrower; the other mutexes lock and
update a value. The APIs and synchronization mechanisms are not identical, so
compare the times with that distinction in mind.

| Workload | BorrowMutex | smol::lock::Mutex | tokio::sync::Mutex |
| --- | ---: | ---: | ---: |
| Uncontended (1 user) | 5.329 µs | 6.091 µs | 21.683 µs |
| Contended (2 users) | 217.25 µs | 197.68 µs | 219.33 µs |
| Contended (8 users) | 1.2501 ms | 1.1821 ms | 0.90964 ms |
| Contended (32 users) | 5.3907 ms | 4.7255 ms | 3.5714 ms |

Uncontended time per 1,000 operations (one `█` ≈ 1 µs; shorter is faster):

```text
BorrowMutex          █████                           5.33 µs
smol::lock::Mutex    ██████                          6.09 µs
tokio::sync::Mutex   ██████████████████████          21.68 µs
```

The barrier and repeated-lock workload is adapted from the methodology of
[ytakano/async_bench](https://github.com/ytakano/async_bench). The contention
interpretation was also informed by
[khonsulabs/async-locking-benchmarks](https://github.com/khonsulabs/async-locking-benchmarks).

Both futures should print interchangeably. See `tests/borrow_basic.rs` for
a full working example.

[`Arc`]: std::sync::Arc
[`Mutex`]: std::sync::Mutex

## Safety

Calling std::mem::forget() on a [`LendGuard`] may lead to undefined behavior.
Dropping it while the value is borrowed is protected by aborting the process.
See [`BorrowMutex::lend`] for the full contract.
