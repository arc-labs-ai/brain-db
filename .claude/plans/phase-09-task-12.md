# Sub-task 9.12 — ArcSwap shared state + crossbeam-epoch reclamation

**Reads:**
- `spec/10_concurrency_epochs/05_arc_swap.md` (full).
- `spec/10_concurrency_epochs/06_crossbeam_epoch.md` (full).
- `docs/spec-deviations.md` SD-4.8-1 (HNSW: `Arc<RwLock<HnswIndex>>`
  instead of `ArcSwap<HnswState>` — locked).
- Existing usage: `crates/brain-index/src/shared.rs` (RwLock-based HNSW);
  workspace already has `arc-swap = "1"` + `crossbeam-epoch = "0.9"`.

**Phase doc:** orientation §11 sub-task **9.12**.

**Done when:** the `RoutingTable` is published via `ArcSwap` so a v1.x
follow-up can hot-reload it without restarting the server;
spec-deviation rows lock in the two cases where Brain consciously
departs from the spec (HNSW via SD-4.8-1, already documented; new
SD-10.6-1 for crossbeam-epoch non-use); a smoke test confirms a swap
is visible to a fresh `shard_for_agent` call.

---

## 1. Scope, pragmatically

Spec §10/05 lists three ArcSwap use sites:

| Use site | 9.12 disposition |
| -------- | ---------------- |
| Per-shard HNSW reference | **Deferred (SD-4.8-1, locked)** — `Arc<RwLock<HnswIndex>>` ships instead. Reason: `hnsw_rs::Hnsw` isn't cheaply cloneable; the spec's clone-and-swap pattern would cost 150 MB and seconds per maintenance flush at 1M nodes. |
| Per-shard configuration | **Deferred** — config is restart-only in v1 (CLAUDE.md §3 + config.rs `deny_unknown_fields`). Hot-reload is a v2 feature. |
| Routing table | **In scope for 9.12** — convert `Arc<RoutingTable>` → `Arc<ArcSwap<RoutingTable>>` so cluster reconfiguration can swap atomically without restarting connections. |

And spec §10/06 (crossbeam-epoch) lists:

| Use case | 9.12 disposition |
| -------- | ---------------- |
| HNSW node management during incremental cleanup | Inside `hnsw_rs` (third-party crate); we don't reach in. |
| Lock-free slot free list | `SlotAllocator` already uses a different mechanism (in-memory `Vec`) under single-writer-per-shard. No CAS loop needed. |
| Other lock-free structures within a shard | None today. Phase 12+ replication / multi-writer paths may need it. |

**Recommendation:** lock in a new spec deviation **SD-10.6-1**
documenting that crossbeam-epoch is intentionally unused in v1 — the
single-writer-per-shard discipline eliminates the contention this
library addresses, and the one third-party consumer (`hnsw_rs`) owns
its own reclamation.

### Why "small" is the right size for 9.12

Most of spec §10/05 + §10/06's machinery is already covered by:
- ArcSwap-equivalent semantics via `Arc<RwLock<…>>` (SD-4.8-1, locked).
- The single-writer-per-shard discipline (`SharedHnsw` / `Writer` type
  split, `RealWriterHandle` chokepoint).
- Glommio's single-threaded executor (no cross-thread reclamation
  inside a shard).

9.12 picks the one place where ArcSwap *does* pay off (routing) and
locks in the architectural decisions for everywhere else. Sizing
~250 LOC — small but high-leverage.

---

## 2. RoutingTable → ArcSwap

### 2.1 Today

```rust
// crates/brain-server/src/dispatch.rs
pub struct Topology {
    pub shards: Arc<Vec<ShardHandle>>,
    pub routing: Arc<RoutingTable>,                 // immutable post-construction
    pub server_caps: Arc<ServerCapabilities>,
}
```

`RoutingTable` is constructed once in `main.rs::linux_main::run`,
wrapped in `Arc`, and never replaced. Cluster reconfiguration (spec
§12/02 §2: "loaded at startup; updates require explicit triggers")
requires a server restart in v1.

### 2.2 9.12 shape

```rust
pub struct Topology {
    pub shards: Arc<Vec<ShardHandle>>,
    pub routing: Arc<ArcSwap<RoutingTable>>,        // CHANGED
    pub server_caps: Arc<ServerCapabilities>,
}
```

Call sites that look up the routing table now do:

```rust
let table = topology.routing.load_full();           // Arc<RoutingTable>
let shard_id = table.shard_for_agent(agent_id);
```

Or for cheaper guards:

```rust
let guard = topology.routing.load();                // arc_swap::Guard<Arc<RoutingTable>>
let shard_id = guard.shard_for_agent(agent_id);
```

We use `load_full()` everywhere — the 50 ns refcount bump is
invisible next to the agent-id hash + shard lookup.

### 2.3 Reload surface

`RoutingTable` gains:

```rust
impl RoutingTable {
    /// Replace the published table. Spec §10/05 §4: cluster
    /// reconfiguration trigger.
    ///
    /// 9.12 ships the seam; the trigger itself (admin RPC + gossip)
    /// lands post-Phase 9.
    pub fn store(swap: &ArcSwap<Self>, new: RoutingTable) {
        swap.store(Arc::new(new));
    }
}
```

We don't add a `reload()` *method* on `RoutingTable` itself because
the `ArcSwap` lives one level up (in `Topology`). A free function or
a `Topology::set_routing(new)` helper is sufficient.

