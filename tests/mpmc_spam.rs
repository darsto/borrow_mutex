// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use borrow_mutex::mpmc::MPMC;

#[derive(Debug)]
struct SpamObject {
    push_counter: AtomicUsize,
    pop_counter: AtomicUsize,
}

#[derive(Debug)]
struct ThreadStats {
    push_counter: usize,
    pop_counter: usize,
    dropped_push_counter: usize,
    dropped_pop_counter: usize,
}

/// multiple threads doing concurrent pushes and pops for a fixed number
/// of iterations. Afterwards the MPMC is checked for validity
#[test]
fn mpmc_spam() {
    let mpmc = MPMC::<128, SpamObject>::new();

    assert!(mpmc.is_empty());
    assert!(!mpmc.is_full());
    assert_eq!(mpmc.len(), 0);
    assert_eq!(mpmc.capacity(), 128 - 1);

    let num_threads = 8;
    let num_iters = 65539;

    let stats = std::thread::scope(|s| {
        let thread_stat_handles = (0..num_threads)
            .map(|_| {
                s.spawn(|| {
                    let mut thread_state = ThreadStats {
                        push_counter: 0,
                        pop_counter: 0,
                        dropped_push_counter: 0,
                        dropped_pop_counter: 0,
                    };

                    for _ in 0..num_iters {
                        if let Ok(_) = mpmc.push(SpamObject {
                            push_counter: AtomicUsize::new(1),
                            pop_counter: AtomicUsize::new(0),
                        }) {
                            thread_state.push_counter += 1;
                        }

                        if let Some(popped) = mpmc.pop() {
                            popped.pop_counter.fetch_add(1, Ordering::Acquire);

                            thread_state.pop_counter += 1;
                            std::thread::sleep(Duration::from_nanos(10)); // let other threads stir up the queue

                            // increment it optimistically
                            popped.push_counter.fetch_add(1, Ordering::Acquire);
                            match mpmc.push(popped) {
                                Ok(_) => thread_state.push_counter += 1,
                                Err(popped) => {
                                    thread_state.dropped_push_counter +=
                                        popped.push_counter.load(Ordering::Acquire) - 1;
                                    thread_state.dropped_pop_counter +=
                                        popped.pop_counter.load(Ordering::Acquire);
                                }
                            }
                        }
                    }

                    thread_state
                })
            })
            .collect::<Vec<_>>();

        thread_stat_handles
            .into_iter()
            .map(|t| t.join())
            .collect::<std::thread::Result<Vec<_>>>()
            .unwrap()
    });

    let mut non_popped_count: isize = 0;
    for stat in stats {
        non_popped_count += stat.push_counter as isize - stat.pop_counter as isize;
    }
    assert_eq!(non_popped_count, mpmc.capacity() as isize);
    assert_eq!(non_popped_count, mpmc.len() as isize);

    let mut obj_average_pushes: usize = 0;
    let mut obj_average_pops: usize = 0;

    let mut popped_count = 0;
    while let Some(popped) = mpmc.pop() {
        obj_average_pushes += popped.push_counter.load(Ordering::Relaxed);
        obj_average_pops += popped.pop_counter.load(Ordering::Relaxed);
        popped_count += 1;
    }
    assert_eq!(popped_count, mpmc.capacity());
    assert_eq!(0, mpmc.len());

    dbg!(obj_average_pushes / mpmc.capacity());
    dbg!(obj_average_pops / mpmc.capacity());

    assert!(obj_average_pushes >= obj_average_pops);
}
