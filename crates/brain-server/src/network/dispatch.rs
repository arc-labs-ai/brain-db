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
    /// Per-connection transaction affinity: which shard each `txn_id` was
    /// begun on. A delegated (`act_as`) `TXN_BEGIN` routes to the target
    /// space's shard, but `TXN_COMMIT` / `TXN_ABORT` carry no `act_as` and
    /// would otherwise route to the connection's own bound shard — a
    /// different shard on a multi-shard deployment, where the txn was never
    /// begun (the commit fails as `TxnNotFound`). The `TxnStore` is
    /// per-shard, so a txn's whole lifecycle must land on one shard. This
    /// router records the begin-time shard and pins the commit/abort to it.
    pub(crate) txn_shards: TxnShardRouter,
}

impl ConnState {
    pub(crate) fn new() -> Self {
        Self {
            phase: ConnPhase::AwaitingHello,
            connection_id: [0u8; 16],
            negotiated_version: 0,
            last_revocation_check: Instant::now(),
            opened_txn: false,
            txn_shards: TxnShardRouter::new(),
        }
    }
}

/// Bound on the terminated-txn retry window kept per connection. Sized to
/// comfortably cover in-flight retries of recently-committed transactions
/// without letting a long-lived pooled connection accumulate a route entry
/// per transaction for its whole lifetime.
const TERMINATED_TXN_ROUTE_WINDOW: usize = 256;

/// Hard cap on the active tier — transactions begun on this connection whose
/// terminal outcome has not (yet) been confirmed. Open transactions are NOT
/// bounded by the stream cap: `TXN_BEGIN` releases its stream permit at ack
/// while the transaction stays open, so a pooled connection (notably a
/// wildcard-`may_act` gateway) can hold arbitrarily many genuinely-open txns.
/// A live route can never be evicted — dropping one would make a later
/// `TXN_COMMIT` / `TXN_ABORT` misroute to the bound shard (`TxnNotFound`,
/// losing the buffered writes). So when the active tier is full a fresh
/// `TXN_BEGIN` is instead REJECTED with `TransactionLimitExceeded`; only
/// confirmed-terminated / begin-rejected routes ever leave the tier (via
/// [`TxnShardRouter::apply_outcome`]). Sized well above any realistic count of
/// concurrently-open transactions on one connection.
const ACTIVE_TXN_ROUTE_CAP: usize = 1024;

/// Outcome of a dispatched transaction op, reported back to the router by the
/// per-op task once the shard has actually answered. Drives route lifetime:
/// a route is only demoted or dropped on a *confirmed* outcome, never
/// optimistically at dispatch time (a commit that is lost, times out, or is
/// rejected before reaching the shard leaves its route in place so an
/// idempotent retry still resolves it).
pub(crate) struct TxnRouteOutcome {
    pub(crate) txn_id: [u8; 16],
    pub(crate) kind: TxnRouteOutcomeKind,
}

pub(crate) enum TxnRouteOutcomeKind {
    /// A `TXN_COMMIT` / `TXN_ABORT` reached a definitive terminal state — the
    /// shard acked it, or reported the txn already gone (`TxnNotFound` /
    /// `TxnExpired`). Demote the route into the bounded retry window so an
    /// idempotent retry still reaches the shard holding the replay response.
    ConfirmedTerminal,
    /// A `TXN_BEGIN` was definitively rejected by the shard (it never opened).
    /// Drop the optimistically-recorded route so it doesn't sit in the active
    /// tier until the backstop cap reclaims it.
    BeginRejected,
}

/// Per-connection `txn_id` → begin-shard routing table.
///
/// Split into two tiers so the state a pooled, long-lived connection carries
/// is bounded regardless of how many transactions it runs:
///
/// - `active`: transactions begun on this connection whose terminal outcome
///   has not been confirmed. A terminal `TXN_COMMIT` / `TXN_ABORT` moves an
///   entry out *only once the shard confirms it* (see [`TxnRouteOutcome`]) —
///   never at dispatch time — so a mid-flight or failed terminal keeps its
///   route long enough for a retry to resolve. The tier is hard-capped at
///   [`ACTIVE_TXN_ROUTE_CAP`]: a live route is never evicted (that would
///   misroute a later commit/abort), so a `TXN_BEGIN` that would exceed the cap
///   is rejected rather than admitted — see [`TxnShardRouter::begin`].
/// - `terminated`: a bounded FIFO window of confirmed-terminated txns. Retained
///   only briefly so an idempotent retry of a delegated `TXN_COMMIT` /
///   `TXN_ABORT` still routes to the shard holding the cached replay response.
///   Oldest entries are evicted once the window is full, so this tier can never
///   grow past [`TERMINATED_TXN_ROUTE_WINDOW`].
pub(crate) struct TxnShardRouter {
    active: std::collections::HashMap<[u8; 16], u16>,
    /// Begin-order of active `txn_id`s, for oldest-begun backstop eviction.
    /// May hold ids no longer in `active` (a confirmed terminal removes from
    /// the map but not from here); such stale ids are skipped on eviction and
    /// compacted out when the deque outgrows the live set.
    active_order: std::collections::VecDeque<[u8; 16]>,
    terminated: std::collections::HashMap<[u8; 16], u16>,
    terminated_order: std::collections::VecDeque<[u8; 16]>,
}

impl TxnShardRouter {
    pub(crate) fn new() -> Self {
        Self {
            active: std::collections::HashMap::new(),
            active_order: std::collections::VecDeque::new(),
            terminated: std::collections::HashMap::new(),
            terminated_order: std::collections::VecDeque::new(),
        }
    }

