# Sub-task 7.10 — SUBSCRIBE (change-feed)

**Spec:** `spec/09_cognitive_operations/09_subscribe.md`
**Phase doc:** `docs/phases/phase-07-operations.md` §7.10
**Done when:** "Subscribe to 'new memories matching filter X'; sink receives events; backpressure works."

---

## 1. Scope (v1) and explicit out-of-scope

SUBSCRIBE is the only **streaming** primitive. The wire layer (Phase 9) will frame
multiple events on one connection. For sub-task 7.10 we land everything **above** the
wire framing: filter validation, event production at every mutator, an in-process
broadcast bus, a subscription registry, backpressure handling, and a public
`register_subscription` API the server connection task will consume in Phase 9.

| In scope (7.10)                                                | Out of scope (Phase 9 / later)         |
| -------------------------------------------------------------- | -------------------------------------- |
| In-process `tokio::sync::broadcast` bus                        | WAL-tail replay for `from_lsn`         |
| Per-subscription filter eval (contexts, kinds, salience)       | True multi-event push over wire        |
| Writer publishes after every successful redb commit            | Cross-shard merge (single-shard only)  |
| Txn commit replays buffered effects in order                   | `LsnTooOld` against checkpointed WAL   |
| `SubscriptionRegistry` with monotonic stream ids               | Snapshot-then-tail pattern (§13)       |
| Backpressure via `RecvError::Lagged` → final_lsn freeze        | `ack_required` flow-control            |
| `include_history=true` with `from_lsn` → `LsnTooOld`           | `SimilarityFilter` (requires HNSW)     |
| Dispatcher returns the **first matching** event (bounded poll) | Continuous push from a single request  |
| 14+ unit/integration tests                                     | Throughput micro-bench                 |

The dispatcher path returns exactly **one** `SubscriptionEvent` per `SubscribeRequest`
(the response shape is single-event today; Phase 9 will frame additional events on the
same stream). The `register_subscription` public API is what Phase 9 will reach for to
push the rest. This split is the only honest mapping of the spec onto v1's
request/response substrate.

---

## 2. Architecture

### 2.1 The pieces

```
                              ┌──────────────────────────┐
   writer commit succeeds ───▶│   EventBus               │── broadcast::Sender
                              │   (tokio broadcast)      │
                              └──────────────────────────┘
                                          │
                  ┌───────────────────────┼────────────────────────┐
                  ▼                       ▼                        ▼
        SubEntry {filter, rx}   SubEntry {filter, rx}    SubEntry {filter, rx}
        target_stream_id=1      target_stream_id=2       target_stream_id=3
                  │                       │                        │
                  ▼                       ▼                        ▼
          Phase 9: wire frame   Phase 9: wire frame      Phase 9: wire frame
```

### 2.2 New types (in `brain-ops`)

```rust
// crates/brain-ops/src/subscribe.rs (full rewrite, replaces stub)

/// Monotonic per-process LSN. Stand-in until Phase 9 wires WAL LSN.
pub struct LsnAllocator(AtomicU64);

/// What gets pushed on the broadcast bus. Mirrors `SubscriptionEvent`
/// but carries the raw `ContextId`/`MemoryKind`/etc. so filter eval
/// is cheap (no wire conversion per subscriber).
#[derive(Clone, Debug)]
pub struct EventEnvelope {
    pub lsn: u64,
    pub event_type: EventType,
    pub memory_id: MemoryId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub salience: f32,
    pub timestamp_unix_nanos: u64,
    pub text: Option<String>,         // None unless mutator carries it
}

pub struct EventBus {
    sender: broadcast::Sender<EventEnvelope>,
    lsn: LsnAllocator,
}

impl EventBus {
    pub fn new(channel_capacity: usize) -> Self;
    pub fn publish(&self, mut env: EventEnvelope);     // stamps lsn
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope>;
    pub fn next_lsn(&self) -> u64;
}

pub struct SubscriptionRegistry {
    inner: parking_lot::Mutex<RegistryInner>,
    bus: Arc<EventBus>,
}

struct RegistryInner {
    next_stream_id: u32,
    streams: HashMap<u32, SubEntry>,
}

struct SubEntry {
    filter: ParsedFilter,                              // pre-converted from wire
    started_at_lsn: u64,
    final_lsn: AtomicU64,                              // last delivered (or last seen)
}

pub struct SubscriptionHandle {
    pub target_stream_id: u32,
    pub started_at_lsn: u64,
    pub receiver: broadcast::Receiver<EventEnvelope>,
    pub filter: ParsedFilter,
}

impl SubscriptionRegistry {
    pub fn register(&self, req: &SubscribeRequest) -> Result<SubscriptionHandle, OpError>;
    pub fn unregister(&self, stream_id: u32) -> Result<u64, OpError>;  // returns final_lsn
    pub fn update_final_lsn(&self, stream_id: u32, lsn: u64);
}
```

