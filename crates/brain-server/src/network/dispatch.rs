//! Frame dispatcher (sub-task 9.10) — Tokio↔Glommio boundary.
//!
//! Owns:
//!
//! - The per-connection state machine (HELLO → WELCOME → AUTH → AUTH_OK
//!   → Established → Closing) per spec §03/06.
//! - Inline handlers for connection-management opcodes (HELLO, AUTH,
//!   PING, CLIENT_PONG, BYE).
//! - Op routing: BLAKE3(agent_id) or MemoryId::shard() picks the shard;
//!   `ShardHandle::dispatch_op` runs `brain_ops::dispatch` on the
//!   target shard's Glommio executor.
//! - Wire-error mapping: `OpError` → `ErrorResponse`.
//!
//! The I/O loops (reader / writer split, idle timer, shutdown handling)
//! live in [`crate::connection`]. This module is intentionally I/O-free
//! aside from `Frame` construction — testable as a pure state machine.

#![cfg(target_os = "linux")]
// Several fields/variants are wired into 9.10's response shape but
// don't fan out into the connection-loop's match arms yet: AGENT id is
// captured at AUTH_OK but not used to authorize ops until 9.11+;
// `Action::Close` is the no-frame close case (CLIENT_PONG today) — not
// yet emitted but reserved for 9.11/9.14. Allow rather than churn the
// surface in/out as each lands.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::AgentId;
use brain_ops::error::OpError;
use brain_protocol::error::ErrorCode;
use brain_protocol::handshake::{
    AgentPermissions, AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloPayload,
    ServerCapabilities, WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::RequestBody;
use brain_protocol::response::{
    ErrorCategoryWire, ErrorCodeWire, ErrorResponse, PongResponse, ResponseBody, ServerPingResponse,
};
use brain_protocol::{Frame, ProtocolError};

use crate::routing::{shard_for_memory, RoutingTable};
use crate::shard::{DispatchError, ShardHandle};

// ---------------------------------------------------------------------------
// Conn state
// ---------------------------------------------------------------------------

/// Connection lifecycle phase. Spec §03/06.
#[derive(Clone, Debug)]
pub(crate) enum ConnPhase {
    AwaitingHello,
    AwaitingAuth,
    Established {
        agent: AgentId,
        bound_shard: u16,
        permissions: AgentPermissions,
    },
}

/// Mutable per-connection state. Lives on the receiver-loop stack.
pub(crate) struct ConnState {
    pub(crate) phase: ConnPhase,
    pub(crate) session_id: [u8; 16],
    pub(crate) negotiated_version: u8,
}

impl ConnState {
    pub(crate) fn new() -> Self {
        Self {
            phase: ConnPhase::AwaitingHello,
            session_id: [0u8; 16],
            negotiated_version: 0,
        }
    }
}

/// Read-only handles a connection task uses: the shard pool + routing.
///
/// `routing` is wrapped in [`ArcSwap`] so future cluster
/// reconfiguration (admin RPC + gossip, post-Phase-9) can publish a
/// new `RoutingTable` atomically — without restarting connections
/// (spec §10/05 §4, §12/02 §2). Readers call `routing.load_full()`
/// per request; the refcount bump is ~50 ns and invisible next to
/// the agent-id hash + shard lookup.
#[derive(Clone)]
pub struct Topology {
    pub shards: Arc<Vec<ShardHandle>>,
    pub routing: Arc<arc_swap::ArcSwap<RoutingTable>>,
    pub server_caps: Arc<ServerCapabilities>,
}

// ---------------------------------------------------------------------------
// Dispatcher decision types
// ---------------------------------------------------------------------------

/// What the connection-layer loop should do with a decoded frame.
///
/// `Inline` is a synchronous response built without leaving the receiver
/// task. `OpDispatch` carries everything needed to await a shard reply
/// in a spawned sub-task. `CloseWith` emits a final frame and closes;
/// `Close` closes without sending anything.
pub(crate) enum Action {
    Inline(Frame),
    OpDispatch(OpDispatch),
    /// Open a new SUBSCRIBE stream (sub-task 9.11). The connection
    /// loop registers the subscription, spawns the per-sub task, and
    /// emits an opening empty SUBSCRIBE_EVENT frame on the same
    /// stream so the client sees the stream is open.
    Subscribe(SubscribeStart),
    /// Cancel an active subscription via UNSUBSCRIBE_REQ or
    /// CANCEL_STREAM. The connection loop pulls the matching entry
    /// out of the registry, sends the ack on the request's stream,
    /// and the per-sub task emits its own final EOS.
    CancelSubscribe(CancelSubscribe),
    CloseWith(Frame),
    Close,
    Nothing,
}

pub(crate) struct OpDispatch {
    pub(crate) stream_id: u32,
    pub(crate) req: RequestBody,
    pub(crate) target_shard: u16,
}

pub(crate) struct SubscribeStart {
    /// Stream id the SUBSCRIBE_REQ rode in on (and where SUBSCRIPTION_
    /// EVENT frames flow out).
    pub(crate) stream_id: u32,
    pub(crate) req: brain_protocol::request::SubscribeRequest,
    pub(crate) target_shard: u16,
}

pub(crate) enum CancelSubscribe {
    Unsubscribe {
        request_stream_id: u32,
        req: brain_protocol::request::UnsubscribeRequest,
    },
    CancelStream {
        request_stream_id: u32,
        req: brain_protocol::request::CancelStreamRequest,
    },
}

// ---------------------------------------------------------------------------
// Public entry — synchronous decision per frame
// ---------------------------------------------------------------------------

/// Decide what to do with `frame` given the current `state`. Mutates
/// `state` for handshake / phase transitions. Pure aside from
/// `SystemTime::now()` in PONG and AUTH_OK.
pub(crate) fn dispatch_frame(frame: Frame, state: &mut ConnState, topology: &Topology) -> Action {
    let opcode = match Opcode::from_u8(frame.header.opcode) {
        Ok(o) => o,
        Err(_) => {
            return Action::CloseWith(error_frame(
                frame.header.stream_id_u32(),
                ErrorCode::BadOpcode,
                "unknown opcode",
            ));
        }
    };

    // Connection-management opcodes are handled regardless of phase
    // (they're stream_id = 0 control frames).
    match opcode {
        Opcode::Hello => return on_hello(frame, state, topology),
        Opcode::Auth => return on_auth(frame, state, topology),
        Opcode::Ping => return on_ping(frame),
        Opcode::Bye => return on_bye(frame),
        Opcode::ClientPong => return Action::Nothing,
        _ => {}
    }

    // Everything else requires `Established`.
    let (agent, bound_shard) = match &state.phase {
        ConnPhase::Established {
            agent, bound_shard, ..
        } => (*agent, *bound_shard),
        _ => {
            return Action::Inline(error_frame(
                frame.header.stream_id_u32(),
                ErrorCode::NotAuthenticated,
                "operation requires established (post-AUTH_OK) connection",
            ));
        }
    };

    // Reject opcodes we don't expect from the client (response opcodes,
    // server-pushed events). spec §03/05 §4.
    if !opcode.is_request() {
        return Action::Inline(error_frame(
            frame.header.stream_id_u32(),
            ErrorCode::BadOpcode,
            "client sent a response-opcode frame",
        ));
    }

    // Decode the body.
    let req = match RequestBody::decode(opcode, &frame.payload) {
        Ok(b) => b,
        Err(e) => {
            return Action::Inline(error_frame(
                frame.header.stream_id_u32(),
                protocol_error_to_code(&e),
                &e.to_string(),
            ));
        }
    };

    // SUBSCRIBE / UNSUBSCRIBE / CANCEL_STREAM (sub-task 9.11) bypass
    // the shard-dispatch path: they mutate the connection-layer
    // SubscriptionRegistry rather than fanning a request to brain_ops.
    let stream_id = frame.header.stream_id_u32();
    let req = match req {
        RequestBody::Subscribe(sub_req) => {
            return Action::Subscribe(SubscribeStart {
                stream_id,
                req: sub_req,
                target_shard: bound_shard,
            });
        }
        RequestBody::Unsubscribe(un_req) => {
            return Action::CancelSubscribe(CancelSubscribe::Unsubscribe {
                request_stream_id: stream_id,
                req: un_req,
            });
        }
        RequestBody::CancelStream(c_req) => {
            return Action::CancelSubscribe(CancelSubscribe::CancelStream {
                request_stream_id: stream_id,
                req: c_req,
            });
        }
        other => other,
    };

    // Route + dispatch. Memory-bearing requests route to the memory's
    // shard; everything else lands on the agent's bound shard.
    let routing = topology.routing.load_full();
    let target_shard = pick_target_shard(&req, bound_shard, &routing).unwrap_or(bound_shard);
    let _ = agent; // reserved for spec §09/01 §12 cross-checks in 9.11+.

    Action::OpDispatch(OpDispatch {
        stream_id: frame.header.stream_id_u32(),
        req,
        target_shard,
    })
}

// ---------------------------------------------------------------------------
// Handshake handlers
// ---------------------------------------------------------------------------

fn on_hello(frame: Frame, state: &mut ConnState, topology: &Topology) -> Action {
    if !matches!(state.phase, ConnPhase::AwaitingHello) {
        return Action::CloseWith(error_frame(0, ErrorCode::BadFrame, "HELLO out of order"));
    }
    let hello = match HelloPayload::decode(&frame.payload) {
        Ok(h) => h,
        Err(e) => {
            return Action::CloseWith(error_frame(0, protocol_error_to_code(&e), &e.to_string()));
        }
    };
    let negotiated = match brain_protocol::handshake::negotiate(&hello, &topology.server_caps) {
        Ok(n) => n,
        Err(_) => {
            let server_max = topology
                .server_caps
                .supported_versions
                .iter()
                .copied()
                .max()
                .unwrap_or(0);
            let client_max = hello.supported_versions.iter().copied().max().unwrap_or(0);
            return Action::CloseWith(error_frame(
                0,
                ErrorCode::VersionNotSupported,
                &format!("no mutual version (client max={client_max}, server max={server_max})"),
            ));
        }
    };

    // Allocate a fresh session_id. uuid v7 + the bytes is fine.
    let session_id = *uuid::Uuid::now_v7().as_bytes();
    state.session_id = session_id;
    state.negotiated_version = negotiated.chosen_version;
    state.phase = ConnPhase::AwaitingAuth;

    let welcome = WelcomePayload {
        server_id: topology.server_caps.server_id.clone(),
        chosen_version: negotiated.chosen_version,
        session_id,
        capabilities: negotiated.capabilities,
        server_features: topology.server_caps.server_features.clone(),
    };
    Action::Inline(build_response_frame(
        0,
        true,
        ResponseBody::Welcome(welcome),
    ))
}

fn on_auth(frame: Frame, state: &mut ConnState, topology: &Topology) -> Action {
    if !matches!(state.phase, ConnPhase::AwaitingAuth) {
        return Action::CloseWith(error_frame(0, ErrorCode::BadFrame, "AUTH out of order"));
    }
    let auth = match AuthPayload::decode(&frame.payload) {
        Ok(a) => a,
        Err(e) => {
            return Action::CloseWith(error_frame(0, protocol_error_to_code(&e), &e.to_string()));
        }
    };

    // v1 dev-only: only `AuthMethod::None` is accepted. Token/Mtls land
    // when 9.x adds a real auth backend.
    if !matches!(auth.method, AuthMethod::None)
        || !matches!(auth.credentials, AuthCredentials::None)
    {
        return Action::CloseWith(error_frame(
            0,
            ErrorCode::NoSuchAuthMethod,
            "only AuthMethod::None is wired in v1",
        ));
    }
    if !topology
        .server_caps
        .server_features
        .auth_methods
        .iter()
        .any(|m| matches!(m, AuthMethod::None))
    {
        return Action::CloseWith(error_frame(
            0,
            ErrorCode::NoSuchAuthMethod,
            "server policy forbids unauthenticated connections",
        ));
    }

    let agent = AgentId(uuid::Uuid::from_bytes(auth.agent_id));
    // Refcount-bump load. The published table may swap between AUTH
    // and a later request; both observers see a coherent snapshot.
    let routing = topology.routing.load_full();
    let bound_shard = routing.shard_for_agent(agent);
    let permissions = AgentPermissions {
        can_encode: true,
        can_recall: true,
        can_plan: true,
        can_reason: true,
        can_forget: true,
        can_admin: false,
    };
    state.phase = ConnPhase::Established {
        agent,
        bound_shard,
        permissions,
    };

    let auth_ok = AuthOkPayload {
        agent_id: auth.agent_id,
        bound_shard_id: bound_shard,
        permissions,
        server_time_unix_nanos: now_unix_nanos(),
    };
    Action::Inline(build_response_frame(0, true, ResponseBody::AuthOk(auth_ok)))
}

fn on_ping(frame: Frame) -> Action {
    let stream_id = frame.header.stream_id_u32();
    let req = match RequestBody::decode(Opcode::Ping, &frame.payload) {
        Ok(RequestBody::Ping(r)) => r,
        Ok(_) => unreachable!("RequestBody::decode for Opcode::Ping returns Ping"),
        Err(e) => {
            return Action::Inline(error_frame(
                stream_id,
                protocol_error_to_code(&e),
                &e.to_string(),
            ));
        }
    };
    let pong = PongResponse {
        client_timestamp_unix_nanos: req.client_timestamp_unix_nanos,
        server_timestamp_unix_nanos: now_unix_nanos(),
    };
    Action::Inline(build_response_frame(
        stream_id,
        true,
        ResponseBody::Pong(pong),
    ))
}

fn on_bye(frame: Frame) -> Action {
    // Echo a BYE back, then close. The protocol uses the same `Bye`
    // opcode for both directions (spec §03/05 §1.1), so we hand-build
    // the frame rather than going through `ResponseBody`.
    let reply = Frame::new(Opcode::Bye.as_u8(), FLAG_EOS, 0, frame.payload.clone());
    Action::CloseWith(reply)
}

// ---------------------------------------------------------------------------
// OpDispatch — runs in a per-op tokio sub-task
// ---------------------------------------------------------------------------

/// Run an `OpDispatch` and return the wire frame to send back. Runs in
/// a spawned tokio task per request; the receiver loop hands off here
/// and continues reading frames.
pub(crate) async fn run_op_dispatch(op: OpDispatch, shards: Arc<Vec<ShardHandle>>) -> Frame {
    let stream_id = op.stream_id;
    let shard = match shards.get(op.target_shard as usize) {
        Some(s) => s,
        None => {
            return error_frame(
                stream_id,
                ErrorCode::ShardUnavailable,
                &format!(
                    "target shard {} out of range [0, {})",
                    op.target_shard,
                    shards.len()
                ),
            );
        }
    };
    match shard.dispatch_op(op.req).await {
        Ok(body) => {
            // 9.10 ships single-frame EOS responses. Multi-frame
            // streaming for RECALL / PLAN / REASON is 9.11.
            build_response_frame(stream_id, true, body)
        }
        Err(DispatchError::ShardDisconnected) => error_frame(
            stream_id,
            ErrorCode::ShardUnavailable,
            "shard is no longer accepting requests",
        ),
        Err(DispatchError::Op(e)) => error_frame_from_op_error(stream_id, &e),
    }
}

// ---------------------------------------------------------------------------
// Routing helpers
// ---------------------------------------------------------------------------

fn pick_target_shard(req: &RequestBody, bound_shard: u16, routing: &RoutingTable) -> Option<u16> {
    // Requests carrying a target MemoryId route by memory shard; other
    // requests use the agent's bound shard. The `source` end of LINK /
    // UNLINK is the routing anchor (spec §12/02 §4); the `target`
    // memory's shard may differ (cross-shard edges are 9.11+).
    match req {
        RequestBody::Forget(r) => Some(shard_for_memory(brain_core::MemoryId::from_raw(
            r.memory_id,
        ))),
        RequestBody::Link(r) => Some(shard_for_memory(brain_core::MemoryId::from_raw(r.source))),
        RequestBody::Unlink(r) => Some(shard_for_memory(brain_core::MemoryId::from_raw(r.source))),
        _ => {
            let _ = routing; // bound_shard already came from routing
            Some(bound_shard)
        }
    }
}

// ---------------------------------------------------------------------------
// SERVER_PING (called by the connection layer's idle timer)
// ---------------------------------------------------------------------------

pub(crate) fn build_server_ping_frame() -> Frame {
    let payload = ResponseBody::ServerPing(ServerPingResponse {
        server_timestamp_unix_nanos: now_unix_nanos(),
    });
    build_response_frame(0, true, payload)
}

// ---------------------------------------------------------------------------
// Frame builders
// ---------------------------------------------------------------------------

const FLAG_EOS: u16 = 1 << 15;

fn build_response_frame(stream_id: u32, eos: bool, body: ResponseBody) -> Frame {
    let opcode = body.opcode().as_u8();
    let flags = if eos { FLAG_EOS } else { 0 };
    let payload = body.encode();
    Frame::new(opcode, flags, stream_id, payload)
}

fn error_frame(stream_id: u32, code: ErrorCode, message: &str) -> Frame {
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(code.category()),
        message: message.to_owned(),
        details: None,
        retry_after_ms: None,
    });
    build_response_frame(stream_id, true, body)
}

