//! Tracing/log subscriber installation.
//!
//! The global subscriber is installed **exactly once**, in
//! [`init_pre_config`], and everything after that reconfigures the same
//! subscriber through a [`tracing_subscriber::reload`] handle. This is the
//! fix for the old two-phase design, where a second `try_init` after
//! config load silently failed (a subscriber already existed), leaving the
//! configured log level, JSON formatter, and OpenTelemetry exporter dead.
//!
//! The lifecycle is:
//!
//! 1. [`init_pre_config`] — installs a minimal `compact` / `info`
//!    subscriber before the config is loaded so startup errors are still
//!    captured, and returns a [`LoggingHandle`].
//! 2. [`LoggingHandle::reconfigure`] — called after `Config::load`. Swaps
//!    in the configured formatter (compact / JSON) and level via the reload
//!    handle. This runs before the Tokio runtime exists.
//! 3. [`LoggingHandle::attach_otel`] — called from *inside* the Tokio
//!    runtime. Builds the OTLP pipeline (its batch exporter needs a running
//!    Tokio runtime) and reloads it into the subscriber, so traces actually
//!    export.
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
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Layered, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, reload, EnvFilter, Layer, Registry};

use crate::config::{LoggingConfig, TracingConfig};

use super::tracing as otel;

/// The concrete OpenTelemetry layer type built by [`super::tracing::build`].
type OtelLayer = OpenTelemetryLayer<Registry, opentelemetry_sdk::trace::Tracer>;

/// Type-erased *output* layer over the root `Registry`. Erasing to
/// `Box<dyn Layer>` lets one reload slot swap between the compact and JSON
/// formatters (different concrete types) and add the OTel layer later. The
/// box carries NO per-layer filter — level filtering is a separate, global
/// `EnvFilter` layer (see below), because a per-layer `Filtered` reloaded
/// into the subscriber has no registered `FilterId` and panics at first use.
type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// The subscriber the global filter layers onto: `Registry` plus the
/// reloadable output slot.
type OutputReload = reload::Layer<BoxedLayer, Registry>;
type FilterSubscriber = Layered<OutputReload, Registry>;

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

/// Assemble the reconfigurable *output* layer: a formatter (compact or JSON)
/// plus an optional OTel layer, as sibling layers with no filter of their
/// own. Type-erased so one reload slot can hold any formatter / OTel combo.
/// Level filtering is applied globally by a separate `EnvFilter` layer.
fn build_output(format: LogFormat, otel: Option<OtelLayer>) -> BoxedLayer {
    let fmt_layer: BoxedLayer = match format {
        LogFormat::Compact => Box::new(fmt::layer().with_target(true)),
        LogFormat::Json => Box::new(fmt::layer().with_target(true).json()),
    };
    let mut layers: Vec<BoxedLayer> = vec![fmt_layer];
    if let Some(o) = otel {
        layers.push(Box::new(o));
    }
    Box::new(layers)
}

/// Handle to the single installed subscriber. Reconfiguring logging (and
/// attaching the trace exporter) goes through the two reload slots here
/// rather than trying to install a second global subscriber.
///
/// Two slots because the concerns reload independently: `filter` is a global
/// `EnvFilter` layer (the level knob), `output` is the fmt/OTel box. Keeping
/// the filter global — rather than a per-layer `Filtered` on `output` —
/// avoids the unregistered-`FilterId` panic a reloaded `Filtered` triggers.
#[derive(Clone)]
pub struct LoggingHandle {
    output: reload::Handle<BoxedLayer, Registry>,
    filter: reload::Handle<EnvFilter, FilterSubscriber>,
}

/// Install the single global subscriber with reload handles. Defaults to a
/// `compact` formatter at `info` (honoring `BRAIN_LOG`) so startup errors
/// before config load are captured. Idempotent: only the first call wins;
/// the returned handle reconfigures whichever subscriber is live.
#[must_use = "reconfigure the returned handle after loading config, or logging stays at the compact/info default"]
pub fn init_pre_config() -> LoggingHandle {
    let (output_layer, output) = reload::Layer::new(build_output(LogFormat::Compact, None));
    let (filter_layer, filter) = reload::Layer::new(build_filter("info"));
    let _ = Registry::default()
        .with(output_layer)
        .with(filter_layer)
        .try_init();
    LoggingHandle { output, filter }
}

impl LoggingHandle {
    /// Swap the live subscriber to the configured formatter + level. Called
    /// after `Config::load`, before the Tokio runtime exists, so the
    /// startup logs that follow already honor `[monitoring.logging]`. The
    /// OTel layer is attached separately (it needs the runtime).
    pub fn reconfigure(&self, logging: &LoggingConfig) {
        let (format, warn) = LogFormat::parse(&logging.format);
        let filter_applied = self
            .filter
            .reload(build_filter(logging.level.as_str()))
            .is_ok();
        let output_applied = self.output.reload(build_output(format, None)).is_ok();
        if let Some(msg) = warn {
            tracing::warn!("{msg}");
        }
        info!(
            format = ?format,
            level = %logging.level,
            output = %logging.output,
            applied = filter_applied && output_applied,
            "logging subscriber reconfigured from config"
        );
    }

    /// Build the OpenTelemetry pipeline and reload it into the live
    /// subscriber. MUST be called from inside a running Tokio runtime — the
    /// OTLP batch exporter spawns its background task there.
    ///
    /// Returns the `TracerProvider` when tracing installed — callers keep it
    /// alive and drop it on shutdown to flush spans. Returns `None` when
    /// tracing is disabled, the sampler is `always_off`, or the exporter
    /// failed to build (logged via `warn!`, never fatal).
    #[must_use = "drop the returned TracerProvider on shutdown to flush spans"]
    pub fn attach_otel(
        &self,
        logging: &LoggingConfig,
        tracing_cfg: &TracingConfig,
    ) -> Option<TracerProvider> {
        let built = match otel::build(tracing_cfg) {
            Ok(Some(built)) => built,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "OTel layer build failed; tracing disabled");
                return None;
            }
        };
        // Rebuild the output slot preserving the configured formatter so the
        // reload doesn't regress logging back to compact. (The filter slot is
        // untouched — the level was already applied in `reconfigure`.)
        let (format, _) = LogFormat::parse(&logging.format);
        if self
            .output
            .reload(build_output(format, Some(built.layer)))
            .is_err()
        {
            tracing::warn!("failed to attach OTel layer (subscriber unavailable)");
            return None;
        }
        info!(endpoint = %tracing_cfg.endpoint, "OpenTelemetry trace exporter attached");
        Some(built.provider)
    }
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

    #[test]
    fn build_output_is_reloadable_across_formats() {
        // Exercises the type-erasure: compact and JSON must both produce the
        // same BoxedLayer type so one reload slot can hold either. (Compile-
        // time proof; asserts it constructs.)
        let _compact = build_output(LogFormat::Compact, None);
        let _json = build_output(LogFormat::Json, None);
    }
}