### 2.3 `OpsContext` extension

```rust
pub struct OpsContext {
    pub executor: ExecutorContext,
    pub planner_ctx: PlannerContext,
    pub txn_store: Arc<TxnStore>,
    pub events: Arc<EventBus>,                  // NEW
    pub subscriptions: Arc<SubscriptionRegistry>, // NEW
}
```

Builder pattern preserved: `OpsContext::new(exec)` defaults to a 1024-slot bus.
`with_event_bus(bus)` / `with_subscriptions(registry)` for overrides.

### 2.4 Writer publishes events

Every mutator path emits one or more events **after** the redb `commit()` succeeds.
Failure paths emit nothing. Inside `do_submit_batch` (txn commit), the writer emits
events **after** the wtxn commits, in buffer order:

```rust
// crates/brain-ops/src/writer.rs (additions)

// At RealWriterHandle construction the writer takes Arc<EventBus>.
impl RealWriterHandle {
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self { ... }
}

// Inside do_submit (single op) and do_submit_batch (txn commit), after commit():
self.bus.publish(EventEnvelope { event_type: Encoded,  memory_id, ... });
self.bus.publish(EventEnvelope { event_type: Forgotten, memory_id, ... });
// Link/Unlink: see §2.5 below.
```

### 2.5 EventType mapping

The wire `EventType` enum is `{ Encoded, Forgotten, Reclaimed, KindChanged }`. The
spec's §2 narrative also lists `EdgeAdded`/`EdgeRemoved`, but the wire schema as it
stands today does **not** carry them. Two paths:

- **Path A (preferred, no spec change):** for 7.10 we only publish `Encoded` and
  `Forgotten` events. Link/Unlink commits write to redb but do **not** emit
  subscription events. Document this as a v1 gap in the commit.
- **Path B:** extend `EventType` + `SubscriptionEvent` to carry edge events. This
  is a wire change and needs explicit user sign-off (spec §05 doesn't allocate the
  variants).

**Plan picks Path A.** Path B is a one-line "next-task" doc note for Phase 9.
`Reclaimed` and `KindChanged` are emitted only by background workers (Phase 8) —
out of scope here; the writer never produces them in 7.10.

### 2.6 Filter evaluation

`ParsedFilter` is the registry-side cached form (avoid re-parsing the wire
`SubscriptionFilter` per event):

```rust
pub struct ParsedFilter {
    pub contexts: Option<HashSet<ContextId>>,
    pub kinds: Option<HashSet<MemoryKind>>,
    pub min_salience: Option<f32>,           // (future; spec §2 lists it but
                                             //  the wire struct lacks it today;
                                             //  always None in 7.10)
    pub similar_to: Option<SimilarityFilter>, // rejected w/ NotYetImplemented
}

fn matches(env: &EventEnvelope, f: &ParsedFilter) -> bool {
    if let Some(ctxs) = &f.contexts && !ctxs.contains(&env.context_id) { return false; }
    if let Some(ks) = &f.kinds && !ks.contains(&env.kind) { return false; }
    if let Some(t) = f.min_salience && env.salience < t { return false; }
    true
}
```

