//! The contract every renderable Brain response implements.
//!
//! Method matrix: `render_table` and `render_json` are required; everything
//! else has a default impl off `render_json`. The dispatcher routes by
//! [`OutputFormat`]. Each renderer owns its table view; the structured-data
//! views derive mechanically so a new format doesn't ripple through every
//! impl.

use std::io::{self, Write};

use super::output_format::OutputFormat;
use crate::term::TermPolicy;
use crate::theme::Theme;

/// The bundle every renderer needs in hand — capability bag, theme, and
/// the format being emitted. Passed by reference so a renderer can hand it
/// down to helpers without cloning the contained [`OutputFormat::JsonPath`]
/// expression.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    pub policy: TermPolicy,
    pub theme: Theme,
    pub format: OutputFormat,
}

/// Implemented by every response shape that wants to participate in the
/// `--output` matrix. Only `render_table` and `render_json` are required;
/// the other methods derive from `render_json` so a new format adds zero
/// boilerplate to existing renderers.
pub trait Render {
    /// Human-readable rendering for a terminal. Required.
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()>;

    /// "Wide" variant — extra columns or sections. Defaults to the same
    /// output as [`render_table`](Self::render_table).
    fn render_wide(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        self.render_table(ctx, w)
    }

    /// Structured view used by every machine format. Required.
    fn render_json(&self, ctx: &RenderCtx) -> serde_json::Value;

    /// Default ndjson rendering: one line per element if the JSON view is
    /// an array, otherwise the value as a single line. Callers that need
    /// envelope wrappers override this.
    fn render_ndjson(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        match self.render_json(ctx) {
            serde_json::Value::Array(items) => {
                for item in items {
                    serde_json::to_writer(&mut *w, &item).map_err(io::Error::other)?;
                    writeln!(w)?;
                }
                Ok(())
            }
            other => {
                serde_json::to_writer(&mut *w, &other).map_err(io::Error::other)?;
                writeln!(w)
            }
        }
    }

    /// Default YAML rendering — emit the JSON view as YAML.
    fn render_yaml(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let value = self.render_json(ctx);
        serde_yaml::to_writer(w, &value).map_err(io::Error::other)
    }

    /// Default JSONPath rendering: parse the expression with
    /// [`serde_json_path`], apply it to the JSON view, and emit each match on
    /// its own line. Strings come out unquoted so shell pipelines stay
    /// ergonomic; structured matches keep their JSON shape.
    fn render_jsonpath(&self, ctx: &RenderCtx, path: &str, w: &mut dyn Write) -> io::Result<()> {
        let value = self.render_json(ctx);
        let parsed = serde_json_path::JsonPath::parse(path)
            .map_err(|e| io::Error::other(format!("invalid jsonpath `{path}`: {e}")))?;
        for item in parsed.query(&value).all() {
            match item {
                serde_json::Value::String(s) => writeln!(w, "{s}")?,
                other => {
                    serde_json::to_writer(&mut *w, other).map_err(io::Error::other)?;
                    writeln!(w)?;
                }
            }
        }
        Ok(())
    }
}

