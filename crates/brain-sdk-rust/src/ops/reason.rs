//! REASON op (spec §07/05 + §13/02 §6).

use brain_core::RequestId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{ObservationInput, ReasonRequest};
use brain_protocol::response::{InferenceStep, ReasonResponseFrame};
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_collect_until_eos, DEFAULT_STREAM_FRAME_CAP, FLAG_EOS};
use crate::ops::stream::FrameStream;
use crate::proto::frames::write_frame;

pub struct ReasonBuilder<'a> {
    client: &'a Client,
    observation: ObservationInput,
    depth: u32,
    confidence_threshold: f32,
    max_inferences: u32,
    budget_wall_time_ms: u32,
    context_filter: Option<Vec<u64>>,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> ReasonBuilder<'a> {
    pub(crate) fn new(client: &'a Client, observation: ObservationInput) -> Self {
        Self {
            client,
            observation,
            depth: 3,
            confidence_threshold: 0.0,
            max_inferences: 16,
            budget_wall_time_ms: 5_000,
            context_filter: None,
            request_id: None,
            txn_id: None,
        }
    }

    #[must_use]
    pub fn depth(mut self, d: u32) -> Self {
        self.depth = d;
        self
    }

    #[must_use]
    pub fn confidence_threshold(mut self, t: f32) -> Self {
        self.confidence_threshold = t;
        self
    }

    #[must_use]
    pub fn max_inferences(mut self, n: u32) -> Self {
        self.max_inferences = n;
        self
    }

    #[must_use]
    pub fn budget_wall_time_ms(mut self, ms: u32) -> Self {
        self.budget_wall_time_ms = ms;
        self
    }

    #[must_use]
    pub fn context_filter(mut self, ctxs: Vec<u64>) -> Self {
        self.context_filter = Some(ctxs);
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

    pub async fn send(self) -> Result<Vec<InferenceStep>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);
        let observation = self.observation;
        let depth = self.depth;
        let confidence_threshold = self.confidence_threshold;
        let max_inferences = self.max_inferences;
        let budget_wall_time_ms = self.budget_wall_time_ms;
        let context_filter = self.context_filter;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("reason", || {
                let client = client.clone();
                let observation = observation.clone();
                let context_filter = context_filter.clone();
                async move {
                    let body = RequestBody::Reason(ReasonRequest {
                        observation,
                        depth,
                        confidence_threshold,
                        context_filter,
                        max_inferences,
                        budget_wall_time_ms,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::ReasonReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let frames = send_and_collect_until_eos(
                        &mut guard,
                        frame,
                        Opcode::ReasonResp,
                        DEFAULT_STREAM_FRAME_CAP,
                    )
                    .await?;
                    let mut out = Vec::new();
                    for f in frames {
                        match ResponseBody::decode(Opcode::ReasonResp, &f.payload)? {
                            ResponseBody::Reason(ReasonResponseFrame { inferences, .. }) => {
                                out.extend(inferences);
                            }
                            _ => {
                                return Err(ClientError::Protocol(
                                    brain_protocol::error::ProtocolError::BadFrame(
                                        "ReasonResp opcode but body variant didn't match".into(),
                                    ),
                                ))
                            }
                        }
                    }
                    Ok(out)
                }
            })
            .await
    }

    /// Streaming form — yields one `InferenceStep` per
    /// `.next().await`.
    pub async fn send_stream(self) -> Result<FrameStream<InferenceStep>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);
        let body = RequestBody::Reason(ReasonRequest {
            observation: self.observation,
            depth: self.depth,
            confidence_threshold: self.confidence_threshold,
            context_filter: self.context_filter,
            max_inferences: self.max_inferences,
            budget_wall_time_ms: self.budget_wall_time_ms,
            request_id: request_id_bytes,
            txn_id: self.txn_id,
        });
        let mut guard = self.client.acquire().await?;
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(
            Opcode::ReasonReq.as_u16(),
            FLAG_EOS,
            stream_id,
            body.encode(),
        );
        write_frame(guard.stream_mut(), &frame).await?;

        let decoder: crate::ops::stream::StreamDecoder<InferenceStep> =
            Box::new(
                |payload| match ResponseBody::decode(Opcode::ReasonResp, payload)? {
                    ResponseBody::Reason(ReasonResponseFrame { inferences, .. }) => Ok(inferences),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "ReasonResp opcode but body variant didn't match".into(),
                        ),
                    )),
                },
            );
        Ok(FrameStream::new(guard, Opcode::ReasonResp, decoder))
    }
}
