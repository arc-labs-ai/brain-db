//! Pretty-printed JSON renderer wrapping serde_json.

use serde::Serialize;

pub fn render<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let mut out = serde_json::to_string_pretty(value)?;
    out.push('\n');
    Ok(out)
}
