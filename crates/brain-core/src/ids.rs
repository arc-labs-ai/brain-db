//! Identifier types.
//!
//! Per `spec/02_data_model/03_identifiers.md`:
//!
//! - **`MemoryId`**: a packed `u128` encoding `(shard, slot, version)` per
//!   the spec's bit layout (§2.1). Lets a server route any operation to the
//!   correct shard without a lookup, and detects stale references after
//!   slot reclamation via the version.
//! - **`AgentId`**, **`RequestId`**, **`TxnId`**: 16-byte UUIDv7s.
//! - **`ContextId`**: 64-bit unsigned integer, server-assigned, agent-scoped.
//! - **Runtime `ShardId`**: 16-bit unsigned integer.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Runtime shard identifier (spec §02/03 §6.2). 16 bits → up to 65,535
/// shards per cluster (id 0 reserved).
pub type ShardId = u16;

/// Slot index within a shard's arena. Storage type is `u64`; the value
/// space is bounded to 48 bits because that's how many bits `MemoryId`
/// can carry (spec §02/03 §2.1).
pub type SlotIndex = u64;

/// Slot version, bumped on reclamation (spec §02/03 §2.1).
pub type SlotVersion = u32;

/// Maximum representable slot index: `(1 << 48) - 1`.
pub const MAX_SLOT_INDEX: u64 = (1u64 << 48) - 1;

/// Externally-supplied agent identifier (spec §02/03 §3).
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

/// Server-assigned context identifier (spec §02/03 §4). Agent-scoped
/// — two agents can both have `ContextId(1)` and they are unrelated.
/// `ContextId(0)` is reserved for the default context.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub struct ContextId(pub u64);

impl ContextId {
    /// The default context, automatically present for every agent.
    pub const DEFAULT: Self = Self(0);

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Client-supplied UUIDv7 used for write-side idempotency (spec §02/03 §5).
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

/// Transaction identifier (spec §03/07 §9). 16 bytes; UUIDv7 recommended.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct TxnId(pub Uuid);

impl TxnId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TxnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Routable, version-stamped reference to a stored memory.
///
/// On-the-wire layout (spec §02/03 §2.1, big-endian):
/// - bytes `0..2`   — `shard_id`  (`u16`)
/// - bytes `2..8`   — `slot_id`   (`u48`)
/// - bytes `8..12`  — `version`   (`u32`)
/// - bytes `12..16` — reserved    (must be zero in v1)
///
/// Internal storage is `u128` in big-endian-equivalent bit ordering so
/// `to_be_bytes` / `from_be_bytes` round-trip the spec's wire layout
/// directly.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MemoryId(u128);

impl MemoryId {
    /// The reserved "null" `MemoryId` (spec §02/03 §2.4). Used as a
    /// sentinel for "no memory"; never returned by an operation.
    pub const NULL: Self = Self(0);

    /// Pack a `(shard, slot, version)` triple into a `MemoryId`.
    ///
    /// `slot` is masked to 48 bits per spec §02/03 §2.1; supplying a
    /// larger value silently truncates. Callers should ensure
    /// `slot <= MAX_SLOT_INDEX`.
    #[must_use]
    pub const fn pack(shard: ShardId, slot: SlotIndex, version: SlotVersion) -> Self {
        let slot_48 = slot & MAX_SLOT_INDEX;
        // Layout (high → low):
        //   shard    : bits 112..128
        //   slot     : bits  64..112
        //   version  : bits  32..64
        //   reserved : bits   0..32   (zero)
        let raw = ((shard as u128) << 112) | ((slot_48 as u128) << 64) | ((version as u128) << 32);
        Self(raw)
    }

    #[must_use]
    pub const fn shard(self) -> ShardId {
        (self.0 >> 112) as ShardId
    }

    #[must_use]
    pub const fn slot(self) -> SlotIndex {
        ((self.0 >> 64) as u64) & MAX_SLOT_INDEX
    }

    #[must_use]
    pub const fn version(self) -> SlotVersion {
        (self.0 >> 32) as SlotVersion
    }

    /// The 32-bit reserved field. Must be zero in v1; future versions
    /// may use it.
    #[must_use]
    pub const fn reserved(self) -> u32 {
        self.0 as u32
    }

    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }

    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// On-the-wire bytes, big-endian per spec §02/03 §2.2.
    #[must_use]
    pub const fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// Inverse of [`Self::to_be_bytes`].
    #[must_use]
    pub const fn from_be_bytes(bytes: [u8; 16]) -> Self {
        Self(u128::from_be_bytes(bytes))
    }

