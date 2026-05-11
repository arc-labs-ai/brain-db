//! Top-level `execute` entry point. Matches an [`ExecutionPlan`] to
//! its `execute_*` async function and wraps each branch in a tracing
//! span for per-request timing.
//!
//! Spec §08/08 §1: "`async fn execute(plan: ExecutionPlan) -> Result<
//! Response, ExecError>`. Each `execute_*` method orchestrates the
//! steps in the plan."
//!
//! PLAN and REASON variants return [`ExecError::Unsupported`] for v1
//! — full execution of those needs the bidirectional-BFS edge
//! traversal that lands with Phase 7 cognitive-ops alongside `LINK` /
//! `UNLINK`.

use crate::plan::ExecutionPlan;

use super::context::ExecutorContext;
use super::encode::execute_encode;
use super::error::ExecError;
use super::forget::execute_forget;
use super::recall::execute_recall;
use super::result::{EncodeResult, ForgetResult, RecallResult};

/// Rust-side union of per-operation results. Phase 9's server maps
/// each variant to the corresponding wire `ResponseBody`.
///
/// PLAN + REASON have no variants here because those dispatch arms
/// return `ExecError::Unsupported`. When Phase 7 lands them, this
/// enum grows two more variants.
#[derive(Debug, Clone)]
pub enum ExecutionResult {
    Recall(RecallResult),
    Encode(EncodeResult),
    Forget(ForgetResult),
}

/// Top-level dispatch. Routes an `ExecutionPlan` to its matching
/// executor and returns a Rust-side `ExecutionResult`.
pub async fn execute(
    plan: ExecutionPlan,
    ctx: &ExecutorContext,
) -> Result<ExecutionResult, ExecError> {
    match plan {
        ExecutionPlan::Recall(p) => {
            let _span = tracing::info_span!("execute", op = "recall").entered();
            execute_recall(p, ctx).await.map(ExecutionResult::Recall)
        }
        ExecutionPlan::Encode(p) => {
            let _span = tracing::info_span!("execute", op = "encode").entered();
            execute_encode(p, ctx).await.map(ExecutionResult::Encode)
        }
        ExecutionPlan::Forget(p) => {
            let _span = tracing::info_span!("execute", op = "forget").entered();
            execute_forget(p, ctx).await.map(ExecutionResult::Forget)
        }
        ExecutionPlan::Plan(_) => Err(ExecError::Unsupported("PLAN execution — Phase 7")),
        ExecutionPlan::Reason(_) => Err(ExecError::Unsupported("REASON execution — Phase 7")),
    }
}
