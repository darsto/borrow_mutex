// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::sync::Arc;

use borrow_mutex::BorrowMutex;
use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use futures::FutureExt;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Barrier;
use tokio::task::JoinHandle;

const OPERATIONS_PER_USER: u64 = 1_000;
const MAX_BORROWERS: usize = 64;
const USERS: [usize; 3] = [2, 8, 32];

fn benchmarks(criterion: &mut Criterion) {
    let runtime = Builder::new_current_thread().build().unwrap();

    benchmark_uncontended(criterion, &runtime);
    benchmark_contended(criterion, &runtime);
}

fn benchmark_uncontended(criterion: &mut Criterion, runtime: &Runtime) {
    let mut group = criterion.benchmark_group("uncontended");
    group.throughput(Throughput::Elements(OPERATIONS_PER_USER));

    group.bench_function("BorrowMutex", |bencher| {
        bencher.iter_batched(
            || setup_uncontended_borrow_mutex(runtime),
            |run| black_box(runtime.block_on(finish_uncontended_borrow_mutex(run))),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("smol::lock::Mutex", |bencher| {
        bencher.iter_batched(
            || setup_uncontended_smol_mutex(runtime),
            |run| black_box(runtime.block_on(finish_uncontended_smol_mutex(run))),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("tokio::sync::Mutex", |bencher| {
        bencher.iter_batched(
            || setup_uncontended_tokio_mutex(runtime),
            |run| black_box(runtime.block_on(finish_uncontended_tokio_mutex(run))),
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

fn benchmark_contended(criterion: &mut Criterion, runtime: &Runtime) {
    let mut group = criterion.benchmark_group("contended");
    for users in USERS {
        group.throughput(Throughput::Elements(OPERATIONS_PER_USER * users as u64));
        group.bench_with_input(
            BenchmarkId::new("BorrowMutex", users),
            &users,
            |bencher, &users| {
                bencher.iter_batched(
                    || setup_contended_borrow_mutex(runtime, users),
                    |run| black_box(runtime.block_on(finish_contended_borrow_mutex(run))),
                    BatchSize::PerIteration,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("smol::lock::Mutex", users),
            &users,
            |bencher, &users| {
                bencher.iter_batched(
                    || setup_contended_smol_mutex(runtime, users),
                    |run| black_box(runtime.block_on(finish_smol_mutex(run))),
                    BatchSize::PerIteration,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("tokio::sync::Mutex", users),
            &users,
            |bencher, &users| {
                bencher.iter_batched(
                    || setup_contended_tokio_mutex(runtime, users),
                    |run| black_box(runtime.block_on(finish_tokio_mutex(run))),
                    BatchSize::PerIteration,
                )
            },
        );
    }
    group.finish();
}

struct UncontendedBorrowRun {
    mutex: Arc<BorrowMutex<MAX_BORROWERS, u64>>,
    start: Arc<Barrier>,
    worker: JoinHandle<u64>,
}

fn setup_uncontended_borrow_mutex(runtime: &Runtime) -> UncontendedBorrowRun {
    let mutex = Arc::new(BorrowMutex::<MAX_BORROWERS, u64>::new());
    let ready = Arc::new(Barrier::new(2));
    let start = Arc::new(Barrier::new(2));
    let worker_mutex = mutex.clone();
    let worker_ready = ready.clone();
    let worker_start = start.clone();
    let worker = runtime.spawn(async move {
        let mut value = 0_u64;
        worker_ready.wait().await;
        worker_start.wait().await;
        for _ in 0..OPERATIONS_PER_USER {
            if worker_mutex.wait_to_lend().now_or_never().is_none() {
                value += 1;
            }
        }
        value
    });

    runtime.block_on(ready.wait());
    UncontendedBorrowRun {
        mutex,
        start,
        worker,
    }
}

async fn finish_uncontended_borrow_mutex(
    run: UncontendedBorrowRun,
) -> (u64, Arc<BorrowMutex<MAX_BORROWERS, u64>>) {
    run.start.wait().await;
    let value = run.worker.await.unwrap();
    assert_eq!(value, OPERATIONS_PER_USER);
    (value, run.mutex)
}

struct UncontendedSmolRun {
    mutex: Arc<smol::lock::Mutex<u64>>,
    start: Arc<Barrier>,
    worker: JoinHandle<()>,
}

fn setup_uncontended_smol_mutex(runtime: &Runtime) -> UncontendedSmolRun {
    let mutex = Arc::new(smol::lock::Mutex::new(0_u64));
    let ready = Arc::new(Barrier::new(2));
    let start = Arc::new(Barrier::new(2));
    let worker_mutex = mutex.clone();
    let worker_ready = ready.clone();
    let worker_start = start.clone();
    let worker = runtime.spawn(async move {
        worker_ready.wait().await;
        worker_start.wait().await;
        for _ in 0..OPERATIONS_PER_USER {
            *worker_mutex.lock().await += 1;
        }
    });

    runtime.block_on(ready.wait());
    UncontendedSmolRun {
        mutex,
        start,
        worker,
    }
}

async fn finish_uncontended_smol_mutex(run: UncontendedSmolRun) -> Arc<smol::lock::Mutex<u64>> {
    run.start.wait().await;
    run.worker.await.unwrap();
    run.mutex
}

struct UncontendedTokioRun {
    mutex: Arc<tokio::sync::Mutex<u64>>,
    start: Arc<Barrier>,
    worker: JoinHandle<()>,
}

fn setup_uncontended_tokio_mutex(runtime: &Runtime) -> UncontendedTokioRun {
    let mutex = Arc::new(tokio::sync::Mutex::new(0_u64));
    let ready = Arc::new(Barrier::new(2));
    let start = Arc::new(Barrier::new(2));
    let worker_mutex = mutex.clone();
    let worker_ready = ready.clone();
    let worker_start = start.clone();
    let worker = runtime.spawn(async move {
        worker_ready.wait().await;
        worker_start.wait().await;
        for _ in 0..OPERATIONS_PER_USER {
            *worker_mutex.lock().await += 1;
        }
    });

    runtime.block_on(ready.wait());
    UncontendedTokioRun {
        mutex,
        start,
        worker,
    }
}

async fn finish_uncontended_tokio_mutex(run: UncontendedTokioRun) -> Arc<tokio::sync::Mutex<u64>> {
    run.start.wait().await;
    run.worker.await.unwrap();
    run.mutex
}

struct ContendedBorrowRun {
    mutex: Arc<BorrowMutex<MAX_BORROWERS, u64>>,
    lender_start: Arc<Barrier>,
    borrowers: Vec<JoinHandle<()>>,
    lender: JoinHandle<u64>,
    expected: u64,
}

fn setup_contended_borrow_mutex(runtime: &Runtime, users: usize) -> ContendedBorrowRun {
    assert!((2..=MAX_BORROWERS).contains(&users));
    let borrower_count = users - 1;
    let mutex = Arc::new(BorrowMutex::<MAX_BORROWERS, u64>::new());
    let ready = Arc::new(Barrier::new(users + 1));
    let lender_start = Arc::new(Barrier::new(2));
    let borrowers_start = Arc::new(Barrier::new(users));
    let expected = OPERATIONS_PER_USER * users as u64;

    let lender_mutex = mutex.clone();
    let lender_ready = ready.clone();
    let task_lender_start = lender_start.clone();
    let lender_borrowers_start = borrowers_start.clone();
    let lender = runtime.spawn(async move {
        let mut value = 0_u64;
        lender_ready.wait().await;
        task_lender_start.wait().await;
        for round in 0..OPERATIONS_PER_USER {
            // The lender is the first owner, equivalent to the first task
            // acquiring a conventional unlocked mutex.
            value += 1;
            if round == 0 {
                lender_borrowers_start.wait().await;
            }
            tokio::task::yield_now().await;
            for _ in 0..borrower_count {
                lender_mutex.lend(&mut value).unwrap().await;
            }
        }
        value
    });

    let mut borrowers = Vec::with_capacity(borrower_count);
    for _ in 0..borrower_count {
        let mutex = mutex.clone();
        let ready = ready.clone();
        let borrowers_start = borrowers_start.clone();
        borrowers.push(runtime.spawn(async move {
            ready.wait().await;
            borrowers_start.wait().await;
            for _ in 0..OPERATIONS_PER_USER {
                let mut guard = mutex.try_borrow().await.unwrap();
                *guard += 1;
                tokio::task::yield_now().await;
            }
        }));
    }

    runtime.block_on(ready.wait());
    ContendedBorrowRun {
        mutex,
        lender_start,
        borrowers,
        lender,
        expected,
    }
}

async fn finish_contended_borrow_mutex(
    run: ContendedBorrowRun,
) -> (u64, Arc<BorrowMutex<MAX_BORROWERS, u64>>) {
    run.lender_start.wait().await;
    for borrower in run.borrowers {
        borrower.await.unwrap();
    }
    let value = run.lender.await.unwrap();
    assert_eq!(value, run.expected);
    (value, run.mutex)
}

struct SmolRun {
    mutex: Arc<smol::lock::Mutex<u64>>,
    leader_start: Arc<Barrier>,
    workers: Vec<JoinHandle<()>>,
}

fn setup_contended_smol_mutex(runtime: &Runtime, users: usize) -> SmolRun {
    let mutex = Arc::new(smol::lock::Mutex::new(0_u64));
    let ready = Arc::new(Barrier::new(users + 1));
    let leader_start = Arc::new(Barrier::new(2));
    let contenders_start = Arc::new(Barrier::new(users));
    let mut tasks = Vec::with_capacity(users);
    for user in 0..users {
        let mutex = mutex.clone();
        let ready = ready.clone();
        let leader_start = leader_start.clone();
        let contenders_start = contenders_start.clone();
        tasks.push(runtime.spawn(async move {
            ready.wait().await;
            if user == 0 {
                leader_start.wait().await;
                let mut guard = mutex.lock().await;
                *guard += 1;
                contenders_start.wait().await;
                tokio::task::yield_now().await;
                drop(guard);
                for _ in 1..OPERATIONS_PER_USER {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    tokio::task::yield_now().await;
                }
            } else {
                contenders_start.wait().await;
                for _ in 0..OPERATIONS_PER_USER {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    runtime.block_on(ready.wait());
    SmolRun {
        mutex,
        leader_start,
        workers: tasks,
    }
}

async fn finish_smol_mutex(run: SmolRun) -> Arc<smol::lock::Mutex<u64>> {
    run.leader_start.wait().await;
    for task in run.workers {
        task.await.unwrap();
    }
    run.mutex
}

struct TokioRun {
    mutex: Arc<tokio::sync::Mutex<u64>>,
    leader_start: Arc<Barrier>,
    workers: Vec<JoinHandle<()>>,
}

fn setup_contended_tokio_mutex(runtime: &Runtime, users: usize) -> TokioRun {
    let mutex = Arc::new(tokio::sync::Mutex::new(0_u64));
    let ready = Arc::new(Barrier::new(users + 1));
    let leader_start = Arc::new(Barrier::new(2));
    let contenders_start = Arc::new(Barrier::new(users));
    let mut tasks = Vec::with_capacity(users);
    for user in 0..users {
        let mutex = mutex.clone();
        let ready = ready.clone();
        let leader_start = leader_start.clone();
        let contenders_start = contenders_start.clone();
        tasks.push(runtime.spawn(async move {
            ready.wait().await;
            if user == 0 {
                leader_start.wait().await;
                let mut guard = mutex.lock().await;
                *guard += 1;
                contenders_start.wait().await;
                tokio::task::yield_now().await;
                drop(guard);
                for _ in 1..OPERATIONS_PER_USER {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    tokio::task::yield_now().await;
                }
            } else {
                contenders_start.wait().await;
                for _ in 0..OPERATIONS_PER_USER {
                    let mut guard = mutex.lock().await;
                    *guard += 1;
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    runtime.block_on(ready.wait());
    TokioRun {
        mutex,
        leader_start,
        workers: tasks,
    }
}

async fn finish_tokio_mutex(run: TokioRun) -> Arc<tokio::sync::Mutex<u64>> {
    run.leader_start.wait().await;
    for task in run.workers {
        task.await.unwrap();
    }
    run.mutex
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
