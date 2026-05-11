//! REASON handler stub. Real supports/contradicts traversal lands
//! in sub-task 7.6 — the executor Phase 6.5 deferred.

use brain_protocol::request::ReasonRequest;
use brain_protocol::response::ReasonResponseFrame;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_reason(
    _req: ReasonRequest,
    _ctx: &OpsContext,
) -> Result<ReasonResponseFrame, OpError> {
    Err(OpError::NotYetImplemented("REASON — sub-task 7.6"))
}
