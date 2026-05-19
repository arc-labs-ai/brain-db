//! Render the `brain-cli extract --backfill` response.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};
use serde_json::json;

use crate::commands::extract::backfill::BackfillReport;
use crate::commands::extract::BackfillKind;

/// Wrap the backfill response together with the selector the operator
/// asked for. The selector isn't on the wire response — repeating it
/// at the top of the table tells operators exactly what they kicked off
/// without scrolling back through the command line.
pub struct ExtractStatusRendered {
    pub selector: BackfillKind,
    pub report: BackfillReport,
}

impl Render for ExtractStatusRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let selector = match self.selector {
            BackfillKind::Memory(id) => format!("memory={id}"),
            BackfillKind::Since(ts) => format!("since={ts}"),
            BackfillKind::All => "all".into(),
        };
        let mut t = build_table(ctx.policy);
        t.set_header(["field", "value"]);
        t.add_row(["selector".to_string(), selector]);
        t.add_row(["shards".to_string(), self.report.shards.to_string()]);
        t.add_row(["enqueued".to_string(), self.report.enqueued.to_string()]);
        t.add_row(["skipped".to_string(), self.report.skipped.to_string()]);
        writeln!(w, "{t}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
        json!({
            "enqueued": self.report.enqueued,
            "skipped": self.report.skipped,
            "shards": self.report.shards,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::dispatch_to_string;
    use brain_explore::OutputFormat;

    fn sample() -> ExtractStatusRendered {
        ExtractStatusRendered {
            selector: BackfillKind::All,
            report: BackfillReport {
                enqueued: 7,
                skipped: 0,
                shards: 2,
            },
        }
    }

    #[test]
    fn table_renders_counts() {
        let out = dispatch_to_string(&sample(), OutputFormat::Table).expect("table");
        assert!(out.contains("selector"));
        assert!(out.contains("all"));
        assert!(out.contains("enqueued"));
    }

    #[test]
    fn json_round_trips() {
        let out = dispatch_to_string(&sample(), OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["enqueued"], 7);
        assert_eq!(v["shards"], 2);
    }
}
