//! Lock-free, HeavyKeeper-style frequency table.
//!
//! Every slot is an `AtomicU64` packing `fingerprint:32 | count:32`. Keys are
//! never stored, only their 32-bit fingerprint, and the slot positions are
//! derived from the fingerprint so a slot's content always tells us which
//! buckets the key can live in.
//!
//! The request path does a couple of atomic loads and at most one atomic RMW
//! per row, never blocks, and never retries: any lost race is just a slightly
//! wrong count, which is acceptable for a popularity estimate.
//!
//! A shared request counter drives maintenance (aging and threshold
//! computation): once it passes the next deadline, requests `try_lock` the
//! maintenance state until one of them gets it. A busy lock means maintenance
//! is in progress, so the others just carry on; nothing is ever lost since the
//! deadline stays due until a pass runs, and a pass catches up on everything
//! the counter says has elapsed.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Mutex, TryLockError};

use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Number of rows (candidate buckets per key).
const DEPTH: usize = 2;
/// `HeavyKeeper` decay base: a colliding slot with count `c` is decremented
/// with probability `DECAY_BASE^(-c)`.
const DECAY_BASE: f64 = 1.08;
/// Past this count, the decay probability rounds to zero.
const DECAY_TABLE_LEN: usize = 288;
/// The threshold is recomputed this many times per window; counts are halved
/// once per window.
pub const TICKS_PER_WINDOW: u64 = 8;
/// Largest accepted window, keeps counts far away from overflowing into the
/// fingerprint bits.
pub const MAX_WINDOW: u64 = 1 << 30;
/// Largest accepted number of slots (2 GiB of table).
pub const MAX_SIZE: usize = 1 << 28;

/// Keeps a value alone on its cache line(s), so writing it doesn't invalidate
/// the read-mostly fields around it.
#[repr(align(128))]
struct CachePadded<T>(T);

pub struct Table {
    /// `DEPTH` rows of `width` slots each.
    slots: Box<[AtomicU64]>,
    width: usize,
    top: usize,
    /// Number of requests between two maintenance passes.
    requests_per_tick: u64,
    /// Total number of requests seen, drives maintenance deadlines. Written
    /// by every request.
    requests: CachePadded<AtomicU64>,
    /// Value of `requests` at which the next maintenance pass is due.
    next_maintenance: AtomicU64,
    /// `decay_probabilities[c]` = probability (scaled to `u32::MAX`) of
    /// decrementing a count of `c`.
    decay_probabilities: Box<[u32]>,
    threshold: AtomicU32,
    maintenance: Mutex<Maintenance>,
}

struct Maintenance {
    /// Ticks (`requests / requests_per_tick`) accounted for by the last pass.
    ticks_done: u64,
    /// One count per tracked key, preallocated for the threshold selection.
    key_counts: Vec<u32>,
}

fn pack(fingerprint: u32, count: u32) -> u64 {
    (u64::from(fingerprint) << 32) | u64::from(count)
}

fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

fn fingerprint_of(hash: u64) -> u32 {
    (hash ^ (hash >> 32)) as u32
}

impl Table {
    /// `top`: how many keys are considered popular; `size`: total number of
    /// slots; `window`: number of requests between two halvings of all counts.
    pub fn new(top: usize, size: usize, window: u64) -> Self {
        assert!(top > 0 && size > top && size <= MAX_SIZE && window <= MAX_WINDOW);
        let width = size.div_ceil(DEPTH);
        let decay_probabilities = (0..DECAY_TABLE_LEN)
            .map(|count| (DECAY_BASE.powi(-(count as i32)) * f64::from(u32::MAX)) as u32)
            .collect();
        let requests_per_tick = (window / TICKS_PER_WINDOW).max(1);
        Table {
            slots: (0..width * DEPTH).map(|_| AtomicU64::new(0)).collect(),
            width,
            top,
            requests_per_tick,
            requests: CachePadded(AtomicU64::new(0)),
            next_maintenance: AtomicU64::new(requests_per_tick),
            decay_probabilities,
            threshold: AtomicU32::new(1),
            maintenance: Mutex::new(Maintenance {
                ticks_done: 0,
                key_counts: Vec::with_capacity(width * DEPTH),
            }),
        }
    }

    fn slot(&self, fingerprint: u32, row: usize) -> &AtomicU64 {
        let column = xxh3_64_with_seed(&fingerprint.to_le_bytes(), row as u64) % self.width as u64;
        &self.slots[row * self.width + column as usize]
    }

