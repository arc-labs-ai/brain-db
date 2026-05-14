//! Gauge primitive — `AtomicI64` so callers can both increment and
//! decrement. Used for "currently in-flight" style measurements.

use std::sync::atomic::{AtomicI64, Ordering};

/// Single gauge cell. Cheap to construct; clone via `Arc` for shared
/// ownership. Signed so `dec` past zero is well-defined (some gauges
/// are deltas from a baseline).
#[derive(Debug, Default)]
pub struct Gauge {
    value: AtomicI64,
}

impl Gauge {
    /// Create a gauge at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: AtomicI64::new(0),
        }
    }

    /// Replace the gauge value.
    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    /// Increment by one.
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement by one.
    pub fn dec(&self) {
        self.value.fetch_sub(1, Ordering::Relaxed);
    }

    /// Read the current value.
    #[must_use]
    pub fn get(&self) -> i64 {
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
        let g = Gauge::new();
        assert_eq!(g.get(), 0);
    }

    #[test]
    fn set_replaces_value() {
        let g = Gauge::new();
        g.set(42);
        g.set(-7);
        assert_eq!(g.get(), -7);
    }

    #[test]
    fn inc_dec_balance() {
        let g = Gauge::new();
        g.inc();
        g.inc();
        g.inc();
        g.dec();
        assert_eq!(g.get(), 2);
    }

    #[test]
    fn race_free_under_contention() {
        let g = Arc::new(Gauge::new());
        let inc_threads: Vec<_> = (0..8)
            .map(|_| {
                let g = g.clone();
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        g.inc();
                    }
                })
            })
            .collect();
        let dec_threads: Vec<_> = (0..8)
            .map(|_| {
                let g = g.clone();
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        g.dec();
                    }
                })
            })
            .collect();
        for t in inc_threads.into_iter().chain(dec_threads) {
            t.join().unwrap();
        }
        assert_eq!(g.get(), 0);
    }
}
