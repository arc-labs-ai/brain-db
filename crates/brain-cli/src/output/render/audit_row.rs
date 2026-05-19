//! Render audit query / export rows.
//!
//! Both actions are deferred in v1 — the server returns a structured 501
//! which the dispatch loop in `main` surfaces as an error. The renderer
//! here exists so the moment `audit query` lands the surface is ready;
//! it wraps a `serde_json::Value` to stay flexible while the shape is
//! still being designed.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};

pub struct AuditRowsRendered(pub serde_json::Value);

impl Render for AuditRowsRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        match &self.0 {
            serde_json::Value::Array(rows) if !rows.is_empty() => {
                // Render the union of all top-level keys as columns. The
                // agent_id column lives first so operators see whose
                // events these are without scrolling — project memory
                // "agent_id is a first-class noun".
                let mut keys: Vec<String> = Vec::new();
                if rows.iter().any(|r| r.get("agent_id").is_some()) {
                    keys.push("agent_id".into());
                }
                for row in rows {
                    if let serde_json::Value::Object(map) = row {
                        for k in map.keys() {
                            if k != "agent_id" && !keys.iter().any(|s| s == k) {
                                keys.push(k.clone());
                            }
                        }
                    }
                }
                let mut t = build_table(ctx.policy);
                t.set_header(keys.iter().map(String::as_str).collect::<Vec<_>>());
                for row in rows {
                    let cells: Vec<String> = keys
                        .iter()
                        .map(|k| {
                            let v = row.get(k).cloned().unwrap_or(serde_json::Value::Null);
                            match v {
                                serde_json::Value::String(s) => s,
                                serde_json::Value::Null => String::new(),
                                other => other.to_string(),
                            }
                        })
                        .collect();
                    t.add_row(cells);
                }
                writeln!(w, "{t}")
            }
            serde_json::Value::Array(_) => writeln!(w, "(no audit rows)"),
            other => writeln!(w, "{other}"),
        }
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;
    use serde_json::json;

    #[test]
    fn empty_array_marker() {
        let item = AuditRowsRendered(json!([]));
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("(no audit rows)"));
    }

    #[test]
    fn agent_id_is_first_column() {
        let item = AuditRowsRendered(json!([
            {"ts": 1, "agent_id": "agent-001", "op": "encode"},
        ]));
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        let first_line = out.lines().find(|l| l.contains("agent_id")).unwrap_or("");
        // agent_id appears in the header line before "op" — the surface
        // boundary the project memory file insists on.
        let aid = first_line.find("agent_id").unwrap();
        let op = first_line.find("op").unwrap();
        assert!(aid < op, "agent_id must come first: {first_line:?}");
        assert!(out.contains("agent-001"));
    }

    #[test]
    fn json_round_trips() {
        let item = AuditRowsRendered(json!([{"k": "v"}]));
        let out = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v[0]["k"], "v");
    }
}
