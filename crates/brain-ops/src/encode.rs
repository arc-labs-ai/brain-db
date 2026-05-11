//! ENCODE handler stub. Real implementation lands in sub-task 7.3.

use brain_protocol::request::EncodeRequest;
use brain_protocol::response::EncodeResponse;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_encode(
    _req: EncodeRequest,
    _ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    Err(OpError::NotYetImplemented("ENCODE — sub-task 7.3"))
}
