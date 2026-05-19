//! JSON / NDJSON / YAML / JsonPath modes.
//!
//! `write_envelope` is the original `{op, result, elapsed_ms?}` shape;
//! the other writers emit the bare `result` body (with or without an op
//! wrapper depending on whether the format is intended for human or
//! tool consumption).

use std::io::{self, Write};

use serde_json::{json, Map, Value};

use super::Render;

/// Pretty-printed JSON envelope.
pub fn write_envelope(
    w: &mut dyn Write,
    op: &str,
    body: &dyn Render,
    elapsed_ms: Option<u128>,
) -> io::Result<()> {
    let envelope = build_envelope(op, body, elapsed_ms);
    let line = serde_json::to_string_pretty(&envelope)
        .unwrap_or_else(|e| format!("{{\"op\":\"{op}\",\"error\":\"{e}\"}}"));
    writeln!(w, "{line}")
}

/// One JSON object per line — `op + result` wrapper preserved so
/// downstream `jq` filters see consistent shape with the envelope mode.
pub fn write_ndjson(w: &mut dyn Write, op: &str, body: &dyn Render) -> io::Result<()> {
    // For arrays we explode into one object per element (one line per
    // record) so `jq -c` and `awk -F` pipelines do the right thing.
    // Singleton values stay as a single line with the op wrapper.
    let result = body.to_json_value();
    match result {
        Value::Array(items) => {
            for item in items {
                let envelope = json!({"op": op, "result": item});
                let line = serde_json::to_string(&envelope)
                    .unwrap_or_else(|e| format!("{{\"op\":\"{op}\",\"error\":\"{e}\"}}"));
                writeln!(w, "{line}")?;
            }
            Ok(())
        }
        other => {
            let envelope = json!({"op": op, "result": other});
            let line = serde_json::to_string(&envelope)
                .unwrap_or_else(|e| format!("{{\"op\":\"{op}\",\"error\":\"{e}\"}}"));
            writeln!(w, "{line}")
        }
    }
}

/// YAML envelope — same shape as `write_envelope`, serialised through
/// a hand-rolled JSON-to-YAML emitter so we don't take a `serde_yaml`
/// dependency for one output mode. Handles the document shapes we
/// emit (scalar / object / array of objects).
pub fn write_yaml(
    w: &mut dyn Write,
    op: &str,
    body: &dyn Render,
    elapsed_ms: Option<u128>,
) -> io::Result<()> {
    let envelope = build_envelope(op, body, elapsed_ms);
    let mut s = String::with_capacity(256);
    s.push_str("---\n");
    emit_yaml(&envelope, &mut s, 0);
    write!(w, "{s}")
}

/// jq-style path extraction. Supported:
///   `.key`            field access
///   `.k1.k2`          nested fields
///   `.arr[0]`         array index
///   `.arr[*].name`    explode array, project field (one line each)
///
/// Resolves against the bare `result` body (not the envelope) so
/// `jsonpath='.memory_id'` returns the value, not `{"op":"…","result":…}`.
pub fn write_jsonpath(w: &mut dyn Write, body: &dyn Render, expr: &str) -> io::Result<()> {
    let root = body.to_json_value();
    let segments = match parse_path(expr) {
        Ok(s) => s,
        Err(e) => {
            return writeln!(w, "jsonpath error: {e}");
        }
    };
    let matches = apply_path(&root, &segments);
    for m in matches {
        let line = match m {
            Value::String(s) => s,
            other => other.to_string(),
        };
        writeln!(w, "{line}")?;
    }
    Ok(())
}

fn build_envelope(op: &str, body: &dyn Render, elapsed_ms: Option<u128>) -> Value {
    let mut envelope = Map::new();
    envelope.insert("op".into(), Value::String(op.to_string()));
    envelope.insert("result".into(), body.to_json_value());
    if let Some(ms) = elapsed_ms {
        let v = if let Ok(n) = u64::try_from(ms) {
            json!(n)
        } else {
            Value::String(ms.to_string())
        };
        envelope.insert("elapsed_ms".into(), v);
    }
    Value::Object(envelope)
}

// ─── YAML emitter ───────────────────────────────────────────────

fn emit_yaml(v: &Value, out: &mut String, indent: usize) {
    match v {
        Value::Null => out.push_str("null\n"),
        Value::Bool(b) => {
            out.push_str(if *b { "true\n" } else { "false\n" });
        }
        Value::Number(n) => {
            out.push_str(&n.to_string());
            out.push('\n');
        }
        Value::String(s) => {
            out.push_str(&yaml_string(s));
            out.push('\n');
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]\n");
                return;
            }
            for item in arr {
                push_indent(out, indent);
                out.push_str("- ");
                emit_yaml_inline(item, out, indent + 2);
            }
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                out.push_str("{}\n");
                return;
            }
            let mut first = true;
            for (k, val) in obj {
                if !first {
                    push_indent(out, indent);
                }
                first = false;
                out.push_str(k);
                out.push_str(": ");
                emit_yaml_inline(val, out, indent + 2);
            }
        }
    }
}

