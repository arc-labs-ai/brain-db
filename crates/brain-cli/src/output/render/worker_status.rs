//! Render the `brain-cli worker list` response.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

use crate::commands::worker::list::WorkerList;

pub struct WorkerStatusRendered(pub WorkerList);

impl Render for WorkerStatusRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        if self.0.workers.is_empty() {
            return writeln!(w, "(no workers)");
        }
        let mut t = build_table(ctx.policy);
        t.set_header([
            "shard",
            "name",
            "cycles",
            "processed",
            "errors",
            "last_run_unix",
        ]);
        for wkr in &self.0.workers {
            t.add_row([
                wkr.shard.to_string(),
                wkr.name.clone(),
                wkr.cycles.to_string(),
                wkr.processed.to_string(),
                wkr.errors.to_string(),
                wkr.last_run_unix.to_string(),
            ]);
        }
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(json!({"workers": []}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worker::list::WorkerEntry;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;

    fn sample() -> WorkerStatusRendered {
        WorkerStatusRendered(WorkerList {
            workers: vec![WorkerEntry {
                shard: 0,
                name: "decay".into(),
                cycles: 3,
                processed: 12,
                errors: 0,
                last_run_unix: 1_700_000_000,
            }],
        })
    }

    #[test]
    fn table_lists_workers() {
        let out = dispatch_to_string(&sample(), OutputFormat::Table).expect("table");
        assert!(out.contains("decay"));
        assert!(out.contains('3'));
    }

    #[test]
    fn empty_marker() {
        let item = WorkerStatusRendered(WorkerList { workers: vec![] });
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("(no workers)"));
    }

    #[test]
    fn json_round_trips() {
        let out = dispatch_to_string(&sample(), OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["workers"][0]["name"], "decay");
    }
}
