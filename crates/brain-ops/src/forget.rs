//! FORGET handler (sub-task 7.7).
//!
//! Wires the planner (6.6) + executor (6.6) + real writer (7.2)
//! through the dispatcher and maps `ForgetResult` into the wire
//! `ForgetResponse`.

use brain_planner::{execute_forget, plan_forget_inner, ForgetOutcome};
use brain_protocol::request::ForgetRequest;
use brain_protocol::response::ForgetResponse;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_forget(
    req: ForgetRequest,
    ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    let memory_id_wire = req.memory_id;
    let plan = plan_forget_inner(&req, &ctx.planner_ctx)?;
    let result = execute_forget(plan, &ctx.executor).await?;

    // Spec §09/06 §14 collapses MemoryNotFound into a no-op on the
    // single-memory wire shape — there's no `not_found` list to
    // surface it on. AlreadyTombstoned has the same caller-visible
    // outcome: "this memory is no longer visible, and we didn't do
    // new work."
    let was_already_forgotten = matches!(
        result.outcome,
        ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
    );

    Ok(ForgetResponse {
        memory_id: memory_id_wire,
        was_already_forgotten,
        // v1 gap: edge cascade is a Phase-11 worker job per spec
        // §09/06 §7. The handler doesn't tombstone dangling edges.
        edges_removed: 0,
    })
}
