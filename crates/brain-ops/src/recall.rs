//! RECALL handler stub. Real implementation lands in sub-task 7.4.

use brain_protocol::request::RecallRequest;
use brain_protocol::response::RecallResponseFrame;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_recall(
    _req: RecallRequest,
    _ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    Err(OpError::NotYetImplemented("RECALL — sub-task 7.4"))
}
