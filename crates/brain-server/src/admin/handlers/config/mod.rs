//! Admin HTTP handlers for `config` (spec §14/06 §7; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/config[?key=a.b.c]` → 200 + JSON (whole config or subtree).
//! - `POST /v1/config/reload` → 501 (no live-reload pathway yet).
//! - `POST /v1/config?key=…` → 501 (no editable in-memory store).

mod get;
mod reload;
mod set;

pub use get::get;
pub use reload::reload;
pub use set::set;

/// Walk a serialized config tree by dotted path. Used by
/// `GET /v1/config?key=...` to scope the response to a subtree.
pub(super) fn walk<'a>(root: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut cursor = root;
    for segment in dotted.split('.') {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_steps_segments() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":{"b":{"c":42}},"x":1}"#).unwrap();
        assert_eq!(walk(&v, "a.b.c").unwrap(), &serde_json::json!(42));
        assert_eq!(walk(&v, "x").unwrap(), &serde_json::json!(1));
        assert!(walk(&v, "missing.path").is_none());
    }
}