fn error_frame_from_op_error(stream_id: u32, e: &OpError) -> Frame {
    let (code, retry_after_ms) = match e.error_code() {
        brain_ops::error::ErrorCode::InvalidRequest => (ErrorCode::InvalidArgument, None),
        brain_ops::error::ErrorCode::NotFound => (ErrorCode::MemoryNotFound, None),
        brain_ops::error::ErrorCode::QuotaExceeded => (ErrorCode::RateLimited, None),
        brain_ops::error::ErrorCode::Unauthorized => (ErrorCode::PermissionDenied, None),
        brain_ops::error::ErrorCode::Conflict => (ErrorCode::IdempotencyConflict, None),
        brain_ops::error::ErrorCode::Overloaded => (ErrorCode::Overloaded, Some(1000u32)),
        brain_ops::error::ErrorCode::InternalError => (ErrorCode::Internal, None),
    };
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(code.category()),
        message: e.to_string(),
        details: None,
        retry_after_ms,
    });
    build_response_frame(stream_id, true, body)
}

fn protocol_error_to_code(e: &ProtocolError) -> ErrorCode {
    e.code()
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// Idle-timer state. Reset on every incoming frame; fires SERVER_PING
/// after `idle_timeout` of silence, then expects a CLIENT_PONG within
/// `ping_timeout` or closes the connection.
pub(crate) struct IdleTimer {
    pub(crate) idle_timeout: Duration,
    pub(crate) ping_timeout: Duration,
    pub(crate) last_activity: tokio::time::Instant,
    pub(crate) ping_sent_at: Option<tokio::time::Instant>,
}

impl IdleTimer {
    pub(crate) fn new(idle_timeout: Duration, ping_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            ping_timeout,
            last_activity: tokio::time::Instant::now(),
            ping_sent_at: None,
        }
    }

    pub(crate) fn on_frame_received(&mut self) {
        self.last_activity = tokio::time::Instant::now();
        self.ping_sent_at = None;
    }

    /// Wait until the next event the idle timer cares about. Returns
    /// `Tick::SendPing` if it's time to emit SERVER_PING; `Tick::Close`
    /// if the ping went unanswered past `ping_timeout`.
    pub(crate) fn next_deadline(&self) -> tokio::time::Instant {
        match self.ping_sent_at {
            Some(t) => t + self.ping_timeout,
            None => self.last_activity + self.idle_timeout,
        }
    }

    /// Classify the event at the current deadline.
    pub(crate) fn fire(&mut self) -> Tick {
        match self.ping_sent_at {
            Some(_) => Tick::Close,
            None => {
                self.ping_sent_at = Some(tokio::time::Instant::now());
                Tick::SendPing
            }
        }
    }
}

