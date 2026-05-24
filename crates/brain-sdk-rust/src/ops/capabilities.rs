//! `GET_CAPABILITIES` SDK helper.
//!
//! One-shot call that returns the shard's capability snapshot — used
//! at session warm-up so the client can avoid issuing requests that
//! would hard-fail with `CapabilityNotEnabled`. The SDK surface is
//! intentionally tiny (a single async method on `Client`) because
//! capability bits are pure server-side state with no request knobs.

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::GetCapabilitiesRequest;
use brain_protocol::envelope::response::Capabilities;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

/// Fetch the per-shard capabilities snapshot. Defined as a free
/// function rather than a builder because the request carries no
/// fields and a builder would only add ceremony.
pub async fn capabilities(client: &Client) -> Result<Capabilities, ClientError> {
    let client = client.clone();
    client
        .run_op("capabilities", || {
            let client = client.clone();
            async move {
                let body = RequestBody::GetCapabilities(GetCapabilitiesRequest {});
                let mut guard = client.acquire().await?;
                let stream_id = guard.next_stream_id();
                let frame = Frame::new(
                    Opcode::GetCapabilitiesReq.as_u16(),
                    FLAG_EOS,
                    stream_id,
                    body.encode(),
                );
                let resp =
                    send_and_read_one(&mut guard, frame, Opcode::GetCapabilitiesResp).await?;
                match ResponseBody::decode(Opcode::GetCapabilitiesResp, &resp.payload)? {
                    ResponseBody::GetCapabilities(r) => Ok(r.capabilities),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "GetCapabilitiesResp opcode but body variant didn't match".into(),
                        ),
                    )),
                }
            }
        })
        .await
}