    /// Record the shard a `TXN_BEGIN` landed on, enforcing the active-tier
    /// hard cap.
    ///
    /// Returns `true` when the route was recorded (dispatch the begin) and
    /// `false` when the active tier is full of live, unconfirmed routes (reject
    /// the begin — the caller surfaces `TransactionLimitExceeded`). A live route
    /// is NEVER evicted to make room: evicting one would make its later
    /// `TXN_COMMIT` / `TXN_ABORT` fail to resolve and misroute to the bound
    /// shard, losing the buffered writes. Re-recording an already-active
    /// `txn_id` (an idempotent re-begin) is always accepted — it updates the
    /// existing entry rather than growing the tier.
    #[must_use]
    pub(crate) fn begin(&mut self, txn_id: [u8; 16], shard: u16) -> bool {
        let known = self.active.contains_key(&txn_id);
        if !known && self.active.len() >= ACTIVE_TXN_ROUTE_CAP {
            // Full of genuinely-open transactions. Reject rather than evict a
            // live route; only terminated/rejected routes free up capacity.
            return false;
        }
        if self.active.insert(txn_id, shard).is_none() {
            self.active_order.push_back(txn_id);
        }
        // Keep the order deque from accumulating stale ids (terminated txns
        // whose id lingers here) without bound on a high-churn connection:
        // once it dwarfs the live set, drop the dead ids.
        if self.active_order.len() > self.active.len().saturating_mul(2) + ACTIVE_TXN_ROUTE_CAP {
            self.active_order.retain(|id| self.active.contains_key(id));
        }
        true
    }

    /// Resolve the begin-shard for a `txn_id`, checking active transactions
    /// first and then the bounded terminated-retry window.
    pub(crate) fn route(&self, txn_id: &[u8; 16]) -> Option<u16> {
        self.active
            .get(txn_id)
            .or_else(|| self.terminated.get(txn_id))
            .copied()
    }

    /// Apply a confirmed transaction outcome reported by the per-op task.
    pub(crate) fn apply_outcome(&mut self, outcome: TxnRouteOutcome) {
        match outcome.kind {
            TxnRouteOutcomeKind::ConfirmedTerminal => self.confirm_terminated(&outcome.txn_id),
            TxnRouteOutcomeKind::BeginRejected => self.remove_active(&outcome.txn_id),
        }
    }

    /// Retire a transaction on a *confirmed* terminal `TXN_COMMIT` /
    /// `TXN_ABORT`: drop it from the active tier and, if it was active, park
    /// its route in the bounded terminated window so a delegated retry still
    /// routes home. A confirmation for an already-terminated (or never-begun)
    /// txn is a no-op against the active tier and leaves the window untouched.
    pub(crate) fn confirm_terminated(&mut self, txn_id: &[u8; 16]) {
        if let Some(shard) = self.active.remove(txn_id) {
            self.push_terminated(*txn_id, shard);
        }
    }

    /// Drop a route from the active tier without parking it in the retry
    /// window — used when a `TXN_BEGIN` was definitively rejected and the txn
    /// never opened, so there is no replay response to route a retry to.
    pub(crate) fn remove_active(&mut self, txn_id: &[u8; 16]) {
        self.active.remove(txn_id);
    }

    fn push_terminated(&mut self, txn_id: [u8; 16], shard: u16) {
        // Re-terminating an entry already in the window (e.g. a duplicate
        // terminal that raced the active removal) refreshes the route without
        // double-counting it in the eviction order.
        if self.terminated.insert(txn_id, shard).is_none() {
            self.terminated_order.push_back(txn_id);
            while self.terminated_order.len() > TERMINATED_TXN_ROUTE_WINDOW {
                if let Some(evicted) = self.terminated_order.pop_front() {
                    self.terminated.remove(&evicted);
                }
            }
        }
    }

