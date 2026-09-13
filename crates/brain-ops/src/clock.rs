//! The shard wall-clock, as a single source of unix-nanoseconds.
//!
//! Handlers and the writer all stamp records with "now". This is the one
//! definition — previously ~8 modules each had their own private
//! `now_unix_nanos()`. Centralizing it removes the drift and gives a
//! single seam for a future injectable test clock.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock as unix-nanoseconds, saturating at 0 on a
/// pre-epoch clock (never panics).
#[must_use]
pub fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
