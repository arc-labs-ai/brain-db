//! Planner side for the `ENCODE` cognitive operation.
//!
//! Maps a wire `EncodeRequest` (from `brain-protocol`) into an
//! 8-step `EncodePlan`. Pure: no I/O, no async, no state.
//!
//! See `spec/08_query_planner/04_encode_planning.md` for the
//! authoritative shape:
//! 1. Idempotency check
//! 2. Embedding
//! 3. Context resolution
//! 4. Slot allocation
//! 5. WAL append + fsync (durability barrier)
//! 6. Apply (arena + metadata + HNSW)
//! 7. Edges
//! 8. Response

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_protocol::request::EncodeRequest;

use crate::config::PlannerConfig;
use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::{
    ApplyStep, ContextResolutionStep, EdgeSpec, EdgeStep, EmbeddingStep, EncodePlan,
    EncodeResponseStep, ExecutionPlan, IdempotencyCheckStep, SlotAllocationStep, WalAppendStep,
};

/// Spec §08/04 §15: "the text is non-empty and within size limits".
/// 1 MiB is a generous upper bound; an embed text approaching this
/// size will saturate the tokeniser anyway.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Build the execution plan for an ENCODE request.
pub fn plan_encode(req: &EncodeRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    Ok(ExecutionPlan::Encode(plan_encode_inner(req, ctx)?))
}

/// Same as [`plan_encode`] but returns the inner struct directly —
/// useful for tests that want to inspect fields without an enum match.
pub fn plan_encode_inner(
    req: &EncodeRequest,
    ctx: &PlannerContext,
) -> Result<EncodePlan, PlanError> {
    validate(req, &ctx.config)?;

    let estimated = cost::cost_encode(/* cache_hit */ false, req.edges.len());
    cost::check_budget(estimated, ctx)?;

    let edges = req
        .edges
        .iter()
        .map(|e| EdgeStep {
            edge: EdgeSpec {
                target: MemoryId::from(e.target),
                kind: EdgeKind::from(e.kind),
                weight: e.weight,
            },
            insert_in_metadata: true,
        })
        .collect();

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
            kind: MemoryKind::from(req.kind),
            salience_initial: req.salience_hint,
            fsync: true,
        },
        apply: ApplyStep {
            arena_write: true,
            metadata_write: true,
            hnsw_insert: true,
        },
        edges,
        response: EncodeResponseStep {
            persistent_id: true,
        },
        estimated_cost_ms: estimated,
        deduplicate: req.deduplicate,
    })
}

fn validate(req: &EncodeRequest, config: &PlannerConfig) -> Result<(), PlanError> {
    if req.text.is_empty() {
        return Err(PlanError::InvalidParameters {
            field: "text",
            reason: "must be non-empty".to_string(),
        });
    }
    if req.text.len() > MAX_TEXT_BYTES {
        return Err(PlanError::InvalidParameters {
            field: "text",
            reason: format!(
                "{} bytes exceeds MAX_TEXT_BYTES = {MAX_TEXT_BYTES}",
                req.text.len()
            ),
        });
    }
    let kind = MemoryKind::from(req.kind);
    if matches!(kind, MemoryKind::Consolidated) {
        return Err(PlanError::InvalidParameters {
            field: "kind",
            reason: "consolidated memories are produced by background workers, \
                     not by direct encode. Use --kind episodic or --kind semantic."
                .to_string(),
        });
    }
    if !(0.0..=1.0).contains(&req.salience_hint) {
        return Err(PlanError::InvalidParameters {
            field: "salience_hint",
            reason: format!("{} must be in [0, 1]", req.salience_hint),
        });
    }
    if req.edges.len() > config.max_edges_per_encode {
        return Err(PlanError::InvalidParameters {
            field: "edges",
            reason: format!(
                "{} edges exceeds max_edges_per_encode = {}",
                req.edges.len(),
                config.max_edges_per_encode
            ),
        });
    }
    for (i, e) in req.edges.iter().enumerate() {
        if !e.weight.is_finite() {
            return Err(PlanError::InvalidParameters {
                field: "edges[].weight",
                reason: format!("edge {i} weight {} is not finite", e.weight),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::request::{EdgeKindWire, EdgeRequest, MemoryKindWire};

    fn base_request() -> EncodeRequest {
        EncodeRequest {
            text: "hello".into(),
            context_id: 42,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: Vec::new(),
            request_id: [1u8; 16],
            txn_id: None,
            deduplicate: false,
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
        assert!(plan.edges.is_empty());
        assert!(plan.response.persistent_id);
        assert!(plan.estimated_cost_ms > 0.0);
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

    #[test]
    fn consolidated_kind_is_rejected() {
        let mut r = base_request();
        r.kind = MemoryKindWire::Consolidated;
        match plan_encode(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, reason }) => {
                assert_eq!(field, "kind");
                // The user-facing message intentionally avoids spec
                // references — it says "consolidated memories are
                // produced by background workers, not by direct
                // encode" — so we match on the consistent lowercase
                // marker word.
                assert!(reason.contains("consolidated"));
            }
            other => panic!("expected InvalidParameters[kind], got {other:?}"),
        }
    }

    #[test]
    fn salience_out_of_range_is_rejected() {
        for bad in [-0.1f32, 1.1] {
            let mut r = base_request();
            r.salience_hint = bad;
            match plan_encode(&r, &PlannerContext::default()) {
                Err(PlanError::InvalidParameters { field, .. }) => {
                    assert_eq!(field, "salience_hint");
                }
                other => panic!("expected InvalidParameters[salience], got {other:?}"),
            }
        }
    }

    #[test]
    fn too_many_edges_is_rejected() {
        let mut r = base_request();
        // PlannerConfig::default().max_edges_per_encode == 64.
        r.edges = (0..65)
            .map(|i| EdgeRequest {
                target: i as u128,
                kind: EdgeKindWire::References,
                weight: 0.5,
            })
            .collect();
        match plan_encode(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => assert_eq!(field, "edges"),
            other => panic!("expected InvalidParameters[edges], got {other:?}"),
        }
    }

    #[test]
    fn non_finite_edge_weight_is_rejected() {
        let mut r = base_request();
        r.edges = vec![EdgeRequest {
            target: 1u128,
            kind: EdgeKindWire::References,
            weight: f32::NAN,
        }];
        match plan_encode(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "edges[].weight");
            }
            other => panic!("expected InvalidParameters[edges.weight], got {other:?}"),
        }
    }

    #[test]
    fn edges_are_translated() {
        let mut r = base_request();
        r.edges = vec![
            EdgeRequest {
                target: 7u128,
                kind: EdgeKindWire::Caused,
                weight: 0.25,
            },
            EdgeRequest {
                target: 8u128,
                kind: EdgeKindWire::FollowedBy,
                weight: -0.75,
            },
        ];
        let plan = unwrap_encode(plan_encode(&r, &PlannerContext::default()).unwrap());
        assert_eq!(plan.edges.len(), 2);
        assert_eq!(plan.edges[0].edge.target, MemoryId::from(7u128));
        assert_eq!(plan.edges[0].edge.kind, EdgeKind::Caused);
        assert!((plan.edges[0].edge.weight - 0.25).abs() < f32::EPSILON);
        assert_eq!(plan.edges[1].edge.kind, EdgeKind::FollowedBy);
        assert!((plan.edges[1].edge.weight - (-0.75)).abs() < f32::EPSILON);
    }
}