`register()` rejects `similar_to.is_some()` with `OpError::NotYetImplemented`.

### 2.7 Dispatcher contract (one-shot)

```rust
// handle_subscribe v1:
//   1. Reject `from_lsn = Some(_)` with LsnTooOld (no WAL replay yet).
//   2. Reject `similar_to` with NotYetImplemented.
//   3. Register subscription, get Handle.
//   4. Poll the receiver with bounded timeout (default 5s, tunable via
//      env BRAIN_SUBSCRIBE_FIRST_EVENT_MS) for the first matching event.
//   5. On match → return that SubscriptionEvent (registry's final_lsn updated).
//      On timeout → return OpError::Unavailable("subscribe: no matching event
//      in window — Phase 9 will keep the stream open"). (One-shot honesty.)
//      On Lagged → final_lsn frozen at started_at_lsn, return Unavailable.
//
// handle_unsubscribe v1:
//   1. Look up stream_id; return NotFound if missing.
//   2. unregister(), return UnsubscribeResponse { target_stream_id, final_lsn }.
```

The `Unavailable` timeout response is the explicit v1 honest behaviour for a
one-shot dispatcher. Tests cover both the matched and the timed-out branches.
**Phase 9's connection task will not call this dispatcher path for streaming**;
it will use `SubscriptionRegistry::register` directly and frame events from the
returned receiver.

### 2.8 Backpressure

`tokio::sync::broadcast` channel of capacity 1024 (configurable). When a
subscriber lags, `recv()` returns `Err(RecvError::Lagged(n))`. The registry's
recv loop (used by tests, and later by Phase 9) handles this by:

- Incrementing a per-stream `dropped_events` counter (tracing).
- Freezing `final_lsn` at the last successfully delivered LSN.
- Continuing to receive (the receiver auto-recovers to the latest in-buffer event).

The dispatcher's one-shot path treats Lagged as timeout (returns Unavailable).

### 2.9 LSN allocator

`LsnAllocator(AtomicU64)`. `next_lsn()` returns a strictly-increasing u64. Writer
calls `bus.publish()` which calls `lsn.fetch_add(1, Ordering::SeqCst)` inside the
envelope build. Single shard → single allocator → ordered per spec §4.

---

## 3. File-by-file diff plan

| File                                             | Action     | Notes |
| ------------------------------------------------ | ---------- | ----- |
| `crates/brain-ops/src/subscribe.rs`              | Full rewrite | EventBus, Registry, ParsedFilter, handle_subscribe/_unsubscribe |
| `crates/brain-ops/src/context.rs`                | Extend     | `events`, `subscriptions` fields + builders + Send/Sync guard |
| `crates/brain-ops/src/writer.rs`                 | Extend     | `with_event_bus`, publish after `do_submit` and `do_submit_batch` commit |
| `crates/brain-ops/src/error.rs`                  | Extend     | Add `Unavailable(&'static str)` variant if missing (check first) |
| `crates/brain-ops/src/lib.rs`                    | Re-export  | `pub use subscribe::{EventBus, SubscriptionRegistry, EventEnvelope, ...}` |
| `crates/brain-ops/Cargo.toml`                    | None       | tokio "macros, rt, rt-multi-thread" already on; broadcast is in `sync` which is default |
| `crates/brain-ops/tests/subscribe.rs`            | NEW        | 14–16 integration tests |
| `crates/brain-protocol/src/...`                  | None       | No wire changes |
| `spec/...`                                       | None       | No spec changes |
| `crates/brain-server/...`                        | None for now | Phase 9 wires Registry into the connection task |

---

## 4. Test plan (`tests/subscribe.rs`)

All tests use `tokio::test(flavor = "current_thread")` and the in-memory ops
context already set up by other test files.

