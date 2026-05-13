# Sub-task 9.11 — Cross-shard SUBSCRIBE fan-out

**Reads:**
- `spec/09_cognitive_operations/09_subscribe.md` (semantics, batches, ack, LSN replay).
- `spec/03_wire_protocol/05_opcodes.md` §1.3 (SUBSCRIBE_REQ/_EVENT/UNSUBSCRIBE_REQ/_RESP).
- `spec/03_wire_protocol/09_streaming.md` §3.3 (open-ended subscription streams), §5 (cancellation).
- `crates/brain-ops/src/subscribe.rs` (in-process `EventBus` + `SubscriptionRegistry` from 7.10).
- `docs/phases/phase-09-glommio-port.md` §8.1 (locked topology: per-shard local bus + connection-layer fan-out).

**Phase doc:** orientation §11 sub-task **9.11**.

**Done when:** A client can SUBSCRIBE post-AUTH and receive SUBSCRIBE_EVENT
frames for the full lifetime of its connection (or until UNSUBSCRIBE).
Cross-shard fan-out: a subscription on agent `A` whose filter spans
multiple shards collects events from each shard's local bus.
CANCEL_STREAM cancels the subscription cleanly. Multi-frame streaming
for RECALL/PLAN/REASON stays out of scope (defer to a follow-up;
SUBSCRIBE is the structural template).

---

## 1. Scope

In:
- A per-connection **SubscriptionRegistry** in the connection layer
  (replaces the brain-ops one for live streaming; brain-ops keeps its
  registry for the one-shot dispatch path which `RECALL`-style callers
  still use).
- A per-shard **fanout task**: spawned inside each shard's Glommio
  executor on `serve_connection` startup; drains the shard's
  `EventBus::receiver()` and pushes envelopes through a
  `flume::Sender<EventEnvelope>` to the connection layer.
- A **SUBSCRIBE_REQ handler** in the connection task that:
  1. Validates filter against agent permissions / shard scope.
  2. Allocates a stream_id, builds a `SubscriptionState` entry in
     the registry, returns a first-frame `SubscriptionEvent` (or an
     empty header frame) on the stream.
  3. Spawns a per-subscription task that reads from the per-shard
     fanout channels, applies the parsed filter, frames events, and
     pushes them to the connection's outgoing queue.
- **UNSUBSCRIBE_REQ** + **CANCEL_STREAM** wire-up: both cancel the
  per-subscription task and emit the final EOS frame on the
  subscription's stream.

Out:
- WAL-replay history (`from_lsn` other than `LatestOnly`) — still
  surfaces `LsnTooOld`-equivalent. Spec §16 acknowledges this as a
  v1-acceptable gap.
- `SimilarityFilter` / `min_salience` — Phase-9-out.
- Ack-required backpressure protocol (`ack_required: true` per spec §7).
- Multi-frame streaming for RECALL/PLAN/REASON. Their existing
  single-frame EOS responses from 9.10 stay; structural multi-frame
  is a future sub-task that reuses 9.11's plumbing.

---

## 2. The runtime shape

```
   Shard 0 Glommio                       Connection layer Tokio
   ┌───────────────────────┐             ┌────────────────────────────────┐
   │ writer.publish(env)   │             │                                │
   │     │                 │             │                                │
   │     ▼                 │             │  SubscriptionRegistry          │
   │ EventBus (broadcast)  │             │   ─────────────────────        │
   │     │                 │             │   HashMap<StreamId, SubState>  │
   │     ▼                 │             │                                │
   │ fanout_task ──────────┼────────────►│  per-conn SUBSCRIBE handler    │
   │ (Glommio Task::local) │  flume::    │                                │
   │ drains EventBus →    │  bounded    │  per-subscription task drains  │
   │ flume::Sender         │  (1024)     │  every shard's fanout chan,    │
   └───────────────────────┘             │  filters, frames, pushes to    │
                                          │  per-conn OutgoingFrame queue │
   Shard 1 Glommio                       │                                │
   ┌───────────────────────┐             └────────────────────────────────┘
   │ ... same shape ...    │
   └───────────────────────┘
```

