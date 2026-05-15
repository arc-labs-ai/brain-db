//! Connection-layer SUBSCRIBE infrastructure (sub-task 9.11).
//!
//! Implements the audit §8.1 topology:
//!
//! ```text
//!   Shard 0 Glommio                  Connection layer Tokio
//!   ┌─────────────────────┐          ┌────────────────────────────────┐
//!   │ writer.publish(env) │          │ ShardEventHub (per-listener)   │
//!   │       │             │ flume    │   per_shard_bus: Vec<broadcast>│
//!   │       ▼             │ bounded  │       ▲                        │
//!   │ EventBus (broadcast)├─►        │       │  bridge_task (×shards) │
//!   │ → fanout_task (9.11)│          │       │  drains flume → bcast  │
//!   └─────────────────────┘          │                                │
//!                                     │ SubscriptionRegistry (per-conn)│
//!                                     │   HashMap<StreamId, State>     │
//!                                     │       │                        │
//!                                     │   per-sub tokio task           │
//!                                     │   drains broadcast →           │
//!                                     │   filters → frames →           │
//!                                     │   outgoing queue               │
//!                                     └────────────────────────────────┘
//! ```
//!
//! - **`ShardEventHub`**: built once per listener at bind time.
//!   Spawns N `bridge_task`s (one per shard) that drain the shard's
//!   per-process flume Receiver (from `ShardHandle::events()`) and
//!   republish into a `tokio::sync::broadcast::Sender<EventEnvelope>`.
//!   Per-connection subscriptions clone receivers off these senders.
//! - **`SubscriptionRegistry`**: per-connection. Owns the
//!   `HashMap<StreamId, SubscriptionState>`. Each entry has a cancel
//!   watch + a final-LSN counter.
//! - **per-subscription task**: spawned on SUBSCRIBE_REQ. Drains
//!   broadcast receivers (one per relevant shard — typically the
//!   agent's bound shard) and pushes filtered events to the per-conn
//!   outgoing-frame queue.
//!
//! Spec §03/05 §1.3 (SUBSCRIBE / UNSUBSCRIBE), §03/09 §3.3 (open-
//! ended streams), §09/09 (SUBSCRIBE semantics).

#![cfg(target_os = "linux")]
// `num_shards` + `active_count` are diagnostic / test-only surfaces;
// production code paths don't call them yet (9.13 admin endpoints
// will). Avoid churning the surface for now.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use brain_core::{MemoryId, ShardId};
use brain_ops::{parse_filter, EventEnvelope, ParsedFilter};
use brain_protocol::error::ErrorCode;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{CancelStreamRequest, SubscribeRequest, UnsubscribeRequest};
use brain_protocol::response::{
    CancelStreamAck, ErrorCategoryWire, ErrorCodeWire, ErrorResponse, ResponseBody,
    SubscriptionEvent, UnsubscribeResponse,
};
use brain_protocol::Frame;
use parking_lot::Mutex;
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

use crate::shard::ShardHandle;

const SUBSCRIPTION_BROADCAST_CAPACITY: usize = 1024;
const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// ShardEventHub
// ---------------------------------------------------------------------------

/// One bridge per shard: drains the shard's flume event Receiver and
/// republishes into a `broadcast::Sender`. Built at listener startup
/// and shared across all connections.
#[derive(Clone)]
pub struct ShardEventHub {
    per_shard_bus: Arc<Vec<broadcast::Sender<EventEnvelope>>>,
}

impl ShardEventHub {
    /// Construct the hub from a slice of `ShardHandle`s and spawn one
    /// `bridge_task` per shard. The returned handles can be discarded;
    /// the bridges shut down when the shard's flume Receiver returns
    /// `Err` (which happens when every shard sender drops — i.e., on
    /// shard shutdown).
    pub fn spawn(shards: &[ShardHandle]) -> Self {
        let mut per_shard_bus = Vec::with_capacity(shards.len());
        for shard in shards {
            let (tx, _rx) = broadcast::channel::<EventEnvelope>(SUBSCRIPTION_BROADCAST_CAPACITY);
            let events_rx = shard.events();
            let tx_for_task = tx.clone();
            let shard_id = shard.shard_id();
            tokio::spawn(async move {
                bridge_task(shard_id, events_rx, tx_for_task).await;
            });
            per_shard_bus.push(tx);
        }
        Self {
            per_shard_bus: Arc::new(per_shard_bus),
        }
    }

    /// Subscribe to one shard's event stream.
    pub fn subscribe_shard(&self, shard_id: ShardId) -> Option<broadcast::Receiver<EventEnvelope>> {
        self.per_shard_bus
            .get(shard_id as usize)
            .map(|tx| tx.subscribe())
    }

    pub fn num_shards(&self) -> usize {
        self.per_shard_bus.len()
    }
}

