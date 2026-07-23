//! Identifier types for every record Brain stores.
//!
//! Three flavors:
//!
//! - **`MemoryId`** — a packed `u128` encoding `(shard, slot, version)`.
//!   Lets a server route any operation to the correct shard without a
//!   lookup, and detects stale references after slot reclamation via the
//!   version.
//! - **UUIDv7** (16 bytes) for first-class records that need globally
//!   unique IDs with time-ordering: `SpaceId`, `RequestId`, `TxnId`,
//!   `EntityId`, `StatementId`, `RelationId`, `AuditId`, `MergeId`,
//!   `EvidenceOverflowId`.
//! - **u32 interned** for registry entries that are user-declared and
//!   table-local: `EntityTypeId`, `RelationTypeId`, `PredicateId`,
//!   `ExtractorId`. Small integers because typical deployments have
//!   tens-to-hundreds of each, not millions — and small keys keep
//!   secondary indexes compact.
//!
//! Other tiny aliases live here too: `ShardId` (`u16`), `ContextId`
//! (`u64`), `SlotIndex` (`u64`), `SlotVersion` (`u32`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Runtime shard identifier. 16 bits → up to 65,535
/// shards per cluster (id 0 reserved).
pub type ShardId = u16;

/// Slot index within a shard's arena. Storage type is `u64`; the value
/// space is bounded to 48 bits because that's how many bits `MemoryId`
/// can carry.
pub type SlotIndex = u64;

/// Slot version, bumped on reclamation.
pub type SlotVersion = u32;

/// Maximum representable slot index: `(1 << 48) - 1`.
pub const MAX_SLOT_INDEX: u64 = (1u64 << 48) - 1;

/// Externally-supplied space identifier.
///
/// Brain treats this as opaque bytes. Most clients use UUIDv7.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SpaceId(pub Uuid);

impl SpaceId {
    /// All-zero "anonymous / unauthenticated" space. Stable across
    /// calls — unlike [`Self::new`] which mints a fresh v7 UUID
    /// every time. Use this for:
    /// - Test fixtures that don't authenticate a connection
    /// - Server-side workers (consolidation, decay) acting without
    ///   a per-request caller
    /// - The dispatch shortcut that compares "is this an anonymous
    ///   request?" (so it knows whether to skip the per-request ctx
    ///   clone)
    pub const NIL: Self = Self(Uuid::nil());

    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

/// `Default::default()` returns [`SpaceId::NIL`] — the stable
/// anonymous sentinel, NOT a fresh UUID. Code that wanted a fresh
/// space id should call [`Self::new`] explicitly.
impl Default for SpaceId {
    fn default() -> Self {
        Self::NIL
    }
}

/// Server-assigned context identifier. Space-scoped
/// — two spaces can both have `ContextId(1)` and they are unrelated.
/// `ContextId(0)` is reserved for the default context.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub struct ContextId(pub u64);

impl ContextId {
    /// The default context, automatically present for every space.
    pub const DEFAULT: Self = Self(0);

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Client-supplied UUIDv7 used for write-side idempotency.
///
/// Idempotency entries are retained for a 24-hour TTL.
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

/// Transaction identifier. 16 bytes; UUIDv7 recommended.
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
/// On-the-wire layout (big-endian):
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
    /// The reserved "null" `MemoryId`. Used as a
    /// sentinel for "no memory"; never returned by an operation.
    pub const NULL: Self = Self(0);

    /// Pack a `(shard, slot, version)` triple into a `MemoryId`.
    ///
    /// `slot` is masked to 48 bits; supplying a
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

    /// On-the-wire bytes, big-endian.
    #[must_use]
    pub const fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// Inverse of [`Self::to_be_bytes`].
    #[must_use]
    pub const fn from_be_bytes(bytes: [u8; 16]) -> Self {
        Self(u128::from_be_bytes(bytes))
    }

