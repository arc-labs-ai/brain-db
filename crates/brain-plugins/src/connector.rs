//! Connector plugins — pull from external sources before encode.
//!
//! A connector is a polled source of raw memories: it receives a
//! `since` watermark, returns items newer than the watermark, and
//! reports the next watermark to persist. The substrate's connector
//! scheduler (not in this slice) drives a per-connector cadence and
//! routes returned items through the encode pipeline.

use crate::errors::PluginResult;
use crate::recall::RecallPlugin;

/// Per-fetch request handed to the connector.
#[derive(Debug, Clone)]
pub struct ConnectorRequest {
    /// Free-text filter the connector applies upstream (the empty
    /// string means "everything").
    pub query: String,
    /// Watermark from the prior successful fetch. `None` for the very
    /// first call (the connector should choose a sensible default, e.g.
    /// "the past 24 hours").
    pub since_unix_nanos: Option<u64>,
    /// Hard cap on items returned. The host treats more than this as a
    /// contract violation and enforces it itself: `fetch_from_connector`
    /// truncates any excess (fail-open) so an over-eager or hostile
    /// connector cannot flood the encode pipeline.
    pub max_items: u32,
}

/// One item returned by a connector.
#[derive(Debug, Clone)]
pub struct ConnectorItem {
    /// Upstream-stable id (e.g. message id, file id). Used by the host
    /// to deduplicate across fetches.
    pub external_id: String,
    /// Raw text the encode pipeline will index and extract from.
    pub text: String,
    /// Wall-clock at which the upstream source reports the item was
    /// created.
    pub created_at_unix_nanos: u64,
    /// Optional source URL (e.g. permalink in the upstream system).
    pub source_url: Option<String>,
    /// Plugin-supplied tags applied verbatim to the encoded memory.
    pub tags: Vec<String>,
}

/// Response shape every connector returns.
#[derive(Debug, Clone, Default)]
pub struct ConnectorResponse {
    /// Items pulled from upstream, ordered however the connector
    /// chooses. The host preserves order through the encode pipeline.
    pub items: Vec<ConnectorItem>,
    /// New watermark to hand back on the next fetch. The connector
    /// scheduler persists this verbatim and uses it as
    /// `since_unix_nanos` on the next call.
    pub next_since_unix_nanos: u64,
}

/// Plugin trait for upstream connectors.
///
/// Implementors must also implement [`RecallPlugin`] for the lifecycle
/// hooks.
pub trait ConnectorPlugin: RecallPlugin {
    /// Fetch new items from the upstream source.
    ///
    /// Called on a per-connector cadence by the substrate's connector
    /// scheduler. The connector must respect `req.max_items` and emit
    /// at least monotonically-non-decreasing `next_since_unix_nanos`
    /// values so the scheduler's watermark never goes backward.
    ///
    /// Errors propagate to the scheduler, which logs them and retries
    /// on the next tick. Repeated failures are reported via the audit
    /// trail; a connector that fails N times in a row may be paused.
    fn fetch(&self, req: ConnectorRequest) -> PluginResult<ConnectorResponse>;
}