    /// Total routing entries currently held (active + terminated window).
    /// Test/introspection helper; the active tier is bounded by
    /// [`ACTIVE_TXN_ROUTE_CAP`] and the terminated tier by
    /// [`TERMINATED_TXN_ROUTE_WINDOW`].
    pub(crate) fn len(&self) -> usize {
        self.active.len() + self.terminated.len()
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
    let mut target_shard =
        pick_target_shard(&req, bound_shard, &routing, brain_protocol::act_as_of(&req))
            .unwrap_or(bound_shard);
    // Transaction shard affinity. A txn's `TxnStore` lives on the shard the
    // `TXN_BEGIN` landed on; a delegated begin routes to the target space's
    // shard (via `pick_target_shard`'s `act_as` branch), but the matching
    // `TXN_COMMIT` / `TXN_ABORT` carry no `act_as`, so without pinning they
    // route to the connection's own bound shard and miss the txn entirely
    // (`TxnNotFound`) whenever those two shards differ. Record the begin-time
    // shard here and route the commit/abort back to it. A non-delegated txn
    // (begin and commit both on the bound shard) is unaffected — the lookup
    // returns the same shard the fallthrough would have picked.
    match &req {
        RequestBody::TxnBegin(r) => {
            // Reject at the active-route cap instead of evicting a live txn
            // route. A pooled wildcard-`may_act` connection can hold more open
            // txns than the stream cap (the begin releases its stream permit at
            // ack while the txn stays open); dropping the oldest live route to
            // admit a new begin would strand that txn's later commit/abort on
            // the wrong shard (`TxnNotFound`, losing its buffered writes).
            if !state.txn_shards.begin(r.txn_id, target_shard) {
                return Action::Inline(error_frame(
                    stream_id,
                    ErrorCode::TransactionLimitExceeded,
                    "too many concurrent open transactions on this connection",
                ));
            }
        }
        RequestBody::TxnCommit(r) => {
            if let Some(shard) = state.txn_shards.route(&r.txn_id) {
                target_shard = shard;
            }
            // The route is NOT evicted here: a terminal dispatched is not a
            // terminal confirmed. If this commit is lost, times out, or is
            // rejected before the shard applies it, the txn stays Active and a
            // retry must still route home. The per-op task reports the shard's
            // actual outcome back via `TxnRouteOutcome`, and only a confirmed
            // terminal demotes the route into the bounded retry window.
        }
        RequestBody::TxnAbort(r) => {
            if let Some(shard) = state.txn_shards.route(&r.txn_id) {
                target_shard = shard;
            }
            // Same as commit: demotion waits for the confirmed outcome.
        }
        _ => {}
    }
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
pub(crate) async fn run_op_dispatch(
    op: OpDispatch,
    shards: Arc<Vec<ShardHandle>>,
    txn_route_tx: Option<flume::Sender<TxnRouteOutcome>>,
) -> Vec<Frame> {
    let stream_id = op.stream_id;
    let shard = match shards.get(op.target_shard as usize) {
        Some(s) => s,
        None => {
            // The op never reached a shard, so any txn route it carries stays
            // active (unconfirmed): no outcome is reported. A retry that lands
            // on a valid shard will still resolve the route.
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
    // Namespace-wide RECALL cannot be served by a single shard — a namespace's
    // spaces are spread across shards. Fan the raw-candidate gather out to every
    // shard, merge the pools, and shape once on the bound (coordinator) shard.
    if let RequestBody::Recall(r) = &op.req {
        if matches!(
            r.scope,
            brain_protocol::ops::memory::RecallScopeWire::Namespace
        ) {
            // A transaction lives on exactly one shard; a namespace-wide read
            // fans out to all of them, where the txn does not exist. The two
            // are incoherent — reject rather than fail with TxnNotFound on the
            // other shards.
            if r.txn_id.is_some() {
                return vec![error_frame(
                    stream_id,
                    ErrorCode::InvalidArgument,
                    "namespace-wide recall (scope=Namespace) is not supported inside a transaction",
                )];
            }
            return run_namespace_recall(
                stream_id,
                r.clone(),
                caller,
                op.target_shard,
                shards,
                request_span,
            )
            .await;
        }
    }

    // Capture the txn identity before the request is consumed by dispatch, so
    // the confirmed outcome can be reported back to the router once the shard
    // answers. Only txn ops carry a kind; everything else reports nothing.
    let txn_kind = txn_route_tx.as_ref().and_then(|_| txn_req_kind(&op.req));

    let result = shard
        .dispatch_op(op.req, caller, request_span.clone())
        .await;

    // Report the confirmed transaction outcome to the per-connection router.
    // Absent a confirmation (transient failure, disconnect, unreachable shard)
    // the route is deliberately left in the active tier so a retry resolves it.
    if let (Some(tx), Some(kind)) = (txn_route_tx.as_ref(), txn_kind) {
        if let Some(outcome) = classify_txn_route_outcome(kind, &result) {
            let _ = tx.send(outcome);
        }
    }

    match result {
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

/// Namespace-wide RECALL fan-out (Phase C). A namespace's spaces are spread
/// across shards, so this:
///   1. fans the raw-candidate gather out to EVERY shard in parallel (each
///      shard scopes to the caller's namespace across its own spaces),
///   2. merges the pools into one global candidate pool (RRF by within-shard
///      rank, deduped, bounded),
///   3. shapes once on the bound (coordinator) shard, over the merged pool.
///
/// Fail-closed: if any shard's gather errors, the whole read fails rather than
/// silently returning a partial answer that drops a shard's spaces. The tenant
/// wall is enforced per shard (Phase B `admits`, namespace unconditional), so
/// the fan-out can never gather another tenant's rows.
async fn run_namespace_recall(
    stream_id: u32,
    req: brain_protocol::ops::memory::RecallRequest,
    caller: brain_ops::RequestCaller,
    coordinator_shard: u16,
    shards: Arc<Vec<ShardHandle>>,
    request_span: tracing::Span,
) -> Vec<Frame> {
    use brain_protocol::envelope::response::ResponseBody;

    if shards.is_empty() {
        return vec![error_frame(
            stream_id,
            ErrorCode::ShardUnavailable,
            "no shards available for namespace-wide recall",
        )];
    }

    // Stage 1 — parallel fan-out of the raw-candidate gather to every shard.
    let mut set = tokio::task::JoinSet::new();
    for shard in shards.iter() {
        let shard = shard.clone();
        let req = req.clone();
        let caller = caller.clone();
        let span = request_span.clone();
        set.spawn(async move { shard.recall_gather(req, caller, span).await });
    }
    let mut partials: Vec<brain_ops::NamespaceRecallPartial> = Vec::with_capacity(shards.len());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(partial)) => partials.push(partial),
            Ok(Err(DispatchError::ShardDisconnected)) => {
                return vec![error_frame(
                    stream_id,
                    ErrorCode::ShardUnavailable,
                    "a shard is no longer accepting requests (namespace recall)",
                )];
            }
            Ok(Err(DispatchError::Op(e))) => return vec![error_frame_from_op_error(stream_id, &e)],
            Err(join_err) => {
                return vec![error_frame(
                    stream_id,
                    ErrorCode::Internal,
                    &format!("namespace recall gather task failed: {join_err}"),
                )];
            }
        }
    }

    // Stage 2 — global merge across shards: candidate pool + HyPE union + the
    // best grounded answer (so a grounded commit fires even when the subject's
    // facts live off the coordinator).
    let merged = brain_ops::merge_namespace_partials(partials);

    // Stage 3 — single global shaping pass on the coordinator (bound) shard.
    let Some(coordinator) = shards.get(coordinator_shard as usize) else {
        return vec![error_frame(
            stream_id,
            ErrorCode::ShardUnavailable,
            &format!(
                "coordinator shard {} out of range [0, {})",
                coordinator_shard,
                shards.len()
            ),
        )];
    };
    match coordinator
        .recall_shape_merged(merged, req, caller, request_span)
        .await
    {
        Ok(frame) => vec![build_response_frame(
            stream_id,
            true,
            ResponseBody::Recall(frame),
        )],
        Err(DispatchError::ShardDisconnected) => vec![error_frame(
            stream_id,
            ErrorCode::ShardUnavailable,
            "coordinator shard is no longer accepting requests",
        )],
        Err(DispatchError::Op(e)) => vec![error_frame_from_op_error(stream_id, &e)],
    }
}

/// The transaction identity of a request, if it is a txn lifecycle op.
/// Captured before `dispatch_op` consumes the request body so the router can
/// be updated from the shard's actual outcome.
enum TxnReqKind {
    Begin([u8; 16]),
    Commit([u8; 16]),
    Abort([u8; 16]),
}

fn txn_req_kind(req: &RequestBody) -> Option<TxnReqKind> {
    match req {
        RequestBody::TxnBegin(r) => Some(TxnReqKind::Begin(r.txn_id)),
        RequestBody::TxnCommit(r) => Some(TxnReqKind::Commit(r.txn_id)),
        RequestBody::TxnAbort(r) => Some(TxnReqKind::Abort(r.txn_id)),
        _ => None,
    }
}

/// Map a dispatched txn op's shard result to the route-lifetime action, if any.
///
/// - A committed/aborted txn that the shard acked, or reports already gone
///   (`TxnNotFound` / `TxnExpired`), is a confirmed terminal: demote the route.
/// - A begin the shard *definitively* rejected (a client/logic error, not a
///   transient infra failure) never opened: drop its optimistic route.
/// - Every other failure (transient overload / internal / retrieval error, a
///   shard disconnect) is left unreported so the route stays active for a retry.
fn classify_txn_route_outcome(
    kind: TxnReqKind,
    result: &Result<brain_ops::DispatchOutcome, DispatchError>,
) -> Option<TxnRouteOutcome> {
    match kind {
        TxnReqKind::Commit(txn_id) | TxnReqKind::Abort(txn_id) => {
            let confirmed = match result {
                Ok(_) => true,
                Err(DispatchError::Op(e)) => matches!(
                    e.error_code(),
                    brain_ops::error::ErrorCode::TxnNotFound
                        | brain_ops::error::ErrorCode::TxnExpired
                ),
                Err(DispatchError::ShardDisconnected) => false,
            };
            confirmed.then_some(TxnRouteOutcome {
                txn_id,
                kind: TxnRouteOutcomeKind::ConfirmedTerminal,
            })
        }
        TxnReqKind::Begin(txn_id) => match result {
            // Begin succeeded: the route stays active for the txn's lifetime.
            Ok(_) => None,
            Err(e) if is_transient_dispatch_error(e) => None,
            // Definitive rejection: the txn never opened — drop the route.
            Err(_) => Some(TxnRouteOutcome {
                txn_id,
                kind: TxnRouteOutcomeKind::BeginRejected,
            }),
        },
    }
}

/// Whether a dispatch failure is transient — the op may have (or may yet)
/// take effect on a retry, so a txn route it carries must be retained.
fn is_transient_dispatch_error(e: &DispatchError) -> bool {
    match e {
        DispatchError::ShardDisconnected => true,
        DispatchError::Op(op) => matches!(
            op.error_code(),
            brain_ops::error::ErrorCode::Overloaded
                | brain_ops::error::ErrorCode::RetrievalUnavailable
                | brain_ops::error::ErrorCode::InternalError
        ),
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

    /// Like [`test_topology`] but with a routing table spanning
    /// `shard_count` shards, so delegated-op routing can land on a shard
    /// other than the connection's bound one.
    fn test_topology_shards(shard_count: u16) -> Topology {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let auth_store = Arc::new(
            crate::auth::AuthStore::open(tmp.path().join("api_keys.redb"))
                .expect("open auth store"),
        );
        std::mem::forget(tmp);
        Topology {
            shards: Arc::new(Vec::new()),
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(
                RoutingTable::new(shard_count, std::collections::HashMap::new()).unwrap(),
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

    /// A delegated (`act_as`) transaction whose target space lands on a
    /// different shard than the connection's bound shard: the `TXN_BEGIN`
    /// routes to the target space's shard, and the subsequent
    /// `TXN_COMMIT` / `TXN_ABORT` — which carry no `act_as` — must be
    /// pinned to that same shard rather than falling back to the bound
    /// shard (where the txn was never begun, yielding `TxnNotFound`).
    #[test]
    fn delegated_txn_commit_abort_pin_to_begin_shard() {
        const SHARDS: u16 = 8;
        let topo = test_topology_shards(SHARDS);
        // Mint a key that holds ACT_AS and may act as "target-ns".
        let minted = topo
            .auth_store
            .mint(
                space_id_bytes(2),
                [0u8; 16],
                "acme".into(),
                space_id_bytes(7),
                bits::STANDARD_SPACE | bits::ACT_AS,
                vec!["target-ns".to_owned()],
                1,
            )
            .unwrap();
        let mut state = establish(&topo, minted.secret_bytes.clone());
        let bound_shard = match &state.phase {
            ConnPhase::Established { bound_shard, .. } => *bound_shard,
            _ => panic!("not established"),
        };

        // Find an act_as target space that routes to a shard other than
        // the connection's bound shard, so the bug is actually exercised.
        let routing = topo.routing.load_full();
        let namespace = "target-ns";
        let (space_sel, target_shard) = (0..10_000)
            .map(|i| format!("space-{i}"))
            .map(|s| {
                let shard = routing.shard_for_space(SpaceId::derive_from_string(namespace, &s));
                (s, shard)
            })
            .find(|(_, shard)| *shard != bound_shard)
            .expect("some target space must hash to a non-bound shard across 8 shards");
        assert_ne!(
            target_shard, bound_shard,
            "test precondition: target must differ from bound shard"
        );

        let txn_id = [0x5a; 16];
        let act_as = brain_protocol::ActAs {
            namespace: namespace.to_owned(),
            space_id: space_sel,
        };

        // TXN_BEGIN with act_as → routes to the target space's shard and
        // records the affinity.
        let begin = RequestBody::TxnBegin(brain_protocol::envelope::request::TxnBeginRequest {
            txn_id,
            timeout_seconds: 30,
            act_as: Some(act_as),
        });
        let frame = Frame::new(Opcode::TxnBegin.as_u16(), FLAG_EOS, 1, begin.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => {
                assert_eq!(
                    op.target_shard, target_shard,
                    "TXN_BEGIN must route to the delegated target's shard"
                );
            }
            _ => panic!("expected OpDispatch for TXN_BEGIN"),
        }
        assert_eq!(state.txn_shards.route(&txn_id), Some(target_shard));

        // TXN_COMMIT carries no act_as → must be pinned to the begin shard,
        // NOT the bound shard.
        let commit =
            RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest { txn_id });
        let frame = Frame::new(Opcode::TxnCommit.as_u16(), FLAG_EOS, 3, commit.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => {
                assert_eq!(
                    op.target_shard, target_shard,
                    "TXN_COMMIT must be pinned to the shard TXN_BEGIN landed on"
                );
                assert_ne!(op.target_shard, bound_shard);
            }
            _ => panic!("expected OpDispatch for TXN_COMMIT"),
        }

        // A retried commit under the same txn_id still routes to the begin
        // shard — the affinity entry is retained so the cached replay
        // response is reachable.
        let commit =
            RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest { txn_id });
        let frame = Frame::new(Opcode::TxnCommit.as_u16(), FLAG_EOS, 5, commit.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => assert_eq!(op.target_shard, target_shard),
            _ => panic!("expected OpDispatch for retried TXN_COMMIT"),
        }

        // TXN_ABORT for the same txn is likewise pinned to the begin shard.
        let abort =
            RequestBody::TxnAbort(brain_protocol::envelope::request::TxnAbortRequest { txn_id });
        let frame = Frame::new(Opcode::TxnAbort.as_u16(), FLAG_EOS, 7, abort.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => {
                assert_eq!(
                    op.target_shard, target_shard,
                    "TXN_ABORT must be pinned to the shard TXN_BEGIN landed on"
                );
            }
            _ => panic!("expected OpDispatch for TXN_ABORT"),
        }
    }

    /// Regression: the per-connection txn→shard router must not grow without
    /// bound across many begin/commit cycles on a single long-lived (pooled)
    /// connection. Each terminal commit evicts the active entry; the retained
    /// retry window is capped at `TERMINATED_TXN_ROUTE_WINDOW`.
    #[test]
    fn txn_router_does_not_grow_unbounded_across_many_cycles() {
        let mut router = TxnShardRouter::new();
        for i in 0..100_000u32 {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            assert!(router.begin(txn_id, (i % 8) as u16));
            // Confirmed terminal: the entry leaves the active tier and parks
            // in the bounded retry window.
            router.confirm_terminated(&txn_id);
            assert!(
                router.len() <= TERMINATED_TXN_ROUTE_WINDOW,
                "router grew to {} entries at cycle {i} — should stay <= {}",
                router.len(),
                TERMINATED_TXN_ROUTE_WINDOW
            );
        }
        // After 100k cycles the router holds only the bounded retry window,
        // not one entry per transaction.
        assert_eq!(router.len(), TERMINATED_TXN_ROUTE_WINDOW);
    }

    /// The active tier is bounded even when transactions never produce a client
    /// terminal — an abandoned / expired txn that only ever gets a `TXN_BEGIN`.
    /// A live route is never evicted to make room: once the tier is full, every
    /// further begin is rejected, so the tier never exceeds the cap and the
    /// order deque never accumulates stale ids without bound.
    #[test]
    fn active_tier_bounded_across_abandoned_begins() {
        let mut router = TxnShardRouter::new();
        for i in 0..(ACTIVE_TXN_ROUTE_CAP as u32 * 4) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            // Only ever a begin — no terminal is confirmed. Accepted until the
            // tier fills, rejected thereafter (never evicting a live route).
            let accepted = router.begin(txn_id, (i % 8) as u16);
            assert_eq!(
                accepted,
                (i as usize) < ACTIVE_TXN_ROUTE_CAP,
                "begin {i} acceptance must flip exactly at the cap"
            );
            assert!(
                router.len() <= ACTIVE_TXN_ROUTE_CAP,
                "active tier grew to {} at begin {i} — should stay <= {}",
                router.len(),
                ACTIVE_TXN_ROUTE_CAP
            );
        }
        assert_eq!(router.active.len(), ACTIVE_TXN_ROUTE_CAP);
        assert_eq!(router.terminated.len(), 0);
        // Rejected begins never push onto the order deque, so it exactly tracks
        // the live set here.
        assert_eq!(router.active_order.len(), ACTIVE_TXN_ROUTE_CAP);
    }

    /// The active tier full of live routes rejects a *new* begin (never
    /// evicting a live route), still admits an idempotent re-begin of an
    /// already-active txn, and — the point of the fix — every one of the
    /// existing open transactions still routes to its own begin shard.
    #[test]
    fn active_tier_full_rejects_new_begin_and_keeps_live_routes() {
        let mut router = TxnShardRouter::new();
        // Fill the active tier to the cap with distinct open txns, each pinned
        // to a distinct-per-id shard so a misroute would be observable.
        for i in 0..(ACTIVE_TXN_ROUTE_CAP as u32) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            assert!(
                router.begin(txn_id, (i % 8) as u16),
                "begin {i} must be accepted while the tier is below the cap"
            );
        }
        assert_eq!(router.active.len(), ACTIVE_TXN_ROUTE_CAP);

        // A brand-new txn is rejected — the tier is full of live routes.
        let fresh = [0xFE; 16];
        assert!(
            !router.begin(fresh, 1),
            "a new begin at the cap must be rejected, not admitted by eviction"
        );
        assert_eq!(
            router.route(&fresh),
            None,
            "the rejected begin left no route"
        );
        assert_eq!(router.active.len(), ACTIVE_TXN_ROUTE_CAP);

        // An idempotent re-begin of an already-active txn is still accepted: it
        // updates the existing entry rather than growing the tier.
        let mut existing = [0u8; 16];
        existing[..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(
            router.begin(existing, 0),
            "re-begin of an active txn must be accepted (no growth)"
        );
        assert_eq!(router.active.len(), ACTIVE_TXN_ROUTE_CAP);

        // Every existing open txn still resolves to its own begin shard — none
        // was evicted to admit the (rejected) new begin.
        for i in 0..(ACTIVE_TXN_ROUTE_CAP as u32) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            assert_eq!(
                router.route(&txn_id),
                Some((i % 8) as u16),
                "live txn {i} must still route to its begin shard after a rejected begin"
            );
        }
    }

    /// End-to-end at the dispatcher: once the connection's active tier is full,
    /// a fresh `TXN_BEGIN` is rejected with `TransactionLimitExceeded` (a
    /// structured error, not a panic and not a misroute), while a `TXN_COMMIT`
    /// for an already-open txn still routes to that txn's begin shard.
    #[test]
    fn txn_begin_rejected_with_transaction_limit_at_cap() {
        let topo = test_topology_shards(8);
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
        let bound_shard = match &state.phase {
            ConnPhase::Established { bound_shard, .. } => *bound_shard,
            _ => panic!("not established"),
        };

        // Pre-fill the active tier to the cap with non-delegated open txns (all
        // pinned to the bound shard, as a real non-act_as begin would be).
        let mut first_open = [0u8; 16];
        first_open[..4].copy_from_slice(&0u32.to_le_bytes());
        for i in 0..(ACTIVE_TXN_ROUTE_CAP as u32) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            assert!(state.txn_shards.begin(txn_id, bound_shard));
        }

        // A fresh begin over the wire is rejected with a structured error.
        let begin = RequestBody::TxnBegin(brain_protocol::envelope::request::TxnBeginRequest {
            txn_id: [0xFE; 16],
            timeout_seconds: 30,
            act_as: None,
        });
        let frame = Frame::new(Opcode::TxnBegin.as_u16(), FLAG_EOS, 1, begin.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::Inline(reply) => {
                assert_eq!(reply.header.opcode_u16(), Opcode::Error.as_u16());
                match ResponseBody::decode(Opcode::Error, &reply.payload) {
                    Ok(ResponseBody::Error(e)) => assert_eq!(
                        e.code,
                        ErrorCodeWire::from(ErrorCode::TransactionLimitExceeded),
                        "a begin at the cap must fail as TransactionLimitExceeded"
                    ),
                    other => panic!("expected an Error body, got {other:?}"),
                }
            }
            _ => panic!("expected an Inline error rejecting the over-cap begin"),
        }
        // The rejected begin recorded no route.
        assert_eq!(state.txn_shards.route(&[0xFE; 16]), None);
        assert_eq!(state.txn_shards.active.len(), ACTIVE_TXN_ROUTE_CAP);

        // A commit for a still-open txn is unaffected — it routes to that txn's
        // begin shard, never misrouted or dropped.
        let commit = RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest {
            txn_id: first_open,
        });
        let frame = Frame::new(Opcode::TxnCommit.as_u16(), FLAG_EOS, 3, commit.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => assert_eq!(
                op.target_shard, bound_shard,
                "an open txn's commit must still route to its begin shard"
            ),
            _ => panic!("expected OpDispatch for a live txn's commit"),
        }
    }

    /// FIX (2): a definitively-rejected `TXN_BEGIN` drops its optimistic route
    /// immediately (not only when the backstop cap reclaims it), and does not
    /// leave a phantom entry in the retry window.
    #[test]
    fn begin_rejected_drops_active_route() {
        let mut router = TxnShardRouter::new();
        let txn_id = [0x33; 16];
        assert!(router.begin(txn_id, 4));
        assert_eq!(router.route(&txn_id), Some(4));
        router.apply_outcome(TxnRouteOutcome {
            txn_id,
            kind: TxnRouteOutcomeKind::BeginRejected,
        });
        assert_eq!(router.route(&txn_id), None);
        assert_eq!(router.len(), 0);
    }

    /// FIX (1): a still-active route survives the terminal churn of *other*
    /// transactions. This is the exact regression: a commit dispatched but not
    /// confirmed (lost / timed-out / rejected before the shard applied it) left
    /// its route in the small terminated window, where 256 later terminals of
    /// unrelated txns evicted it — so a retry misrouted to the bound shard and
    /// hit `TxnNotFound`. With demotion gated on a confirmed terminal, an
    /// unconfirmed route stays in the active tier, untouched by that churn.
    #[test]
    fn active_route_survives_terminal_churn_of_other_txns() {
        let mut router = TxnShardRouter::new();
        let a = [0xAA; 16];
        assert!(router.begin(a, 5));

        // Many other transactions begin and confirm-terminate, filling and
        // churning the bounded retry window several times over.
        for i in 0..(TERMINATED_TXN_ROUTE_WINDOW as u32 * 3) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            txn_id[15] = 0xBB; // keep distinct from `a`
            assert!(router.begin(txn_id, 6));
            router.confirm_terminated(&txn_id);
        }

        // `a` was never confirmed terminal, so its route is still resolvable.
        assert_eq!(
            router.route(&a),
            Some(5),
            "an unconfirmed route must survive churn of other terminals"
        );
        assert!(router.active.contains_key(&a));
    }