**The flume channel is per-shard, not per-subscription.** One channel
between each shard and the connection layer carries every event that
shard publishes; the per-subscription tasks all read from the same
shared channels (via `tokio::sync::broadcast` on the Tokio side that
wraps the flume Receiver). This keeps fanout O(shards), not O(shards
× subscriptions).

Wait — `flume::Receiver` is single-consumer. To let N subscriptions
each see every event, we wrap each shard's fanout output in a
**`tokio::sync::broadcast::Sender`** at the connection-layer boundary:

```
shard_event_bus  →  fanout_task  →  flume::Sender   (per shard, into connection layer)
                                          │
                                          ▼
                              flume::Receiver  ──drains──→  tokio::broadcast::Sender
                                                                     │
                                                                ┌────┴────┐
                                                                ▼         ▼
                                                       per-sub-task   per-sub-task
                                                          (sub A)        (sub B)
```

Same `tokio::sync::broadcast` shape the in-process `EventBus` already
uses — the change is *where* it lives (connection-layer Tokio side
rather than per-shard).

---

## 3. New module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/src/subscribe.rs` | new — connection-layer SubscriptionRegistry, per-conn handler, per-sub task | ~500 |
| `crates/brain-server/src/shard.rs` | extend — spawn fanout_task inside the shard's Glommio closure; expose a per-shard `flume::Receiver<EventEnvelope>` clone on `ShardHandle` | ~150 delta |
| `crates/brain-server/src/connection.rs` | extend — handle SUBSCRIBE in `dispatch_frame` (or via a new `Action::Subscribe`); thread the registry into `ConnState` | ~100 delta |
| `crates/brain-server/src/dispatch.rs` | extend — add `Action::Subscribe { subscription_request, stream_id }` / `Action::Unsubscribe { ... }` variants | ~80 delta |
| `crates/brain-server/Cargo.toml` | brain-ops dep already; no new crate deps | 0 |
| `crates/brain-server/tests/subscribe.rs` | new — 6 integration tests | ~400 |

Total: ~1230 LOC. Single commit if cascade allows; structural fault
line is shard.rs (fanout_task wiring) vs subscribe.rs (registry +
per-sub task) — both can ship in one commit because their interface is
the flume channel, set up in shard.rs and consumed in subscribe.rs.

---

## 4. Shard-side: fanout_task

Inside the existing `spawn_shard` Glommio closure (after `ops` is built):

```rust
let event_bus = ops.executor.events.clone(); // brain_ops::EventBus
let (event_tx, event_rx) = flume::bounded::<EventEnvelope>(1024);

glommio::spawn_local(async move {
    let mut rx = event_bus.receiver();
    loop {
        match rx.recv().await {
            Ok(env) => {
                if event_tx.send_async(env).await.is_err() {
                    break; // connection layer dropped the channel
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue, // skip
            Err(_) => break,
        }
    }
}).detach();
```

`ShardHandle` gains:

```rust
pub struct ShardHandle {
    shard_id: ShardId,
    tx: flume::Sender<ShardRequest>,
    events: flume::Receiver<EventEnvelope>,   // NEW
}
```

The events receiver is `Clone` (flume Receivers are; `clone` shares
the queue). For the connection layer's broadcast bridge, we
**don't** clone the flume Receiver per subscription — we clone it
once into the bridge task, then publish through
`tokio::sync::broadcast`.

---

## 5. Connection-layer SubscriptionRegistry

```rust
pub struct SubscriptionRegistry {
    // One broadcast per shard — fans events to every subscription on
    // this connection that cares about that shard.
    per_shard_bus: Vec<tokio::sync::broadcast::Sender<EventEnvelope>>,
    next_stream_id: AtomicU32,
    streams: Mutex<HashMap<u32, SubscriptionState>>,
}

struct SubscriptionState {
    filter: ParsedFilter,
    final_lsn: AtomicU64,
    cancel: tokio::sync::watch::Sender<bool>,
}
```

`per_shard_bus` is built on connection setup: one `broadcast::channel`
per shard, plus one *bridge task* per shard that drains the shard's
`events` flume Receiver and forwards into the broadcast.

