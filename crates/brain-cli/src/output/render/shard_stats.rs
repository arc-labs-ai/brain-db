//! Render shard-level stats responses: parsed Prometheus snapshot
//! (`stats`) and the shard topology list (`shard list`).

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

use crate::commands::shard::list::ShardList;
use crate::commands::stats::StatsReport;

/// Newtype wrap so we can impl [`Render`] for the foreign
/// `BTreeMap<String, Vec<MetricSample>>`. Orphan rule.
pub struct ShardStatsRendered(pub StatsReport);

impl Render for ShardStatsRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let mut t = build_table(ctx.policy);
        t.set_header(["metric", "value"]);
        for (name, samples) in &self.0 {
            for s in samples {
                let label_str = if s.labels.is_empty() {
                    String::new()
                } else {
                    let inner: Vec<String> =
                        s.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    format!("{{{}}}", inner.join(","))
                };
                t.add_row([format!("{name}{label_str}"), format!("{}", s.value)]);
            }
        }
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or_else(|_| json!({}))
    }
}

/// Newtype around the shard topology list. Empty body renders a
/// `(no shards)` marker so scripts/tests can detect "fresh cluster" the
/// same way they do for snapshots and workers.
pub struct ShardListRendered(pub ShardList);

impl Render for ShardListRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        if self.0.shards.is_empty() {
            return writeln!(w, "(no shards)");
        }
        let mut t = build_table(ctx.policy);
        t.set_header(["index", "shard_id"]);
        for s in &self.0.shards {
            t.add_row([s.index.to_string(), s.shard_id.to_string()]);
        }
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(json!({"shards": []}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::shard::list::ShardEntry;
    use crate::commands::stats::MetricSample;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;
    use std::collections::BTreeMap;

    fn sample() -> ShardStatsRendered {
        let mut report: StatsReport = BTreeMap::new();
        report.insert(
            "brain_x".into(),
            vec![MetricSample {
                labels: BTreeMap::new(),
                value: 42.0,
            }],
        );
        let mut labels = BTreeMap::new();
        labels.insert("shard".into(), "0".into());
        report.insert("brain_y".into(), vec![MetricSample { labels, value: 1.0 }]);
        ShardStatsRendered(report)
    }

    #[test]
    fn table_lists_metrics() {
        let out = dispatch_to_string(&sample(), OutputFormat::Table).expect("table");
        assert!(out.contains("brain_x"));
        assert!(out.contains("42"));
        assert!(out.contains("brain_y{shard=0}"));
    }

    #[test]
    fn json_round_trips() {
        let out = dispatch_to_string(&sample(), OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["brain_x"][0]["value"], 42.0);
    }

    #[test]
    fn shard_list_empty_marker() {
        let item = ShardListRendered(ShardList { shards: vec![] });
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("(no shards)"));
    }

    #[test]
    fn shard_list_table_includes_rows() {
        let item = ShardListRendered(ShardList {
            shards: vec![
                ShardEntry {
                    index: 0,
                    shard_id: 0,
                },
                ShardEntry {
                    index: 1,
                    shard_id: 1,
                },
            ],
        });
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        // The header column literally reads "shard_id" so existing
        // assertions on that substring keep working.
        assert!(out.contains("shard_id"));
        let j = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(j.trim()).expect("parse");
        assert_eq!(v["shards"][1]["shard_id"], 1);
    }
}
