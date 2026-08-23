//! Frame dispatcher — Tokio↔Glommio boundary.
//!
//! Owns:
//!
//! - The per-connection state machine (HELLO → WELCOME → AUTH → AUTH_OK
//!   → Established → Closing).
//! - Inline handlers for connection-management opcodes (HELLO, AUTH,
//!   PING, CLIENT_PONG, BYE).
//! - Op routing: BLAKE3(space_id) or MemoryId::shard() picks the shard;
//!   `ShardHandle::dispatch_op` runs `brain_ops::dispatch` on the
//!   target shard's Glommio executor.
//! - Wire-error mapping: `OpError` → `ErrorResponse`.
//!
//! The I/O loops (reader / writer split, idle timer, shutdown handling)
//! live in [`crate::connection`]. This module is intentionally I/O-free
//! aside from `Frame` construction — testable as a pure state machine.

#![cfg(target_os = "linux")]
// Several fields/variants are wired into the response shape but
// don't fan out into the connection-loop's match arms yet: SPACE id is
// captured at AUTH_OK but not yet used to authorize ops;
// `Action::Close` is the no-frame close case (CLIENT_PONG today) — not
// yet emitted but reserved. Allow rather than churn the
// surface in/out as each lands.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::SpaceId;
use brain_metadata::api_keys::bits;
use brain_ops::error::OpError;
use brain_protocol::connection::handshake::{
    AuthOkPayload, AuthPayload, HelloPayload, ServerCapabilities, SpacePermissions, WelcomePayload,
};
use brain_protocol::error::ErrorCode;

use crate::auth::{derive_scope_from_handshake, hex32, AuthError, AuthStore, RequestScope};
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
        space: SpaceId,
        bound_shard: u16,
        permissions: SpacePermissions,
        /// Resolved scope for this connection, derived entirely from the
        /// presented API key.
        scope: RequestScope,
    },
}

/// Mutable per-connection state. Lives on the receiver-loop stack.
pub(crate) struct ConnState {
    pub(crate) phase: ConnPhase,
    pub(crate) connection_id: [u8; 16],
    pub(crate) negotiated_version: u8,
    /// Monotonic instant of the last authoritative revocation check for
    /// this connection's key. Stamped at AUTH (the full credential
    /// resolution) and refreshed by the live re-check in `dispatch_frame`.
    /// The common per-op path only compares this against
    /// [`REVOCATION_RECHECK_WINDOW`]; the redb read fires at most once per
    /// window. See [`enforce_live_revocation`].
    pub(crate) last_revocation_check: Instant,
    /// Set once this connection dispatches a `TXN_BEGIN`. Gates the
    /// disconnect-time orphan-txn sweep: a connection that never opened a
    /// transaction has nothing to abort, so it skips the per-shard sweep
    /// entirely (the common case — most connections never open a txn).
    /// Conservatively latched on dispatch (not on shard-side success): if
    /// a `TXN_BEGIN` was sent we sweep, so a txn that did open is never
    /// missed even if its response never made it back.
    pub(crate) opened_txn: bool,
}

