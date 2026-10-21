use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use heavykeeper::{CuckooTopK, TopK};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, Zipf};

use super::*;

fn hash_key(key: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&key.to_le_bytes())
}

fn zipf_stream(keys: u64, samples: usize, skew: f64, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let zipf = Zipf::new(keys as f64, skew).unwrap();
    // Shuffle ranks so popularity is not correlated with key value.
    let salt: u64 = rng.random();
    (0..samples)
        .map(|_| hash_key(zipf.sample(&mut rng) as u64 ^ salt))
        .collect()
}

fn exact_top(stream: &[u64], top: usize) -> HashSet<u64> {
    let mut counts: HashMap<u64, u64> = HashMap::new();
    for &key in stream {
        *counts.entry(key).or_default() += 1;
    }
    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_unstable_by_key(|&(_, count)| std::cmp::Reverse(count));
    ranked.into_iter().take(top).map(|(key, _)| key).collect()
}

fn popular_set(table: &Table, stream: &[u64]) -> HashSet<u64> {
    let threshold = table.threshold();
    stream
        .iter()
        .copied()
        .filter(|&key| table.estimate(hash_key(key)) > threshold)
        .collect()
}

/// (recall, precision) of `found` against `truth`
fn score(found: &HashSet<u64>, truth: &HashSet<u64>) -> (f64, f64) {
    let true_positives = found.intersection(truth).count() as f64;
    (
        true_positives / truth.len() as f64,
        if found.is_empty() {
            0.0
        } else {
            true_positives / found.len() as f64
        },
    )
}

fn run_table(table: &Table, stream: &[u64]) {
    for &key in stream {
        table.hit(hash_key(key));
    }
}

#[test]
fn cold_start_single_hit_is_not_popular() {
    let table = Table::new(10, 100, 1000);
    assert_eq!(table.hit(hash_key(1)), 1);
    assert_eq!(table.threshold(), 1);
    assert_eq!(table.estimate(hash_key(1)), 1);
    assert_eq!(table.hit(hash_key(1)), 2);
    assert!(table.estimate(hash_key(1)) > table.threshold());
    assert_eq!(table.estimate(hash_key(2)), 0);
}

#[test]
fn threshold_caps_popular_set_at_top() {
    // Large enough that the 10 keys never share a slot, so no decay happens.
    let table = Table::new(3, 1024, 1 << 20);
    for key in 0..10u64 {
        for _ in 0..(key + 1) * 10 {
            table.update(hash_key(key));
        }
    }
    let slots: HashSet<_> = (0..10u64)
        .flat_map(|key| (0..DEPTH).map(move |row| (key, row)))
        .map(|(key, row)| std::ptr::from_ref(table.slot(fingerprint_of(hash_key(key)), row)))
        .collect();
    assert_eq!(slots.len(), 10 * DEPTH);
    table.maintain(true);
    let popular: Vec<u64> = (0..10)
        .filter(|&key| table.estimate(hash_key(key)) > table.threshold())
        .collect();
    assert_eq!(popular, vec![7, 8, 9]);
}

#[test]
fn halving_once_per_elapsed_window() {
    let window = 800;
    let table = Table::new(1, 16, window);
    for _ in 0..100 {
        table.update(hash_key(1));
    }
    table.requests.0.store(window - 1, Relaxed);
    table.maintain(true);
    assert_eq!(table.estimate(hash_key(1)), 100);
    table.requests.0.store(window, Relaxed);
    table.maintain(true);
    assert_eq!(table.estimate(hash_key(1)), 50);
    // a late pass catches up on every window it missed
    table.requests.0.store(3 * window, Relaxed);
    table.maintain(true);
    assert_eq!(table.estimate(hash_key(1)), 12);
    // more windows than bits: everything is gone, no shift overflow
    table.requests.0.store(100 * window, Relaxed);
    table.maintain(true);
    assert_eq!(table.estimate(hash_key(1)), 0);
}

#[test]
fn busy_lock_delays_maintenance_but_does_not_lose_it() {
    let window = 800;
    let table = Table::new(1, 16, window);
    for _ in 0..100 {
        table.update(hash_key(1));
    }
    table.requests.0.store(window - 1, Relaxed);
    let guard = table.maintenance.lock().unwrap();
    table.hit(hash_key(2)); // due, but lock is busy: skipped
    assert_eq!(table.estimate(hash_key(1)), 100);
    assert_eq!(
        table.next_maintenance.load(Relaxed),
        window / TICKS_PER_WINDOW
    );
    drop(guard);
    table.hit(hash_key(2)); // still due: runs now
    assert_eq!(table.estimate(hash_key(1)), 50);
    assert_eq!(
        table.next_maintenance.load(Relaxed),
        window + window / TICKS_PER_WINDOW
    );
}

#[test]
fn stale_deadline_does_not_rerun_maintenance() {
    let table = Table::new(1, 1024, 8000);
    let requests_per_tick = 8000 / TICKS_PER_WINDOW;
    for (key, hits) in [(1, 10), (2, 5)] {
        for _ in 0..hits {
            table.update(hash_key(key));
        }
    }
    table.requests.0.store(requests_per_tick - 1, Relaxed);
    table.hit(hash_key(3)); // due: threshold = 2nd largest count
    assert_eq!(table.threshold(), 5);
    for _ in 0..10 {
        table.update(hash_key(2));
    }
    // A request that read the deadline before the pass moved it: no new tick
    // elapsed, so nothing is recomputed.
    table.next_maintenance.store(requests_per_tick, Relaxed);
    table.hit(hash_key(3));
    assert_eq!(table.threshold(), 5);
    // A forced pass still runs.
    table.maintain(true);
    assert_eq!(table.threshold(), 10);
}

