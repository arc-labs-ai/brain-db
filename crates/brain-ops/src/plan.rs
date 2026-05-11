//! PLAN handler stub. Real BFS traversal implementation lands in
//! sub-task 7.5 — this is the executor Phase 6.5 deferred.

use brain_protocol::request::PlanRequest;
use brain_protocol::response::PlanResponseFrame;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_plan(
    _req: PlanRequest,
    _ctx: &OpsContext,
) -> Result<PlanResponseFrame, OpError> {
    Err(OpError::NotYetImplemented("PLAN — sub-task 7.5"))
}
