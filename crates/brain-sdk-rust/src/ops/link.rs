//! LINK op (spec §07/05 + §13/02 §8).

use brain_core::{MemoryId, RequestId};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{EdgeKindWire, LinkRequest};
use brain_protocol::response::LinkResponse;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

pub struct LinkBuilder<'a> {
    client: &'a Client,
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKindWire,
    weight: f32,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> LinkBuilder<'a> {
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
            weight: 1.0,
            request_id: None,
            txn_id: None,
        }
    }

    #[must_use]
    pub fn weight(mut self, weight: f32) -> Self {
        self.weight = weight;
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

    pub async fn send(self) -> Result<LinkResponse, ClientError> {
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();
        let source_raw = self.source.raw();
        let target_raw = self.target.raw();
        let kind = self.kind;
        let weight = self.weight;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("link", || {
                let client = client.clone();
                async move {
                    let body = RequestBody::Link(LinkRequest {
                        source: source_raw,
                        target: target_raw,
                        kind,
                        weight,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame =
                        Frame::new(Opcode::LinkReq.as_u16(), FLAG_EOS, stream_id, body.encode());
                    let resp = send_and_read_one(&mut guard, frame, Opcode::LinkResp).await?;
                    match ResponseBody::decode(Opcode::LinkResp, &resp.payload)? {
                        ResponseBody::Link(r) => Ok(r),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "LinkResp opcode but body variant didn't match".into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}
