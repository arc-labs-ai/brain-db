# Sub-task 9.3 — Routing (pure functions)

**Reads:** `spec/12_sharding_clustering/02_routing.md`
**Phase doc:** `docs/phases/phase-09-server.md` §9.3 (was §9.5; orientation renumbered)
**Done when:** `agent_id → shard_id` and `memory_id → shard_id` are deterministic, O(1), unit-tested. A `RoutingTable` struct loads override map from config.

---

## 1. Scope

Tiny sub-task. Pure functions; no I/O, no state mutation, no async. Independent of every other Phase 9 sub-task — the frame dispatcher (9.10) and connection layer (9.9) consume the output without coupling back.

**In scope (per spec §12/02):**
- `shard_for_memory(memory_id) -> ShardId` — bit extraction, already exists as `MemoryId::shard()` in brain-core. Routing module just re-exports / wraps for symmetry.
- `shard_for_agent(agent_id, shard_count) -> ShardId` — BLAKE3 hash of agent UUID bytes, modulo shard_count.
- `RoutingTable { shard_count, overrides }` — checks overrides first, falls back to hash.
- Validation: shard_count ≥ 1; override values < shard_count.

**Out of scope (per spec §12/02 §8 / §16, deferred to v2):**
- Multi-shard agents (§8) — single-shard-per-agent for v1.
- "WrongShard" handling — connection-layer error path lives in 9.10.
- Fan-out for ADMIN_STATS (§16) — 9.13's concern.
- Consistent hashing (§6) — fixed shard count for v1.
- Clustered-mode endpoint resolution (§12).

---

## 2. File-by-file

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-server/src/routing.rs` | NEW | ~120 LOC: types + funcs + unit tests |
| `crates/brain-server/src/main.rs`    | Edit | `mod routing;` declaration (no callers yet — module exists for 9.10 to consume) |
| `crates/brain-server/Cargo.toml`     | Edit | Add `brain-core` (workspace path) + `blake3` (workspace) |

---

## 3. Surface

```rust
//! Spec §12/02 — agent_id → shard and memory_id → shard.

use brain_core::{AgentId, MemoryId, ShardId};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct RoutingTable {
    shard_count: u16,
    overrides: HashMap<AgentId, ShardId>,
}

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("shard_count must be >= 1, got 0")]
    ZeroShardCount,
    #[error("override for agent {agent:?} maps to shard {shard}, out of range [0, {shard_count})")]
    OverrideOutOfRange { agent: AgentId, shard: u16, shard_count: u16 },
}

impl RoutingTable {
    pub fn new(shard_count: u16, overrides: HashMap<AgentId, ShardId>) -> Result<Self, RoutingError>;

    pub fn shard_for_agent(&self, agent: AgentId) -> ShardId;

    pub fn shard_count(&self) -> u16;
}

/// Free function — same semantics as `RoutingTable::shard_for_agent`
/// with no overrides. Useful when the caller already has shard_count
/// and doesn't need the override map.
pub fn hash_agent_to_shard(agent: AgentId, shard_count: u16) -> ShardId;

/// Free function — bit extraction. Convenience wrapper over
/// `memory_id.shard()` so callers don't need to import brain-core
/// directly when they're already using `routing::`.
pub fn shard_for_memory(memory_id: MemoryId) -> ShardId;
```

### Hash function

Per spec §12/02 §4–§5:
```rust
fn hash_agent_to_shard(agent: AgentId, shard_count: u16) -> ShardId {
    let bytes = agent.0.as_bytes();           // Uuid -> [u8; 16]
    let h = blake3::hash(bytes);
    let hi = u64::from_le_bytes(h.as_bytes()[..8].try_into().expect("16 -> 8 ok"));
    (hi % u64::from(shard_count)) as ShardId
}
```

Single `% u64` is sufficient — uniformly distributed for BLAKE3 hashes against any `shard_count ≤ 65535`.

---

## 4. Tests (all unit, inside `routing.rs`)

1. `shard_for_memory_extracts_shard_bits` — pack(7, 0, 0).shard() == 7.
2. `hash_agent_to_shard_is_deterministic` — same agent → same shard across calls.
3. `hash_agent_to_shard_respects_shard_count` — result < shard_count for shard_count in {1, 2, 4, 8, 16, 256}.
4. `hash_agent_to_shard_distributes_uniformly` — 10k random agents, 16 shards, every shard receives at least 10% of mean.
5. `routing_table_honors_override` — override agent A → shard 3 returns 3 even if hash would say 7.
6. `routing_table_rejects_zero_shard_count`.
7. `routing_table_rejects_override_out_of_range`.
8. `routing_table_falls_back_to_hash` — no override → matches `hash_agent_to_shard`.

Test 4 is statistical. Use a fixed seed (or `AgentId::new()` × 10000, accept the tiny flake risk — BLAKE3 distribution is good enough that 10% lower-bound is a 0.0000…% flake).

---

## 5. Sizing

- `routing.rs` impl: ~80 LOC
- Tests (inline): ~120 LOC
- `Cargo.toml`: 2 lines
- `main.rs`: 1 line (mod decl)

Single commit on `feature/brain-server`. Subject: `feat(brain-server): routing (sub-task 9.3)`.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `hash_agent_to_shard` doesn't match the SDK's routing → cross-shard mis-route | Single source of truth in this module. SDKs vendor or re-derive — out of scope here, but the function is deliberately small + spec-compliant so re-implementation is trivial. |
| Override map mutation at runtime | Not supported in v1 — spec §12/02 §2 says "loaded at startup; updates require explicit triggers". `RoutingTable` is immutable after `new()`. |
| `blake3` workspace dep already pulled by other crates → no version skew | Verified — `blake3 = "1"` is in workspace `[dependencies]`. Just add `blake3.workspace = true` to brain-server. |

---

*Implement on approval.*
