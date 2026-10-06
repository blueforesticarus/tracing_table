//! Explicit synthetic workload: no sleeps, renderer, or timing assertions.
use std::{
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};
use tracing_subscriber::{Registry, layer::SubscriberExt};
use tracing_table::{
    data::dyntuple::ValueData,
    subscriber::{RecordKind, RecordingLayer},
};

// Each expansion creates its own static tracing callsite; a runtime target or
// message alone would still exercise only one callsite.
macro_rules! define_callsite {
    ($name:ident, $id:literal) => {
        fn $name(worker: usize, iteration: usize) {
            tracing::trace!(target: "synthetic", callsite = $id as u64,
                thread = worker as u64, iteration = iteration as u64,
                entity = ((worker as u64) << 32) | (iteration as u64 % 128),
                flag = true, wide = iteration as u128,
                repeated = "repeated interned payload for compression",
                changing = %format!("changing payload {}", iteration % 257),
                message = "synthetic workload");
        }
    };
}
define_callsite!(site_0, 0);
define_callsite!(site_1, 1);
define_callsite!(site_2, 2);
define_callsite!(site_3, 3);
define_callsite!(site_4, 4);
define_callsite!(site_5, 5);
define_callsite!(site_6, 6);
define_callsite!(site_7, 7);
define_callsite!(site_8, 8);
define_callsite!(site_9, 9);
define_callsite!(site_10, 10);
define_callsite!(site_11, 11);
define_callsite!(site_12, 12);
define_callsite!(site_13, 13);
define_callsite!(site_14, 14);
define_callsite!(site_15, 15);
define_callsite!(site_16, 16);
define_callsite!(site_17, 17);
define_callsite!(site_18, 18);
define_callsite!(site_19, 19);
define_callsite!(site_20, 20);
define_callsite!(site_21, 21);
define_callsite!(site_22, 22);
define_callsite!(site_23, 23);
define_callsite!(site_24, 24);
define_callsite!(site_25, 25);
define_callsite!(site_26, 26);
define_callsite!(site_27, 27);
define_callsite!(site_28, 28);
define_callsite!(site_29, 29);
define_callsite!(site_30, 30);
define_callsite!(site_31, 31);
define_callsite!(site_32, 32);
define_callsite!(site_33, 33);
define_callsite!(site_34, 34);
define_callsite!(site_35, 35);
define_callsite!(site_36, 36);
define_callsite!(site_37, 37);
define_callsite!(site_38, 38);
define_callsite!(site_39, 39);
define_callsite!(site_40, 40);
define_callsite!(site_41, 41);
define_callsite!(site_42, 42);
define_callsite!(site_43, 43);
define_callsite!(site_44, 44);
define_callsite!(site_45, 45);
define_callsite!(site_46, 46);
define_callsite!(site_47, 47);
define_callsite!(site_48, 48);
define_callsite!(site_49, 49);
define_callsite!(site_50, 50);
define_callsite!(site_51, 51);
define_callsite!(site_52, 52);
define_callsite!(site_53, 53);
define_callsite!(site_54, 54);
define_callsite!(site_55, 55);
define_callsite!(site_56, 56);
define_callsite!(site_57, 57);
define_callsite!(site_58, 58);
define_callsite!(site_59, 59);
define_callsite!(site_60, 60);
define_callsite!(site_61, 61);
define_callsite!(site_62, 62);
define_callsite!(site_63, 63);
const CALLSITES: [fn(usize, usize); 64] = [
    site_0, site_1, site_2, site_3, site_4, site_5, site_6, site_7, site_8, site_9, site_10,
    site_11, site_12, site_13, site_14, site_15, site_16, site_17, site_18, site_19, site_20,
    site_21, site_22, site_23, site_24, site_25, site_26, site_27, site_28, site_29, site_30,
    site_31, site_32, site_33, site_34, site_35, site_36, site_37, site_38, site_39, site_40,
    site_41, site_42, site_43, site_44, site_45, site_46, site_47, site_48, site_49, site_50,
    site_51, site_52, site_53, site_54, site_55, site_56, site_57, site_58, site_59, site_60,
    site_61, site_62, site_63,
];

