//! Render the `brain-cli config get` response.
//!
//! v1 only `get` is wired; `reload` / `set` return 501 and bail before
//! reaching the renderer. The response shape is a free-form JSON value
//! (top-level object for whole-config dumps, scalar/object for keyed
//! queries) so the table view flattens objects to a two-column kv view
//! and prints scalars as-is.

use std::io::{self, Write};

use brain_explore::{table::build_table, Render, RenderCtx};

/// Newtype around `serde_json::Value` — orphan rule, plus a stable name
/// makes the `Render` impl easy to find when adding new config formats.
pub struct ConfigDumpRendered(pub serde_json::Value);

impl Render for ConfigDumpRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        match &self.0 {
            serde_json::Value::Object(map) => {
                let mut t = build_table(ctx.policy);
                t.set_header(["key", "value"]);
                for (k, v) in map {
                    t.add_row([k.clone(), v.to_string()]);
                }
                writeln!(w, "{t}")
            }
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
    fn object_renders_as_table() {
        let item = ConfigDumpRendered(json!({"server": {"listen_addr": "127.0.0.1:9090"}}));
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("server"));
        assert!(out.contains("listen_addr"));
    }

    #[test]
    fn scalar_passes_through() {
        let item = ConfigDumpRendered(json!("127.0.0.1:9090"));
        let out = dispatch_to_string(&item, OutputFormat::Table).expect("table");
        assert!(out.contains("127.0.0.1:9090"));
    }

    #[test]
    fn json_round_trips() {
        let item = ConfigDumpRendered(json!({"a": 1}));
        let out = dispatch_to_string(&item, OutputFormat::Json).expect("json");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("parse");
        assert_eq!(v["a"], 1);
    }
}
