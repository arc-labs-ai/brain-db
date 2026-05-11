//! Executor for the `FORGET` cognitive operation.
//!
//! Translates the planner's `ForgetPlan` into a `ForgetOp`, hands it
//! to the writer, returns the ack as a `ForgetResult`.
//!
//! Spec §08/06 §14 step ordering (WAL fsync → arena tombstone →
//! metadata commit → HNSW mark removed) is enforced by the writer
//! implementation; the executor doesn't sequence the steps itself.
//! CLAUDE.md §5 invariant 1 (WAL-before-ack) is honoured because we
//! only return `Ok` after `submit_forget.await`.

use crate::plan::forget::ForgetPlan;

use super::context::ExecutorContext;
use super::error::ExecError;
use super::result::ForgetResult;
use super::writer::ForgetOp;

pub async fn execute_forget(
    plan: ForgetPlan,
    ctx: &ExecutorContext,
) -> Result<ForgetResult, ExecError> {
    let op = ForgetOp {
        request_id: plan.idempotency_check.request_id,
        memory_id: plan.memory_id,
        mode: plan.mode,
    };
    let ack = ctx.writer.submit_forget(op).await?;
    Ok(ForgetResult {
        memory_id: ack.memory_id,
        outcome: ack.outcome,
        replayed: ack.replayed,
    })
}
