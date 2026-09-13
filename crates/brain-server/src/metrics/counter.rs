//! Monotonic counter primitive — `AtomicU64` under the hood.
//!
//! Counters never decrease. Reset only happens on process restart;
//! PromQL `rate()` handles resets via
//! Prometheus' counter-reset detection.

use std::sync::atomic::{AtomicU64, Ordering};

/// Single counter cell. Cheap to construct (one allocation-free
/// `AtomicU64`); clone via `Arc` if you need shared ownership.
#[derive(Debug, Default)]
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    /// Create a counter at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// Increment by one. Wraps on overflow — at one inc/ns that's
    /// 584 years, so we don't guard against it.
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Add `n` to the counter.
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Read the current value. Use `Relaxed` because metrics are
    /// best-effort eventual-consistency by convention.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn new_starts_at_zero() {
        let c = Counter::new();
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn inc_increments_by_one() {
        let c = Counter::new();
        c.inc();
        c.inc();
        c.inc();
        assert_eq!(c.get(), 3);
    }

    #[test]
    fn add_increments_by_n() {
        let c = Counter::new();
        c.add(42);
        c.add(58);
        assert_eq!(c.get(), 100);
    }

    #[test]
    fn race_free_under_contention() {
        let c = Arc::new(Counter::new());
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let c = c.clone();
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        c.inc();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(), 16 * 10_000);
    }
}
