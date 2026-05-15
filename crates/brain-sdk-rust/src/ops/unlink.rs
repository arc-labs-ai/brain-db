//! UNLINK op (spec §07/06 + §13/02 §8).

use brain_core::{MemoryId, RequestId};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{EdgeKindWire, UnlinkRequest};
use brain_protocol::response::UnlinkResponse;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

pub struct UnlinkBuilder<'a> {
    client: &'a Client,
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKindWire,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> UnlinkBuilder<'a> {
    pub(crate) fn new(
        client: &'a Client,
        source: MemoryId,
        kind: EdgeKindWire,
        target: MemoryId,
    ) -> Self {
        Self {
            client,
            source,
            target,
            kind,
            request_id: None,
            txn_id: None,
        }
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

    pub async fn send(self) -> Result<UnlinkResponse, ClientError> {
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();
        let source_raw = self.source.raw();
        let target_raw = self.target.raw();
        let kind = self.kind;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("unlink", || {
                let client = client.clone();
                async move {
                    let body = RequestBody::Unlink(UnlinkRequest {
                        source: source_raw,
                        target: target_raw,
                        kind,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::UnlinkReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let resp = send_and_read_one(&mut guard, frame, Opcode::UnlinkResp).await?;
                    match ResponseBody::decode(Opcode::UnlinkResp, &resp.payload)? {
                        ResponseBody::Unlink(r) => Ok(r),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "UnlinkResp opcode but body variant didn't match".into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}