The bridge tasks are owned by the connection task and unwind when the
shutdown signal fires.

---

## 6. SUBSCRIBE handler

In `dispatch.rs`, add an `Action` variant:

```rust
enum Action {
    Inline(Frame),
    OpDispatch(OpDispatch),
    Subscribe(SubscribeStart),
    Unsubscribe { stream_id: u32, target_stream_id: u32 },
    CloseWith(Frame),
    Close,
    Nothing,
}

struct SubscribeStart {
    stream_id: u32,
    filter: ParsedFilter,
}
```

`dispatch_frame` recognises `RequestBody::Subscribe`, parses the
filter (reuse `brain_ops::subscribe::parse_filter` — needs to become
`pub`), returns `Action::Subscribe(SubscribeStart { … })`.

The connection's receiver loop sees `Action::Subscribe` and:
1. Allocates an entry in the connection's `SubscriptionRegistry`
   (the `stream_id` is the one the client picked).
2. Spawns a per-subscription task:
   ```rust
   tokio::spawn(async move {
       let mut shard_rxs: Vec<broadcast::Receiver<_>> = ...;
       loop {
           tokio::select! {
               _ = cancel_rx.changed() => break,
               Ok(env) = next_event(&mut shard_rxs) => {
                   if filter.matches(&env) {
                       let frame = build_subscription_event_frame(stream_id, &env);
                       if frame_tx.send_async(...).await.is_err() { break; }
                   }
               }
           }
       }
       // Emit final EOS frame.
       let _ = frame_tx.send_async(eos_frame_for(stream_id)).await;
   });
   ```
3. Immediately replies with a header-only SUBSCRIBE_EVENT frame (no
   data event, no EOS) so the client sees the stream is open.

For 9.11's single-shard initial cut, `shard_rxs` is `[conn.bound_shard]`.
Multi-shard fan-out picks every shard whose `bound_shard` matches the
filter's `agent_id` (typically one; cross-shard agents are the rare case).

---

## 7. UNSUBSCRIBE + CANCEL_STREAM

Both result in the same handler:

```rust
fn cancel_subscription(stream_id: u32, registry: &SubscriptionRegistry) -> Option<u64> {
    let mut g = registry.streams.lock();
    let state = g.remove(&stream_id)?;
    let _ = state.cancel.send(true);
    Some(state.final_lsn.load(Ordering::SeqCst))
}
```

UNSUBSCRIBE replies with `UnsubscribeResponse { stream_id, final_lsn }`
on its *own* stream (per spec §03/05 §5.3); the original
subscription's stream emits a final EOS frame from inside the
per-subscription task as it observes `cancel_rx.changed()`.

CANCEL_STREAM replies with `CancelStreamAck { target_stream_id,
cancelled_at_unix_nanos }` on its own stream and does not emit an
extra EOS on the cancelled stream (the canceled task does its own
final EOS emission).

---

## 8. Spec deviations accepted in 9.11

| Spec requirement | 9.11 disposition |
| ---------------- | ---------------- |
| `from_lsn = FromLsn(...)` history replay | Reject as `LsnTooOld`. Carry forward — needs WAL-tail reader. |
| `ack_required: true` flow control | Reject as `NotYetImplemented`. v2. |
| `min_salience`, `similar_to` filters | Reject as `NotYetImplemented` until brain-ops gains them. |
| `EdgeAdded` / `EdgeRemoved` events | Not emitted today (brain-ops's writer doesn't publish them). Spec doesn't change here; the registry will deliver any future event types automatically. |
| Cross-region replication via subscribe | Out of v1 scope (spec §06/§17 acknowledges replication is v2). |

Each deviation gets a brief paragraph in commit body + the existing
spec-deviations doc isn't touched (9.11 just inherits the v1 gaps).

---

## 9. Tests (`tests/subscribe.rs`)

The scaffold reuses `start_with_shards` from `tests/dispatch.rs` (move
it to a shared `tests/util/` helper, or duplicate — simpler).

