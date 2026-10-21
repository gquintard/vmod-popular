//! Accuracy report: how well `hit() > threshold()` matches the true top keys.
//!
//! ```sh
//! cargo bench --bench accuracy
//! cargo bench --bench accuracy -- --top 100 --skew 0.8,1.0 --size-factor 5,10 --windows 6
//! ```
//!
//! Keys follow a stationary Zipf law, so the truth is the distribution itself:
//! the popular keys are Zipf ranks `1..=top`, no sampling noise. Each config
//! replays `windows` and a half windows single-threaded; the first window is
//! warm-up and not scored.
//!
//! Columns:
//! - `top share`: fraction of requests going to a true top key (best case for
//!   the hot backend);
//! - `req R/P`: per request decision, the metric that matters for routing.
//!   Recall = top-key requests routed hot, precision = hot-routed requests that
//!   were for a top key;
//! - `key R/P`: same, over distinct keys, using `count()` vs `threshold()` once
//!   at the end;
//! - `popular`: number of distinct keys above the threshold at the end (target:
//!   `top`).

#[allow(unused)]
#[path = "../src"]
mod src {
    pub mod table;
}

use std::collections::HashSet;

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};

use src::table::{MAX_WINDOW, Table};

struct Args {
    top: usize,
    keys: u64,
    skews: Vec<f64>,
    size_factors: Vec<usize>,
    window_factor: u64,
    windows: u64,
}

fn usage() -> ! {
    eprintln!(
        "usage: cargo bench --bench accuracy -- [--top 1000] [--keys 1000000] \
         [--skew 0.7,0.9,1.1,1.3] [--size-factor 2,5,10,20] [--window-factor 1000] [--windows 4]"
    );
    std::process::exit(2);
}

fn parse_list<T: std::str::FromStr>(value: &str) -> Vec<T> {
    value
        .split(',')
        .map(|item| item.parse().unwrap_or_else(|_| usage()))
        .collect()
}

fn parse_args() -> Args {
    let mut args = Args {
        top: 1000,
        keys: 1_000_000,
        skews: vec![0.7, 0.9, 1.1, 1.3],
        size_factors: vec![2, 5, 10, 20],
        window_factor: 1000,
        windows: 4,
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
            "--top" => args.top = value.parse().unwrap_or_else(|_| usage()),
            "--keys" => args.keys = value.parse().unwrap_or_else(|_| usage()),
            "--skew" => args.skews = parse_list(&value),
            "--size-factor" => args.size_factors = parse_list(&value),
            "--window-factor" => args.window_factor = value.parse().unwrap_or_else(|_| usage()),
            "--windows" => args.windows = value.parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    if args.top == 0
        || args.keys <= args.top as u64
        || args.size_factors.contains(&0)
        || args.size_factors.contains(&1)
        || args.window_factor == 0
        || args.windows < 2
    {
        usage();
    }
    args
}

fn hash_key(key: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&key.to_le_bytes())
}

/// Zipf ranks, 1-based: rank `r` is the `r`-th most likely key.
fn ranks(keys: u64, skew: f64, len: u64, seed: u64) -> Vec<u32> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let zipf = Zipf::new(keys as f64, skew).unwrap();
    (0..len).map(|_| zipf.sample(&mut rng) as u32).collect()
}

/// (recall, precision); precision is 0 when nothing was selected.
fn ratios(true_positives: u64, relevant: u64, selected: u64) -> (f64, f64) {
    let ratio = |a: u64, b: u64| if b == 0 { 0.0 } else { a as f64 / b as f64 };
    (
        ratio(true_positives, relevant),
        ratio(true_positives, selected),
    )
}

fn main() {
    let args = parse_args();
    let top = args.top;
    let window = (args.window_factor * top as u64).min(MAX_WINDOW);
    // End mid-window, so the final snapshot isn't taken right after a halving.
    let len = window * args.windows + window / 2;
    println!(
        "top={top} keys={} window={window} windows={} (1 warm-up)",
        args.keys, args.windows
    );
    println!(
        "{:>5} {:>8} {:>10} {:>10} {:>12} {:>12} {:>8}",
        "skew", "size", "top share", "threshold", "req R/P", "key R/P", "popular"
    );

    // Keys are salted so popularity isn't correlated with the hashed value.
    let salt: u64 = SmallRng::seed_from_u64(0).random();
    for (i, &skew) in args.skews.iter().enumerate() {
        let ranks = ranks(args.keys, skew, len, i as u64);
        let hashes: Vec<u64> = ranks
            .iter()
            .map(|&rank| hash_key(u64::from(rank) ^ salt))
            .collect();
        let is_top = |rank: u32| rank as usize <= top;

        for &size_factor in &args.size_factors {
            let size = size_factor * top;
            if window < size as u64 {
                println!("{skew:>5} {size:>8} skipped: window < size");
                continue;
            }
            let table = Table::new(top, size, window);

            let (mut top_requests, mut hot_requests, mut hot_top_requests) = (0, 0, 0);
            for (request, (&rank, &hash)) in ranks.iter().zip(&hashes).enumerate() {
                // Same decision as `is_popular()`.
                let hot = table.hit(hash) > table.threshold();
                if (request as u64) < window {
                    continue;
                }
                top_requests += u64::from(is_top(rank));
                hot_requests += u64::from(hot);
                hot_top_requests += u64::from(hot && is_top(rank));
            }
            let scored = len - window;
            let (request_recall, request_precision) =
                ratios(hot_top_requests, top_requests, hot_requests);

            let threshold = table.threshold();
            let seen: HashSet<u32> = ranks.iter().copied().collect();
            let popular: Vec<u32> = seen
                .into_iter()
                .filter(|&rank| table.estimate(hash_key(u64::from(rank) ^ salt)) > threshold)
                .collect();
            let popular_top = popular.iter().filter(|&&rank| is_top(rank)).count();
            let (key_recall, key_precision) =
                ratios(popular_top as u64, top as u64, popular.len() as u64);

            println!(
                "{skew:>5} {size:>8} {:>10.3} {threshold:>10} {:>12} {:>12} {:>8}",
                top_requests as f64 / scored as f64,
                format!("{request_recall:.2}/{request_precision:.2}"),
                format!("{key_recall:.2}/{key_precision:.2}"),
                popular.len(),
            );
        }
    }
}
