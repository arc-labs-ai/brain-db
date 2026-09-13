//! OpenTelemetry tracing — OTLP exporter pipeline.
//!
//! Returns a `Layer` that `bootstrap::logging` composes into the
//! global subscriber. The layer is wired to an OTLP/HTTP exporter
//! sending to the collector at `tracing.endpoint`. If `enabled =
//! false` (or the endpoint is empty), this module installs *no
//! tracer* "no-trace fallback" guarantees the
//! substrate runs unchanged.
//!
//! ## Trace context propagation
//!
//! The wire protocol does not currently carry a `traceparent`
//! header (amendment required). v1 emits
//! server-side spans only; client-supplied trace context is not
//! consumed.
//!
//! ## Glommio note
//!
//! `tracing_opentelemetry::OpenTelemetryLayer` records spans into
//! thread-local context, and `tracing`'s current-span stack is
//! thread-local too. Glommio's per-core executors keep their own
//! thread-locals, so span context does *not* follow the Tokio→Glommio
//! hop on its own. The connection layer (`network::dispatch`) opens the
//! `client.request` span and carries the `tracing::Span` handle (which
//! is `Send + Sync`) across the shard channel in
//! `ShardRequest::DispatchOp`; the shard re-enters it via
//! `.instrument()`, so the `brain.encode` span and its storage
//! sub-spans (`brain.embed`, `brain.wal.append`, `brain.metadata.write`,
//! `brain.hnsw.insert`) nest under the request span as one trace tree.
//! Span *creation* and *close* are thread-agnostic — only the batched
//! OTLP export needs the Tokio runtime, and that runs on its own task,
//! so emitting spans from a Glommio thread is sound.

#![cfg(target_os = "linux")]

use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace as sdk_trace;
use opentelemetry_sdk::trace::{Sampler, TracerProvider};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetryLayer;

use crate::config::TracingConfig;

/// Default OTLP/HTTP endpoint per OTel convention.
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318/v1/traces";

/// Builder result: the `Layer` to install plus the `TracerProvider`
/// the caller keeps alive so its background batch processor stays
/// running. Drop the provider to flush + stop the exporter.
pub struct BuiltTracing {
    pub layer: OpenTelemetryLayer<tracing_subscriber::Registry, sdk_trace::Tracer>,
    pub provider: TracerProvider,
}

/// Build an OpenTelemetry pipeline from `[monitoring.tracing]` config. Returns
/// `None` when tracing is disabled or the resolved sampler is
/// `always_off`.
///
/// # Errors
///
/// Returns `Err` if the OTLP exporter cannot construct its HTTP
/// client (typically a TLS / cert issue) or the endpoint URL is
/// malformed.
pub fn build(cfg: &TracingConfig) -> Result<Option<BuiltTracing>, String> {
    if !cfg.enabled {
        return Ok(None);
    }
    let sampler = resolve_sampler(&cfg.sampler, cfg.sample_ratio);
    if matches!(sampler, Sampler::AlwaysOff) {
        return Ok(None);
    }

    let endpoint = if cfg.endpoint.is_empty() {
        DEFAULT_OTLP_ENDPOINT.to_string()
    } else {
        cfg.endpoint.clone()
    };

    let exporter_builder = opentelemetry_otlp::new_exporter()
        .http()
        .with_endpoint(endpoint);

    let resource = Resource::new(vec![KeyValue::new(
        opentelemetry_semantic_conventions::resource::SERVICE_NAME,
        cfg.service_name.clone(),
    )]);

    let trace_config = sdk_trace::Config::default()
        .with_sampler(sampler)
        .with_resource(resource);

    let provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter_builder)
        .with_trace_config(trace_config)
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .map_err(|e| format!("OTLP pipeline install failed: {e}"))?;

    let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, "brain");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    install_error_handler();

    Ok(Some(BuiltTracing { layer, provider }))
}

/// Feed the trace-pipeline self-metrics from OpenTelemetry's process-global
/// error handler. The batch processor reports a full export buffer as
/// `TraceError::Other` (a "send failed" on the internal channel) and an
/// export failure/timeout as `ExportFailed` / `ExportTimedOut`; we map the
/// former to dropped spans and the latter to export errors. The handler is a
/// process singleton, so this is only meaningfully installed once — a second
/// `build()` (config reload) just re-points it at the same global counters.
fn install_error_handler() {
    use opentelemetry::global::Error as OtelError;
    use opentelemetry::trace::TraceError;

    let metrics = crate::metrics::otel::global().clone();
    let _ = opentelemetry::global::set_error_handler(move |err| {
        match &err {
            OtelError::Trace(TraceError::ExportFailed(_) | TraceError::ExportTimedOut(_)) => {
                metrics.export_errors.inc();
            }
            OtelError::Trace(TraceError::Other(_)) => {
                metrics.spans_dropped.inc();
            }
            _ => {}
        }
        tracing::warn!(otel_error = %err, "opentelemetry trace pipeline error");
    });
}

/// Resolve the sampler.
fn resolve_sampler(name: &str, ratio: f64) -> Sampler {
    match name.to_ascii_lowercase().as_str() {
        "always_on" => Sampler::AlwaysOn,
        "" | "always_off" => Sampler::AlwaysOff,
        "ratio" => Sampler::TraceIdRatioBased(ratio.clamp(0.0, 1.0)),
        "parent_based" => {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio.clamp(0.0, 1.0))))
        }
        // Unknown sampler — fail-closed: don't sample, log a warning
        // at install time "no-trace fallback".
        other => {
            tracing::warn!(
                sampler = other,
                "unknown tracing.sampler; falling back to always_off",
            );
            Sampler::AlwaysOff
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_sampler_handles_known() {
        assert!(matches!(
            resolve_sampler("always_on", 0.0),
            Sampler::AlwaysOn
        ));
        assert!(matches!(
            resolve_sampler("always_off", 0.0),
            Sampler::AlwaysOff
        ));
        assert!(matches!(
            resolve_sampler("ratio", 0.5),
            Sampler::TraceIdRatioBased(_)
        ));
        assert!(matches!(
            resolve_sampler("parent_based", 0.1),
            Sampler::ParentBased(_)
        ));
    }

    #[test]
    fn resolve_sampler_unknown_is_off() {
        assert!(matches!(
            resolve_sampler("nonsense", 0.0),
            Sampler::AlwaysOff
        ));
    }

    #[test]
    fn ratio_is_clamped() {
        assert!(matches!(
            resolve_sampler("ratio", 1.5),
            Sampler::TraceIdRatioBased(_)
        ));
        assert!(matches!(
            resolve_sampler("ratio", -0.5),
            Sampler::TraceIdRatioBased(_)
        ));
    }
}
