//! Render `brain-cli agent list / stats / delete` responses.
//!
//! All three are deferred (phase-11 agent index); v1 surfaces them as
//! structured 501s and `main` bails before reaching the renderer. This
//! file provides the rendering shape so the moment the backend lands,
//! the consumer side is ready.
//!
//! agent_id is rendered as the first / featured column in both the list
//! and the stats views — project memory "agent_id is first-class".

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

/// Wrap a list of agent records (raw JSON until the backend ships a
/// stable shape). `agent_id` is the leading column.
pub struct AgentListRendered(pub serde_json::Value);

impl Render for AgentListRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        match &self.0 {
            serde_json::Value::Array(rows) if !rows.is_empty() => {
                let mut keys: Vec<String> = vec!["agent_id".into()];
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
                        .map(|k| match row.get(k) {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(serde_json::Value::Null) | None => String::new(),
                            Some(other) => other.to_string(),
                        })
                        .collect();
                    t.add_row(cells);
                }
                writeln!(w, "{t}")
            }
            serde_json::Value::Array(_) => writeln!(w, "(no agents)"),
            other => writeln!(w, "{other}"),
        }
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        self.0.clone()
    }
}

/// Stats for one agent. The agent_id shows up in its own labeled row
/// (a stacked "card" rather than a column) so operators reading
/// `agent stats <id>` see the id reaffirmed at the top of the output.
pub struct AgentStatsRendered {
    pub agent_id: String,
    pub stats: serde_json::Value,
}

impl Render for AgentStatsRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["agent_id".to_string(), self.agent_id.clone()]);
        if let serde_json::Value::Object(map) = &self.stats {
            for (k, v) in map {
                t.add_row([k.clone(), v.to_string()]);
            }
        }
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "agent_id".into(),
            serde_json::Value::String(self.agent_id.clone()),
        );
        if let serde_json::Value::Object(map) = &self.stats {
            for (k, v) in map {
                obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::Value::Object(obj)
    }
}

pub struct AgentDeleteRendered {
    pub agent_id: String,
    pub status: String,
}

impl Render for AgentDeleteRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["agent_id".to_string(), self.agent_id.clone()]);
        t.add_row(["status".to_string(), self.status.clone()]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        json!({"agent_id": self.agent_id, "status": self.status})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;

    #[test]
    fn agent_list_table_puts_id_first() {
        let item = AgentListRendered(json!([
            {"agent_id": "abc", "memory_count": 7},
        ]));
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        let header_line = out.lines().find(|l| l.contains("agent_id")).unwrap_or("");
        let aid = header_line.find("agent_id").unwrap();
        let mc = header_line.find("memory_count").unwrap();
        assert!(aid < mc, "agent_id must come first: {header_line:?}");
        assert!(out.contains("abc"));
    }

    #[test]
    fn agent_stats_surfaces_id_row() {
        let item = AgentStatsRendered {
            agent_id: "abc".into(),
            stats: json!({"memory_count": 7}),
        };
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("agent_id"));
        assert!(out.contains("abc"));
        let j = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(j.trim()).expect("parse");
        assert_eq!(v["agent_id"], "abc");
        assert_eq!(v["memory_count"], 7);
    }

    #[test]
    fn agent_delete_renders_status() {
        let item = AgentDeleteRendered {
            agent_id: "abc".into(),
            status: "deleted".into(),
        };
        let out = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["agent_id"], "abc");
        assert_eq!(v["status"], "deleted");
    }
}
