//! SUBSCRIBE op (spec §07/07 + §13/02 §10).
//!
//! 10.5 ships a `collect(max_events)` form that gathers up to N
//! events then returns. The real streaming surface (async
//! iterator with backpressure) lands in 10.6.

use brain_protocol::opcode::Opcode;
use brain_protocol::request::{
    MemoryKindWire, SimilarityFilter, SubscribeRequest, SubscriptionFilter,
};
use brain_protocol::response::SubscriptionEvent;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{map_error_frame, DEFAULT_STREAM_FRAME_CAP, FLAG_EOS};
use crate::ops::stream::FrameStream;
use crate::proto::frames::{read_one_frame, write_frame};

pub struct SubscribeBuilder<'a> {
    client: &'a Client,
    contexts: Option<Vec<u64>>,
    kinds: Option<Vec<MemoryKindWire>>,
    similar_to: Option<SimilarityFilter>,
    include_history: bool,
    from_lsn: Option<u64>,
    max_inflight: u32,
}

impl<'a> SubscribeBuilder<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            contexts: None,
            kinds: None,
            similar_to: None,
            include_history: false,
            from_lsn: None,
            max_inflight: 64,
        }
    }

    #[must_use]
    pub fn contexts(mut self, ctxs: Vec<u64>) -> Self {
        self.contexts = Some(ctxs);
        self
    }

    #[must_use]
    pub fn kinds(mut self, kinds: Vec<MemoryKindWire>) -> Self {
        self.kinds = Some(kinds);
        self
    }

    #[must_use]
    pub fn similar_to(mut self, filter: SimilarityFilter) -> Self {
        self.similar_to = Some(filter);
        self
    }

    #[must_use]
    pub fn include_history(mut self, on: bool) -> Self {
        self.include_history = on;
        self
    }

    #[must_use]
    pub fn start_lsn(mut self, lsn: u64) -> Self {
        self.from_lsn = Some(lsn);
        self
    }

    #[must_use]
    pub fn max_inflight(mut self, n: u32) -> Self {
        self.max_inflight = n;
        self
    }

    /// Pre-10.6 form: open the subscription, collect up to
    /// `max_events` `SubscriptionEvent`s, then stop reading
    /// (the server-side subscription may keep producing but we
    /// stop draining). Returns the events in arrival order.
    ///
    /// 10.6 will add `send_stream()` returning
    /// `impl Stream<Item = Result<SubscriptionEvent, ClientError>>`
    /// with proper backpressure.
    pub async fn collect(self, max_events: usize) -> Result<Vec<SubscriptionEvent>, ClientError> {
        let filter = SubscriptionFilter {
            contexts: self.contexts,
            kinds: self.kinds,
            similar_to: self.similar_to,
        };
        let include_history = self.include_history;
        let from_lsn = self.from_lsn;
        let max_inflight = self.max_inflight;
        let client = self.client.clone();
        let cap = max_events.min(DEFAULT_STREAM_FRAME_CAP);

        // SUBSCRIBE is intentionally NOT wrapped in retry — restarting
        // a stream mid-flight has subtle semantics around `from_lsn`.
        // 10.6 + 11.x will design the retry/restart story for streams.
        let mut guard = client.acquire().await?;
        let body = RequestBody::Subscribe(SubscribeRequest {
            filter,
            include_history,
            from_lsn,
            max_inflight,
        });
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            stream_id,
            body.encode(),
        );
        write_frame(guard.stream_mut(), &frame).await?;

        let mut events = Vec::with_capacity(cap);
        while events.len() < cap {
            let resp = read_one_frame(guard.stream_mut()).await?;
            if resp.header.opcode_u16() == Opcode::Error.as_u16() {
                return Err(map_error_frame(&resp.payload));
            }
            if resp.header.opcode_u16() != Opcode::SubscribeEvent.as_u16() {
                return Err(ClientError::Protocol(
                    brain_protocol::error::ProtocolError::BadFrame(format!(
                        "expected SubscribeEvent, got 0x{:02x}",
                        resp.header.opcode_u16()
                    )),
                ));
            }
            match ResponseBody::decode(Opcode::SubscribeEvent, &resp.payload)? {
                ResponseBody::SubscribeEvent(ev) => events.push(ev),
                _ => {
                    return Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "SubscribeEvent opcode but body variant didn't match".into(),
                        ),
                    ))
                }
            }
        }
        Ok(events)
    }

    /// Open the subscription and return an unbounded
    /// `Stream<Item = Result<SubscriptionEvent, ClientError>>`.
    /// Demand-driven — each `.next().await` reads one event off
    /// the wire. The stream holds a `PoolGuard` for its lifetime;
    /// drop it to release the connection back to the pool.
    ///
    /// SUBSCRIBE streams do NOT retry on transient errors;
    /// resume semantics (spec §13/05 §8) require server-side
    /// support beyond v1. Surface errors to the caller.
    pub async fn send_stream(self) -> Result<FrameStream<SubscriptionEvent>, ClientError> {
        let filter = SubscriptionFilter {
            contexts: self.contexts,
            kinds: self.kinds,
            similar_to: self.similar_to,
        };
        let body = RequestBody::Subscribe(SubscribeRequest {
            filter,
            include_history: self.include_history,
            from_lsn: self.from_lsn,
            max_inflight: self.max_inflight,
        });
        let mut guard = self.client.acquire().await?;
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            stream_id,
            body.encode(),
        );
        write_frame(guard.stream_mut(), &frame).await?;

        let decoder: crate::ops::stream::StreamDecoder<SubscriptionEvent> =
            Box::new(
                |payload| match ResponseBody::decode(Opcode::SubscribeEvent, payload)? {
                    ResponseBody::SubscribeEvent(ev) => Ok(vec![ev]),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "SubscribeEvent opcode but body variant didn't match".into(),
                        ),
                    )),
                },
            );
        Ok(FrameStream::new(guard, Opcode::SubscribeEvent, decoder))
    }
}
