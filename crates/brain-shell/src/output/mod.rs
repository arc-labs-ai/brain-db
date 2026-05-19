//! Response rendering — `table` (human) and `json` (machine).

pub mod json;
pub mod table;

use std::io::{self, Write};

use serde_json::Value;

pub use crate::parser::OutputFormatArg;

/// Trait implemented by every response we render.
pub trait Render {
    /// Write a table-style rendering to `w`.
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()>;
    /// Build a JSON value for the `json` output format. Should be
    /// the `result` field — `json::render_envelope` wraps it.
    fn to_json_value(&self) -> Value;
}

/// Write a rendered response to `w` using the requested format.
pub fn write_rendered(
    w: &mut dyn Write,
    op: &str,
    body: &dyn Render,
    format: OutputFormatArg,
    elapsed_ms: Option<u128>,
) -> io::Result<()> {
    match format {
        OutputFormatArg::Table => {
            body.render_table(w)?;
            if let Some(ms) = elapsed_ms {
                writeln!(w, "({} ms)", ms)?;
            }
            Ok(())
        }
        OutputFormatArg::Json => json::write_envelope(w, op, body, elapsed_ms),
    }
}