1. **`subscribe_receives_encode_events`** — agent A subscribes, then a
   second connection performs ENCODE under the same agent. The
   subscriber sees a SUBSCRIBE_EVENT frame with the new memory_id.
2. **`unsubscribe_emits_final_eos`** — subscribe, then unsubscribe;
   observe UNSUBSCRIBE_RESP on the unsubscribe stream + a final EOS
   on the subscription stream.
3. **`cancel_stream_terminates_subscription`** — subscribe, then
   CANCEL_STREAM; observe CANCEL_STREAM_ACK and a final EOS on the
   subscription stream.
4. **`subscribe_filter_by_kind_drops_non_matching`** — subscribe
   filtering on `Episodic`; an ENCODE with `Semantic` is not
   delivered; an ENCODE with `Episodic` is.
5. **`subscribe_filter_by_context`** — same shape but filter on a
   specific context_id.
6. **`subscribe_rejects_from_lsn`** — `from_lsn: Some(123)` returns
   `ERROR(LsnTooOld)` — equivalent wire shape.

(`similar_to` rejection + `min_salience` rejection can be folded into
test 6 as parameterised assertions; keeps the test count at 6.)

---

## 10. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `brain_ops::EventBus` is per-`OpsContext`; the per-shard fanout assumes one bus per shard | Already true — `OpsContext` is per-shard since 9.7b. Confirm `ops.executor.events` is the right field name during impl. |
| flume bounded channel back-pressures the shard executor under bursty load | Default capacity 1024 envelopes ≈ 1–2 MB; ample for typical workloads. If full, the fanout_task awaits — that yields the executor cleanly. |
| broadcast lag drops events to slow subscribers | Same as current brain-ops behavior. Surface as `Lagged` → close subscription with ERROR(Overloaded). Spec §17.4 acknowledges TooManySubscribers / lag are fail-closed. |
| Per-shard broadcast topology means a connection-layer bridge task per shard, even if no subscriptions exist | Cheap: each bridge is one task per connection per shard, idle most of the time. Could be lazy-instantiated per-subscription if profiling shows pain. |
| EventBus is wired into `OpsContext` but writer code may not publish on every ENCODE | Confirm during impl. If gaps exist, file them as v1.1 follow-ups (don't try to fix in 9.11). |
| 9.10's `ConnState` is `!Send + !Sync` (carries `Arc<Vec<ShardHandle>>` which contains flume `Receiver` per-shard) | `Sender`/`Receiver` are `Send + Sync` (flume designed for it). Fine. |

---

## 11. Done criteria

- [ ] `crates/brain-server/src/subscribe.rs` ships SubscriptionRegistry + per-sub task.
- [ ] `ShardHandle` carries a per-shard `events: flume::Receiver<EventEnvelope>` plus the existing `tx`.
- [ ] `spawn_shard` spawns a `fanout_task` inside the Glommio executor that drains the shard's `EventBus`.
- [ ] `ConnState` holds a per-connection `SubscriptionRegistry` + per-shard `broadcast::Sender` pair.
- [ ] `dispatch_frame` handles SUBSCRIBE_REQ / UNSUBSCRIBE_REQ / CANCEL_STREAM into the registry.
- [ ] 6 integration tests in `tests/subscribe.rs` pass.
- [ ] All 9.9 + 9.10 tests still pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.11 marked `[x]`.
- [ ] Audit doc §8.1 status row flipped to **done**.

---

## 12. What 9.11 explicitly defers

- **WAL-replay history** (`from_lsn = FromLsn(_)`) — needs a WAL-tail
  reader hooked into the SUBSCRIBE path. Track as a follow-up.
- **Ack-required flow control** — spec §7.
- **EdgeAdded / EdgeRemoved events** — brain-ops writer doesn't emit.
- **Multi-frame streaming for RECALL/PLAN/REASON** — reuses 9.11's
  per-sub task shape but with different lifecycle (response is bounded
  by a result set rather than open-ended).
- **Cross-shard agents** — spec §5 supports them; v1's routing assumes
  one shard per agent, so 9.11 implements single-shard for now and
  leaves multi-shard fan-out as a structural extension.

---

*Implement on approval.*
