//! FORGET op (spec §07/06 + §13/02 §7). Single-id mode.
//!
//! 10.5 ships single-id only; batch/filter modes (spec §13/02 §7)
//! land post-Phase-10.

use brain_core::{MemoryId, RequestId};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{ForgetMode, ForgetRequest};
use brain_protocol::response::ForgetResponse;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

pub struct ForgetBuilder<'a> {
    client: &'a Client,
    memory_id: MemoryId,
    mode: ForgetMode,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> ForgetBuilder<'a> {
    pub(crate) fn new(client: &'a Client, memory_id: MemoryId) -> Self {
        Self {
            client,
            memory_id,
            mode: ForgetMode::Soft,
            request_id: None,
            txn_id: None,
        }
    }

    #[must_use]
    pub fn mode(mut self, mode: ForgetMode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    #[must_use]
    pub fn txn(mut self, txn_id: [u8; 16]) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    pub async fn send(self) -> Result<ForgetResponse, ClientError> {
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();
        let memory_id_raw: u128 = self.memory_id.raw();
        let mode = self.mode;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("forget", || {
                let client = client.clone();
                async move {
                    let body = RequestBody::Forget(ForgetRequest {
                        memory_id: memory_id_raw,
                        mode,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::ForgetReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let resp = send_and_read_one(&mut guard, frame, Opcode::ForgetResp).await?;
                    match ResponseBody::decode(Opcode::ForgetResp, &resp.payload)? {
                        ResponseBody::Forget(r) => Ok(r),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "ForgetResp opcode but body variant didn't match".into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}