    /// Whether this is the null sentinel (`Self::NULL`).
    /// No operation ever returns a null `MemoryId`.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

// ---------------------------------------------------------------------------
// Primitive-representation conversions.
//
// These are placed here (rather than in `brain-protocol`'s `convert` module)
// so the orphan rules cooperate: `MemoryId` / `ContextId` / `SpaceId` / etc.
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

impl From<SpaceId> for [u8; 16] {
    #[inline]
    fn from(id: SpaceId) -> Self {
        *id.0.as_bytes()
    }
}

impl From<[u8; 16]> for SpaceId {
    #[inline]
    fn from(bytes: [u8; 16]) -> Self {
        SpaceId(Uuid::from_bytes(bytes))
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

// ---------------------------------------------------------------------------
// Graph identifiers — UUIDv7-backed.
// ---------------------------------------------------------------------------

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(u: Uuid) -> Self {
                Self(u)
            }

            #[must_use]
            pub const fn to_bytes(self) -> [u8; 16] {
                *self.0.as_bytes()
            }

            #[must_use]
            pub const fn from_bytes(b: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(b))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<$name> for [u8; 16] {
            #[inline]
            fn from(id: $name) -> Self {
                id.to_bytes()
            }
        }

        impl From<[u8; 16]> for $name {
            #[inline]
            fn from(b: [u8; 16]) -> Self {
                Self::from_bytes(b)
            }
        }
    };
}

uuid_id! {
    /// Canonical entity identifier. UUIDv7; immutable across
    /// renames and attribute updates.
    EntityId
}

uuid_id! {
    /// Statement identifier. UUIDv7. A new `StatementId` is
    /// minted on every supersession; the chain is traversed via
    /// `chain_root` in `statement_chain`.
    StatementId
}

uuid_id! {
    /// Relation identifier. UUIDv7. A new `RelationId` is
    /// minted on every supersession.
    RelationId
}

uuid_id! {
    /// Audit record identifier. UUIDv7 because audits are
    /// append-only and time-ordered traversal is the dominant query
    /// shape.
    AuditId
}

uuid_id! {
    /// Entity-merge record identifier (— merge log).
    MergeId
}

uuid_id! {
    /// Evidence overflow row identifier. Points to a
    /// `Vec<MemoryId>` blob when a statement's inline evidence list
    /// outgrows the inline cap (8 by default).
    EvidenceOverflowId
}

// ---------------------------------------------------------------------------
// Graph identifiers — u32-interned.
// ---------------------------------------------------------------------------

macro_rules! u32_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        pub struct $name(pub u32);

        impl $name {
            #[must_use]
            pub const fn raw(self) -> u32 {
                self.0
            }
        }

        impl From<$name> for u32 {
            #[inline]
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl From<u32> for $name {
            #[inline]
            fn from(raw: u32) -> Self {
                Self(raw)
            }
        }
    };
}

u32_id! {
    /// Interned entity-type identifier. Stable within a
    /// deployment; assigned at schema upload.
    EntityTypeId
}

u32_id! {
    /// Interned relation-type identifier.
    RelationTypeId
}

u32_id! {
    /// Interned predicate identifier. A predicate is a
    /// namespaced string (e.g. `acme:reports_to`); the namespace+name
    /// pair is the primary key in the `predicates` table.
    PredicateId
}

u32_id! {
    /// Extractor identifier. Assigned at schema upload.
    ExtractorId
}

u32_id! {
    /// Interned namespace (tenant) identifier — the company-level data
    /// boundary. Every memory, entity, statement, and relation is owned
    /// by exactly one namespace; combined with the owning `SpaceId` it
    /// forms the `(namespace, space)` scope key under which all data is
    /// isolated. Distinct from the *schema* namespace prefix on a type
    /// name (`acme:Person`): a row owned by namespace `acme` may still
    /// reference a shared `brain:`-namespace type. The reserved system
    /// namespace `brain` is [`NamespaceId::SYSTEM`].
    NamespaceId
}

impl NamespaceId {
    /// The always-present `brain` system namespace. Reserved at id `0`;
    /// user namespaces are interned starting at `1`.
    pub const SYSTEM: NamespaceId = NamespaceId(0);

    /// Whether this is the reserved system namespace.
    #[must_use]
    pub const fn is_system(self) -> bool {
        self.0 == Self::SYSTEM.0
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

    /// byte layout: bytes 0..2 = shard (BE), bytes 2..8
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

    #[test]
    fn graph_uuid_round_trip_through_bytes() {
        let id = EntityId::new();
        let bytes = id.to_bytes();
        let back = EntityId::from_bytes(bytes);
        assert_eq!(id, back);
    }

    #[test]
    fn graph_u32_round_trip() {
        let id = EntityTypeId::from(42);
        assert_eq!(id.raw(), 42);
        let back: u32 = id.into();
        assert_eq!(back, 42);
    }

    #[test]
    fn graph_default_uuid_ids_are_unique() {
        let a = StatementId::new();
        let b = StatementId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn graph_default_u32_ids_are_zero() {
        assert_eq!(PredicateId::default().raw(), 0);
        assert_eq!(ExtractorId::default().raw(), 0);
    }
}
