//! Response rendering — handles the full `{Auto, Table, Wide, Json,
//! Ndjson, Yaml, JsonPath}` output matrix. Verbs return a boxed
//! [`Render`], the dispatcher picks the writer.

pub mod json;
pub mod render;
pub mod table;
pub mod term;

use std::io::{self, IsTerminal, Write};

use serde_json::Value;

pub use crate::parser::OutputFormatArg;

/// Trait implemented by every response we render.
pub trait Render {
    /// Write a table-style rendering to `w`.
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()>;
    /// Build a JSON value for the JSON / ndjson / yaml / jsonpath
    /// modes. Should be the `result` field — `json::write_envelope`
    /// wraps it.
    fn to_json_value(&self) -> Value;
}

/// Resolve `Auto` to a concrete format based on whether stdout is a
/// TTY. Pure function so it's testable without process state.
#[must_use]
pub fn resolve_auto(format: OutputFormatArg, stdout_is_tty: bool) -> OutputFormatArg {
    match format {
        OutputFormatArg::Auto => {
            if stdout_is_tty {
                OutputFormatArg::Table
            } else {
                OutputFormatArg::Ndjson
            }
        }
        other => other,
    }
}

/// Write a rendered response to `w` using the requested format.
pub fn write_rendered(
    w: &mut dyn Write,
    op: &str,
    body: &dyn Render,
    format: OutputFormatArg,
    elapsed_ms: Option<u128>,
) -> io::Result<()> {
    let resolved = resolve_auto(format, io::stdout().is_terminal());
    match resolved {
        OutputFormatArg::Auto => {
            // resolve_auto already mapped this away; defensive arm.
            body.render_table(w)?;
            if let Some(ms) = elapsed_ms {
                writeln!(w, "({} ms)", ms)?;
            }
            Ok(())
        }
        OutputFormatArg::Table | OutputFormatArg::Wide => {
            body.render_table(w)?;
            if let Some(ms) = elapsed_ms {
                writeln!(w, "({} ms)", ms)?;
            }
            Ok(())
        }
        OutputFormatArg::Json => json::write_envelope(w, op, body, elapsed_ms),
        OutputFormatArg::Ndjson => json::write_ndjson(w, op, body),
        OutputFormatArg::Yaml => json::write_yaml(w, op, body, elapsed_ms),
        OutputFormatArg::JsonPath(expr) => json::write_jsonpath(w, body, &expr),
    }
}
