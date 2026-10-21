//! Cost of a maintenance pass, as paid by the request that triggers it.
//!
//! ```sh
//! cargo bench --bench maintain
//! cargo bench --bench maintain -- --top 1000,100000 --reps 9
//! ```
//!
//! For each `top`, a table with the VMOD default `size = 10 * top` is filled
//! through `hit()` with a Zipf stream, then the bench times the single `hit()`
//! call that crosses a deadline, i.e. the stall one request sees:
//!
//! - `halve`: first pass of a new window, halves every count then recomputes
//!   the threshold;
//! - `threshold`: the next pass, threshold only.
//!
//! The window is shortened to `4 * size` so filling stays cheap; pass cost
//! only depends on `size` and occupancy. `amortized` is the per-request cost
//! with the VMOD default window (`1000 * top`): 1 halving + 7 threshold passes
//! per window.

// Same trick as `benches/hit.rs`: pull the algorithm in without Varnish.
#[allow(unused)]
#[path = "../src"]
mod src {
    pub mod table;
}

use std::hint::black_box;
use std::time::{Duration, Instant};

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};

use src::table::{MAX_WINDOW, TICKS_PER_WINDOW, Table};

struct Args {
    tops: Vec<usize>,
    reps: usize,
}

fn usage() -> ! {
    eprintln!("usage: cargo bench --bench maintain -- [--top 100,1000,10000,100000] [--reps 5]");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        tops: vec![100, 1000, 10_000, 100_000],
        reps: 5,
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
            "--top" => {
                args.tops = value
                    .split(',')
                    .map(|top| top.parse().unwrap_or_else(|_| usage()))
                    .collect();
            }
            "--reps" => args.reps = value.parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    if args.tops.contains(&0) || args.reps == 0 {
        usage();
    }
    args
}

fn hash_key(key: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&key.to_le_bytes())
}

/// Zipf s=1.1 over `100 * top` keys: enough distinct keys to fill every slot.
fn stream(top: usize, len: usize, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let zipf = Zipf::new(100.0 * top as f64, 1.1).unwrap();
    let salt: u64 = rng.random();
    (0..len)
        .map(|_| hash_key(zipf.sample(&mut rng) as u64 ^ salt))
        .collect()
}

/// Replay `hashes`, timing only the call that makes the counter reach
/// `timed_request` (1-based).
fn replay(table: &Table, hashes: &[u64], timed_request: usize) -> Duration {
    let (before, rest) = hashes.split_at(timed_request - 1);
    for &hash in before {
        black_box(table.hit(hash));
    }
    let start = Instant::now();
    black_box(table.hit(rest[0]));
    start.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let args = parse_args();
    println!("reps={} (median), size = 10 * top", args.reps);
    println!(
        "{:>8} {:>10} {:>12} {:>14} {:>12} {:>14}",
        "top", "size", "halve µs", "threshold µs", "ns/slot", "amortized ns"
    );

    for &top in &args.tops {
        let size = 10 * top;
        let window = 4 * size as u64;
        let requests_per_tick = (window / TICKS_PER_WINDOW) as usize;
        let halving_request = window as usize;
        let threshold_request = halving_request + requests_per_tick;
        let hashes = stream(top, threshold_request, top as u64);

        let mut halve = Vec::with_capacity(args.reps);
        let mut threshold = Vec::with_capacity(args.reps);
        for _ in 0..args.reps {
            let table = Table::new(top, size, window);
            halve.push(replay(&table, &hashes, halving_request));
            threshold.push(replay(
                &table,
                &hashes[halving_request..],
                requests_per_tick,
            ));
        }
        let (halve, threshold) = (median(halve), median(threshold));

        let default_window = (1000 * top as u64).min(MAX_WINDOW);
        let per_window = halve + threshold * (TICKS_PER_WINDOW as u32 - 1);
        println!(
            "{top:>8} {size:>10} {:>12.1} {:>14.1} {:>12.2} {:>14.3}",
            halve.as_secs_f64() * 1e6,
            threshold.as_secs_f64() * 1e6,
            halve.as_secs_f64() * 1e9 / size as f64,
            per_window.as_secs_f64() * 1e9 / default_window as f64,
        );
    }
}