/// Route a [`Render`] impl through the format requested in `ctx`.
///
/// `Auto` collapses to either table (TTY) or ndjson (piped) using the
/// detected `stdout_is_tty` capability — the only place in the library that
/// branches on TTY-ness, so policy lives in one spot.
pub fn dispatch(item: &dyn Render, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
    match &ctx.format {
        OutputFormat::Auto => {
            if ctx.policy.stdout_is_tty {
                item.render_table(ctx, w)
            } else {
                item.render_ndjson(ctx, w)
            }
        }
        OutputFormat::Table => item.render_table(ctx, w),
        OutputFormat::Wide => item.render_wide(ctx, w),
        OutputFormat::Json => {
            let value = item.render_json(ctx);
            serde_json::to_writer_pretty(&mut *w, &value).map_err(io::Error::other)?;
            writeln!(w)
        }
        OutputFormat::Ndjson => item.render_ndjson(ctx, w),
        OutputFormat::Yaml => item.render_yaml(ctx, w),
        OutputFormat::JsonPath(p) => item.render_jsonpath(ctx, p, w),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    /// A renderer that records which method was called. Lets dispatch tests
    /// assert routing without parsing the bytes the real renderers emit.
    struct Spy {
        calls: RefCell<Vec<&'static str>>,
        json: serde_json::Value,
    }

    impl Spy {
        fn new(json: serde_json::Value) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                json,
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl Render for Spy {
        fn render_table(&self, _ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
            self.calls.borrow_mut().push("table");
            writeln!(w, "table")
        }
        fn render_wide(&self, _ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
            self.calls.borrow_mut().push("wide");
            writeln!(w, "wide")
        }
        fn render_json(&self, _ctx: &RenderCtx) -> serde_json::Value {
            self.calls.borrow_mut().push("json");
            self.json.clone()
        }
    }

    fn ctx(format: OutputFormat, tty: bool) -> RenderCtx {
        let mut policy = TermPolicy::plain();
        policy.stdout_is_tty = tty;
        RenderCtx {
            policy,
            theme: Theme::default(),
            format,
        }
    }

    #[test]
    fn dispatch_routes_each_variant() {
        let cases = [
            (OutputFormat::Table, "table"),
            (OutputFormat::Wide, "wide"),
            // Json + Ndjson + Yaml all consume `render_json`; the spy
            // records that call.
            (OutputFormat::Json, "json"),
            (OutputFormat::Ndjson, "json"),
            (OutputFormat::Yaml, "json"),
            (OutputFormat::JsonPath("$".into()), "json"),
        ];
        for (format, expected) in cases {
            let spy = Spy::new(json!({"k": "v"}));
            let mut buf = Vec::new();
            dispatch(&spy, &ctx(format.clone(), true), &mut buf).unwrap();
            assert!(
                spy.calls().contains(&expected),
                "format {format:?} did not invoke {expected}, got {:?}",
                spy.calls()
            );
        }
    }

    #[test]
    fn auto_picks_ndjson_when_not_tty() {
        let spy = Spy::new(json!([{"id": 1}, {"id": 2}]));
        let mut buf = Vec::new();
        dispatch(&spy, &ctx(OutputFormat::Auto, false), &mut buf).unwrap();
        assert_eq!(spy.calls(), vec!["json"]);
        let out = String::from_utf8(buf).unwrap();
        // Two array elements → two ndjson lines.
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn auto_picks_table_when_tty() {
        let spy = Spy::new(json!({"k": "v"}));
        let mut buf = Vec::new();
        dispatch(&spy, &ctx(OutputFormat::Auto, true), &mut buf).unwrap();
        assert_eq!(spy.calls(), vec!["table"]);
    }

    #[test]
    fn yaml_renders_from_json() {
        let spy = Spy::new(json!({"name": "alice", "age": 30}));
        let mut buf = Vec::new();
        dispatch(&spy, &ctx(OutputFormat::Yaml, true), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("name: alice"), "yaml missing name: {out}");
        assert!(out.contains("age: 30"), "yaml missing age: {out}");
    }

    #[test]
    fn jsonpath_filters_array() {
        let spy = Spy::new(json!({"items": [{"name": "a"}, {"name": "b"}, {"name": "c"}]}));
        let mut buf = Vec::new();
        dispatch(
            &spy,
            &ctx(OutputFormat::JsonPath("$.items[*].name".into()), true),
            &mut buf,
        )
        .unwrap();
        let lines: Vec<String> = String::from_utf8(buf)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn jsonpath_invalid_expression_surfaces_io_error() {
        let spy = Spy::new(json!({}));
        let mut buf = Vec::new();
        // `not-a-path` doesn't start with `$` — serde_json_path rejects it.
        let err = dispatch(
            &spy,
            &ctx(OutputFormat::JsonPath("not-a-path".into()), true),
            &mut buf,
        )
        .expect_err("invalid jsonpath must error");
        assert!(err.to_string().contains("invalid jsonpath"));
    }
}
