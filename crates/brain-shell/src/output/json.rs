//! JSON output mode — `{ "op": …, "result": …, "elapsed_ms"? }`.

use std::io::{self, Write};

use serde_json::{json, Map, Value};

use super::Render;

/// Write the standard JSON envelope.
pub fn write_envelope(
    w: &mut dyn Write,
    op: &str,
    body: &dyn Render,
    elapsed_ms: Option<u128>,
) -> io::Result<()> {
    let mut envelope = Map::new();
    envelope.insert("op".into(), Value::String(op.to_string()));
    envelope.insert("result".into(), body.to_json_value());
    if let Some(ms) = elapsed_ms {
        // u128 → JSON number: fits if within `u64::MAX`. The shell
        // is hopefully not waiting that long; if it is, we fall
        // back to a string.
        let v = if let Ok(n) = u64::try_from(ms) {
            json!(n)
        } else {
            Value::String(ms.to_string())
        };
        envelope.insert("elapsed_ms".into(), v);
    }
    let line = serde_json::to_string(&Value::Object(envelope))
        .unwrap_or_else(|e| format!("{{\"op\":\"{op}\",\"error\":\"{e}\"}}"));
    writeln!(w, "{line}")
}