### Lifecycle (3)
1. `subscribe_then_unsubscribe_returns_final_lsn`
2. `unsubscribe_unknown_stream_id_returns_not_found`
3. `register_with_similar_to_filter_returns_not_yet_implemented`

### Event publication (4)
4. `encode_publishes_encoded_event_with_increasing_lsn`
5. `forget_publishes_forgotten_event`
6. `txn_commit_publishes_all_buffered_events_in_order`
7. `txn_abort_publishes_nothing`

### Filter matching (4)
8. `context_filter_drops_off_context_events`
9. `kind_filter_drops_off_kind_events`
10. `combined_context_and_kind_filter_is_AND`
11. `null_filter_passes_every_event`

### One-shot dispatcher (3)
12. `handle_subscribe_returns_first_matching_event` (encode → subscribe → encode → event)
13. `handle_subscribe_times_out_when_no_event_matches` (returns Unavailable)
14. `handle_subscribe_with_from_lsn_returns_lsn_too_old`

### Backpressure (1)
15. `slow_subscriber_lagged_recv_freezes_final_lsn` (publish 2000 events into a 1024-cap bus without recv → Lagged returned, final_lsn frozen at started_at_lsn)

### Optional (if time): cross-handler ordering (1)
16. `encode_link_forget_sequence_preserves_lsn_order` — link not in event stream (Path A), so checks only encode→forget ordering.

---

## 5. Risks and mitigations

| Risk                                                        | Mitigation                                              |
| ----------------------------------------------------------- | ------------------------------------------------------- |
| Adding `events` field breaks every `OpsContext::new` caller | `new()` defaults to a private 1024-cap bus; no caller breaks |
| broadcast::Sender requires `Clone` envelopes                | `EventEnvelope: Clone` (small struct + Option<String>)  |
| Writer publish in critical path slows commits               | publish is a non-blocking `send`; bounded fixed cost    |
| Spec lists `EdgeAdded`/`EdgeRemoved` but wire enum lacks them | Path A: document v1 gap; defer to Phase 9 + user        |
| Dispatcher one-shot vs spec's long-lived stream             | Explicit v1 contract: handler returns first event or Unavailable; Phase 9 owns the long-lived path via `register_subscription` |
| Lagged subscriber recovery semantics                        | Tests assert final_lsn freezes; no panic, no data corruption |

---

## 6. Out-of-scope, surfaced as commit-message gaps

- WAL-LSN integration & history replay (Phase 9).
- Cross-shard subscription merge (Phase 8+).
- `ack_required` flow-control protocol (Phase 9).
- `EdgeAdded` / `EdgeRemoved` event types (needs wire-enum extension; user signoff).
- `min_salience` filter (wire `SubscriptionFilter` lacks the field today; spec §2 lists it as desirable — leave the `ParsedFilter` slot but never populated in 7.10).
- `SimilarityFilter` evaluation (requires HNSW lookup per event; defer).
- Throughput bench against §10 (10K events/sec target) — Phase 9 owns benching the stream-framed path.

---

## 7. Done criteria (matches phase-07 doc §7.10)

- [ ] `subscribe.rs` rewritten with EventBus, Registry, real handlers.
- [ ] Writer publishes events on every successful encode/forget single-op commit AND on txn commit (in buffer order).
- [ ] Filter eval (contexts, kinds) works; SimilarityFilter rejected cleanly.
- [ ] `OpsContext::new` is backwards-compatible (existing tests still compile/pass).
- [ ] 14+ new tests pass first run; existing workspace tests stay green.
- [ ] `just verify` clean (build, test, clippy, fmt).
- [ ] Commit subject: `feat(brain-ops): SUBSCRIBE change-feed (sub-task 7.10)`.

---

## 8. Estimated effort

~600 LOC of impl + ~700 LOC of tests. One container session.

Single commit, no spec changes, no wire bump.