### 2.4 Test

A single unit test exercises the swap:

```rust
let original = Arc::new(ArcSwap::from_pointee(RoutingTable::new(2, …).unwrap()));
let pre = original.load_full();
assert_eq!(pre.shard_for_agent(agent), some_shard);

original.store(Arc::new(RoutingTable::new(8, …).unwrap()));
let post = original.load_full();
assert_ne!(pre.shard_count(), post.shard_count());
```

(Probabilistic — the agent hashes to different shards under different
counts, modulo collisions. We test on a fixed agent_id and assert
shard_count changes; that's enough to prove the swap landed.)

---

## 3. New SD-10.6-1: crossbeam-epoch unused in v1

New entry in `docs/spec-deviations.md`:

- **Spec:** `spec/10_concurrency_epochs/06_crossbeam_epoch.md` says
  Brain uses crossbeam-epoch for HNSW node management, slot free
  lists, and other lock-free shard-internal structures.
- **Implementation:** Brain doesn't directly import crossbeam-epoch
  in any first-party crate. The dependency stays in `Cargo.toml`'s
  workspace block because (a) the spec calls for it, and (b) future
  Phase 12+ work (replication, parallel HNSW) may need it.
- **Reason:** The single-writer-per-shard discipline (audit §10/02)
  eliminates the contention that crossbeam-epoch was designed for.
  Inside a shard's Glommio executor, there are no concurrent writers
  to coordinate; readers either don't share state with the writer
  (separate Glommio task) or coordinate via `Arc` refcount semantics
  (SharedHnsw via RwLock per SD-4.8-1). The one place the spec
  prescribes crossbeam-epoch is HNSW node management, which lives
  inside `hnsw_rs` — a third-party crate that owns its own
  reclamation strategy.
- **What's not implemented:** None of the spec's named use cases.
- **Reconcile by:** Phase 12+ — when we introduce parallel HNSW
  workers or cross-shard reclamation, revisit. v1 doesn't need it.

This is genuinely the right state: the spec was written assuming
custom HNSW with internal lock-free structures. With `hnsw_rs` as the
locked dependency (CLAUDE.md §6), the crossbeam-epoch surface area
shrinks to nothing in first-party code.

---

## 4. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/Cargo.toml` | add `arc-swap.workspace = true` to the Linux-only dependencies block | 1 |
| `crates/brain-server/src/dispatch.rs` | `Topology.routing: Arc<ArcSwap<RoutingTable>>` | ~5 lines delta |
| `crates/brain-server/src/main.rs` | `Arc::new(ArcSwap::from_pointee(routing))` at construction | ~3 lines delta |
| `crates/brain-server/src/dispatch.rs` (dispatch_frame) | `topology.routing.load_full().shard_for_agent(…)` | ~10 lines delta |
| `crates/brain-server/tests/connection.rs` + `tests/dispatch.rs` + `tests/subscribe.rs` | update test scaffolds to wrap RoutingTable | ~9 lines delta total |
| `crates/brain-server/src/routing.rs` | add a `swap`-helper module test | ~25 LOC |
| `docs/spec-deviations.md` | new SD-10.6-1 entry | ~30 LOC |
| `docs/phases/phase-09-glommio-port.md` | flip §10 status row for ArcSwap + epoch | ~5 LOC |

Total: ~90 LOC code + ~30 LOC docs. Smallest sub-task in Phase 9.

---

## 5. Risks

| Risk | Mitigation |
| ---- | ---------- |
| ArcSwap adds a refcount bump per request | Negligible (~50 ns) compared to embedder + index search. |
| Test scaffolds break in 3 files (topology wraps differently) | All three already share the `Topology { routing: Arc::new(RoutingTable::new(…)) }` shape — converting to `Arc::new(ArcSwap::from_pointee(…))` is mechanical. |
| Future cluster-reconfiguration consumers expect a richer reload API | 9.12 ships the *primitive* (ArcSwap field). The admin RPC + gossip layer that drives it is post-Phase-9. The primitive doesn't constrain that layer. |
| SD-10.6-1 looks like dependency rot ("why is crossbeam-epoch in Cargo.toml if unused?") | The SD entry explains. We keep the dep in the workspace block so future sub-tasks don't pay a cargo-resolution cost when they need it. |

---

## 6. Done criteria

- [ ] `Topology.routing` is `Arc<ArcSwap<RoutingTable>>`.
- [ ] `dispatch_frame` looks up the routing via `load_full()`.
- [ ] `main.rs::linux_main::run` constructs via `ArcSwap::from_pointee`.
- [ ] Test scaffolds (3 test files) updated mechanically.
- [ ] A new `routing.rs` unit test proves a swap is visible to a
  fresh `shard_for_agent`.
- [ ] `docs/spec-deviations.md` gains **SD-10.6-1** for
  crossbeam-epoch non-use.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.12 marked `[x]`.

---

## 7. What 9.12 explicitly defers

- **`ArcSwap<HnswState>`** — covered by SD-4.8-1, locked.
- **`ArcSwap<Config>` for hot reload** — v2 (config is restart-only in v1).
- **Cluster-reconfiguration trigger** (admin RPC + gossip) — post-Phase-9.
- **Direct crossbeam-epoch usage** — locked as SD-10.6-1; first-party code
  doesn't need it under single-writer-per-shard.

---

*Implement on approval.*
