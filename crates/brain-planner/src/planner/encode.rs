//! Planner side for the `ENCODE` cognitive operation.
//!
//! Maps a wire `EncodeRequest` (from `brain-protocol`) into an
//! 8-step `EncodePlan`. Pure: no I/O, no async, no state.
//!
//! Step shape:
//! 1. Idempotency check
//! 2. Embedding
//! 3. Context resolution
//! 4. Slot allocation
//! 5. WAL append + fsync (durability barrier)
//! 6. Apply (arena + metadata + HNSW)
//! 7. Edges
//! 8. Response

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_protocol::envelope::request::EncodeRequest;

use crate::config::PlannerConfig;
use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::{
    ApplyStep, ContextResolutionStep, EmbeddingStep, EncodePlan, EncodeResponseStep, ExecutionPlan,
    IdempotencyCheckStep, SlotAllocationStep, WalAppendStep,
};

/// "the text is non-empty and within size limits".
/// 1 MiB is a generous upper bound; an embed text approaching this
/// size will saturate the tokeniser anyway.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Server-policy memory kind for the shrunk ENCODE contract. ENCODE
/// expresses intent (text + where + when); the kind is decided by the
/// write router, not the client. A future classifier may upgrade this
/// per-memory — until then every text encode files as Episodic.
pub const DEFAULT_ENCODE_KIND: MemoryKind = MemoryKind::Episodic;

/// Server-policy salience floor for the shrunk ENCODE contract.
/// Mirrors the historical `salience_hint` default the client used to
/// send; the router owns it now.
pub const DEFAULT_ENCODE_SALIENCE: f32 = 0.5;

/// Build the execution plan for an ENCODE request.
pub fn plan_encode(req: &EncodeRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    Ok(ExecutionPlan::Encode(plan_encode_inner(req, ctx)?))
}

/// Same as [`plan_encode`] but returns the inner struct directly —
/// useful for tests that want to inspect fields without an enum match.
///
/// ENCODE now carries only client *intent* (text / context / when).
/// The mechanical decisions — kind, salience, dedup, edges — are made
/// server-side here, not read off the request. The plan therefore
/// always reports the policy kind/salience, dedup on, and no edges.
pub fn plan_encode_inner(
    req: &EncodeRequest,
    ctx: &PlannerContext,
) -> Result<EncodePlan, PlanError> {
    validate_text(&req.text)?;

    // Edges are a server concern now (auto/temporal-edge workers wire
    // them post-commit); a text encode never carries client edges.
    let estimated = cost::cost_encode(/* cache_hit */ false, /* edges */ 0);
    cost::check_budget(estimated, ctx)?;

    Ok(EncodePlan {
        shard: 0,
        idempotency_check: IdempotencyCheckStep {
            request_id: RequestId::from(req.request_id),
        },
        embedding: EmbeddingStep {
            text: req.text.clone(),
            cache_lookup: true,
        },
        context_resolution: ContextResolutionStep::Explicit(ContextId::from(req.context_id)),
        allocation: SlotAllocationStep {
            arena_grow_if_needed: true,
        },
        wal_append: WalAppendStep {
            kind: DEFAULT_ENCODE_KIND,
            salience_initial: DEFAULT_ENCODE_SALIENCE,
            fsync: true,
        },
        apply: ApplyStep {
            arena_write: true,
            metadata_write: true,
            hnsw_insert: true,
        },
        // The write router decides edges; ENCODE never carries them.
        edges: Vec::new(),
        response: EncodeResponseStep {
            persistent_id: true,
        },
        estimated_cost_ms: estimated,
        // Dedup is a DB policy, always on for text encode.
        deduplicate: true,
    })
}

/// Text-only validation shared by the text-encode and vector-direct
/// paths: non-empty and within the size cap.
pub fn validate_text(text: &str) -> Result<(), PlanError> {
    if text.is_empty() {
        return Err(PlanError::InvalidParameters {
            field: "text",
            reason: "must be non-empty".to_string(),
        });
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(PlanError::InvalidParameters {
            field: "text",
            reason: format!(
                "{} bytes exceeds MAX_TEXT_BYTES = {MAX_TEXT_BYTES}",
                text.len()
            ),
        });
    }
    Ok(())
}

