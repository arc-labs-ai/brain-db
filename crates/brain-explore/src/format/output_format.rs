//! How the renderer turns a [`Render`](super::Render) impl into bytes.
//!
//! `Auto` defers the decision until dispatch (table on a TTY, ndjson when
//! piped) so a single command surface — `--output auto` — does the right
//! thing in both human and script contexts. The other variants are
//! explicit overrides; once set, dispatch never second-guesses.

use std::fmt;
use std::str::FromStr;

/// One of the seven supported wire formats for rendered output.
///
/// `JsonPath` carries its expression so the same enum can flow from
/// command-line parsing all the way through dispatch without an extra
/// "and here's the path" parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Pick at dispatch: table on a TTY, ndjson when piped.
    #[default]
    Auto,
    /// Always emit the table renderer.
    Table,
    /// Wide variant of the table renderer (extra columns).
    Wide,
    /// Pretty-printed JSON.
    Json,
    /// Newline-delimited JSON (one record per line).
    Ndjson,
    /// YAML serialisation of the JSON view.
    Yaml,
    /// JSONPath expression applied to the JSON view; one match per line.
    JsonPath(String),
}

/// Error returned by [`OutputFormat::from_str`] when the input doesn't match
/// any known variant or carries a malformed `jsonpath=` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutputFormatError(pub String);

impl fmt::Display for ParseOutputFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown output format: {}", self.0)
    }
}

impl std::error::Error for ParseOutputFormatError {}

impl FromStr for OutputFormat {
    type Err = ParseOutputFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        // Case-insensitive on the bare variants because shells happily forward
        // user typos in casing and this is a user-facing flag.
        match trimmed.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "table" => Ok(Self::Table),
            "wide" => Ok(Self::Wide),
            "json" => Ok(Self::Json),
            "ndjson" => Ok(Self::Ndjson),
            "yaml" => Ok(Self::Yaml),
            other => {
                // `jsonpath=<expr>` is the only variant that carries data.
                if let Some(expr) = other.strip_prefix("jsonpath=") {
                    if expr.is_empty() {
                        Err(ParseOutputFormatError(s.to_string()))
                    } else {
                        // The expression is preserved verbatim — it must round-trip
                        // through Display, and JSONPath is case-sensitive.
                        let raw = trimmed.strip_prefix("jsonpath=").unwrap_or(expr);
                        Ok(Self::JsonPath(raw.to_string()))
                    }
                } else {
                    Err(ParseOutputFormatError(s.to_string()))
                }
            }
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Table => f.write_str("table"),
            Self::Wide => f.write_str("wide"),
            Self::Json => f.write_str("json"),
            Self::Ndjson => f.write_str("ndjson"),
            Self::Yaml => f.write_str("yaml"),
            Self::JsonPath(p) => write!(f, "jsonpath={p}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_default() {
        assert_eq!(OutputFormat::default(), OutputFormat::Auto);
    }

    #[test]
    fn roundtrips_through_from_str_and_display() {
        for f in [
            OutputFormat::Auto,
            OutputFormat::Table,
            OutputFormat::Wide,
            OutputFormat::Json,
            OutputFormat::Ndjson,
            OutputFormat::Yaml,
            OutputFormat::JsonPath("$.memory_id".into()),
        ] {
            let rendered = f.to_string();
            let parsed: OutputFormat = rendered.parse().expect("must parse");
            assert_eq!(parsed, f, "round-trip failed for {rendered}");
        }
    }

    #[test]
    fn jsonpath_carries_expression() {
        let f: OutputFormat = "jsonpath=$.steps[*].text".parse().expect("parses");
        assert_eq!(f, OutputFormat::JsonPath("$.steps[*].text".into()));
    }

    #[test]
    fn jsonpath_empty_expression_is_rejected() {
        let err = "jsonpath=".parse::<OutputFormat>().unwrap_err();
        assert!(err.to_string().contains("jsonpath="));
    }

    #[test]
    fn parse_is_case_insensitive_on_bare_variants() {
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert_eq!(
            "NDJSON".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ndjson
        );
    }

    #[test]
    fn parse_rejects_unknown_variant() {
        assert!("csv".parse::<OutputFormat>().is_err());
    }
}
