//! Tracing/log subscriber installation.
//!
//! Two entry points:
//!
//! - [`init_pre_config`] — called before the config is loaded so
//!   startup errors are still captured. Defaults to a `compact`
//!   formatter at `info` level; honors `BRAIN_LOG`.
//! - [`reinit_from_config`] — called after `Config::load`. Switches
//!   the formatter and level per the `[monitoring.logging]` section. Because
//!   `tracing` only allows one global subscriber, this is a no-op if
//!   `init_pre_config` already installed one — but the values are
//!   logged for operator visibility.
//!
//! ## Formats supported
//!
//! - `compact` — single-line `<ts> <LEVEL> <target>: <message>`. Dev
//!   default; readable in a terminal.
//! - `json` — newline-delimited JSON. Production
//!   default; ingestible by Loki / Elastic / Splunk.
//!
//! ## Environment
//!
//! There is exactly one log-level knob with one env override: the
//! config `[monitoring.logging] level` is the source of truth, and
//! `BRAIN_LOG` overrides it at runtime (the same env-first /
//! config-fallback pattern Brain uses everywhere else). `RUST_LOG` is
//! deliberately NOT consulted — a second env var that silently wins
//! over the configured level is exactly the kind of surprise this
//! consolidation removes.

#![cfg(target_os = "linux")]

use opentelemetry_sdk::trace::TracerProvider;
use tracing::info;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry};

use crate::config::{LoggingConfig, TracingConfig};

use super::tracing as otel;

/// Resolved log format — one of `compact`, `json`. Unrecognised
/// strings fall back to `Compact` with a warning at install time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Compact,
    Json,
}

impl LogFormat {
    /// Parse the `[monitoring.logging] format = "..."` config knob.
    #[must_use]
    pub fn parse(s: &str) -> (Self, Option<String>) {
        match s.to_ascii_lowercase().as_str() {
            "compact" | "" => (LogFormat::Compact, None),
            "json" => (LogFormat::Json, None),
            other => (
                LogFormat::Compact,
                Some(format!(
                    "unrecognised logging.format `{other}` (allowed: compact, json) — using compact",
                )),
            ),
        }
    }
}

/// Build an [`EnvFilter`] with `default_level` (the configured
/// `[monitoring.logging] level`) as the source of truth and `BRAIN_LOG`
/// as the single runtime override. `RUST_LOG` is intentionally ignored
/// so there is exactly one way to set the level.
fn build_filter(default_level: &str) -> EnvFilter {
    if let Ok(s) = std::env::var("BRAIN_LOG") {
        if let Ok(f) = EnvFilter::try_new(s) {
            return f;
        }
    }
    EnvFilter::new(default_level)
}

/// Install a minimal subscriber before the config is loaded. Idempotent
/// via `try_init` — only the first call succeeds.
pub fn init_pre_config() {
    let filter = build_filter("info");
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

/// Re-install or update the subscriber from `[monitoring.logging]` + `[monitoring.tracing]`.
///
/// Composes three layers:
/// - `EnvFilter` (env-driven level filter).
/// - Format layer (compact or JSON).
/// - Optional OpenTelemetry layer (built by
///   `bootstrap::tracing::build` when `[monitoring.tracing] enabled = true`).
///
/// Returns the `TracerProvider` when the OTel pipeline installed —
/// callers must keep it alive (drop on shutdown to flush). Returns
/// `None` when tracing is disabled or the OTel exporter failed to
/// build (failure is logged via `warn!`, not propagated — tracing
/// degrades to a no-trace fallback rather than failing startup).
#[must_use = "drop the returned TracerProvider on shutdown to flush spans"]
pub fn reinit_from_config(
    logging: &LoggingConfig,
    tracing_cfg: &TracingConfig,
) -> Option<TracerProvider> {
    let (format, warn) = LogFormat::parse(&logging.format);
    let filter = build_filter(logging.level.as_str());

    let (otel_layer, provider) = match otel::build(tracing_cfg) {
        Ok(Some(built)) => (Some(built.layer), Some(built.provider)),
        Ok(None) => (None, None),
        Err(e) => {
            tracing::warn!(error = %e, "OTel layer build failed; tracing disabled");
            (None, None)
        }
    };

    // Compose layers per format. OTel layer is attached first so its
    // `S = Registry` parameter matches; filter + fmt layers (generic
    // over `S`) wrap around it.
    let installed = match format {
        LogFormat::Compact => Registry::default()
            .with(otel_layer)
            .with(filter)
            .with(fmt::layer().with_target(true))
            .try_init()
            .is_ok(),
        LogFormat::Json => Registry::default()
            .with(otel_layer)
            .with(filter)
            .with(fmt::layer().with_target(true).json())
            .try_init()
            .is_ok(),
    };

    if let Some(msg) = warn {
        tracing::warn!("{msg}");
    }
    info!(
        format = ?format,
        level = %logging.level,
        output = %logging.output,
        otel_enabled = provider.is_some(),
        installed,
        "logging subscriber configured"
    );
    provider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compact_is_default() {
        assert_eq!(LogFormat::parse("compact").0, LogFormat::Compact);
        assert_eq!(LogFormat::parse("Compact").0, LogFormat::Compact);
        assert_eq!(LogFormat::parse("").0, LogFormat::Compact);
    }

    #[test]
    fn parse_json_recognised() {
        assert_eq!(LogFormat::parse("json").0, LogFormat::Json);
        assert_eq!(LogFormat::parse("JSON").0, LogFormat::Json);
    }

    #[test]
    fn parse_unknown_falls_back_with_warning() {
        let (fmt, warn) = LogFormat::parse("yaml");
        assert_eq!(fmt, LogFormat::Compact);
        assert!(warn.is_some(), "unknown format must surface a warning");
        assert!(warn.unwrap().contains("yaml"));
    }
}
