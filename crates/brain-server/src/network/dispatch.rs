//! Frame dispatcher — Tokio↔Glommio boundary.
//!
//! Owns:
//!
//! - The per-connection state machine (HELLO → WELCOME → AUTH → AUTH_OK
//!   → Established → Closing).
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
// Several fields/variants are wired into the response shape but
// don't fan out into the connection-loop's match arms yet: AGENT id is
// captured at AUTH_OK but not yet used to authorize ops;
// `Action::Close` is the no-frame close case (CLIENT_PONG today) — not
// yet emitted but reserved. Allow rather than churn the
// surface in/out as each lands.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::AgentId;
use brain_ops::error::OpError;
use brain_protocol::connection::handshake::{
    AgentPermissions, AuthOkPayload, AuthPayload, HelloPayload, ServerCapabilities, WelcomePayload,
};
use brain_protocol::error::ErrorCode;

use crate::auth::{derive_scope_from_handshake, AuthError, AuthStore, RequestScope};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::{
    ErrorCategoryWire, ErrorCodeWire, ErrorResponse, PongResponse, ResponseBody, ServerPingResponse,
};
use brain_protocol::{Frame, ProtocolError};

use crate::routing::{shard_for_memory, RoutingTable};
use crate::shard::{DispatchError, ShardHandle};

// ---------------------------------------------------------------------------
// Conn state
// ---------------------------------------------------------------------------