    /// A committed txn's route survives one commit into the bounded retry
    /// window so an idempotent delegated retry still pins to the begin shard;
    /// the active tier no longer holds it.
    #[test]
    fn txn_route_survives_commit_into_retry_window() {
        let mut router = TxnShardRouter::new();
        let txn_id = [0x11; 16];
        assert!(router.begin(txn_id, 5));
        assert_eq!(router.route(&txn_id), Some(5));
        router.confirm_terminated(&txn_id);
        // Retry after terminal still routes home (from the retry window).
        assert_eq!(router.route(&txn_id), Some(5));
        // But the entry no longer occupies the active tier.
        assert_eq!(router.active.len(), 0);
        assert_eq!(router.terminated.len(), 1);
    }

    /// The retry window evicts oldest-first, so a route pushed out of the
    /// window no longer resolves while recent ones still do.
    #[test]
    fn txn_retry_window_evicts_oldest_first() {
        let mut router = TxnShardRouter::new();
        // Fill the window plus one, all begun-then-terminated.
        for i in 0..=(TERMINATED_TXN_ROUTE_WINDOW as u32) {
            let mut txn_id = [0u8; 16];
            txn_id[..4].copy_from_slice(&i.to_le_bytes());
            assert!(router.begin(txn_id, 3));
            router.confirm_terminated(&txn_id);
        }
        // The very first txn was evicted; the last remains.
        let mut first = [0u8; 16];
        first[..4].copy_from_slice(&0u32.to_le_bytes());
        let mut last = [0u8; 16];
        last[..4].copy_from_slice(&(TERMINATED_TXN_ROUTE_WINDOW as u32).to_le_bytes());
        assert_eq!(router.route(&first), None, "oldest entry must be evicted");
        assert_eq!(router.route(&last), Some(3), "newest entry must survive");
        assert_eq!(router.len(), TERMINATED_TXN_ROUTE_WINDOW);
    }