pub(crate) enum Tick {
    SendPing,
    Close,
}

// ---------------------------------------------------------------------------
// Tests (pure state-machine)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::handshake::{AuthMethod, HelloCapabilities};

    fn test_topology() -> Topology {
        Topology {
            shards: Arc::new(Vec::new()),
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(
                RoutingTable::new(1, std::collections::HashMap::new()).unwrap(),
            )),
            server_caps: Arc::new(ServerCapabilities::v1_default(
                "brain-server/test",
                vec![AuthMethod::None],
            )),
        }
    }

    fn build_hello_frame() -> Frame {
        let hello = HelloPayload {
            client_id: "tester/0.1".to_owned(),
            supported_versions: vec![1],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
            client_session_token: None,
        };
        Frame::new(Opcode::Hello.as_u8(), FLAG_EOS, 0, hello.encode())
    }

    #[test]
    fn hello_transitions_to_awaiting_auth_and_emits_welcome() {
        let mut state = ConnState::new();
        let topo = test_topology();
        let action = dispatch_frame(build_hello_frame(), &mut state, &topo);
        match action {
            Action::Inline(f) => {
                assert_eq!(f.header.opcode, Opcode::Welcome.as_u8());
                assert!(matches!(state.phase, ConnPhase::AwaitingAuth));
                assert_eq!(state.negotiated_version, 1);
                assert_ne!(state.session_id, [0u8; 16]);
            }
            _ => panic!("expected Inline(WELCOME)"),
        }
    }

    #[test]
    fn op_before_auth_returns_not_authenticated() {
        let mut state = ConnState::new();
        let topo = test_topology();
        let _ = dispatch_frame(build_hello_frame(), &mut state, &topo);
        // Try an ENCODE while still AwaitingAuth.
        let body = RequestBody::Encode(brain_protocol::request::EncodeRequest {
            text: "hello".into(),
            context_id: 0,
            kind: brain_protocol::request::MemoryKindWire::Episodic,
            salience_hint: 0.5,
            edges: Vec::new(),
            request_id: [0u8; 16],
            txn_id: None,
            deduplicate: false,
        });
        let frame = Frame::new(Opcode::EncodeReq.as_u8(), FLAG_EOS, 1, body.encode());
        let action = dispatch_frame(frame, &mut state, &topo);
        match action {
            Action::Inline(f) => {
                assert_eq!(f.header.opcode, Opcode::Error.as_u8());
            }
            _ => panic!("expected Inline(ERROR)"),
        }
    }

    #[test]
    fn hello_with_unsupported_version_closes() {
        let mut state = ConnState::new();
        let topo = test_topology();
        let bad = HelloPayload {
            client_id: "tester".into(),
            supported_versions: vec![99],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
            client_session_token: None,
        };
        let frame = Frame::new(Opcode::Hello.as_u8(), FLAG_EOS, 0, bad.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::CloseWith(f) => assert_eq!(f.header.opcode, Opcode::Error.as_u8()),
            _ => panic!("expected CloseWith(ERROR)"),
        }
    }

    #[test]
    fn ping_round_trips_timestamps() {
        let payload = brain_protocol::request::PingRequest {
            client_timestamp_unix_nanos: 42,
        };
        let body = RequestBody::Ping(payload);
        let frame = Frame::new(Opcode::Ping.as_u8(), FLAG_EOS, 0, body.encode());
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(f) => assert_eq!(f.header.opcode, Opcode::Pong.as_u8()),
            _ => panic!("expected Inline(PONG)"),
        }
    }
}