/// Validate the admin/bulk-import vector-direct encode inputs. That
/// path still carries an explicit kind, salience, and edge list (it is
/// not the shrunk client ENCODE contract), so these checks live here
/// rather than on the [`EncodeRequest`] surface.
pub fn validate_vector_direct(
    text: &str,
    kind: MemoryKind,
    salience_hint: f32,
    edges: &[(MemoryId, EdgeKind, f32)],
    config: &PlannerConfig,
) -> Result<(), PlanError> {
    validate_text(text)?;
    if matches!(kind, MemoryKind::Consolidated) {
        return Err(PlanError::InvalidParameters {
            field: "kind",
            reason: "consolidated memories are produced by background workers, \
                     not by direct encode. Use --kind episodic or --kind semantic."
                .to_string(),
        });
    }
    if !(0.0..=1.0).contains(&salience_hint) {
        return Err(PlanError::InvalidParameters {
            field: "salience_hint",
            reason: format!("{salience_hint} must be in [0, 1]"),
        });
    }
    if edges.len() > config.max_edges_per_encode {
        return Err(PlanError::InvalidParameters {
            field: "edges",
            reason: format!(
                "{} edges exceeds max_edges_per_encode = {}",
                edges.len(),
                config.max_edges_per_encode
            ),
        });
    }
    for (i, (_, _, weight)) in edges.iter().enumerate() {
        if !weight.is_finite() {
            return Err(PlanError::InvalidParameters {
                field: "edges[].weight",
                reason: format!("edge {i} weight {weight} is not finite"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> EncodeRequest {
        EncodeRequest {
            text: "hello".into(),
            context_id: 42,
            request_id: [1u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
        }
    }

    fn unwrap_encode(plan: ExecutionPlan) -> EncodePlan {
        match plan {
            ExecutionPlan::Encode(p) => p,
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    #[test]
    fn happy_path_plan_shape() {
        let plan = unwrap_encode(plan_encode(&base_request(), &PlannerContext::default()).unwrap());
        assert_eq!(plan.shard, 0);
        match plan.context_resolution {
            ContextResolutionStep::Explicit(id) => assert_eq!(id, ContextId(42)),
            other => panic!("expected Explicit, got {other:?}"),
        }
        assert_eq!(
            plan.idempotency_check.request_id,
            RequestId::from([1u8; 16])
        );
        assert!(plan.wal_append.fsync);
        assert!(plan.apply.arena_write);
        assert!(plan.apply.metadata_write);
        assert!(plan.apply.hnsw_insert);
        assert!(plan.response.persistent_id);
        assert!(plan.estimated_cost_ms > 0.0);
    }

    /// The router decides kind/salience/dedup/edges; the plan reflects
    /// the server policy, never client-supplied machinery.
    #[test]
    fn router_defaults_are_applied() {
        let plan = unwrap_encode(plan_encode(&base_request(), &PlannerContext::default()).unwrap());
        assert_eq!(plan.wal_append.kind, DEFAULT_ENCODE_KIND);
        assert!((plan.wal_append.salience_initial - DEFAULT_ENCODE_SALIENCE).abs() < f32::EPSILON);
        assert!(plan.deduplicate, "text encode dedup is always on");
        assert!(plan.edges.is_empty(), "ENCODE never carries client edges");
    }

    #[test]
    fn empty_text_is_rejected() {
        let mut r = base_request();
        r.text = String::new();
        match plan_encode(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "text"),
            other => panic!("expected InvalidParameters[text], got {other:?}"),
        }
    }

    #[test]
    fn oversize_text_is_rejected() {
        let mut r = base_request();
        r.text = "a".repeat(MAX_TEXT_BYTES + 1);
        match plan_encode(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "text");
                assert!(reason.contains("MAX_TEXT_BYTES"));
            }
            other => panic!("expected InvalidParameters[text], got {other:?}"),
        }
    }

    // The vector-direct path retains explicit kind/salience/edges; its
    // validation lives in `validate_vector_direct` and is exercised
    // here directly.

    #[test]
    fn vector_direct_consolidated_kind_is_rejected() {
        let cfg = PlannerConfig::default();
        match validate_vector_direct("hello", MemoryKind::Consolidated, 0.5, &[], &cfg) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "kind");
                assert!(reason.contains("consolidated"));
            }
            other => panic!("expected InvalidParameters[kind], got {other:?}"),
        }
    }

    #[test]
    fn vector_direct_salience_out_of_range_is_rejected() {
        let cfg = PlannerConfig::default();
        for bad in [-0.1f32, 1.1] {
            match validate_vector_direct("hello", MemoryKind::Episodic, bad, &[], &cfg) {
                Err(PlanError::InvalidParameters { field, .. }) => {
                    assert_eq!(field, "salience_hint");
                }
                other => panic!("expected InvalidParameters[salience], got {other:?}"),
            }
        }
    }

    #[test]
    fn vector_direct_too_many_edges_is_rejected() {
        let cfg = PlannerConfig::default();
        // PlannerConfig::default().max_edges_per_encode == 64.
        let edges: Vec<(MemoryId, EdgeKind, f32)> = (0..65)
            .map(|i| (MemoryId::from(i as u128), EdgeKind::References, 0.5))
            .collect();
        match validate_vector_direct("hello", MemoryKind::Episodic, 0.5, &edges, &cfg) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "edges"),
            other => panic!("expected InvalidParameters[edges], got {other:?}"),
        }
    }

    #[test]
    fn vector_direct_non_finite_edge_weight_is_rejected() {
        let cfg = PlannerConfig::default();
        let edges = [(MemoryId::from(1u128), EdgeKind::References, f32::NAN)];
        match validate_vector_direct("hello", MemoryKind::Episodic, 0.5, &edges, &cfg) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "edges[].weight");
            }
            other => panic!("expected InvalidParameters[edges.weight], got {other:?}"),
        }
    }
}