    /// A commit for a txn_id this connection never began falls through to
    /// the bound shard (no affinity entry) — unchanged prior behavior.
    #[test]
    fn unknown_txn_commit_falls_through_to_bound_shard() {
        const SHARDS: u16 = 8;
        let topo = test_topology_shards(SHARDS);
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
        let bound_shard = match &state.phase {
            ConnPhase::Established { bound_shard, .. } => *bound_shard,
            _ => panic!("not established"),
        };
        let commit = RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest {
            txn_id: [0x11; 16],
        });
        let frame = Frame::new(Opcode::TxnCommit.as_u16(), FLAG_EOS, 1, commit.encode());
        match dispatch_frame(frame, &mut state, &topo) {
            Action::OpDispatch(op) => assert_eq!(op.target_shard, bound_shard),
            _ => panic!("expected OpDispatch"),
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

    fn test_scope() -> RequestScope {
        RequestScope {
            space_id: SpaceId::from([0u8; 16]),
            org_id: [0u8; 16],
            user_id: [0u8; 16],
            namespace: "acme".to_owned(),
            permissions: 0,
            may_act: Vec::new(),
            key_hash: [0u8; 32],
        }
    }

    /// Regression for FIX (1): dispatching a `TXN_COMMIT` must NOT demote the
    /// route at dispatch time. Previously `terminate()` ran synchronously here,
    /// parking the route in the small retry window before the terminal outcome
    /// was known; the route must instead stay in the active tier until the
    /// shard confirms the terminal.
    #[test]
    fn txn_commit_dispatch_does_not_demote_route() {
        let topo = test_topology_shards(8);
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
        let txn_id = [0x7e; 16];

        let begin = RequestBody::TxnBegin(brain_protocol::envelope::request::TxnBeginRequest {
            txn_id,
            timeout_seconds: 30,
            act_as: None,
        });
        let frame = Frame::new(Opcode::TxnBegin.as_u16(), FLAG_EOS, 1, begin.encode());
        assert!(matches!(
            dispatch_frame(frame, &mut state, &topo),
            Action::OpDispatch(_)
        ));
        assert!(state.txn_shards.active.contains_key(&txn_id));

        let commit =
            RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest { txn_id });
        let frame = Frame::new(Opcode::TxnCommit.as_u16(), FLAG_EOS, 3, commit.encode());
        assert!(matches!(
            dispatch_frame(frame, &mut state, &topo),
            Action::OpDispatch(_)
        ));
        assert!(
            state.txn_shards.active.contains_key(&txn_id),
            "route must stay active until the terminal is confirmed"
        );
        assert_eq!(
            state.txn_shards.terminated.len(),
            0,
            "the commit dispatch must not demote the route into the retry window"
        );

        // Only the confirmed terminal (reported by the per-op task) demotes it.
        state.txn_shards.apply_outcome(TxnRouteOutcome {
            txn_id,
            kind: TxnRouteOutcomeKind::ConfirmedTerminal,
        });
        assert!(!state.txn_shards.active.contains_key(&txn_id));
        assert!(
            state.txn_shards.route(&txn_id).is_some(),
            "a confirmed terminal keeps the route reachable in the retry window"
        );
    }