/// Compare against exact top-K and against the `heavykeeper` crate's
/// `HeavyKeeper` and Cuckoo `HeavyKeeper`, all with ~the same number of slots.
/// Run with `--nocapture` to see the table.
#[test]
fn accuracy_vs_oracles() {
    const K: usize = 100;
    const SIZE: usize = 1024;
    const SAMPLES: usize = 1_000_000;
    const DECAY: f64 = 1.0 / DECAY_BASE;

    println!(
        "{:>5} {:>22} {:>22} {:>22} {:>14} {:>14}",
        "skew",
        "ours (no aging) R/P",
        "ours (window 1000K) R/P",
        "ours (window 100K) R/P",
        "hk R",
        "cuckoo-hk R"
    );
    for (i, skew) in [0.8, 1.0, 1.2].into_iter().enumerate() {
        let stream = zipf_stream(100_000, SAMPLES, skew, i as u64);
        let truth = exact_top(&stream, K);

        let no_aging = Table::new(K, SIZE, MAX_WINDOW);
        for &key in &stream {
            no_aging.update(hash_key(key));
        }
        no_aging.maintain(true);
        let (no_aging_recall, no_aging_precision) = score(&popular_set(&no_aging, &stream), &truth);

        let long_window = Table::new(K, SIZE, 1000 * K as u64);
        run_table(&long_window, &stream);
        let (long_window_recall, long_window_precision) =
            score(&popular_set(&long_window, &stream), &truth);

        let short_window = Table::new(K, SIZE, 100 * K as u64);
        run_table(&short_window, &stream);
        let (short_window_recall, short_window_precision) =
            score(&popular_set(&short_window, &stream), &truth);

        let mut heavykeeper: TopK<u64> = TopK::new(K, SIZE / 2, 2, DECAY);
        let mut cuckoo: CuckooTopK<u64> = CuckooTopK::new(K, SIZE / 4, 3, DECAY);
        for key in &stream {
            heavykeeper.add(key, 1);
            cuckoo.add(key, 1);
        }
        let (heavykeeper_recall, _) = score(
            &heavykeeper
                .list()
                .into_iter()
                .map(|node| node.item)
                .collect(),
            &truth,
        );
        let (cuckoo_recall, _) = score(
            &cuckoo.list().into_iter().map(|node| node.item).collect(),
            &truth,
        );

        println!(
            "{skew:>5} {:>22} {:>22} {:>22} {heavykeeper_recall:>14.2} {cuckoo_recall:>14.2}",
            format!("{no_aging_recall:.2}/{no_aging_precision:.2}"),
            format!("{long_window_recall:.2}/{long_window_precision:.2}"),
            format!("{short_window_recall:.2}/{short_window_precision:.2}"),
        );

        assert!(
            no_aging_recall >= 0.8 && no_aging_precision >= 0.8,
            "no aging, s={skew}: {no_aging_recall}/{no_aging_precision}"
        );
        assert!(
            long_window_recall >= 0.7 && long_window_precision >= 0.7,
            "window 1000K, s={skew}: {long_window_recall}/{long_window_precision}"
        );
    }
}

#[test]
fn popularity_shifts_after_a_few_windows() {
    const K: usize = 20;
    let window = 1000 * K as u64;
    let table = Table::new(K, 10 * K, window);
    let mut rng = SmallRng::seed_from_u64(7);

    // Phase A: keys 0..K are hot; phase B: keys 1000..1000+K are hot.
    let mut run_phase = |hot_base: u64, requests: u64| {
        for _ in 0..requests {
            let key = if rng.random_bool(0.5) {
                hot_base + rng.random_range(0..K as u64)
            } else {
                rng.random_range(1_000_000..2_000_000)
            };
            table.hit(hash_key(key));
        }
    };
    run_phase(0, 5 * window);
    let popular = |hot_base: u64| {
        (hot_base..hot_base + K as u64)
            .filter(|&key| table.estimate(hash_key(key)) > table.threshold())
            .count()
    };
    assert!(popular(0) >= K * 8 / 10, "phase A: {}", popular(0));

    run_phase(1000, 3 * window);
    assert!(
        popular(1000) >= K * 8 / 10,
        "phase B new: {}",
        popular(1000)
    );
    assert!(popular(0) <= K / 10, "phase B old: {}", popular(0));
}

#[test]
fn concurrent_smoke() {
    const K: usize = 50;
    let table = Arc::new(Table::new(K, 10 * K, 1000 * K as u64));
    let stream = Arc::new(zipf_stream(10_000, 400_000, 1.1, 3));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let table = Arc::clone(&table);
            let stream = Arc::clone(&stream);
            std::thread::spawn(move || {
                let chunk_len = stream.len() / 8;
                run_table(&table, &stream[i * chunk_len..(i + 1) * chunk_len]);
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    table.maintain(true);
    let truth = exact_top(&stream, K);
    let (recall, precision) = score(&popular_set(&table, &stream), &truth);
    assert!(recall >= 0.7 && precision >= 0.7, "{recall}/{precision}");
}