    /// Whether this is the null sentinel (`Self::NULL`). Per spec
    /// §02/03 §2.4, no operation ever returns a null `MemoryId`.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

// ---------------------------------------------------------------------------
// Primitive-representation conversions.
//
// These are placed here (rather than in `brain-protocol`'s `convert` module)
// so the orphan rules cooperate: `MemoryId` / `ContextId` / `AgentId` / etc.
// are local to brain-core, and the "wire-domain" aliases in brain-protocol
// (`WireMemoryId = u128`, `WireUuid = [u8; 16]`, `WireContextId = u64`)
// are just type aliases for primitives — so impls written here against the
// primitives apply transparently in brain-protocol.
// ---------------------------------------------------------------------------

impl From<MemoryId> for u128 {
    #[inline]
    fn from(id: MemoryId) -> Self {
        id.raw()
    }
}

impl From<u128> for MemoryId {
    #[inline]
    fn from(raw: u128) -> Self {
        MemoryId::from_raw(raw)
    }
}

impl From<ContextId> for u64 {
    #[inline]
    fn from(c: ContextId) -> Self {
        c.0
    }
}

impl From<u64> for ContextId {
    #[inline]
    fn from(raw: u64) -> Self {
        ContextId(raw)
    }
}

impl From<AgentId> for [u8; 16] {
    #[inline]
    fn from(id: AgentId) -> Self {
        *id.0.as_bytes()
    }
}

impl From<[u8; 16]> for AgentId {
    #[inline]
    fn from(bytes: [u8; 16]) -> Self {
        AgentId(Uuid::from_bytes(bytes))
    }
}

impl From<RequestId> for [u8; 16] {
    #[inline]
    fn from(id: RequestId) -> Self {
        *id.0.as_bytes()
    }
}

impl From<[u8; 16]> for RequestId {
    #[inline]
    fn from(bytes: [u8; 16]) -> Self {
        RequestId(Uuid::from_bytes(bytes))
    }
}

impl From<TxnId> for [u8; 16] {
    #[inline]
    fn from(id: TxnId) -> Self {
        *id.0.as_bytes()
    }
}

impl From<[u8; 16]> for TxnId {
    #[inline]
    fn from(bytes: [u8; 16]) -> Self {
        TxnId(Uuid::from_bytes(bytes))
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
        assert_eq!(id.reserved(), 0);
    }

    #[test]
    fn shard_and_version_use_full_widths() {
        // ShardId is u16; SlotVersion is u32. Pack at the limits and
        // verify nothing wraps or collides with adjacent fields.
        let id = MemoryId::pack(u16::MAX, 0, u32::MAX);
        assert_eq!(id.shard(), u16::MAX);
        assert_eq!(id.slot(), 0);
        assert_eq!(id.version(), u32::MAX);
        assert_eq!(id.reserved(), 0);
    }

    #[test]
    fn slot_truncates_to_48_bits() {
        // Anything past 2^48 - 1 is masked.
        let id = MemoryId::pack(0, u64::MAX, 0);
        assert_eq!(id.slot(), MAX_SLOT_INDEX);
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

    #[test]
    fn null_is_zero() {
        assert!(MemoryId::NULL.is_null());
        assert_eq!(MemoryId::NULL.raw(), 0);
    }

    /// Spec §02/03 §2.1 byte layout: bytes 0..2 = shard (BE), bytes 2..8
    /// = slot (BE u48), bytes 8..12 = version (BE), bytes 12..16 = zero.
    #[test]
    fn byte_layout_matches_spec() {
        // shard = 0x0102, slot = 0x0304_0506_0708, version = 0x090A_0B0C
        let id = MemoryId::pack(0x0102, 0x0304_0506_0708, 0x090A_0B0C);
        let bytes = id.to_be_bytes();
        assert_eq!(
            bytes,
            [
                0x01, 0x02, // shard
                0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // slot (u48)
                0x09, 0x0A, 0x0B, 0x0C, // version
                0x00, 0x00, 0x00, 0x00, // reserved
            ]
        );
        // Round-trip through bytes.
        assert_eq!(MemoryId::from_be_bytes(bytes), id);
    }

    #[test]
    fn context_id_default_is_zero() {
        assert_eq!(ContextId::default(), ContextId::DEFAULT);
        assert_eq!(ContextId::DEFAULT.raw(), 0);
    }

    proptest! {
        #[test]
        fn pack_unpack_arbitrary(
            shard in 0u16..=u16::MAX,
            slot in 0u64..=MAX_SLOT_INDEX,
            version in 0u32..=u32::MAX,
        ) {
            let id = MemoryId::pack(shard, slot, version);
            prop_assert_eq!(id.shard(), shard);
            prop_assert_eq!(id.slot(), slot);
            prop_assert_eq!(id.version(), version);
            prop_assert_eq!(id.reserved(), 0);
        }

        #[test]
        fn byte_round_trip_arbitrary(
            shard in 0u16..=u16::MAX,
            slot in 0u64..=MAX_SLOT_INDEX,
            version in 0u32..=u32::MAX,
        ) {
            let id = MemoryId::pack(shard, slot, version);
            prop_assert_eq!(MemoryId::from_be_bytes(id.to_be_bytes()), id);
        }
    }
}
