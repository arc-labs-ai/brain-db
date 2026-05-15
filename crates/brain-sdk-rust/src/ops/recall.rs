//! RECALL op (spec §07/03 + §13/02 §4).
//!
//! Streaming response — 10.5 ships a Vec-collecting `send()`.
//! 10.6 will add `send_stream()` returning `impl Stream`.

use brain_core::RequestId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{MemoryKindWire, RecallRequest, RecallStrategy};
use brain_protocol::response::{MemoryResult, RecallResponseFrame};
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_collect_until_eos, DEFAULT_STREAM_FRAME_CAP, FLAG_EOS};
use crate::ops::stream::FrameStream;
use crate::proto::frames::write_frame;

pub struct RecallBuilder<'a> {
    client: &'a Client,
    cue_text: String,
    top_k: u32,
    confidence_threshold: f32,
    context_filter: Option<Vec<u64>>,
    age_bound_unix_nanos: Option<u64>,
    kind_filter: Option<Vec<MemoryKindWire>>,
    salience_floor: f32,
    strategy_hint: Option<RecallStrategy>,
    include_vectors: bool,
    include_edges: bool,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> RecallBuilder<'a> {
    pub(crate) fn new(client: &'a Client, cue: impl Into<String>) -> Self {
        Self {
            client,
            cue_text: cue.into(),
            top_k: 10,
            confidence_threshold: 0.0,
            context_filter: None,
            age_bound_unix_nanos: None,
            kind_filter: None,
            salience_floor: 0.0,
            strategy_hint: None,
            include_vectors: false,
            include_edges: false,
            request_id: None,
            txn_id: None,
        }
    }

    #[must_use]
    pub fn top_k(mut self, k: u32) -> Self {
        self.top_k = k;
        self
    }

    #[must_use]
    pub fn confidence_threshold(mut self, t: f32) -> Self {
        self.confidence_threshold = t;
        self
    }

    #[must_use]
    pub fn context_filter(mut self, ctxs: Vec<u64>) -> Self {
        self.context_filter = Some(ctxs);
        self
    }

    #[must_use]
    pub fn kind_filter(mut self, kinds: Vec<MemoryKindWire>) -> Self {
        self.kind_filter = Some(kinds);
        self
    }

    #[must_use]
    pub fn salience_floor(mut self, floor: f32) -> Self {
        self.salience_floor = floor;
        self
    }

    #[must_use]
    pub fn include_vectors(mut self, on: bool) -> Self {
        self.include_vectors = on;
        self
    }

    #[must_use]
    pub fn include_edges(mut self, on: bool) -> Self {
        self.include_edges = on;
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

    /// Collect all RECALL frames into a single `Vec<MemoryResult>`,
    /// ordered as the server emitted them.
    pub async fn send(self) -> Result<Vec<MemoryResult>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);

        let cue_text = self.cue_text;
        let top_k = self.top_k;
        let confidence_threshold = self.confidence_threshold;
        let context_filter = self.context_filter;
        let age_bound_unix_nanos = self.age_bound_unix_nanos;
        let kind_filter = self.kind_filter;
        let salience_floor = self.salience_floor;
        let strategy_hint = self.strategy_hint;
        let include_vectors = self.include_vectors;
        let include_edges = self.include_edges;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("recall", || {
                let client = client.clone();
                let cue_text = cue_text.clone();
                let context_filter = context_filter.clone();
                let kind_filter = kind_filter.clone();
                async move {
                    let body = RequestBody::Recall(RecallRequest {
                        cue_text,
                        cue_vector_offset: 0,
                        cue_vector_dim: 0,
                        top_k,
                        confidence_threshold,
                        context_filter,
                        age_bound_unix_nanos,
                        kind_filter,
                        salience_floor,
                        strategy_hint,
                        include_vectors,
                        include_edges,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::RecallReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let frames = send_and_collect_until_eos(
                        &mut guard,
                        frame,
                        Opcode::RecallResp,
                        DEFAULT_STREAM_FRAME_CAP,
                    )
                    .await?;
                    let mut out = Vec::new();
                    for f in frames {
                        match ResponseBody::decode(Opcode::RecallResp, &f.payload)? {
                            ResponseBody::Recall(r) => {
                                let RecallResponseFrame { results, .. } = r;
                                out.extend(results);
                            }
                            _ => {
                                return Err(ClientError::Protocol(
                                    brain_protocol::error::ProtocolError::BadFrame(
                                        "RecallResp opcode but body variant didn't match".into(),
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

    /// Open the recall and return a `Stream` that yields one
    /// `MemoryResult` per `.next().await`. Demand-driven —
    /// reads happen only when the caller polls. Useful for
    /// large `top_k` where the Vec form would buffer too much.
    ///
    /// Unlike `send`, this does **not** retry on transient
    /// errors: the response is opened once.
    pub async fn send_stream(self) -> Result<FrameStream<MemoryResult>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);
        let body = RequestBody::Recall(RecallRequest {
            cue_text: self.cue_text,
            cue_vector_offset: 0,
            cue_vector_dim: 0,
            top_k: self.top_k,
            confidence_threshold: self.confidence_threshold,
            context_filter: self.context_filter,
            age_bound_unix_nanos: self.age_bound_unix_nanos,
            kind_filter: self.kind_filter,
            salience_floor: self.salience_floor,
            strategy_hint: self.strategy_hint,
            include_vectors: self.include_vectors,
            include_edges: self.include_edges,
            request_id: request_id_bytes,
            txn_id: self.txn_id,
        });
        let mut guard = self.client.acquire().await?;
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(
            Opcode::RecallReq.as_u16(),
            FLAG_EOS,
            stream_id,
            body.encode(),
        );
        write_frame(guard.stream_mut(), &frame).await?;

        let decoder: crate::ops::stream::StreamDecoder<MemoryResult> =
            Box::new(
                |payload| match ResponseBody::decode(Opcode::RecallResp, payload)? {
                    ResponseBody::Recall(RecallResponseFrame { results, .. }) => Ok(results),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "RecallResp opcode but body variant didn't match".into(),
                        ),
                    )),
                },
            );
        Ok(FrameStream::new(guard, Opcode::RecallResp, decoder))
    }
}
