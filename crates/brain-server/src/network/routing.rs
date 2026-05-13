//! Routing — spec §12/02.
//!
//! Pure functions that map an `AgentId` or `MemoryId` to the `ShardId`
//! that owns the request. Two routing modes:
//!
//! - **Memory-based**: `MemoryId` already encodes its shard in the high
//!   16 bits (`brain-core` §02/03 §2.1). O(1) bit extraction.
//! - **Agent-based**: BLAKE3 hash of the agent's UUID bytes, modulo
//!   `shard_count`. Optionally overridden via a startup-time map for
//!   VIP / extra-large agents (spec §12/02 §7).
//!
//! Out of scope for v1 (deferred to v2 per spec §12/02 §6, §8, §14):
//!   - Multi-shard agents (§8).
//!   - "WrongShard" handling (§14) — connection layer's concern.
//!   - Consistent hashing for elastic shard counts (§6).

use std::collections::HashMap;

use brain_core::{AgentId, MemoryId, ShardId};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("shard_count must be >= 1, got 0")]
    ZeroShardCount,

    #[error(
        "override for agent {agent:?} maps to shard {shard}, \
         which is out of range [0, {shard_count})"
    )]
    OverrideOutOfRange {
        agent: AgentId,
        shard: ShardId,
        shard_count: u16,
    },
}

// ---------------------------------------------------------------------------
// RoutingTable
// ---------------------------------------------------------------------------

/// Immutable post-`new`. v1 has no dynamic shard reconfiguration —
/// spec §12/02 §2: "loaded at startup; updates require explicit
/// triggers (configuration reload or cluster events)".
#[derive(Clone, Debug)]
pub struct RoutingTable {
    shard_count: u16,
    overrides: HashMap<AgentId, ShardId>,
}

impl RoutingTable {
    /// Construct + validate. Rejects `shard_count == 0` and any
    /// override whose target is `>= shard_count`.
    pub fn new(
        shard_count: u16,
        overrides: HashMap<AgentId, ShardId>,
    ) -> Result<Self, RoutingError> {
        if shard_count == 0 {
            return Err(RoutingError::ZeroShardCount);
        }
        for (&agent, &shard) in &overrides {
            if shard >= shard_count {
                return Err(RoutingError::OverrideOutOfRange {
                    agent,
                    shard,
                    shard_count,
                });
            }
        }
        Ok(Self {
            shard_count,
            overrides,
        })
    }

    #[must_use]
    #[allow(dead_code)] // surface for diagnostics / future 9.13 admin
    pub fn shard_count(&self) -> u16 {
        self.shard_count
    }

