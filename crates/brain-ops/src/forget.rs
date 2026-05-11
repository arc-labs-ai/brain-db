//! FORGET handler stub. Real implementation lands in sub-task 7.7
//! (also adds the UNFORGET wire variant + handler).

use brain_protocol::request::ForgetRequest;
use brain_protocol::response::ForgetResponse;

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_forget(
    _req: ForgetRequest,
    _ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    Err(OpError::NotYetImplemented("FORGET — sub-task 7.7"))
}
