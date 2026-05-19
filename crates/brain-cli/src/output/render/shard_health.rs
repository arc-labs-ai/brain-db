//! Render shard-state probes: health, ANN rebuild, debug-snapshot.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

use crate::commands::diagnostics::debug_snapshot::DebugSnapshot;
use crate::commands::health::HealthReport;
use crate::commands::rebuild::RebuildReport;

/// Newtype wrapper so we can impl [`Render`] for a type from
/// `crate::commands` — the orphan rule blocks a direct impl.
pub struct ShardHealthRendered(pub HealthReport);

impl Render for ShardHealthRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["status", r.status.as_str()]);
        t.add_row(["admin_endpoint", r.admin_endpoint.as_str()]);
        t.add_row(["probe", r.probe]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        let r = &self.0;
        json!({
            "status": r.status,
            "admin_endpoint": r.admin_endpoint,
            "probe": r.probe,
        })
    }
}

/// Newtype wrap around the ANN rebuild response.
pub struct RebuildRendered(pub RebuildReport);

impl Render for RebuildRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["shard".to_string(), r.shard.to_string()]);
        t.add_row(["entries".to_string(), r.entries.to_string()]);
        t.add_row(["elapsed_ms".to_string(), r.elapsed_ms.to_string()]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        let r = &self.0;
        json!({
            "shard": r.shard,
            "entries": r.entries,
            "elapsed_ms": r.elapsed_ms,
        })
    }
}

/// Newtype wrap for the diagnostics debug-snapshot. Includes the
/// partial-vs-complete flag + the worker rollup so operators can spot
/// what's not yet wired without parsing JSON.
pub struct DebugSnapshotRendered(pub DebugSnapshot);

impl Render for DebugSnapshotRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let snap = &self.0;
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["shard".to_string(), snap.shard.to_string()]);
        t.add_row([
            "captured_at_unix".to_string(),
            snap.captured_at_unix.to_string(),
        ]);
        t.add_row(["partial".to_string(), snap.partial.to_string()]);
        if !snap.deferred.is_empty() {
            t.add_row(["deferred".to_string(), snap.deferred.join(", ")]);
        }
        writeln!(w, "{t}")?;
        if !snap.workers.is_empty() {
            let mut wt = build_table(ctx.policy);
            wt.set_header(["worker", "cycles", "processed", "errors", "last_run_unix"]);
            for wkr in &snap.workers {
                wt.add_row([
                    wkr.name.clone(),
                    wkr.cycles.to_string(),
                    wkr.processed.to_string(),
                    wkr.errors.to_string(),
                    wkr.last_run_unix.to_string(),
                ]);
            }
            writeln!(w, "{wt}")?;
        }
        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::diagnostics::debug_snapshot::WorkerStatus;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;

    fn sample() -> ShardHealthRendered {
        ShardHealthRendered(HealthReport {
            status: "healthy".into(),
            admin_endpoint: "127.0.0.1:9092".into(),
            probe: "/healthz",
        })
    }

    #[test]
    fn renders_table_under_narrow_width() {
        let out = dispatch_to_string(&sample(), OutputFormat::Table).expect("table");
        assert!(out.contains("status"), "table missing status: {out}");
        assert!(out.contains("healthy"), "table missing healthy: {out}");
        assert!(
            out.contains("127.0.0.1:9092"),
            "table missing endpoint: {out}"
        );
    }

    #[test]
    fn json_envelope_shape() {
        let out = dispatch_to_string(&sample(), OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["probe"], "/healthz");
        assert_eq!(v["admin_endpoint"], "127.0.0.1:9092");
    }

    #[test]
    fn rebuild_table_and_json() {
        let item = RebuildRendered(RebuildReport {
            shard: 2,
            entries: 7,
            elapsed_ms: 100,
        });
        let t = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(t.contains("shard"));
        assert!(t.contains("entries"));
        assert!(t.contains("100"));
        let j = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(j.trim()).expect("parse");
        assert_eq!(v["entries"], 7);
    }

    #[test]
    fn debug_snapshot_table_includes_deferred_and_workers() {
        let item = DebugSnapshotRendered(DebugSnapshot {
            shard: 0,
            captured_at_unix: 1_700_000_000,
            partial: true,
            deferred: vec!["active_tasks".into(), "pending_requests".into()],
            workers: vec![WorkerStatus {
                name: "decay".into(),
                cycles: 3,
                processed: 12,
                errors: 0,
                last_run_unix: 1_699_999_000,
            }],
        });
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("partial"));
        assert!(out.contains("active_tasks"));
        assert!(out.contains("pending_requests"));
        assert!(out.contains("decay"));
    }
}