/// Connection lifecycle phase.
#[derive(Clone, Debug)]
pub(crate) enum ConnPhase {
    AwaitingHello,
    AwaitingAuth,
    Established {
        agent: AgentId,
        bound_shard: u16,
        permissions: AgentPermissions,
        /// Resolved scope for this connection. In permissive mode
        /// (no `BRAIN_REQUIRE_SCOPED_API_KEYS`) this is permissive
        /// over the agent the client claimed; in strict mode it's
        /// derived from the API key.
        scope: RequestScope,
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
/// `routing` is wrapped in [`arc_swap::ArcSwap`] so future cluster
/// reconfiguration (admin RPC + gossip) can publish a
/// new `RoutingTable` atomically — without restarting connections.
/// Readers call `routing.load_full`
/// per request; the refcount bump is ~50 ns and invisible next to
/// the agent-id hash + shard lookup.
#[derive(Clone)]
pub struct Topology {
    pub shards: Arc<Vec<ShardHandle>>,
    pub routing: Arc<arc_swap::ArcSwap<RoutingTable>>,
    pub server_caps: Arc<ServerCapabilities>,
    /// Per-operation request metrics. Shared with the admin
    /// exposition path via `AdminState::request_metrics`.
    pub request_metrics: Arc<crate::metrics::request::RequestMetrics>,
    /// Scope-bound API key store. The AUTH handler resolves the
    /// presented secret here and stamps the result on `ConnPhase`.
    pub auth_store: Arc<AuthStore>,
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
    /// Open a new SUBSCRIBE stream. The connection
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
    /// Resolved AUTH-time scope (org / user / namespace / agent /
    /// permissions). Threaded into `brain_ops::dispatch` as a
    /// `RequestCaller` so handlers read scope from here instead of
    /// trusting client-supplied fields.
    pub(crate) scope: RequestScope,
    /// Wire-level session id minted at HELLO. Stamped onto the
    /// `RequestCaller` so TXN_BEGIN can link the new entry back to
    /// the originating connection — drives the connection-drop
    /// auto-abort sweep.
    pub(crate) session_id: [u8; 16],
}

pub(crate) struct SubscribeStart {
    /// Stream id the SUBSCRIBE_REQ rode in on (and where SUBSCRIPTION_
    /// EVENT frames flow out).
    pub(crate) stream_id: u32,
    pub(crate) req: brain_protocol::envelope::request::SubscribeRequest,
    pub(crate) target_shard: u16,
}

pub(crate) enum CancelSubscribe {
    Unsubscribe {
        request_stream_id: u32,
        req: brain_protocol::envelope::request::UnsubscribeRequest,
    },
    CancelStream {
        request_stream_id: u32,
        req: brain_protocol::envelope::request::CancelStreamRequest,
    },
}

// ---------------------------------------------------------------------------
// Public entry — synchronous decision per frame
// ---------------------------------------------------------------------------

/// Decide what to do with `frame` given the current `state`. Mutates
/// `state` for handshake / phase transitions. Pure aside from
/// `SystemTime::now()` in PONG and AUTH_OK.
pub(crate) fn dispatch_frame(frame: Frame, state: &mut ConnState, topology: &Topology) -> Action {
    let opcode = match Opcode::from_u16(frame.header.opcode_u16()) {
        Ok(o) => o,
        Err(_) => {
            // Unknown opcodes return BadOpcode and
            // the connection stays open (rather than closing).
            return Action::Inline(error_frame(
                frame.header.stream_id_u32(),
                ErrorCode::BadOpcode,
                "unknown opcode",
            ));
        }
    };

    // Stream-id rules:
    // - Connection-level opcodes (HELLO/AUTH/PING/PONG/BYE/…)
    //   MUST ride on stream_id = 0.
    // - Client-initiated op streams MUST be odd (and != 0).
    // - The ERROR opcode is exempt — the server can emit on
    //   either stream_id = 0 (handshake error) or the offending
    //   op's stream.
    let stream_id = frame.header.stream_id_u32();
    if opcode != Opcode::Error {
        if opcode.is_connection_level() {
            if stream_id != 0 {
                return Action::Inline(error_frame(
                    stream_id,
                    ErrorCode::BadFrame,
                    "connection-level opcode requires stream_id = 0",
                ));
            }
        } else if stream_id == 0 || stream_id.is_multiple_of(2) {
            return Action::Inline(error_frame(
                stream_id,
                ErrorCode::BadFrame,
                "op streams must be non-zero and odd (client-initiated)",
            ));
        }
    }

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
    let (bound_shard, scope) = match &state.phase {
        ConnPhase::Established {
            bound_shard, scope, ..
        } => (*bound_shard, scope.clone()),
        _ => {
            return Action::Inline(error_frame(
                frame.header.stream_id_u32(),
                ErrorCode::NotAuthenticated,
                "operation requires established (post-AUTH_OK) connection",
            ));
        }
    };

    // Reject opcodes we don't expect from the client (response opcodes,
    // server-pushed events).
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

    // SUBSCRIBE / UNSUBSCRIBE / CANCEL_STREAM bypass
    // the shard-dispatch path: they mutate the connection-layer
    // SubscriptionRegistry rather than fanning a request to brain_ops.
    let stream_id = frame.header.stream_id_u32();
    let req = match req {
        RequestBody::Subscribe(sub_req) => {
            // Under scoped API-key auth, a subscriber may only receive its
            // own agent's events. `filter.agents == None`/empty means "all
            // agents" (a cross-tenant leak on a shared shard), and any id
            // other than the caller's own agent is likewise forbidden —
            // mirrors RECALL's `enforce_agent_filter`.
            if !subscribe_agents_allowed(
                scope.scope_enforced,
                scope.agent_id,
                sub_req.filter.agents.as_deref(),
            ) {
                return Action::Inline(error_frame(
                    stream_id,
                    ErrorCode::PermissionDenied,
                    "subscribe: filter.agents must name only the API key's own \
                     agent under scoped API-key auth",
                ));
            }
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

    Action::OpDispatch(OpDispatch {
        stream_id: frame.header.stream_id_u32(),
        req,
        target_shard,
        // Stamp the AUTH-bound scope on every dispatched op. Handlers
        // read agent / namespace / permissions from this object and
        // ignore whatever the client supplied — the shared-shard
        // cross-agent leak the `agents` subscribe filter needs to
        // enforce closes here.
        scope,
        // Wire session id rides alongside so TXN_BEGIN can stamp it
        // on the new entry; the connection-drop sweep needs it to
        // find buffered work owned by a dying connection.
        session_id: state.session_id,
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
    let negotiated =
        match brain_protocol::connection::handshake::negotiate(&hello, &topology.server_caps) {
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
                    &format!(
                        "no mutual version (client max={client_max}, server max={server_max})"
                    ),
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

    // The server still advertises its accepted methods; reject anything
    // the policy doesn't allow. Both `Token` (scoped API key) and
    // `None` (dev / trusted-network mode) flow through `derive_scope`.
    if !topology
        .server_caps
        .server_features
        .auth_methods
        .iter()
        .any(|m| std::mem::discriminant(m) == std::mem::discriminant(&auth.method))
    {
        return Action::CloseWith(error_frame(
            0,
            ErrorCode::NoSuchAuthMethod,
            "auth method not in server policy",
        ));
    }

    let scope = match derive_scope_from_handshake(&auth, &topology.auth_store) {
        Ok(s) => s,
        Err(e) => {
            let code = match e {
                AuthError::Missing | AuthError::Unknown | AuthError::Revoked => {
                    ErrorCode::Unauthenticated
                }
                AuthError::PolicyForbidsAnonymous => ErrorCode::NoSuchAuthMethod,
                AuthError::Storage(_) => ErrorCode::Internal,
            };
            return Action::CloseWith(error_frame(0, code, &e.to_string()));
        }
    };

    let agent = scope.agent_id;
    // Refcount-bump load. The published table may swap between AUTH
    // and a later request; both observers see a coherent snapshot.
    let routing = topology.routing.load_full();
    let bound_shard = routing.shard_for_agent(agent);
    let permissions = scope.to_agent_permissions();
    state.phase = ConnPhase::Established {
        agent,
        bound_shard,
        permissions,
        scope: scope.clone(),
    };

    let auth_ok = AuthOkPayload {
        // Echo the AUTH-bound agent (which may differ from what the
        // client claimed when strict mode is on).
        agent_id: *agent.0.as_bytes(),
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
    // opcode for both directions, so we hand-build
    // the frame rather than going through `ResponseBody`.
    let reply = Frame::new(Opcode::Bye.as_u16(), FLAG_EOS, 0, frame.payload.clone());
    Action::CloseWith(reply)
}

// ---------------------------------------------------------------------------
// OpDispatch — runs in a per-op tokio sub-task
// ---------------------------------------------------------------------------

/// Run an `OpDispatch` and return the wire frames to send back. Runs
/// in a spawned tokio task per request; the receiver loop hands off
/// here and continues reading frames.
///
/// Single-frame ops return a one-element `Vec`. Streaming ops (PLAN /
/// REASON) return one frame per emitted body, with `is_final = true`
/// on the last frame only.
pub(crate) async fn run_op_dispatch(op: OpDispatch, shards: Arc<Vec<ShardHandle>>) -> Vec<Frame> {
    let stream_id = op.stream_id;
    let shard = match shards.get(op.target_shard as usize) {
        Some(s) => s,
        None => {
            return vec![error_frame(
                stream_id,
                ErrorCode::ShardUnavailable,
                &format!(
                    "target shard {} out of range [0, {})",
                    op.target_shard,
                    shards.len()
                ),
            )];
        }
    };
    let caller = op.scope.to_caller(op.session_id);
    // Root of the per-request trace. Held open for the whole op; the shard
    // re-enters a clone via `.instrument()` so `brain.encode` nests under it
    // across the Tokio→Glommio hop. In Phase 1 this is a trace root; once the
    // wire carries `traceparent` it becomes a child of the remote context.
    let request_span = tracing::info_span!(
        "client.request",
        brain.operation = ?op.req.opcode(),
        brain.agent_id = %caller.agent_id.0,
        brain.shard = op.target_shard,
        brain.stream_id = stream_id,
        trace_id = tracing::field::Empty,
        span_id = tracing::field::Empty,
    );
    // Surface the OTel trace/span id on the request span so the JSON log
    // formatter (which emits span fields) carries them — operators pivot
    // trace↔logs by id. The id is assigned synchronously when the OTel layer
    // sees the new span, so it is readable here. No-op when tracing is
    // disabled: the context then holds an invalid (all-zero) span context.
    {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let cx = request_span.context();
        let span_ctx = cx.span().span_context().clone();
        if span_ctx.is_valid() {
            request_span.record("trace_id", span_ctx.trace_id().to_string());
            request_span.record("span_id", span_ctx.span_id().to_string());
        }
    }
    match shard
        .dispatch_op(op.req, caller, request_span.clone())
        .await
    {
        Ok(outcome) => match outcome {
            brain_ops::DispatchOutcome::Single(body) => {
                vec![build_response_frame(stream_id, true, body)]
            }
            brain_ops::DispatchOutcome::Stream(bodies) => {
                let n = bodies.len();
                bodies
                    .into_iter()
                    .enumerate()
                    .map(|(i, body)| build_response_frame(stream_id, i + 1 == n, body))
                    .collect()
            }
        },
        Err(DispatchError::ShardDisconnected) => vec![error_frame(
            stream_id,
            ErrorCode::ShardUnavailable,
            "shard is no longer accepting requests",
        )],
        Err(DispatchError::Op(e)) => vec![error_frame_from_op_error(stream_id, &e)],
    }
}

// ---------------------------------------------------------------------------
// Routing helpers
// ---------------------------------------------------------------------------

fn pick_target_shard(req: &RequestBody, bound_shard: u16, routing: &RoutingTable) -> Option<u16> {
    // Requests carrying a target MemoryId route by memory shard; other
    // requests use the agent's bound shard. The `source` end of LINK /
    // UNLINK is the routing anchor; the `target`
    // memory's shard may differ (cross-shard edges land later).
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

const FLAG_EOS: u8 = 1 << 7;

fn build_response_frame(stream_id: u32, eos: bool, body: ResponseBody) -> Frame {
    let opcode = body.opcode().as_u16();
    let flags = if eos { FLAG_EOS } else { 0 };
    let payload = body.encode();
    Frame::new(opcode, flags, stream_id, payload)
}

/// Whether a SUBSCRIBE's `filter.agents` is allowed for this scope.
///
/// Under scoped API-key auth a subscriber may only receive its own
/// agent's events, so `agents` must be a non-empty list naming only the
/// caller's own agent — `None`/empty (= all agents on the shard) is a
/// cross-tenant leak and is rejected. Permissive mode allows any filter.
/// Mirrors RECALL's `enforce_agent_filter`.
fn subscribe_agents_allowed(
    scope_enforced: bool,
    own: AgentId,
    agents: Option<&[[u8; 16]]>,
) -> bool {
    if !scope_enforced {
        return true;
    }
    agents.is_some_and(|a| !a.is_empty() && a.iter().all(|b| AgentId::from(*b) == own))
}

pub(crate) fn error_frame(stream_id: u32, code: ErrorCode, message: &str) -> Frame {
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
        brain_ops::error::ErrorCode::EntityNotFound => (ErrorCode::EntityNotFound, None),
        brain_ops::error::ErrorCode::StatementNotFound => (ErrorCode::StatementNotFound, None),
        brain_ops::error::ErrorCode::QuotaExceeded => (ErrorCode::RateLimited, None),
        brain_ops::error::ErrorCode::Unauthorized => (ErrorCode::PermissionDenied, None),
        brain_ops::error::ErrorCode::Conflict => (ErrorCode::IdempotencyConflict, None),
        brain_ops::error::ErrorCode::TxnExpired => (ErrorCode::TransactionTimeout, None),
        brain_ops::error::ErrorCode::TxnNotFound => (ErrorCode::TxnNotFound, None),
        brain_ops::error::ErrorCode::TransactionTooLarge => (ErrorCode::TransactionTooLarge, None),
        brain_ops::error::ErrorCode::PredicateNotInSchema => {
            (ErrorCode::PredicateNotInSchema, None)
        }
        brain_ops::error::ErrorCode::RelationTypeNotInSchema => {
            (ErrorCode::RelationTypeNotInSchema, None)
        }
        brain_ops::error::ErrorCode::CardinalityViolation => {
            (ErrorCode::CardinalityViolation, None)
        }
        brain_ops::error::ErrorCode::Overloaded => (ErrorCode::Overloaded, Some(1000u32)),
        brain_ops::error::ErrorCode::RetrievalUnavailable => (ErrorCode::ShardUnavailable, None),
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
    use brain_protocol::connection::handshake::{AuthMethod, HelloCapabilities};

    fn test_topology() -> Topology {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let auth_store = Arc::new(
            crate::auth::AuthStore::open(tmp.path().join("api_keys.redb"), false)
                .expect("open auth store"),
        );
        // Leak the tempdir into a 'static slot — we want the file alive
        // for the lifetime of the test, and the per-test `Topology` is
        // dropped at the end of the test anyway.
        std::mem::forget(tmp);
        Topology {
            shards: Arc::new(Vec::new()),
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(
                RoutingTable::new(1, std::collections::HashMap::new()).unwrap(),
            )),
            server_caps: Arc::new(ServerCapabilities::v1_default(
                "brain-server/test",
                vec![AuthMethod::None, AuthMethod::Token],
            )),
            request_metrics: Arc::new(crate::metrics::request::RequestMetrics::new()),
            auth_store,
        }
    }

    fn build_hello_frame() -> Frame {
        let hello = HelloPayload {
            client_id: "tester/0.1".to_owned(),
            supported_versions: vec![brain_protocol::VERSION],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
            client_session_token: None,
        };
        Frame::new(Opcode::Hello.as_u16(), FLAG_EOS, 0, hello.encode())
    }

    #[test]
    fn hello_transitions_to_awaiting_auth_and_emits_welcome() {
        let mut state = ConnState::new();
        let topo = test_topology();
        let action = dispatch_frame(build_hello_frame(), &mut state, &topo);
        match action {
            Action::Inline(f) => {
                assert_eq!(f.header.opcode_u16(), Opcode::Welcome.as_u16());
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
        let body = RequestBody::Encode(brain_protocol::envelope::request::EncodeRequest {
            text: "hello".into(),
            context_id: 0,
            request_id: [0u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
        });
        let frame = Frame::new(Opcode::EncodeReq.as_u16(), FLAG_EOS, 1, body.encode());
        let action = dispatch_frame(frame, &mut state, &topo);
        match action {
            Action::Inline(f) => {
                assert_eq!(f.header.opcode_u16(), Opcode::Error.as_u16());
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
        let frame = Frame::new(Opcode::Hello.as_u16(), FLAG_EOS, 0, bad.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::CloseWith(f) => assert_eq!(f.header.opcode_u16(), Opcode::Error.as_u16()),
            _ => panic!("expected CloseWith(ERROR)"),
        }
    }

    #[test]
    fn ping_round_trips_timestamps() {
        let payload = brain_protocol::envelope::request::PingRequest {
            client_timestamp_unix_nanos: 42,
        };
        let body = RequestBody::Ping(payload);
        let frame = Frame::new(Opcode::Ping.as_u16(), FLAG_EOS, 0, body.encode());
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(f) => assert_eq!(f.header.opcode_u16(), Opcode::Pong.as_u16()),
            _ => panic!("expected Inline(PONG)"),
        }
    }

    /// HELLO with stream_id != 0 returns BadFrame and stays
    /// open.
    #[test]
    fn connection_level_opcode_rejects_nonzero_stream() {
        // HELLO payload is valid; the only violation is stream_id=1.
        let hello = build_hello_frame();
        let frame = Frame::new(Opcode::Hello.as_u16(), FLAG_EOS, 1, hello.payload);
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(reply) => {
                assert_eq!(reply.header.opcode_u16(), Opcode::Error.as_u16());
                assert_eq!(reply.header.stream_id_u32(), 1);
            }
            _ => panic!("expected Inline(ERROR) on bad-stream HELLO"),
        }
    }

    /// Client op on even stream_id is BadFrame.
    #[test]
    fn op_stream_must_be_odd() {
        // EncodeReq on stream_id = 2 (even). The op is client-bound
        // (`is_request() == true`, `is_connection_level() == false`).
        let frame = Frame::new(Opcode::EncodeReq.as_u16(), FLAG_EOS, 2, Vec::new());
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(reply) => {
                assert_eq!(reply.header.opcode_u16(), Opcode::Error.as_u16());
                assert_eq!(reply.header.stream_id_u32(), 2);
            }
            _ => panic!("expected Inline(ERROR) on even op stream_id"),
        }
    }

    #[test]
    fn subscribe_agents_scope_guard() {
        let own = AgentId::from([7u8; 16]);
        let other = [9u8; 16];
        // Permissive mode: any filter is allowed.
        assert!(subscribe_agents_allowed(false, own, None));
        assert!(subscribe_agents_allowed(false, own, Some(&[other])));
        // Scoped mode: only a non-empty list of the caller's own agent.
        assert!(subscribe_agents_allowed(true, own, Some(&[[7u8; 16]])));
        // Scoped mode rejects: None (= all), empty (= all), other agent,
        // and any list that includes another agent.
        assert!(!subscribe_agents_allowed(true, own, None));
        assert!(!subscribe_agents_allowed(true, own, Some(&[])));
        assert!(!subscribe_agents_allowed(true, own, Some(&[other])));
        assert!(!subscribe_agents_allowed(
            true,
            own,
            Some(&[[7u8; 16], other])
        ));
    }

    /// Client op on stream_id = 0 is BadFrame.
    #[test]
    fn op_stream_must_be_nonzero() {
        let frame = Frame::new(Opcode::EncodeReq.as_u16(), FLAG_EOS, 0, Vec::new());
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(reply) => {
                assert_eq!(reply.header.opcode_u16(), Opcode::Error.as_u16());
            }
            _ => panic!("expected Inline(ERROR) on stream_id=0 op"),
        }
    }

    /// Unknown opcode returns BadOpcode but the connection stays
    /// open (Action::Inline, not CloseWith).
    #[test]
    fn unknown_opcode_stays_open() {
        // 0xAA is not in the Opcode enum.
        let frame = Frame::new(0xAA, 0, 7, Vec::new());
        let mut state = ConnState::new();
        let topo = test_topology();
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(reply) => {
                assert_eq!(
                    reply.header.opcode_u16(),
                    Opcode::Error.as_u16(),
                    "expected an Error frame"
                );
                assert_eq!(
                    reply.header.stream_id_u32(),
                    7,
                    "error should be on the offending stream id"
                );
            }
            Action::CloseWith(_) => panic!("F-3 regression: connection closed on unknown opcode"),
            _ => panic!("expected Action::Inline(ERROR)"),
        }
    }
}
