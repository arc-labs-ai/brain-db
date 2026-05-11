//! SUBSCRIBE / UNSUBSCRIBE handler stubs.
//!
//! Real implementation lands in sub-task 7.10. Subscribe is the only
//! streaming primitive (spec §09/01 §13): the first response is a
//! `SubscriptionEvent`; subsequent events flow through a broadcast
//! channel wired in 7.10. Wire-level stream framing is Phase 9.

use brain_protocol::request::{SubscribeRequest, UnsubscribeRequest};
use brain_protocol::response::{SubscriptionEvent, UnsubscribeResponse};

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_subscribe(
    _req: SubscribeRequest,
    _ctx: &OpsContext,
) -> Result<SubscriptionEvent, OpError> {
    Err(OpError::NotYetImplemented("SUBSCRIBE — sub-task 7.10"))
}

pub async fn handle_unsubscribe(
    _req: UnsubscribeRequest,
    _ctx: &OpsContext,
) -> Result<UnsubscribeResponse, OpError> {
    Err(OpError::NotYetImplemented("UNSUBSCRIBE — sub-task 7.10"))
}
