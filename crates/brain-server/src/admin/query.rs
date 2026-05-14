//! Shared query-string parsers for admin handlers.
//!
//! Consolidates the four near-identical `parse_shard` / `parse_key`
//! copies that previously lived in `snapshot.rs`, `rebuild.rs`,
//! `worker.rs`, `diagnostics.rs`, and `config_route.rs`.

/// Parse `?shard=N` from a URI query string. Returns `0` when the
/// query is empty or has no `shard=` parameter — the historical
/// default for handlers that always target a specific shard
/// (snapshot, rebuild, debug-snapshot).
pub fn shard_required(query: &str) -> Result<usize, String> {
    if query.is_empty() {
        return Ok(0);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map_err(|e| format!("invalid shard: {e}"));
        }
    }
    Ok(0)
}

/// Parse `?shard=N` returning `None` when the query is empty or has
/// no `shard=` parameter. Used by handlers that filter optionally
/// (worker list).
pub fn shard_optional(query: &str) -> Result<Option<usize>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map(Some)
                .map_err(|e| format!("invalid shard: {e}"));
        }
    }
    Ok(None)
}

/// Parse `?key=dotted.path` returning `None` if absent or empty.
/// Used by the config-get handler.
pub fn config_key(query: &str) -> Option<&str> {
    if query.is_empty() {
        return None;
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("key=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_required_defaults_to_zero() {
        assert_eq!(shard_required("").unwrap(), 0);
        assert_eq!(shard_required("other=1").unwrap(), 0);
    }

    #[test]
    fn shard_required_explicit() {
        assert_eq!(shard_required("shard=3").unwrap(), 3);
        assert_eq!(shard_required("other=1&shard=7").unwrap(), 7);
        assert_eq!(shard_required("duration_secs=30&shard=5").unwrap(), 5);
    }

    #[test]
    fn shard_required_rejects_garbage() {
        assert!(shard_required("shard=abc").is_err());
    }

    #[test]
    fn shard_optional_returns_none_when_absent() {
        assert_eq!(shard_optional("").unwrap(), None);
        assert_eq!(shard_optional("other=1").unwrap(), None);
    }

    #[test]
    fn shard_optional_extracts() {
        assert_eq!(shard_optional("shard=2").unwrap(), Some(2));
    }

    #[test]
    fn shard_optional_rejects_garbage() {
        assert!(shard_optional("shard=abc").is_err());
    }

    #[test]
    fn config_key_extracts_dotted() {
        assert_eq!(config_key(""), None);
        assert_eq!(
            config_key("key=workers.decay.interval"),
            Some("workers.decay.interval")
        );
        assert_eq!(config_key("other=1"), None);
        assert_eq!(config_key("key="), None);
    }
}
