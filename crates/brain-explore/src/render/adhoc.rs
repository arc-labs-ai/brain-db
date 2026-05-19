//! Header + rows table for browse-style commands.
//!
//! Some commands (entity / statement / relation / mention list) produce
//! shapes that don't deserve a dedicated renderer — they're rows of
//! already-stringified columns. This wrapper builds the table from
//! `headers` + `rows` so those callers don't each pull in comfy-table.
//!
//! JSON view: an array of objects keyed by the header strings. That
//! keeps `jq` / jsonpath consumers from having to know about positional
//! columns.

use std::io::{self, Write};

use comfy_table::Cell;
use serde_json::{Map, Value};

use crate::table::build_table;
use crate::{Render, RenderCtx};

/// Lightweight wrapper used by browse-style list commands that build a
/// table out of pre-stringified columns.
pub struct AdHocTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Render for AdHocTable {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        if self.rows.is_empty() {
            return writeln!(w, "(no rows)");
        }
        let mut table = build_table(ctx.policy);
        table.set_header(self.headers.iter().map(Cell::new).collect::<Vec<_>>());
        for row in &self.rows {
            table.add_row(row.iter().map(Cell::new).collect::<Vec<_>>());
        }
        writeln!(w, "{table}")?;
        writeln!(w, "{} rows", self.rows.len())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let items: Vec<Value> = self
            .rows
            .iter()
            .map(|row| {
                let mut obj = Map::new();
                for (h, v) in self.headers.iter().zip(row.iter()) {
                    obj.insert(h.clone(), Value::String(v.clone()));
                }
                Value::Object(obj)
            })
            .collect();
        Value::Array(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    #[test]
    fn empty_writes_no_rows_marker() {
        let t = AdHocTable {
            headers: vec!["a".into()],
            rows: vec![],
        };
        let mut buf = Vec::new();
        t.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("(no rows)"));
    }

    #[test]
    fn renders_rows() {
        let t = AdHocTable {
            headers: vec!["name".into(), "kind".into()],
            rows: vec![
                vec!["alice".into(), "Person".into()],
                vec!["bob".into(), "Person".into()],
            ],
        };
        let mut buf = Vec::new();
        t.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("alice"));
        assert!(s.contains("bob"));
        assert!(s.contains("2 rows"));
    }

    #[test]
    fn json_keys_by_header() {
        let t = AdHocTable {
            headers: vec!["id".into(), "name".into()],
            rows: vec![vec!["1".into(), "alice".into()]],
        };
        let v = t.render_json(&ctx());
        assert_eq!(v[0]["id"], "1");
        assert_eq!(v[0]["name"], "alice");
    }
}