    /// FIX (1): a commit that never reaches a shard (here the target shard is
    /// out of range) reports no confirmed terminal, so its route is left active
    /// for a retry rather than being dropped.
    #[tokio::test]
    async fn commit_that_never_reaches_shard_reports_no_terminal() {
        let (tx, rx) = flume::unbounded::<TxnRouteOutcome>();
        let op = OpDispatch {
            stream_id: 3,
            req: RequestBody::TxnCommit(brain_protocol::envelope::request::TxnCommitRequest {
                txn_id: [0x9c; 16],
            }),
            target_shard: 0,
            scope: test_scope(),
            connection_id: [0u8; 16],
            act_as: None,
        };
        // Empty shard vec → target shard 0 is out of range → ShardUnavailable,
        // and crucially no terminal is confirmed on the outcome channel.
        let frames = run_op_dispatch(op, Arc::new(Vec::new()), Some(tx)).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.opcode_u16(), Opcode::Error.as_u16());
        assert!(
            rx.try_recv().is_err(),
            "a commit that never reached a shard must not confirm a terminal"
        );
    }

    #[test]
    fn classify_commit_success_confirms_terminal() {
        let out = classify_txn_route_outcome(
            TxnReqKind::Commit([1u8; 16]),
            &Ok(brain_ops::DispatchOutcome::Single(ResponseBody::TxnCommit(
                brain_protocol::envelope::response::TxnCommitResponse {
                    txn_id: [1u8; 16],
                    committed_at_unix_nanos: 1,
                    operations_applied: 0,
                },
            ))),
        );
        assert!(matches!(
            out,
            Some(TxnRouteOutcome {
                kind: TxnRouteOutcomeKind::ConfirmedTerminal,
                ..
            })
        ));
    }

    #[test]
    fn classify_commit_transient_failure_keeps_route() {
        let out = classify_txn_route_outcome(
            TxnReqKind::Commit([1u8; 16]),
            &Err(DispatchError::Op(brain_ops::error::OpError::Overloaded(
                "shedding".into(),
            ))),
        );
        assert!(out.is_none(), "an overloaded shard is a transient failure");
        let out = classify_txn_route_outcome(
            TxnReqKind::Abort([1u8; 16]),
            &Err(DispatchError::ShardDisconnected),
        );
        assert!(out.is_none(), "a disconnect is a transient failure");
    }

    #[test]
    fn classify_commit_txn_gone_confirms_terminal() {
        for e in [
            brain_ops::error::OpError::TxnNotFound,
            brain_ops::error::OpError::TxnExpired,
        ] {
            let out = classify_txn_route_outcome(
                TxnReqKind::Commit([1u8; 16]),
                &Err(DispatchError::Op(e)),
            );
            assert!(
                matches!(
                    out,
                    Some(TxnRouteOutcome {
                        kind: TxnRouteOutcomeKind::ConfirmedTerminal,
                        ..
                    })
                ),
                "a txn the shard reports gone is a confirmed terminal"
            );
        }
    }

    #[test]
    fn classify_begin_rejection_vs_transient_vs_success() {
        // Definitive rejection (idempotency conflict) drops the route.
        let out = classify_txn_route_outcome(
            TxnReqKind::Begin([1u8; 16]),
            &Err(DispatchError::Op(brain_ops::error::OpError::Conflict(
                "dup".into(),
            ))),
        );
        assert!(matches!(
            out,
            Some(TxnRouteOutcome {
                kind: TxnRouteOutcomeKind::BeginRejected,
                ..
            })
        ));
        // Transient begin failure keeps the optimistic route.
        let out = classify_txn_route_outcome(
            TxnReqKind::Begin([1u8; 16]),
            &Err(DispatchError::Op(brain_ops::error::OpError::Overloaded(
                "busy".into(),
            ))),
        );
        assert!(out.is_none());
        // Successful begin keeps the route active for the txn's lifetime.
        let out = classify_txn_route_outcome(
            TxnReqKind::Begin([1u8; 16]),
            &Ok(brain_ops::DispatchOutcome::Single(ResponseBody::TxnBegin(
                brain_protocol::envelope::response::TxnBeginResponse {
                    txn_id: [1u8; 16],
                    timeout_seconds: 30,
                    started_at_unix_nanos: 1,
                },
            ))),
        );
        assert!(out.is_none());
    }
}