async fn bridge_task(
    shard_id: ShardId,
    events_rx: flume::Receiver<EventEnvelope>,
    bcast_tx: broadcast::Sender<EventEnvelope>,
) {
    loop {
        match events_rx.recv_async().await {
            Ok(env) => {
                // `send` returns Err only when no receivers exist;
                // that's fine — drop on the floor.
                let _ = bcast_tx.send(env);
            }
            Err(_) => {
                debug!(shard_id, "event bridge exiting (shard disconnected)");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SubscriptionRegistry (per-connection)
// ---------------------------------------------------------------------------

pub struct SubscriptionRegistry {
    hub: ShardEventHub,
    inner: Mutex<RegistryInner>,
    next_stream_id: AtomicU32,
}

struct RegistryInner {
    streams: HashMap<u32, SubscriptionState>,
}

struct SubscriptionState {
    final_lsn: Arc<AtomicU64>,
    cancel: watch::Sender<bool>,
}

impl SubscriptionRegistry {
    pub fn new(hub: ShardEventHub) -> Self {
        Self {
            hub,
            inner: Mutex::new(RegistryInner {
                streams: HashMap::new(),
            }),
            next_stream_id: AtomicU32::new(0),
        }
    }

    /// Start a subscription. Spawns the per-sub task that drains one
    /// shard's broadcast Receiver, filters events, and pushes
    /// SUBSCRIPTION_EVENT frames to `frame_tx`.
    ///
    /// Returns the stream id the client should observe events on. For
    /// 9.11 we use the client's request `stream_id` directly (so the
    /// returned id == `client_stream_id`); future per-listener stream
    /// allocation may reuse the internal `next_stream_id`.
    pub fn start(
        &self,
        client_stream_id: u32,
        target_shard: ShardId,
        req: &SubscribeRequest,
        frame_tx: flume::Sender<crate::connection::OutgoingFrame>,
    ) -> Result<u32, OpError> {
        if req.from_lsn.is_some() {
            // Spec §16: `LsnTooOld`-equivalent. WAL-replay history is
            // a follow-up.
            return Err(OpError::LsnTooOld);
        }
        let filter = parse_filter(req).map_err(OpError::Ops)?;
        let rx = self
            .hub
            .subscribe_shard(target_shard)
            .ok_or(OpError::ShardOutOfRange(target_shard))?;

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let final_lsn = Arc::new(AtomicU64::new(0));

        {
            let mut inner = self.inner.lock();
            if inner.streams.contains_key(&client_stream_id) {
                return Err(OpError::StreamIdInUse);
            }
            inner.streams.insert(
                client_stream_id,
                SubscriptionState {
                    final_lsn: final_lsn.clone(),
                    cancel: cancel_tx,
                },
            );
        }
        let _ = self
            .next_stream_id
            .fetch_max(client_stream_id, Ordering::SeqCst);

        let stream_id = client_stream_id;
        let final_lsn_for_task = final_lsn;
        let frame_tx_for_task = frame_tx;
        tokio::spawn(async move {
            run_subscription_task(
                stream_id,
                target_shard,
                filter,
                rx,
                cancel_rx,
                final_lsn_for_task,
                frame_tx_for_task,
            )
            .await;
        });

        Ok(stream_id)
    }

    /// Cancel a subscription (UNSUBSCRIBE_REQ or CANCEL_STREAM path).
    /// Returns the last-delivered LSN, or None if the stream id isn't
    /// registered.
    pub fn cancel(&self, stream_id: u32) -> Option<u64> {
        let entry = {
            let mut inner = self.inner.lock();
            inner.streams.remove(&stream_id)?
        };
        let lsn = entry.final_lsn.load(Ordering::SeqCst);
        let _ = entry.cancel.send(true);
        Some(lsn)
    }

    pub fn active_count(&self) -> usize {
        self.inner.lock().streams.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("subscribe: from_lsn replay not implemented (LsnTooOld)")]
    LsnTooOld,
    #[error("subscribe: stream_id already in use")]
    StreamIdInUse,
    #[error("subscribe: target shard {0} out of range")]
    ShardOutOfRange(u16),
    #[error("subscribe: filter parse: {0}")]
    Ops(#[from] brain_ops::OpError),
}

impl OpError {
    pub fn to_error_frame(&self, stream_id: u32) -> Frame {
        let (code, message) = match self {
            Self::LsnTooOld => (
                ErrorCode::SubscriptionLsnTooOld,
                "from_lsn replay not implemented in v1".to_owned(),
            ),
            Self::StreamIdInUse => (ErrorCode::StreamIdInUse, self.to_string()),
            Self::ShardOutOfRange(_) => (ErrorCode::ShardUnavailable, self.to_string()),
            Self::Ops(e) => (ErrorCode::InvalidArgument, format!("subscribe filter: {e}")),
        };
        let body = ResponseBody::Error(ErrorResponse {
            code: ErrorCodeWire::from(code),
            category: ErrorCategoryWire::from(code.category()),
            message,
            details: None,
            retry_after_ms: None,
        });
        Frame::new(Opcode::Error.as_u16(), FLAG_EOS, stream_id, body.encode())
    }
}

// ---------------------------------------------------------------------------
// Per-subscription task
// ---------------------------------------------------------------------------

async fn run_subscription_task(
    stream_id: u32,
    target_shard: ShardId,
    filter: ParsedFilter,
    mut rx: broadcast::Receiver<EventEnvelope>,
    mut cancel_rx: watch::Receiver<bool>,
    final_lsn: Arc<AtomicU64>,
    frame_tx: flume::Sender<crate::connection::OutgoingFrame>,
) {
    loop {
        tokio::select! {
            biased;
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            res = rx.recv() => {
                match res {
                    Ok(env) => {
                        if !filter.matches(&env) {
                            continue;
                        }
                        let frame = build_subscription_event_frame(stream_id, &env);
                        final_lsn.store(env.lsn, Ordering::SeqCst);
                        if frame_tx
                            .send_async(crate::connection::OutgoingFrame {
                                bytes: frame.encode(),
                                close_after: false,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Slow subscriber — drop the subscription with
                        // an ERROR(Overloaded). Spec §17.4 / audit §8.1.
                        warn!(stream_id, target_shard, skipped, "subscription lagged");
                        let f = error_frame(
                            stream_id,
                            ErrorCode::Overloaded,
                            "subscription lagged; reconnect with a fresh from_lsn",
                        );
                        let _ = frame_tx
                            .send_async(crate::connection::OutgoingFrame {
                                bytes: f.encode(),
                                close_after: false,
                            })
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    // Final EOS frame on this subscription's stream.
    let eos = empty_subscription_event_frame(stream_id, final_lsn.load(Ordering::SeqCst));
    let _ = frame_tx
        .send_async(crate::connection::OutgoingFrame {
            bytes: eos.encode(),
            close_after: false,
        })
        .await;
}

fn build_subscription_event_frame(stream_id: u32, env: &EventEnvelope) -> Frame {
    let payload = ResponseBody::SubscribeEvent(env.to_wire()).encode();
    // Intermediate frames in an open-ended subscription don't carry
    // EOS — spec §03/09 §3.3.
    Frame::new(Opcode::SubscribeEvent.as_u16(), 0, stream_id, payload)
}

fn empty_subscription_event_frame(stream_id: u32, last_lsn: u64) -> Frame {
    // Terminal EOS frame for a cancelled / lagged subscription. The
    // body carries the last-delivered LSN so the client can resume.
    let payload = ResponseBody::SubscribeEvent(SubscriptionEvent {
        event_type: brain_protocol::response::EventType::Forgotten,
        memory_id: MemoryId::NULL.raw(),
        context_id: 0,
        text: String::new(),
        kind: brain_protocol::request::MemoryKindWire::Episodic,
        salience: 0.0,
        timestamp_unix_nanos: 0,
        lsn: last_lsn,
        knowledge_payload: None,
    })
    .encode();
    Frame::new(Opcode::SubscribeEvent.as_u16(), FLAG_EOS, stream_id, payload)
}

fn error_frame(stream_id: u32, code: ErrorCode, message: &str) -> Frame {
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(code.category()),
        message: message.to_owned(),
        details: None,
        retry_after_ms: None,
    });
    Frame::new(Opcode::Error.as_u16(), FLAG_EOS, stream_id, body.encode())
}

// ---------------------------------------------------------------------------
// UNSUBSCRIBE / CANCEL_STREAM frame builders
// ---------------------------------------------------------------------------

pub fn build_unsubscribe_response_frame(
    request_stream_id: u32,
    req: &UnsubscribeRequest,
    final_lsn: u64,
) -> Frame {
    let body = ResponseBody::Unsubscribe(UnsubscribeResponse {
        target_stream_id: req.target_stream_id,
        final_lsn,
    });
    Frame::new(
        Opcode::UnsubscribeResp.as_u16(),
        FLAG_EOS,
        request_stream_id,
        body.encode(),
    )
}

pub fn build_cancel_stream_ack_frame(
    request_stream_id: u32,
    req: &CancelStreamRequest,
    now_unix_nanos: u64,
) -> Frame {
    let body = ResponseBody::CancelStreamAck(CancelStreamAck {
        target_stream_id: req.target_stream_id,
        cancelled_at_unix_nanos: now_unix_nanos,
    });
    Frame::new(
        Opcode::CancelStreamAck.as_u16(),
        FLAG_EOS,
        request_stream_id,
        body.encode(),
    )
}
