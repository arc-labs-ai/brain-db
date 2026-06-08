//! Shared parsers for worker env-var knobs.
//!
//! Every periodic worker reads a `*_INTERVAL_SECONDS` / `*_GRACE_SECONDS`
//! / `*_ENABLED` override from the environment. These were copy-pasted
//! per worker (`parse_interval_override` lived verbatim in three files);
//! the formula now lives here once.

use std::time::Duration;

/// Parse a positive seconds value. Returns `None` for unset, empty,
/// non-numeric, negative, or zero — where zero means "no override, use
/// the worker's default cadence" rather than "every 0 seconds".
#[must_use]
pub fn parse_positive_seconds(raw: Option<&str>) -> Option<u64> {
    let v: u64 = raw?.trim().parse().ok()?;
    (v > 0).then_some(v)
}

/// Parse a positive-seconds interval override into a [`Duration`].
/// `None` under the same conditions as [`parse_positive_seconds`].
#[must_use]
pub fn parse_interval_override(raw: Option<&str>) -> Option<Duration> {
    parse_positive_seconds(raw).map(Duration::from_secs)
}

/// Parse a boolean enable flag. `1` / `true` / `yes` / `on`
/// (case-insensitive, trimmed) enable; everything else — including
/// unset — is `false`.
#[must_use]
pub fn parse_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_seconds_rejects_invalid() {
        assert_eq!(parse_positive_seconds(Some("2592000")), Some(2_592_000));
        assert_eq!(parse_positive_seconds(Some(" 1 ")), Some(1));
        assert!(parse_positive_seconds(None).is_none());
        assert!(parse_positive_seconds(Some("")).is_none());
        assert!(parse_positive_seconds(Some("0")).is_none());
        assert!(parse_positive_seconds(Some("-5")).is_none());
        assert!(parse_positive_seconds(Some("abc")).is_none());
    }

    #[test]
    fn interval_override_maps_to_duration() {
        assert_eq!(parse_interval_override(Some("60")), Some(Duration::from_secs(60)));
        assert_eq!(parse_interval_override(Some("300")), Some(Duration::from_secs(300)));
        assert_eq!(parse_interval_override(Some("0")), None);
        assert_eq!(parse_interval_override(None), None);
        assert_eq!(parse_interval_override(Some("not-a-number")), None);
        assert_eq!(parse_interval_override(Some("-5")), None);
    }

    #[test]
    fn enabled_truthy_and_falsey() {
        for t in ["1", "true", "TRUE", "Yes", "on", " on "] {
            assert!(parse_enabled(Some(t)), "{t} should enable");
        }
        for f in [None, Some(""), Some("0"), Some("false"), Some("nope")] {
            assert!(!parse_enabled(f), "{f:?} should not enable");
        }
    }
}