fn emit_yaml_inline(v: &Value, out: &mut String, indent: usize) {
    match v {
        Value::Object(_) | Value::Array(_) => {
            out.push('\n');
            emit_yaml(v, out, indent);
        }
        _ => emit_yaml(v, out, indent),
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push(' ');
    }
}

fn yaml_string(s: &str) -> String {
    // Quote when needed: special characters, leading/trailing space,
    // looks-like-bool/number, contains colon-space (key/value separator).
    let needs_quote = s.is_empty()
        || s.contains(": ")
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains('\n')
        || s.contains('"')
        || matches!(s, "true" | "false" | "null" | "~")
        || s.parse::<f64>().is_ok();
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

// ─── JsonPath subset ────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Seg {
    Field(String),
    Index(usize),
    Wildcard,
}

fn parse_path(expr: &str) -> Result<Vec<Seg>, String> {
    let trimmed = expr.trim();
    let body = trimmed
        .strip_prefix('.')
        .or(Some(trimmed))
        .map(str::to_string)
        .unwrap_or_default();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !cur.is_empty() {
                    segs.push(Seg::Field(std::mem::take(&mut cur)));
                }
            }
            '[' => {
                if !cur.is_empty() {
                    segs.push(Seg::Field(std::mem::take(&mut cur)));
                }
                let mut inner = String::new();
                while let Some(ic) = chars.next() {
                    if ic == ']' {
                        break;
                    }
                    inner.push(ic);
                }
                if inner == "*" {
                    segs.push(Seg::Wildcard);
                } else {
                    let idx: usize = inner
                        .parse()
                        .map_err(|e| format!("bad index `[{inner}]`: {e}"))?;
                    segs.push(Seg::Index(idx));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        segs.push(Seg::Field(cur));
    }
    Ok(segs)
}

fn apply_path(root: &Value, segs: &[Seg]) -> Vec<Value> {
    let mut current = vec![root.clone()];
    for seg in segs {
        let mut next = Vec::new();
        for v in current.drain(..) {
            match seg {
                Seg::Field(k) => {
                    if let Some(obj) = v.as_object() {
                        if let Some(child) = obj.get(k) {
                            next.push(child.clone());
                        }
                    }
                }
                Seg::Index(i) => {
                    if let Some(arr) = v.as_array() {
                        if let Some(child) = arr.get(*i) {
                            next.push(child.clone());
                        }
                    }
                }
                Seg::Wildcard => {
                    if let Some(arr) = v.as_array() {
                        for child in arr {
                            next.push(child.clone());
                        }
                    }
                }
            }
        }
        current = next;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock(Value);
    impl Render for Mock {
        fn render_table(&self, _w: &mut dyn Write) -> io::Result<()> {
            Ok(())
        }
        fn to_json_value(&self) -> Value {
            self.0.clone()
        }
    }

    #[test]
    fn ndjson_explodes_array_into_lines() {
        let mock = Mock(json!([{"a": 1}, {"a": 2}]));
        let mut buf = Vec::new();
        write_ndjson(&mut buf, "test", &mock).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 2);
        assert!(s.contains("\"a\":1"));
        assert!(s.contains("\"a\":2"));
    }

    #[test]
    fn ndjson_singleton_is_one_line() {
        let mock = Mock(json!({"a": 1}));
        let mut buf = Vec::new();
        write_ndjson(&mut buf, "test", &mock).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 1);
    }

    #[test]
    fn yaml_renders_simple_object() {
        let mock = Mock(json!({"name": "alice", "age": 30}));
        let mut buf = Vec::new();
        write_yaml(&mut buf, "test", &mock, None).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("op: test"));
        assert!(s.contains("name: alice"));
        assert!(s.contains("age: 30"));
    }

    #[test]
    fn jsonpath_extracts_scalar() {
        let mock = Mock(json!({"memory_id": "0xdeadbeef", "lsn": 17}));
        let mut buf = Vec::new();
        write_jsonpath(&mut buf, &mock, ".memory_id").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().trim(), "0xdeadbeef");
    }

    #[test]
    fn jsonpath_explodes_wildcard() {
        let mock = Mock(json!([{"name": "a"}, {"name": "b"}, {"name": "c"}]));
        let mut buf = Vec::new();
        write_jsonpath(&mut buf, &mock, ".[*].name").unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn jsonpath_indexed_access() {
        let mock = Mock(json!({"items": [{"id": 1}, {"id": 2}]}));
        let mut buf = Vec::new();
        write_jsonpath(&mut buf, &mock, ".items[1].id").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().trim(), "2");
    }
}