    fn decay_probability(&self, count: u32) -> u32 {
        self.decay_probabilities
            .get(count as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Count one occurrence of the key hashed to `hash`, run maintenance if
    /// it's due, and return the key's updated estimate.
    pub fn hit(&self, hash: u64) -> u32 {
        let request_number = self.requests.0.fetch_add(1, Relaxed) + 1;
        let estimate = self.update(hash);
        // Plain load first, so the request path stays off the mutex.
        if request_number >= self.next_maintenance.load(Relaxed) {
            self.maintain(false);
        }
        estimate
    }

    /// Update the slots of the key hashed to `hash`, and return its estimate.
    fn update(&self, hash: u64) -> u32 {
        let fingerprint = fingerprint_of(hash);
        let mut estimate = 0;
        for row in 0..DEPTH {
            let slot = self.slot(fingerprint, row);
            let current = slot.load(Relaxed);
            let (slot_fingerprint, slot_count) = unpack(current);
            if slot_count == 0 {
                if slot
                    .compare_exchange(current, pack(fingerprint, 1), Relaxed, Relaxed)
                    .is_ok()
                {
                    estimate = estimate.max(1);
                }
            } else if slot_fingerprint == fingerprint {
                // If the slot changed owner since the load, we bump someone
                // else's count. Rare and harmless.
                let (previous_fingerprint, previous_count) = unpack(slot.fetch_add(1, Relaxed));
                if previous_fingerprint == fingerprint {
                    estimate = estimate.max(previous_count + 1);
                }
            } else if fastrand::u32(..) < self.decay_probability(slot_count) {
                let decayed = if slot_count == 1 {
                    pack(fingerprint, 1)
                } else {
                    pack(slot_fingerprint, slot_count - 1)
                };
                if slot
                    .compare_exchange(current, decayed, Relaxed, Relaxed)
                    .is_ok()
                    && slot_count == 1
                {
                    estimate = estimate.max(1);
                }
            }
        }
        estimate
    }

    /// Current estimate for the key hashed to `hash`, without counting it.
    pub fn estimate(&self, hash: u64) -> u32 {
        let fingerprint = fingerprint_of(hash);
        (0..DEPTH)
            .map(|row| unpack(self.slot(fingerprint, row).load(Relaxed)))
            .filter(|&(slot_fingerprint, _)| slot_fingerprint == fingerprint)
            .map(|(_, count)| count)
            .max()
            .unwrap_or(0)
    }

    /// Keys with an estimate strictly above this are popular.
    pub fn threshold(&self) -> u32 {
        self.threshold.load(Relaxed)
    }

    /// Halve counts once per elapsed window, and recompute the threshold.
    /// Skipped if another thread is already doing it. Unless `force`, also
    /// skipped if no tick elapsed since the last pass: a request may have
    /// seen a stale deadline, set before the pass that just finished.
    pub fn maintain(&self, force: bool) {
        let mut guard = match self.maintenance.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let state = &mut *guard;

        let elapsed_ticks = self.requests.0.load(Relaxed) / self.requests_per_tick;
        if !force && elapsed_ticks <= state.ticks_done {
            return;
        }
        self.next_maintenance
            .store((elapsed_ticks + 1) * self.requests_per_tick, Relaxed);
        let elapsed_windows =
            elapsed_ticks / TICKS_PER_WINDOW - state.ticks_done / TICKS_PER_WINDOW;
        state.ticks_done = elapsed_ticks;
        if elapsed_windows > 0 {
            let shift = u32::try_from(elapsed_windows).unwrap_or(u32::MAX);
            for slot in &self.slots {
                // Leave empty slots alone: no write, no cache line invalidation.
                let _ = slot.try_update(Relaxed, Relaxed, |packed| {
                    let (fingerprint, count) = unpack(packed);
                    (count != 0).then(|| pack(fingerprint, count.checked_shr(shift).unwrap_or(0)))
                });
            }
        }

        // A key can occupy a slot in every row; only count it once, from the
        // row holding its highest count (lowest row on ties).
        state.key_counts.clear();
        for row in 0..DEPTH {
            for slot in &self.slots[row * self.width..(row + 1) * self.width] {
                let (fingerprint, count) = unpack(slot.load(Relaxed));
                if count == 0 {
                    continue;
                }
                let dominated = (0..DEPTH)
                    .filter(|&other_row| other_row != row)
                    .any(|other_row| {
                        let (other_fingerprint, other_count) =
                            unpack(self.slot(fingerprint, other_row).load(Relaxed));
                        other_fingerprint == fingerprint
                            && (other_count > count || (other_count == count && other_row < row))
                    });
                if !dominated {
                    state.key_counts.push(count);
                }
            }
        }

        // (top+1)-th largest count: at most `top` keys are strictly above it.
        let threshold = if state.key_counts.len() > self.top {
            *state
                .key_counts
                .select_nth_unstable_by(self.top, |a, b| b.cmp(a))
                .1
        } else {
            0
        };
        self.threshold.store(threshold.max(1), Relaxed);
    }
}

#[cfg(test)]
mod tests;
