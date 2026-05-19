//! Render the snapshot family — create / list / delete responses.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

use crate::commands::snapshot::create::CreateReport;
use crate::commands::snapshot::delete::DeleteReport;
use crate::commands::snapshot::list::ListEntry;

pub struct SnapshotCreateRendered(pub CreateReport);
pub struct SnapshotListRendered(pub Vec<ListEntry>);
pub struct SnapshotDeleteRendered(pub DeleteReport);
pub struct SnapshotRestoreStubRendered(pub u64);

impl Render for SnapshotCreateRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["id".to_string(), r.id.to_string()]);
        t.add_row(["shard".to_string(), r.shard.to_string()]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        json!({"id": self.0.id, "shard": self.0.shard})
    }
}

impl Render for SnapshotListRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        if self.0.is_empty() {
            // Preserve the "(no snapshots)" hint that scripts and tests rely
            // on — empty comfy-tables look like noise without it.
            return writeln!(w, "(no snapshots)");
        }
        let mut t = build_table(ctx.policy);
        t.set_header(["shard", "id", "size_bytes", "taken_at_unix_nanos"]);
        for e in &self.0 {
            t.add_row([
                e.shard.to_string(),
                e.id.to_string(),
                e.size_bytes.to_string(),
                e.taken_at_unix_nanos.to_string(),
            ]);
        }
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(json!([]))
    }
}

impl Render for SnapshotDeleteRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["id".to_string(), r.id.to_string()]);
        t.add_row(["shard".to_string(), r.shard.to_string()]);
        t.add_row(["status".to_string(), r.status.clone()]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        json!({
            "id": self.0.id,
            "shard": self.0.shard,
            "status": self.0.status,
        })
    }
}

impl Render for SnapshotRestoreStubRendered {
    fn render_table(&self, _ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        // Restore is destructive and v1 expects an operator-driven runbook;
        // the message points at the workflow rather than a half-wired call.
        writeln!(
            w,
            "snapshot restore <id={id}>: not yet supported in v1.\n\
             Restore is destructive and requires the substrate to be stopped.\n\
             v1 workflow: stop brain-server → swap files → restart. A scripted\n\
             online-restore lands in v2 and is tracked separately.",
            id = self.0
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        json!({
            "id": self.0,
            "status": "not_yet_supported",
            "detail": "v1 restore is an operator runbook, not a CLI one-liner",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;

    #[test]
    fn create_table_and_json() {
        let item = SnapshotCreateRendered(CreateReport { id: 1, shard: 0 });
        let t = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(t.contains("id"));
        assert!(t.contains('1'));
        let j = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(j.trim()).expect("parse");
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn list_empty_keeps_marker() {
        let item = SnapshotListRendered(Vec::new());
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("(no snapshots)"));
    }

    #[test]
    fn list_renders_rows() {
        let item = SnapshotListRendered(vec![ListEntry {
            shard: 0,
            id: 7,
            taken_at_unix_nanos: 1_700_000_000_000_000_000,
            size_bytes: 4096,
        }]);
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains('7'));
        assert!(out.contains("4096"));
    }

    #[test]
    fn restore_stub_message_is_self_explanatory() {
        let out = dispatch_to_string(&SnapshotRestoreStubRendered(42), OutputFormat::Table)
            .expect("table");
        assert!(out.contains("not yet supported"));
        assert!(out.contains("42"));
    }
}
