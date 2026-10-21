mod table;

use table::Table;

varnish::run_vtc_tests!("tests/*.vtc");

/// Tracks how often strings are seen, and tells whether one is among the most
/// frequent ones.
pub struct Pool {
    table: Table,
    /// Random per-pool hash seed, so clients can't precompute colliding strings.
    seed: u64,
}

impl Pool {
    fn hash(&self, key: &std::ffi::CStr) -> u64 {
        xxhash_rust::xxh3::xxh3_64_with_seed(key.to_bytes(), self.seed)
    }
}

/// Approximate, lock-free tracking of popular strings (typically URLs).
#[varnish::vmod(docs = "API.md")]
mod popular {
    use std::ffi::CStr;

    use super::Pool;
    use super::table::{MAX_SIZE, MAX_WINDOW, Table};

    impl Pool {
        /// Create a pool considering the `top` most frequent strings as popular.
        ///
        /// `size` is the number of tracked entries (default: `10 * top`, must be
        /// larger than `top` and at most 2^28), and `window` the number of
        /// requests after which all counts are halved, letting old popular
        /// strings fade away (default: `1000 * top` capped at 2^30, must be
        /// between `size` and 2^30).
        pub fn new(top: i64, size: Option<i64>, window: Option<i64>) -> Result<Self, String> {
            if top <= 0 {
                return Err(format!("popular: top must be positive (got {top})"));
            }
            let size = size.unwrap_or(top.saturating_mul(10));
            if size <= top {
                return Err(format!(
                    "popular: size ({size}) must be larger than top ({top})"
                ));
            }
            if size as u64 > MAX_SIZE as u64 {
                return Err(format!("popular: size ({size}) must be at most {MAX_SIZE}"));
            }
            let window = window.unwrap_or(top.saturating_mul(1000).min(MAX_WINDOW as i64));
            if window < size || window as u64 > MAX_WINDOW {
                return Err(format!(
                    "popular: window ({window}) must be between size ({size}) and {MAX_WINDOW}"
                ));
            }
            Ok(Pool {
                table: Table::new(top as usize, size as usize, window as u64),
                seed: fastrand::u64(..),
            })
        }

        /// Count one occurrence of `key`, and return whether it is popular.
        pub fn is_popular(&self, key: &CStr) -> bool {
            self.table.hit(self.hash(key)) > self.table.threshold()
        }

        /// Estimated count of `key`, without counting it. For debugging.
        pub fn count(&self, key: &CStr) -> i64 {
            i64::from(self.table.estimate(self.hash(key)))
        }

        /// Count a string must exceed to be popular. For debugging.
        pub fn threshold(&self) -> i64 {
            i64::from(self.table.threshold())
        }

        /// Run a maintenance pass now: update the threshold, and halve counts
        /// if a window elapsed. For testing.
        pub fn maintain(&self) {
            self.table.maintain(true);
        }
    }
}
