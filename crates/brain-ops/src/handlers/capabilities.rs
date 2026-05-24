//! Handler for the `GET_CAPABILITIES` wire op.
//!
//! Reads the per-shard `OpsContext` slots that gate behaviour and
//! projects them onto a wire `Capabilities` snapshot. The op is
//! intentionally cheap — no embedder calls, no HNSW reads — so a
//! client can warm-up its session without paying for one.
//!
//! Capability bits are derived as follows:
//!
//! * `rerank` — `OpsContext.cross_encoder` is `Enabled`. Reflects
//!   the operator's `[rerank] enabled = true/false` config plus the
//!   model-load outcome at shard spawn (a failed load is a spawn
//!   error, so by the time we get here `Enabled` means a working
//!   encoder).
//! * `pattern_extractor` / `classifier_extractor` / `llm_extractor` —
//!   the registry's tier gate is `Enabled` for the matching tier
//!   AND at least one wired extractor of that kind is registered.
//!   Returning `false` for "gated enabled but no wired row" tells
//!   the client the capability is mute, even though the operator
//!   opted in — useful diagnostic surface for "API key missing"
//!   style misconfigurations.
//! * `schema_namespaces` — every namespace with an active schema
//!   version on the shard. Excludes empty-string entries defensively
//!   (the system schema is normally `brain`; user schemas have
//!   non-empty names).
//! * `vector_dim` — the embedder's output dimensionality (currently
//!   384 for BGE-small). Hard-coded to the substrate constant so the
//!   wire response stays accurate even when the dispatcher is a
//!   per-shard stub (tests).

use brain_core::ExtractorKind;
use brain_metadata::schema::store::schema_namespaces;
use brain_protocol::envelope::response::{
    Capabilities, GetCapabilitiesRequest, GetCapabilitiesResponse,
};

use crate::context::{CrossEncoderSlot, OpsContext};
use crate::error::OpError;

/// Embedder output dimensionality. Mirrors `brain_embed::VECTOR_DIM`
/// (384 for BGE-small). Kept as a local constant so this crate doesn't
/// have to depend on `brain-embed`; the value is part of Brain's
/// substrate contract and shouldn't change underneath us.
const VECTOR_DIM_U16: u16 = 384;

pub async fn handle_get_capabilities(
    _req: GetCapabilitiesRequest,
    ctx: &OpsContext,
) -> Result<GetCapabilitiesResponse, OpError> {
    let rerank = matches!(ctx.cross_encoder, CrossEncoderSlot::Enabled(_));

    // Snapshot the registry under one read lock — three booleans
    // out, no need to hold the lock for the I/O steps below.
    let (pattern_extractor, classifier_extractor, llm_extractor) = {
        let registry = ctx.extractor_registry.read();
        let gate = registry.tier_gate();
        let mut pattern = false;
        let mut classifier = false;
        let mut llm = false;
        for ext in registry.iter_enabled() {
            match ext.kind() {
                ExtractorKind::Pattern => pattern = true,
                ExtractorKind::Classifier => classifier = true,
                ExtractorKind::Llm => {
                    // Only wired LLM extractors count — a degraded
                    // row (no API key, unknown model) is registered
                    // for diagnostics but can't produce statements.
                    if ext.is_wired() {
                        llm = true;
                    }
                }
            }
        }
        (
            pattern && gate.pattern.is_enabled(),
            classifier && gate.classifier.is_enabled(),
            llm && gate.llm.is_enabled(),
        )
    };

    // Per-shard schema namespaces. A failure here downgrades to "no
    // schemas" rather than killing the handler — capability discovery
    // shouldn't poison a session when redb is briefly busy.
    let schema_namespaces: Vec<String> = {
        let db = ctx.executor.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| OpError::Internal(format!("metadata read_txn: {e}")))?;
        match schema_namespaces(&rtxn) {
            Ok(list) => list.into_iter().filter(|n| !n.is_empty()).collect(),
            Err(err) => {
                tracing::warn!(
                    target: "brain_ops::capabilities",
                    error = %err,
                    "schema_namespaces read failed; reporting empty list",
                );
                Vec::new()
            }
        }
    };

    Ok(GetCapabilitiesResponse {
        capabilities: Capabilities {
            rerank,
            llm_extractor,
            classifier_extractor,
            pattern_extractor,
            schema_namespaces,
            vector_dim: VECTOR_DIM_U16,
        },
    })
}
