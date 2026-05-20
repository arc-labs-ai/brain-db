//! Human-friendly age formatting.
//!
//! ENCODE / RECALL renderers want to say "3 ms ago" rather than print
//! a raw `created_at_unix_nanos` value. Centralising the bucketing
//! here keeps the phrasing identical across renderers — operators
//! reading two outputs side-by-side see the same scale.

use std::time::{SystemTime, UNIX_EPOCH};

/// Format an event's age relative to *now* (wall clock).
///
/// `event_unix_nanos == 0` is the wire sentinel for "no timestamp"
/// (substrate-only / test paths). We return `"just now"` in that case
/// so the line still reads cleanly; the LSN sentinel handling lives
/// upstream.
///
/// Buckets:
///   * `< 1s`     → `N ms ago` (or `just now` for ≤ 1ms)
///   * `< 1min`   → `N s ago`
///   * `< 1h`     → `N min ago`
///   * `< 1d`     → `N h ago`
///   * otherwise  → `N d ago`
///
/// Future timestamps (clock skew, replayed events with an out-of-band
/// nanos) read as `just now` rather than negative-time-ago — the
/// renderer's job isn't to expose every clock anomaly, only to give a
/// quick "is this fresh" sense.
#[must_use]
pub fn humanize_age(event_unix_nanos: u64) -> String {
    if event_unix_nanos == 0 {
        return "just now".to_string();
    }
    let now_nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos(),
        Err(_) => return "just now".to_string(),
    };
    let event = u128::from(event_unix_nanos);
    if event >= now_nanos {
        return "just now".to_string();
    }
    let delta_nanos = now_nanos - event;
    humanize_nanos_delta(delta_nanos)
}

/// Pure helper exposed for tests — humanises a positive nanosecond
/// delta without consulting the wall clock.
#[must_use]
pub fn humanize_nanos_delta(delta_nanos: u128) -> String {
    const NS_PER_MS: u128 = 1_000_000;
    const NS_PER_S: u128 = 1_000_000_000;
    const NS_PER_MIN: u128 = 60 * NS_PER_S;
    const NS_PER_H: u128 = 60 * NS_PER_MIN;
    const NS_PER_D: u128 = 24 * NS_PER_H;

    if delta_nanos < NS_PER_MS {
        return "just now".to_string();
    }
    if delta_nanos < NS_PER_S {
        return format!("{} ms ago", delta_nanos / NS_PER_MS);
    }
    if delta_nanos < NS_PER_MIN {
        return format!("{} s ago", delta_nanos / NS_PER_S);
    }
    if delta_nanos < NS_PER_H {
        return format!("{} min ago", delta_nanos / NS_PER_MIN);
    }
    if delta_nanos < NS_PER_D {
        return format!("{} h ago", delta_nanos / NS_PER_H);
    }
    format!("{} d ago", delta_nanos / NS_PER_D)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_unix_nanos_reads_as_just_now() {
        assert_eq!(humanize_age(0), "just now");
    }

    #[test]
    fn sub_millisecond_reads_as_just_now() {
        assert_eq!(humanize_nanos_delta(500), "just now");
    }

    #[test]
    fn milliseconds_bucket() {
        assert_eq!(humanize_nanos_delta(3 * 1_000_000), "3 ms ago");
    }

    #[test]
    fn seconds_bucket() {
        assert_eq!(humanize_nanos_delta(5 * 1_000_000_000), "5 s ago");
    }

    #[test]
    fn minutes_bucket() {
        assert_eq!(humanize_nanos_delta(2 * 60 * 1_000_000_000), "2 min ago");
    }

    #[test]
    fn hours_bucket() {
        assert_eq!(humanize_nanos_delta(3 * 60 * 60 * 1_000_000_000), "3 h ago");
    }

    #[test]
    fn days_bucket() {
        assert_eq!(
            humanize_nanos_delta(2 * 24 * 60 * 60 * 1_000_000_000),
            "2 d ago"
        );
    }
}
