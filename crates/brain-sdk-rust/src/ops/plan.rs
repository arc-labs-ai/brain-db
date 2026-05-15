//! PLAN op (spec §07/04 + §13/02 §5).
//!
//! Streaming response — 10.5 ships a Vec-collecting `send()`.

use brain_core::RequestId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{PlanBudget, PlanRequest, PlanState, PlanStrategy};
use brain_protocol::response::{PlanResponseFrame, PlanStep};
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_collect_until_eos, DEFAULT_STREAM_FRAME_CAP, FLAG_EOS};
use crate::ops::stream::FrameStream;
use crate::proto::frames::write_frame;

pub struct PlanBuilder<'a> {
    client: &'a Client,
    start: PlanState,
    goal: PlanState,
    budget: PlanBudget,
    strategy_hint: Option<PlanStrategy>,
    context_filter: Option<Vec<u64>>,
    request_id: Option<RequestId>,
    txn_id: Option<[u8; 16]>,
}

impl<'a> PlanBuilder<'a> {
    pub(crate) fn new(client: &'a Client, start: PlanState, goal: PlanState) -> Self {
        Self {
            client,
            start,
            goal,
            budget: PlanBudget {
                max_steps: 10,
                max_wall_time_ms: 5_000,
                max_branches_explored: 256,
            },
            strategy_hint: None,
            context_filter: None,
            request_id: None,
            txn_id: None,
        }
    }

    #[must_use]
    pub fn budget(mut self, budget: PlanBudget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn strategy(mut self, hint: PlanStrategy) -> Self {
        self.strategy_hint = Some(hint);
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

    pub async fn send(self) -> Result<Vec<PlanStep>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);
        let start = self.start;
        let goal = self.goal;
        let budget = self.budget;
        let strategy_hint = self.strategy_hint;
        let context_filter = self.context_filter;
        let txn_id = self.txn_id;
        let client = self.client.clone();

        client
            .run_op("plan", || {
                let client = client.clone();
                let start = start.clone();
                let goal = goal.clone();
                let context_filter = context_filter.clone();
                async move {
                    let body = RequestBody::Plan(PlanRequest {
                        start,
                        goal,
                        budget,
                        strategy_hint,
                        context_filter,
                        request_id: request_id_bytes,
                        txn_id,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame =
                        Frame::new(Opcode::PlanReq.as_u16(), FLAG_EOS, stream_id, body.encode());
                    let frames = send_and_collect_until_eos(
                        &mut guard,
                        frame,
                        Opcode::PlanResp,
                        DEFAULT_STREAM_FRAME_CAP,
                    )
                    .await?;
                    let mut out = Vec::new();
                    for f in frames {
                        match ResponseBody::decode(Opcode::PlanResp, &f.payload)? {
                            ResponseBody::Plan(PlanResponseFrame { steps, .. }) => {
                                out.extend(steps);
                            }
                            _ => {
                                return Err(ClientError::Protocol(
                                    brain_protocol::error::ProtocolError::BadFrame(
                                        "PlanResp opcode but body variant didn't match".into(),
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

    /// Streaming form — yields one `PlanStep` per `.next().await`.
    pub async fn send_stream(self) -> Result<FrameStream<PlanStep>, ClientError> {
        let request_id = Some(
            self.request_id
                .unwrap_or_else(|| self.client.next_request_id()),
        );
        let request_id_bytes: Option<[u8; 16]> = request_id.map(Into::into);
        let body = RequestBody::Plan(PlanRequest {
            start: self.start,
            goal: self.goal,
            budget: self.budget,
            strategy_hint: self.strategy_hint,
            context_filter: self.context_filter,
            request_id: request_id_bytes,
            txn_id: self.txn_id,
        });
        let mut guard = self.client.acquire().await?;
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(Opcode::PlanReq.as_u16(), FLAG_EOS, stream_id, body.encode());
        write_frame(guard.stream_mut(), &frame).await?;

        let decoder: crate::ops::stream::StreamDecoder<PlanStep> =
            Box::new(
                |payload| match ResponseBody::decode(Opcode::PlanResp, payload)? {
                    ResponseBody::Plan(PlanResponseFrame { steps, .. }) => Ok(steps),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "PlanResp opcode but body variant didn't match".into(),
                        ),
                    )),
                },
            );
        Ok(FrameStream::new(guard, Opcode::PlanResp, decoder))
    }
}