fn setting(name: &str, default: usize, maximum: usize) -> usize {
    let value = std::env::var(name)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("benchmark setting must be a positive integer")
        })
        .unwrap_or(default);
    assert!(
        (1..=maximum).contains(&value),
        "{name} must be between 1 and {maximum}"
    );
    value
}

#[test]
#[ignore = "explicit throughput workload; run with --release --ignored --nocapture"]
fn synthetic_multithreaded_tracing() {
    let threads = setting(
        "TRACING_BENCH_THREADS",
        thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8),
        1024,
    );
    let iterations = setting("TRACING_BENCH_ITERATIONS", 100_000, usize::MAX / 64);
    let callsites = setting("TRACING_BENCH_CALLSITES", 32, 64);
    let recorder = Arc::new(RecordingLayer::new());
    let dispatch = tracing::Dispatch::new(Registry::default().with(recorder.layer()));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut workers = Vec::new();
    for worker in 0..threads {
        let dispatch = dispatch.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || tracing::dispatcher::with_default(&dispatch, || {
            barrier.wait();
            let mut emitted = 0usize;
            for iteration in 0..iterations {
                // Mostly unparented records plus occasional spans create varied
                // schemas without assigning different rates via artificial sleeps.
                let span = (iteration % 4096 == 0).then(|| tracing::trace_span!(target: "synthetic", "burst", entity = worker as u64));
                let _entered = span.as_ref().map(|span| span.enter());
                for (site, emit) in CALLSITES.iter().enumerate().take(callsites) {
                    let stride = 1usize << ((site % 3) * 3);
                    if (site % 2 == 0 || worker % 4 == site % 4) && iteration % stride == 0 {
                        emit(worker, iteration);
                        emitted += 1;
                    }
                }
            }
            emitted
        })));
    }
    let started = Instant::now();
    barrier.wait();
    let emitted: usize = workers
        .into_iter()
        .map(|worker| worker.join().expect("logging worker panicked"))
        .sum();
    let elapsed = started.elapsed();
    // Expected counts use a closed-form schedule, independent of emitted rows.
    let expected: usize = (0..threads)
        .flat_map(|worker| {
            (0..callsites).filter(move |site| *site % 2 == 0 || worker % 4 == *site % 4)
        })
        .map(|site| (iterations - 1) / (1usize << ((site % 3) * 3)) + 1)
        .sum();
    assert_eq!(emitted, expected);
    let inspected = Instant::now();
    let snapshots = recorder.inspect(&Default::default());
    let event_rows: usize = snapshots
        .iter()
        .filter(|s| s.kind == RecordKind::Event)
        .map(|s| s.rows)
        .sum();
    assert_eq!(event_rows, expected);
    let distinct: std::collections::HashSet<_> = snapshots
        .iter()
        .filter(|s| s.kind == RecordKind::Event)
        .map(|s| s.id.callsite)
        .collect();
    let expected_callsites = (0..callsites)
        .filter(|site| *site % 2 == 0 || (0..threads).any(|worker| worker % 4 == *site % 4))
        .count();
    assert_eq!(distinct.len(), expected_callsites);
    for snapshot in snapshots.iter().filter(|s| s.kind == RecordKind::Event) {
        for row in recorder.rows(snapshot.id, None, 4) {
            let number = |name: &str| {
                row.fields
                    .iter()
                    .find_map(|(field, value)| match value {
                        ValueData::U64(value) if field == name => Some(*value),
                        _ => None,
                    })
                    .unwrap()
            };
            let worker = number("thread");
            let iteration = number("iteration");
            assert!(worker < threads as u64 && iteration < iterations as u64);
            assert_eq!(number("entity"), (worker << 32) | iteration % 128);
            assert!(row.fields.iter().any(|(name, value)| name == "wide"
                && matches!(value, ValueData::U128(v) if *v == iteration as u128)));
            assert!(row.fields.iter().any(|(name, value)| name == "changing" && matches!(value, ValueData::Str(v) if &**v == format!("changing payload {}", iteration % 257))));
        }
    }
    let stats = recorder.compression_ratio();
    println!(
        "threads={threads} iterations/thread={iterations} callsites={callsites} events={emitted}"
    );
    println!(
        "emission={elapsed:?} events/sec={:.0} inspection={:?}",
        emitted as f64 / elapsed.as_secs_f64(),
        inspected.elapsed()
    );
    println!("frame compression (excludes interned string allocations): {stats:?}");
}
