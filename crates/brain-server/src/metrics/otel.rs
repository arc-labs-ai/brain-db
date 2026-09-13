//! OpenTelemetry trace-pipeline self-metrics.
//!
//! Two counters the observability spec mandates under "trace not
//! exported":
//!
//! - `brain_tracing_spans_dropped_total` — a finished span was discarded
//!   because the batch export buffer was full.
//! - `brain_tracing_export_errors_total` — an OTLP export attempt failed
//!   or timed out.
//!
//! Both are fed from OpenTelemetry's process-global error handler
//! (installed in [`crate::bootstrap::tracing`]). Because that handler is
//! itself a singleton, the counters live in a process-global home rather
//! than threaded through `AdminState`; the `/metrics` exposition reads the
//! same instance. The handler classifies by `TraceError` variant:
//! `ExportFailed` / `ExportTimedOut` are export errors; `Other` is the
//! batch processor's "buffer full" send failure, i.e. a dropped span.

use std::sync::{Arc, OnceLock};

use super::counter::Counter;

/// Self-metrics for the OpenTelemetry trace export pipeline.
#[derive(Debug, Default)]
pub struct TracingMetrics {
    /// Spans dropped because the export buffer was full when they ended.
    pub spans_dropped: Counter,
    /// Export attempts that failed or timed out.
    pub export_errors: Counter,
}

static GLOBAL: OnceLock<Arc<TracingMetrics>> = OnceLock::new();

/// The process-wide tracing self-metrics, created on first access so the
/// OTel error handler and the `/metrics` exposition share one instance
/// regardless of which runs first.
#[must_use]
pub fn global() -> &'static Arc<TracingMetrics> {
    GLOBAL.get_or_init(|| Arc::new(TracingMetrics::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_is_stable_across_calls() {
        let a = global();
        let b = global();
        assert!(Arc::ptr_eq(a, b), "global() must return the same instance");
    }

    #[test]
    fn counters_start_at_zero_and_increment() {
        let m = TracingMetrics::default();
        assert_eq!(m.spans_dropped.get(), 0);
        assert_eq!(m.export_errors.get(), 0);
        m.spans_dropped.inc();
        m.export_errors.inc();
        m.export_errors.inc();
        assert_eq!(m.spans_dropped.get(), 1);
        assert_eq!(m.export_errors.get(), 2);
    }
}
