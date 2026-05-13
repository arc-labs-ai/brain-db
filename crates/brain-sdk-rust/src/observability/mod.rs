//! SDK-level observability: tracing-span attributes + internal
//! metric counters. Spec §13/07.
//!
//! 10.7 ships the minimum that's useful for v1:
//! - [`MetricsSnapshot`] — point-in-time copy of the
//!   internal counters; applications poll it.
//! - [`attributes`] — OpenTelemetry-style key constants used in
//!   tracing spans.
//!
//! Direct `prometheus_client` / `opentelemetry-otlp` exporter
//! integrations are application choices; 10.7 hands callers
//! the data and they wire it wherever they like.

pub mod attributes;
pub mod metrics;

pub use metrics::{InFlightGuard, MetricsSnapshot, MetricsState, OpCounters, OpMetrics};