impl ConnState {
    pub(crate) fn new() -> Self {
        Self {
            phase: ConnPhase::AwaitingHello,
            connection_id: [0u8; 16],
            negotiated_version: 0,
            last_revocation_check: Instant::now(),
            opened_txn: false,
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
/// the space-id hash + shard lookup.
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
// `Action` is an ephemeral per-frame decision returned by value from
// `dispatch_frame` and matched immediately by the connection loop. Boxing
// the largest variant to satisfy `large_enum_variant` would add a heap
// allocation on the dispatch hot path (every op) — which the perf guidance
// forbids — so we keep the variant inline and accept the size spread.
#[allow(clippy::large_enum_variant)]
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
    /// Resolved AUTH-time scope (org / user / namespace / space /
    /// permissions). Threaded into `brain_ops::dispatch` as a
    /// `RequestCaller` so handlers read scope from here instead of
    /// trusting client-supplied fields.
    pub(crate) scope: RequestScope,
    /// Wire-level session id minted at HELLO. Stamped onto the
    /// `RequestCaller` so TXN_BEGIN can link the new entry back to
    /// the originating connection — drives the connection-drop
    /// auto-abort sweep.
    pub(crate) connection_id: [u8; 16],
    /// Effective-identity selector, present iff the request carried an
    /// `act_as` field that passed the R1/R2 authorization checks in
    /// `dispatch_frame`. When `Some`, `run_op_dispatch` builds the
    /// EFFECTIVE caller (target `(namespace, space)` under the fixed
    /// `STANDARD_SPACE` mask) instead of the principal's own caller, and
    /// records both principals on the request span. `None` = the op runs
    /// as the connection's own key-bound identity.
    pub(crate) act_as: Option<brain_protocol::ActAs>,
}

pub(crate) struct SubscribeStart {
    /// Stream id the SUBSCRIBE_REQ rode in on (and where SUBSCRIPTION_
    /// EVENT frames flow out).
    pub(crate) stream_id: u32,
    pub(crate) req: brain_protocol::envelope::request::SubscribeRequest,
    pub(crate) target_shard: u16,
    /// Effective space this subscription runs under (the `act_as`
    /// target when present, else the connection's key-bound space).
    /// Used to space-wall the one-time `similar_to` reference-vector
    /// lookup against the owning shard.
    pub(crate) space: SpaceId,
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

    // Live revocation re-check. The key is fully resolved once at AUTH, but
    // a long-lived connection (notably a wildcard gateway/edge with
    // `may_act = ["*"]`) can outlive a mid-session revoke; without a live
    // re-check a revoked key keeps full access until the TCP connection
    // drops. Re-look-up the key by its hash at a bounded interval so
    // revocation takes effect promptly while the common per-op path stays a
    // single monotonic-clock comparison. Fail-closed: a revoked/gone key —
    // or a store error — denies the in-flight op and closes the connection.
    if let Some(action) = enforce_live_revocation(state, &scope, topology, stream_id) {
        return action;
    }

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
            // SUBSCRIBE supports `act_as` the same way every other
            // act-as-capable op does, but reaches the R1/R2 checks here
            // instead of via the normal `act_as_of` block below (this
            // branch returns before that point) because SUBSCRIBE
            // structurally bypasses `brain_ops::dispatch` entirely. The
            // checks themselves — and the effective-identity / shard
            // routing they gate — are the exact same logic the normal
            // path runs; see `check_act_as` and `pick_target_shard`'s
            // `act_as` branch.
            let (effective_space, target_shard) = match &sub_req.act_as {
                Some(a) => {
                    if let Err((code, message)) = check_act_as(&scope, a) {
                        return Action::Inline(error_frame(stream_id, code, message));
                    }
                    let routing = topology.routing.load_full();
                    // Ingress hashing: derive the 16-byte effective space from
                    // the structured string selector (namespace folded into the
                    // seed). An empty selector means the connection's key-bound
                    // space. Must match `RequestScope::to_effective_caller`.
                    let eff = if a.space_id.is_empty() {
                        scope.space_id
                    } else {
                        SpaceId::derive_from_string(&a.namespace, &a.space_id)
                    };
                    (eff, routing.shard_for_space(eff))
                }
                None => (scope.space_id, bound_shard),
            };
            // Under scoped API-key auth, a subscriber may only receive its
            // own (effective) space's events. `filter.spaces == None`/empty
            // means "all spaces" (a cross-tenant leak on a shared shard),
            // and any id other than the effective space is likewise
            // forbidden — the SUBSCRIBE analogue of RECALL/QUERY per-space
            // read isolation. Compared against the EFFECTIVE space (the
            // act_as target when present) so a `may_act`-permitted caller
            // can scope a subscription to the identity it's acting as,
            // never to some third space.
            if !subscribe_spaces_allowed(effective_space, sub_req.filter.spaces.as_deref()) {
                return Action::Inline(error_frame(
                    stream_id,
                    ErrorCode::PermissionDenied,
                    "subscribe: filter.spaces must name only the effective space",
                ));
            }
            return Action::Subscribe(SubscribeStart {
                stream_id,
                req: sub_req,
                target_shard,
                space: effective_space,
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

    // Effective-identity (`act_as`) enforcement, fully generic over
    // whichever op variants `act_as_of` matches — no verb is special-cased
    // here. When a request carries an `act_as` selector, the connection
    // principal must (R1) hold the ACT_AS grant and (R2) name a target
    // namespace inside its `may_act` allowlist. Denials are hard: a
    // failed act_as request is rejected with `ActAsDenied`, never silently
    // downgraded to the principal's own identity.
    if let Some(a) = brain_protocol::act_as_of(&req) {
        if let Err((code, message)) = check_act_as(&scope, a) {
            return Action::Inline(error_frame(stream_id, code, message));
        }
    }

    // Route + dispatch. `act_as` ops route to the TARGET space's shard so
    // the impersonated op lands on that identity's real timeline /
    // idempotency domain; memory-bearing requests route to the memory's
    // shard; everything else lands on the principal's bound shard.
    let routing = topology.routing.load_full();
    let target_shard =
        pick_target_shard(&req, bound_shard, &routing, brain_protocol::act_as_of(&req))
            .unwrap_or(bound_shard);
    // Capture the effective-identity selector for the dispatch task, which
    // builds the effective caller and records the delegation on the span.
    let act_as = brain_protocol::act_as_of(&req).cloned();

    Action::OpDispatch(OpDispatch {
        stream_id: frame.header.stream_id_u32(),
        req,
        target_shard,
        // Stamp the AUTH-bound scope on every dispatched op. Handlers
        // read space / namespace / permissions from this object and
        // ignore whatever the client supplied — the shared-shard
        // cross-space leak the `spaces` subscribe filter needs to
        // enforce closes here.
        scope,
        // Wire session id rides alongside so TXN_BEGIN can stamp it
        // on the new entry; the connection-drop sweep needs it to
        // find buffered work owned by a dying connection.
        connection_id: state.connection_id,
        act_as,
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

    // Allocate a fresh connection_id. uuid v7 + the bytes is fine.
    let connection_id = *uuid::Uuid::now_v7().as_bytes();
    state.connection_id = connection_id;
    state.negotiated_version = negotiated.chosen_version;
    state.phase = ConnPhase::AwaitingAuth;

    let welcome = WelcomePayload {
        server_id: topology.server_caps.server_id.clone(),
        chosen_version: negotiated.chosen_version,
        connection_id,
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

    // The server advertises its accepted methods; reject anything the
    // policy doesn't allow before resolving the credential.
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
                AuthError::UnsupportedMethod => ErrorCode::NoSuchAuthMethod,
                AuthError::Storage(_) => ErrorCode::Internal,
            };
            return Action::CloseWith(error_frame(0, code, &e.to_string()));
        }
    };

    let space = scope.space_id;
    // Refcount-bump load. The published table may swap between AUTH
    // and a later request; both observers see a coherent snapshot.
    let routing = topology.routing.load_full();
    let bound_shard = routing.shard_for_space(space);
    let permissions = scope.to_space_permissions();
    state.phase = ConnPhase::Established {
        space,
        bound_shard,
        permissions,
        scope: scope.clone(),
    };
    // AUTH is the authoritative revocation check; anchor the live-re-check
    // window here so the first post-AUTH op inside the window skips the redb
    // read.
    state.last_revocation_check = Instant::now();

    let auth_ok = AuthOkPayload {
        // The space is resolved entirely from the key — the client never
        // claims one. This is how the client learns its identity.
        space_id: *space.0.as_bytes(),
        bound_shard_id: bound_shard,
        permissions,
        // Surface the tenant the connection resolved to (the key's
        // namespace). The client only displays this — it never sends one.
        namespace: scope.namespace.clone(),
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
    // Build the caller. For an `act_as` request the effective caller runs
    // as the target `(namespace, space)` under the fixed STANDARD_SPACE
    // mask; otherwise the op runs as the connection principal's own
    // key-bound identity. Authorization (R1/R2) was already enforced in
    // `dispatch_frame`; this only materializes the identity.
    let caller = match &op.act_as {
        Some(a) => op.scope.to_effective_caller(a, op.connection_id),
        None => op.scope.to_caller(op.connection_id),
    };
    // Root of the per-request trace. Held open for the whole op; the shard
    // re-enters a clone via `.instrument()` so `brain.encode` nests under it
    // across the Tokio→Glommio hop. In Phase 1 this is a trace root; once the
    // wire carries `traceparent` it becomes a child of the remote context.
    //
    // `brain.space_id` is the EFFECTIVE identity (the target under act_as).
    // Under delegation we also record the acting connection principal —
    // per RFC 8693 the acting party is never erased from the audit trail.
    let request_span = tracing::info_span!(
        "client.request",
        brain.operation = ?op.req.opcode(),
        brain.space_id = %caller.space_id.0,
        brain.shard = op.target_shard,
        brain.stream_id = stream_id,
        trace_id = tracing::field::Empty,
        span_id = tracing::field::Empty,
        brain.act_as = tracing::field::Empty,
        brain.principal_space_id = tracing::field::Empty,
        brain.principal_key_hash = tracing::field::Empty,
        brain.effective_namespace = tracing::field::Empty,
    );
    if let Some(a) = &op.act_as {
        request_span.record("brain.act_as", true);
        request_span.record(
            "brain.principal_space_id",
            tracing::field::display(op.scope.space_id.0),
        );
        request_span.record(
            "brain.principal_key_hash",
            tracing::field::display(hex32(&op.scope.key_hash)),
        );
        request_span.record(
            "brain.effective_namespace",
            tracing::field::display(&a.namespace),
        );
    }
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

/// R1 (`ACT_AS` grant) + R2 (`may_act` allowlist, wildcard-`"*"`-aware)
/// authorization for an `act_as` selector. Returns `Err((code,
/// message))` on denial — always `ErrorCode::ActAsDenied`, the same
/// code and the same two checks every act-as-capable op's normal
/// dispatch path enforces. The single point both the normal
/// `act_as_of` block and SUBSCRIBE's structurally-separate branch
/// call into, so the two can never drift.
fn check_act_as(
    scope: &RequestScope,
    a: &brain_protocol::ActAs,
) -> Result<(), (ErrorCode, &'static str)> {
    if scope.permissions & bits::ACT_AS == 0 {
        return Err((
            ErrorCode::ActAsDenied,
            "act_as: connection principal lacks the ACT_AS grant",
        ));
    }
    // A `"*"` entry is the wildcard grant: the principal may act as any
    // namespace. This is the trusted-front-door case — a gateway/edge that
    // fronts every tenant can't enumerate a `may_act` allowlist that grows
    // with each new tenant, so it holds `["*"]` and Brain admits any target.
    // Named entries still match exactly; the two forms compose.
    let namespace_allowed = scope
        .may_act
        .iter()
        .any(|ns| ns == "*" || ns == &a.namespace);
    if !namespace_allowed {
        return Err((
            ErrorCode::ActAsDenied,
            "act_as: target namespace is not in the principal's may_act allowlist",
        ));
    }
    Ok(())
}

/// Maximum staleness of a connection's revocation status.
///
/// A revoked key loses access on an OPEN connection within this window on
/// the connection's next op after the window elapses. 5s is short enough
/// that revocation is effectively prompt for an operator, yet long enough
/// that the redb read is amortised to at most once per 5s per connection —
/// every other op on the hot path pays only a monotonic-clock comparison.
const REVOCATION_RECHECK_WINDOW: Duration = Duration::from_secs(5);

/// Bounded-staleness live revocation re-check for a data-plane op.
///
/// Returns `None` to admit the op (window not yet elapsed, or the key is
/// still active) and `Some(Action::CloseWith(..))` to deny it and tear the
/// connection down. Off the hot path except for the `Instant` comparison:
/// the redb read fires only when the window has elapsed, and stamps
/// `state.last_revocation_check` so the next window starts fresh. Fail-closed
/// — a revoked row, a missing row, or a store error all deny.
fn enforce_live_revocation(
    state: &mut ConnState,
    scope: &RequestScope,
    topology: &Topology,
    stream_id: u32,
) -> Option<Action> {
    let now = Instant::now();
    if now.duration_since(state.last_revocation_check) < REVOCATION_RECHECK_WINDOW {
        return None;
    }
    state.last_revocation_check = now;
    match topology.auth_store.is_key_active(&scope.key_hash) {
        Ok(true) => None,
        Ok(false) => {
            tracing::warn!(
                key_hash = %hex32(&scope.key_hash),
                "revoked key detected on live connection; denying op and closing"
            );
            Some(Action::CloseWith(error_frame(
                stream_id,
                ErrorCode::Unauthenticated,
                "API key has been revoked",
            )))
        }
        Err(e) => {
            // Fail-closed: an unavailable key store means we cannot prove the
            // key is still valid, so we deny rather than trust the cached
            // AUTH-time scope.
            tracing::warn!(
                key_hash = %hex32(&scope.key_hash),
                error = %e,
                "api-key store unavailable during live revocation re-check; closing"
            );
            Some(Action::CloseWith(error_frame(
                stream_id,
                ErrorCode::Internal,
                "api-key store unavailable; closing connection",
            )))
        }
    }
}

fn pick_target_shard(
    req: &RequestBody,
    bound_shard: u16,
    routing: &RoutingTable,
    act_as: Option<&brain_protocol::ActAs>,
) -> Option<u16> {
    // `act_as` ops route to the TARGET space's shard, unconditionally and
    // generically over every act-as-capable op — the impersonated op must
    // land on the target identity's timeline / idempotency domain, not the
    // memory's or the principal's. This takes precedence over the
    // memory-shard routing below; for a target's own memories the two
    // agree (the MemoryId was minted on that same shard at write time).
    if let Some(a) = act_as {
        // A non-empty selector routes to the derived target space's shard;
        // an empty selector (key-bound space) falls through to the
        // memory/space routing below. Must match `to_effective_caller`.
        if !a.space_id.is_empty() {
            return Some(
                routing.shard_for_space(SpaceId::derive_from_string(&a.namespace, &a.space_id)),
            );
        }
    }
    // Requests carrying a target MemoryId route by memory shard; other
    // requests use the space's bound shard. The `source` end of LINK /
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

/// Whether a SUBSCRIBE's `filter.spaces` is allowed for this scope.
///
/// A subscriber may only receive its own space's events, so `spaces` must
/// be a non-empty list naming only the caller's own space — `None`/empty
/// (= all spaces on the shard) is a cross-tenant leak and is rejected. This
/// is the SUBSCRIBE analogue of the per-space read isolation RECALL / QUERY
/// enforce structurally.
fn subscribe_spaces_allowed(own: SpaceId, spaces: Option<&[[u8; 16]]>) -> bool {
    spaces.is_some_and(|a| !a.is_empty() && a.iter().all(|b| SpaceId::from(*b) == own))
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
    use brain_protocol::connection::handshake::{AuthCredentials, AuthMethod, HelloCapabilities};

    fn test_topology() -> Topology {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let auth_store = Arc::new(
            crate::auth::AuthStore::open(tmp.path().join("api_keys.redb"))
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
                vec![AuthMethod::Token],
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
            client_connection_token: None,
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
                assert_ne!(state.connection_id, [0u8; 16]);
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
            session_id: 0,
            request_id: [0u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: None,
            wait: brain_protocol::WaitMode::Ack,
            allow_duplicates: false,
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
            client_connection_token: None,
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
    fn subscribe_spaces_scope_guard() {
        let own = SpaceId::from([7u8; 16]);
        let other = [9u8; 16];
        // Allowed: a non-empty list naming only the caller's own space.
        assert!(subscribe_spaces_allowed(own, Some(&[[7u8; 16]])));
        // Rejected: None (= all), empty (= all), another space, and any
        // list that includes another space.
        assert!(!subscribe_spaces_allowed(own, None));
        assert!(!subscribe_spaces_allowed(own, Some(&[])));
        assert!(!subscribe_spaces_allowed(own, Some(&[other])));
        assert!(!subscribe_spaces_allowed(own, Some(&[[7u8; 16], other])));
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

    fn space_id_bytes(byte: u8) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[15] = byte;
        a
    }

    fn encode_op_frame() -> Frame {
        let body = RequestBody::Encode(brain_protocol::envelope::request::EncodeRequest {
            text: "hello".into(),
            session_id: 0,
            request_id: [0u8; 16],
            txn_id: None,
            occurred_at_unix_nanos: None,
            act_as: None,
            wait: brain_protocol::WaitMode::Ack,
            allow_duplicates: false,
        });
        Frame::new(Opcode::EncodeReq.as_u16(), FLAG_EOS, 1, body.encode())
    }

    /// Drive HELLO + AUTH with `secret` and return the established state.
    fn establish(topo: &Topology, secret: Vec<u8>) -> ConnState {
        let mut state = ConnState::new();
        let _ = dispatch_frame(build_hello_frame(), &mut state, topo);
        let auth = AuthPayload {
            method: AuthMethod::Token,
            credentials: AuthCredentials::Token(secret),
        };
        let frame = Frame::new(Opcode::Auth.as_u16(), FLAG_EOS, 0, auth.encode());
        let action = dispatch_frame(frame, &mut state, topo);
        assert!(
            matches!(action, Action::Inline(_)),
            "AUTH should be accepted"
        );
        assert!(matches!(state.phase, ConnPhase::Established { .. }));
        state
    }

    /// A key revoked mid-connection loses access on the next op once the
    /// live-re-check staleness window has elapsed, and the connection closes.
    #[test]
    fn revoked_key_denied_after_window_on_live_connection() {
        let topo = test_topology();
        let minted = topo
            .auth_store
            .mint(
                space_id_bytes(2),
                [0u8; 16],
                "acme".into(),
                space_id_bytes(7),
                bits::STANDARD_SPACE,
                Vec::new(),
                1,
            )
            .unwrap();
        let mut state = establish(&topo, minted.secret_bytes.clone());

        // Within the freshly-anchored window: the op is admitted.
        match dispatch_frame(encode_op_frame(), &mut state, &topo) {
            Action::OpDispatch(_) => {}
            _ => panic!("expected OpDispatch within the re-check window"),
        }

        // Revoke mid-connection, then force the window to have elapsed.
        assert!(topo.auth_store.revoke(&minted.key_hash).unwrap());
        state.last_revocation_check = Instant::now()
            .checked_sub(REVOCATION_RECHECK_WINDOW * 2)
            .unwrap();

        // The next op re-looks-up the key, finds it revoked, denies, and closes.
        match dispatch_frame(encode_op_frame(), &mut state, &topo) {
            Action::CloseWith(f) => {
                assert_eq!(f.header.opcode_u16(), Opcode::Error.as_u16());
            }
            _ => panic!("expected CloseWith(ERROR) after revoke + elapsed window"),
        }
    }

    /// A non-revoked key keeps working across the staleness window: the
    /// re-check refreshes the timestamp and admits the op.
    #[test]
    fn active_key_survives_window_on_live_connection() {
        let topo = test_topology();
        let minted = topo
            .auth_store
            .mint(
                space_id_bytes(2),
                [0u8; 16],
                "acme".into(),
                space_id_bytes(7),
                bits::STANDARD_SPACE,
                Vec::new(),
                1,
            )
            .unwrap();
        let mut state = establish(&topo, minted.secret_bytes.clone());

        // Force the window to have elapsed without revoking the key.
        state.last_revocation_check = Instant::now()
            .checked_sub(REVOCATION_RECHECK_WINDOW * 2)
            .unwrap();

        match dispatch_frame(encode_op_frame(), &mut state, &topo) {
            Action::OpDispatch(_) => {}
            _ => panic!("expected OpDispatch for an active key across the window"),
        }
        // The re-check refreshed the anchor, so the next op stays on the cheap path.
        assert!(state.last_revocation_check.elapsed() < REVOCATION_RECHECK_WINDOW);
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
