//! Identifier types.
//!
//! Per `spec/02_data_model/03_identifiers.md`, IDs are:
//!
//! - **`MemoryId`**: a packed `u128` encoding `(shard, slot, version)`. Lets a
//!   server route any operation to the correct shard without a lookup, and
//!   detects stale references after slot reclamation via the version.
//! - **`AgentId`**, **`ContextId`**: opaque externally-supplied IDs. UUIDv7
//!   is the recommended generator.
//! - **`RequestId`**: client-supplied UUIDv7 used for idempotency.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 4-bit shard count cap is 16; we use a `u8` for headroom.
pub type ShardId = u8;

/// 56-bit slot index inside a shard's arena. Plenty for ~10^16 slots.
pub type SlotIndex = u64;

/// 16-bit slot version, bumped on reclamation.
pub type SlotVersion = u16;

/// Externally-supplied agent identifier.
///
/// Brain treats this as opaque bytes. Most clients use UUIDv7.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

/// Externally-supplied context identifier (a logical bucket within an agent).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ContextId(pub Uuid);

impl ContextId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ContextId {
    fn default() -> Self {
        Self::new()
    }
}

/// Client-supplied UUIDv7 used for write-side idempotency.
///
/// See `spec/09_cognitive_operations/` for idempotency semantics and the
/// 24-hour TTL.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);

impl RequestId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Routable, version-stamped reference to a stored memory.
///
/// Layout (low → high bits):
/// - `[0..56]`  — slot index (`u64`, top 8 bits unused)
/// - `[56..72]` — slot version
/// - `[72..80]` — shard id
/// - `[80..128]` — reserved
///
/// See `spec/02_data_model/03_identifiers.md`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MemoryId(u128);

impl MemoryId {
    /// Pack a `(shard, slot, version)` triple into a `MemoryId`.
    ///
    /// The slot is masked to 56 bits; supplying a larger value silently
    /// truncates. Callers should ensure `slot < 2^56`.
    #[must_use]
    pub const fn pack(shard: ShardId, slot: SlotIndex, version: SlotVersion) -> Self {
        let slot_56 = slot & ((1u64 << 56) - 1);
        let raw = (slot_56 as u128)
            | ((version as u128) << 56)
            | ((shard as u128) << 72);
        Self(raw)
    }

    #[must_use]
    pub const fn shard(self) -> ShardId {
        ((self.0 >> 72) & 0xFF) as ShardId
    }

    #[must_use]
    pub const fn slot(self) -> SlotIndex {
        (self.0 & ((1u128 << 56) - 1)) as SlotIndex
    }

    #[must_use]
    pub const fn version(self) -> SlotVersion {
        ((self.0 >> 56) & 0xFFFF) as SlotVersion
    }

    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }

    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let id = MemoryId::pack(7, 0x1234_5678, 42);
        assert_eq!(id.shard(), 7);
        assert_eq!(id.slot(), 0x1234_5678);
        assert_eq!(id.version(), 42);
    }

    #[test]
    fn distinct_components_produce_distinct_ids() {
        let a = MemoryId::pack(0, 1, 0);
        let b = MemoryId::pack(0, 2, 0);
        let c = MemoryId::pack(1, 1, 0);
        let d = MemoryId::pack(0, 1, 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    proptest! {
        #[test]
        fn pack_unpack_arbitrary(
            shard in 0u8..=255,
            slot in 0u64..(1u64 << 56),
            version in 0u16..=u16::MAX,
        ) {
            let id = MemoryId::pack(shard, slot, version);
            prop_assert_eq!(id.shard(), shard);
            prop_assert_eq!(id.slot(), slot);
            prop_assert_eq!(id.version(), version);
        }
    }
}
