//! Throughput of `Table::hit()` under concurrency.
//!
//! ```sh
//! cargo bench --bench hit
//! cargo bench --bench hit -- --threads 1,12 --dist hot --secs 1 --reps 1
//! ```
//!
//! Each run builds a fresh table with the VMOD defaults (`size = 10 * top`,
//! `window = 1000 * top`), so maintenance runs at its normal rate. Every
//! thread replays its own pre-generated hash stream, keeping RNG and hashing
//! out of the timed loop. `speedup` is relative to the first thread count.

// Pull the algorithm in directly: the crate is a cdylib, and this keeps
// Varnish out of the benchmark binary. The wrapper module keeps nested module
// paths (`table/tests.rs`, built under `clippy --all-targets`) resolving.
#[allow(unused)]
#[path = "../src"]
mod src {
    pub mod table;
}

use std::hint::black_box;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};

use src::table::{self, Table};

/// Hashes per thread stream, a power of two so indexing is a mask.
const STREAM_LEN: usize = 1 << 16;
/// `hit()` calls between two checks of the stop flag.
const CHUNK_LEN: usize = 1024;
/// Distinct streams; beyond that, threads share one at different offsets, so
/// 1000 threads don't need 512 MiB of streams.
const MAX_STREAMS: usize = 64;

#[derive(Clone, Copy)]
enum Dist {
    /// Zipf s=1.1 over 100k keys, a typical URL popularity curve.
    Zipf,
    /// 1M equally likely keys: mostly collisions and decay.
    Uniform,
    /// A single key for everyone: worst-case slot contention.
    Hot,
}

struct Args {
    threads: Vec<usize>,
    secs: f64,
    reps: usize,
    dist: Dist,
    top: usize,
}

fn usage() -> ! {
    eprintln!(
        "usage: cargo bench --bench hit -- [--threads 1,10,100,1000] [--secs 2] \
         [--reps 3] [--dist zipf|uniform|hot] [--top 1000]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        threads: vec![1, 10, 100, 1000],
        secs: 2.0,
        reps: 3,
        dist: Dist::Zipf,
        top: 1000,
    };
    let mut cli_args = std::env::args().skip(1);
    while let Some(flag) = cli_args.next() {
        // cargo bench passes `--bench` to harness=false binaries.
        if flag == "--bench" {
            continue;
        }
        let Some(value) = cli_args.next() else {
            usage()
        };
        match flag.as_str() {
            "--threads" => {
                args.threads = value
                    .split(',')
                    .map(|count| count.parse().unwrap_or_else(|_| usage()))
                    .collect();
            }
            "--secs" => args.secs = value.parse().unwrap_or_else(|_| usage()),
            "--reps" => args.reps = value.parse().unwrap_or_else(|_| usage()),
            "--top" => args.top = value.parse().unwrap_or_else(|_| usage()),
            "--dist" => {
                args.dist = match value.as_str() {
                    "zipf" => Dist::Zipf,
                    "uniform" => Dist::Uniform,
                    "hot" => Dist::Hot,
                    _ => usage(),
                }
            }
            _ => usage(),
        }
    }
    if args.threads.contains(&0) || args.reps == 0 || args.top == 0 || args.secs <= 0.0 {
        usage();
    }
    args
}

fn hash_key(key: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&key.to_le_bytes())
}

/// Hash stream for one thread. `salt` is shared so all threads agree on which
/// keys are hot; `seed` differs so they don't replay the same sequence.
fn stream(dist: Dist, salt: u64, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    match dist {
        Dist::Zipf => {
            let zipf = Zipf::new(100_000.0, 1.1).unwrap();
            (0..STREAM_LEN)
                .map(|_| hash_key(zipf.sample(&mut rng) as u64 ^ salt))
                .collect()
        }
        Dist::Uniform => (0..STREAM_LEN)
            .map(|_| hash_key(rng.random_range(0..1_000_000u64) ^ salt))
            .collect(),
        Dist::Hot => vec![hash_key(salt); STREAM_LEN],
    }
}

/// One timed run, returns (total hits, wall time).
fn run(args: &Args, threads: usize, streams: &[Vec<u64>]) -> (u64, Duration) {
    let top = args.top;
    let table = Table::new(top, 10 * top, window(top));
    let stop = AtomicBool::new(false);
    let barrier = Barrier::new(threads + 1);

    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|i| {
                let stream = &streams[i % streams.len()];
                let (table, stop, barrier) = (&table, &stop, &barrier);
                scope.spawn(move || {
                    let mask = STREAM_LEN - 1;
                    let mut position = (i / streams.len()) * 7919;
                    let mut ops = 0u64;
                    barrier.wait();
                    let start = Instant::now();
                    while !stop.load(Relaxed) {
                        for _ in 0..CHUNK_LEN {
                            black_box(table.hit(stream[position & mask]));
                            position += 1;
                        }
                        ops += CHUNK_LEN as u64;
                    }
                    (ops, start, Instant::now())
                })
            })
            .collect();

        barrier.wait();
        std::thread::sleep(Duration::from_secs_f64(args.secs));
        stop.store(true, Relaxed);
        // Workers time themselves: with many more threads than cores, this
        // thread can be descheduled for a long time after the barrier.
        let runs: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let ops = runs.iter().map(|&(ops, _, _)| ops).sum();
        let start = runs.iter().map(|&(_, start, _)| start).min().unwrap();
        let end = runs.iter().map(|&(_, _, end)| end).max().unwrap();
        (ops, end - start)
    })
}

/// The VMOD's default window.
fn window(top: usize) -> u64 {
    (1000 * top as u64).min(table::MAX_WINDOW)
}

fn main() {
    let args = parse_args();
    let max_threads = args.threads.iter().copied().max().unwrap();
    let salt: u64 = rand::random();
    let streams: Vec<_> = (0..max_threads.min(MAX_STREAMS))
        .map(|i| stream(args.dist, salt, i as u64))
        .collect();

    let nproc = std::thread::available_parallelism().map_or(0, usize::from);
    let dist = match args.dist {
        Dist::Zipf => "zipf",
        Dist::Uniform => "uniform",
        Dist::Hot => "hot",
    };
    println!(
        "dist={dist} top={} size={} window={} secs={} reps={} (median), nproc={nproc}",
        args.top,
        10 * args.top,
        window(args.top),
        args.secs,
        args.reps,
    );
    println!(
        "{:>8} {:>14} {:>14} {:>10}",
        "threads", "total Mops/s", "ns/op/thread", "speedup"
    );

    let mut baseline = None;
    for &threads in &args.threads {
        let mut results: Vec<_> = (0..args.reps)
            .map(|_| {
                let (ops, wall) = run(&args, threads, &streams);
                ops as f64 / wall.as_secs_f64()
            })
            .collect();
        results.sort_by(f64::total_cmp);
        let ops_per_sec = results[results.len() / 2];
        let ns_per_op = threads as f64 * 1e9 / ops_per_sec;
        let baseline = *baseline.get_or_insert(ops_per_sec);
        println!(
            "{threads:>8} {:>14.2} {ns_per_op:>14.1} {:>9.2}x",
            ops_per_sec / 1e6,
            ops_per_sec / baseline,
        );
    }
}