    /// Resolve the shard for an agent. Spec §12/02 §4: overrides
    /// first, then BLAKE3 modulo `shard_count`.
    #[must_use]
    pub fn shard_for_agent(&self, agent: AgentId) -> ShardId {
        if let Some(&s) = self.overrides.get(&agent) {
            return s;
        }
        hash_agent_to_shard(agent, self.shard_count)
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// BLAKE3-of-uuid-bytes modulo `shard_count`. Spec §12/02 §4–§5.
///
/// Panics if `shard_count == 0` (precondition; the typed `RoutingTable`
/// constructor enforces this — direct callers must validate first).
#[must_use]
pub fn hash_agent_to_shard(agent: AgentId, shard_count: u16) -> ShardId {
    assert!(shard_count > 0, "shard_count must be > 0");
    let bytes = agent.0.as_bytes();
    let h = blake3::hash(bytes);
    let prefix: [u8; 8] = h.as_bytes()[..8]
        .try_into()
        .expect("BLAKE3 output is 32 bytes; first 8 always fit");
    let hi = u64::from_le_bytes(prefix);
    (hi % u64::from(shard_count)) as ShardId
}

/// Bit-extraction shortcut for memory-based routing. Identical to
/// `memory_id.shard()`; exists so callers that already `use routing::`
/// don't need a second import for the most common operation.
#[must_use]
pub fn shard_for_memory(memory_id: MemoryId) -> ShardId {
    memory_id.shard()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::SlotIndex;
    use uuid::Uuid;

    fn agent(seed: u64) -> AgentId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        AgentId(Uuid::from_bytes(bytes))
    }

    #[test]
    fn shard_for_memory_extracts_shard_bits() {
        let id = MemoryId::pack(7, 0, 0);
        assert_eq!(shard_for_memory(id), 7);
        let id = MemoryId::pack(0xFFFF, 42, 1);
        assert_eq!(shard_for_memory(id), 0xFFFF);
    }

    #[test]
    fn hash_agent_to_shard_is_deterministic() {
        let a = agent(0x1234_5678);
        let s1 = hash_agent_to_shard(a, 8);
        let s2 = hash_agent_to_shard(a, 8);
        let s3 = hash_agent_to_shard(a, 8);
        assert_eq!(s1, s2);
        assert_eq!(s2, s3);
    }

    #[test]
    fn hash_agent_to_shard_respects_shard_count() {
        // For each shard count, every result must be in [0, shard_count).
        for &shard_count in &[1u16, 2, 4, 8, 16, 256, 1024, 65535] {
            for seed in 0..1000u64 {
                let s = hash_agent_to_shard(agent(seed), shard_count);
                assert!(
                    s < shard_count,
                    "shard {s} out of range for shard_count {shard_count}"
                );
            }
        }
    }

    #[test]
    fn hash_agent_to_shard_distributes_uniformly() {
        // 10k agents across 16 shards. Each shard's count should be
        // within [0.5 × mean, 1.5 × mean] = [312, 938] of the mean 625.
        // BLAKE3 distribution is much tighter than this in practice;
        // the wide band makes the test flake-free.
        const N: u64 = 10_000;
        const SHARDS: u16 = 16;
        let mut buckets = [0u64; SHARDS as usize];
        for seed in 0..N {
            let s = hash_agent_to_shard(agent(seed), SHARDS) as usize;
            buckets[s] += 1;
        }
        let mean = N / u64::from(SHARDS);
        let lo = mean / 2;
        let hi = mean + mean / 2;
        for (i, &c) in buckets.iter().enumerate() {
            assert!(
                c >= lo && c <= hi,
                "shard {i} got {c} agents, expected within [{lo}, {hi}]"
            );
        }
    }

    #[test]
    fn routing_table_honors_override() {
        let a = agent(42);
        let natural = hash_agent_to_shard(a, 8);
        // Pick an override that's not equal to the natural hash so
        // we know the override path actually executed.
        let forced: ShardId = if natural == 3 { 5 } else { 3 };
        let mut overrides = HashMap::new();
        overrides.insert(a, forced);
        let table = RoutingTable::new(8, overrides).unwrap();
        assert_eq!(table.shard_for_agent(a), forced);
    }

    #[test]
    fn routing_table_falls_back_to_hash_when_no_override() {
        let a = agent(99);
        let table = RoutingTable::new(8, HashMap::new()).unwrap();
        assert_eq!(table.shard_for_agent(a), hash_agent_to_shard(a, 8));
    }

    #[test]
    fn routing_table_rejects_zero_shard_count() {
        let err = RoutingTable::new(0, HashMap::new()).unwrap_err();
        assert!(matches!(err, RoutingError::ZeroShardCount));
    }

    #[test]
    fn routing_table_rejects_override_out_of_range() {
        let a = agent(1);
        let mut overrides = HashMap::new();
        overrides.insert(a, 8); // shard_count = 4; 8 is out of range.
        let err = RoutingTable::new(4, overrides).unwrap_err();
        match err {
            RoutingError::OverrideOutOfRange {
                shard, shard_count, ..
            } => {
                assert_eq!(shard, 8);
                assert_eq!(shard_count, 4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn shard_count_accessor() {
        let table = RoutingTable::new(7, HashMap::new()).unwrap();
        assert_eq!(table.shard_count(), 7);
    }

    #[test]
    fn slot_packed_memory_id_routes_to_packed_shard() {
        // End-to-end: pack a MemoryId with shard 11, route it via
        // shard_for_memory, get 11 back.
        let id = MemoryId::pack(11, SlotIndex::from(42u64), 1);
        assert_eq!(shard_for_memory(id), 11);
    }

    /// Sub-task 9.12: the `RoutingTable` is published via `ArcSwap`
    /// in production (`Topology.routing`). This test confirms a
    /// follow-up `store()` is visible to a fresh `load_full()`
    /// without restarting the server (spec §10/05 §4).
    #[test]
    fn arc_swap_publishes_a_new_routing_table_atomically() {
        use arc_swap::ArcSwap;
        use std::sync::Arc;

        let initial = RoutingTable::new(2, HashMap::new()).unwrap();
        let swap = Arc::new(ArcSwap::from_pointee(initial));
        let pre = swap.load_full();
        assert_eq!(pre.shard_count(), 2);

        // A "cluster reconfiguration" doubles the shard count.
        let updated = RoutingTable::new(8, HashMap::new()).unwrap();
        swap.store(Arc::new(updated));

        let post = swap.load_full();
        assert_eq!(post.shard_count(), 8);

        // The previously-acquired `pre` Arc still observes the old
        // table — readers in flight see a coherent snapshot until
        // they drop their Arc. Spec §10/05 §5 "the reader's
        // load_full() returns an Arc; while held, the Arc keeps the
        // state alive".
        assert_eq!(pre.shard_count(), 2);
    }
}
