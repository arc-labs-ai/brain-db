//! Planner side for the `FORGET` cognitive operation.
//!
//! Maps a wire `ForgetRequest` into a [`ForgetPlan`]. Pure.
//!
//! Single-shard, single-memory v1 (spec §08/06 §2.1). The wire shape
//! only carries one `MemoryId`; batch + filter modes need a wire bump.

use brain_core::{MemoryId, RequestId};
use brain_protocol::request::{ForgetMode, ForgetRequest};

use crate::context::PlannerContext;
use crate::cost;
use crate::error::PlanError;
use crate::plan::encode::IdempotencyCheckStep;
use crate::plan::forget::{ForgetApplyStep, ForgetPlan, ForgetResponseStep, ForgetWalStep};
use crate::plan::ExecutionPlan;

const NIL_REQUEST_ID: [u8; 16] = [0u8; 16];

pub fn plan_forget(req: &ForgetRequest, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
    Ok(ExecutionPlan::Forget(plan_forget_inner(req, ctx)?))
}

pub fn plan_forget_inner(
    req: &ForgetRequest,
    ctx: &PlannerContext,
) -> Result<ForgetPlan, PlanError> {
    validate(req)?;

    let hard = matches!(req.mode, ForgetMode::Hard);
    let estimated = cost::cost_forget(hard);
    cost::check_budget(estimated, ctx)?;

    Ok(ForgetPlan {
        shard: 0,
        memory_id: MemoryId::from(req.memory_id),
        mode: req.mode,
        idempotency_check: IdempotencyCheckStep {
            request_id: RequestId::from(req.request_id),
        },
        wal_append: ForgetWalStep {
            fsync: true,
            mode: req.mode,
        },
        apply: ForgetApplyStep {
            arena_tombstone: true,
            metadata_commit: true,
            hnsw_mark_removed: true,
            arena_zero_vector: hard,
            text_zero: hard,
        },
        response: ForgetResponseStep {
            include_outcome: true,
        },
        estimated_cost_ms: estimated,
    })
}

fn validate(req: &ForgetRequest) -> Result<(), PlanError> {
    if req.request_id == NIL_REQUEST_ID {
        return Err(PlanError::InvalidParameters {
            field: "request_id",
            reason: "must be set".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::request::ForgetMode;

    fn base_request() -> ForgetRequest {
        ForgetRequest {
            memory_id: 7u128,
            mode: ForgetMode::Soft,
            request_id: [1u8; 16],
            txn_id: None,
        }
    }

    fn unwrap_forget(plan: ExecutionPlan) -> ForgetPlan {
        match plan {
            ExecutionPlan::Forget(p) => p,
            other => panic!("expected Forget, got {other:?}"),
        }
    }

    #[test]
    fn soft_plan_shape() {
        let plan = unwrap_forget(plan_forget(&base_request(), &PlannerContext::default()).unwrap());
        assert_eq!(plan.shard, 0);
        assert_eq!(plan.memory_id, MemoryId::from(7u128));
        assert_eq!(plan.mode, ForgetMode::Soft);
        assert!(plan.wal_append.fsync);
        assert_eq!(plan.wal_append.mode, ForgetMode::Soft);
        assert!(plan.apply.arena_tombstone);
        assert!(plan.apply.metadata_commit);
        assert!(plan.apply.hnsw_mark_removed);
        assert!(!plan.apply.arena_zero_vector, "soft does NOT zero");
        assert!(!plan.apply.text_zero, "soft does NOT zero");
        assert!(plan.response.include_outcome);
        assert!(plan.estimated_cost_ms > 0.0);
        assert_eq!(
            plan.idempotency_check.request_id,
            RequestId::from([1u8; 16])
        );
    }

    #[test]
    fn hard_plan_zeroes() {
        let mut r = base_request();
        r.mode = ForgetMode::Hard;
        let plan = unwrap_forget(plan_forget(&r, &PlannerContext::default()).unwrap());
        assert_eq!(plan.mode, ForgetMode::Hard);
        assert!(plan.apply.arena_zero_vector, "hard zeroes vector");
        assert!(plan.apply.text_zero, "hard zeroes text");
    }

    #[test]
    fn nil_request_id_rejected() {
        let mut r = base_request();
        r.request_id = [0u8; 16];
        match plan_forget(&r, &PlannerContext::default()) {
            Err(PlanError::InvalidParameters { field, .. }) => {
                assert_eq!(field, "request_id");
            }
            other => panic!("expected InvalidParameters[request_id], got {other:?}"),
        }
    }

    #[test]
    fn hard_costs_more_than_soft() {
        let soft = unwrap_forget(plan_forget(&base_request(), &PlannerContext::default()).unwrap());
        let mut r = base_request();
        r.mode = ForgetMode::Hard;
        let hard = unwrap_forget(plan_forget(&r, &PlannerContext::default()).unwrap());
        assert!(hard.estimated_cost_ms > soft.estimated_cost_ms);
    }

    #[test]
    fn idempotency_carries_request_id() {
        let plan = unwrap_forget(plan_forget(&base_request(), &PlannerContext::default()).unwrap());
        assert_eq!(
            plan.idempotency_check.request_id,
            RequestId::from([1u8; 16])
        );
    }
}
