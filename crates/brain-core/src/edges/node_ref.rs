//! `NodeRef` — a discriminated reference to either a substrate memory or
//! a opaque-body entity.
//!
//! Brain v1 stored substrate edges and opaque-body relations in
//! separate redb tables. Every node a substrate edge could touch was a
//! `MemoryId`; every node a typed relation could touch was an
//! `EntityId`. When mention edges (`Memory →(mentions)→ Entity`) arrive,
//! the two-table split cannot represent them without a third table. The
//! unified edge layer keys both endpoints as `NodeRef` so a single table
//! holds memory-to-memory, entity-to-entity, and memory-to-entity edges
//! without inventing a new vocabulary per node-kind combination.
//!
//! ## Byte layout (stable)
//!
//! ```text
//!   0      tag        u8   (0 = Memory, 1 = Entity)
//!   1..17  id_bytes   [u8; 16]
//! ```
//!
//! Total: 17 bytes. Sort order is `Memory(_) < Entity(_)` because the
//! tag byte sorts first under `[u8]` lexicographic order — this lets a
//! prefix scan `(NodeRef from, *, *)` keep all memory-anchored edges
//! contiguous for a given source, which the substrate graph retriever
//! relies on for cache locality.

use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::ids::MemoryId;

/// Reference to one node in the unified edge graph.
///
/// Either a substrate `MemoryId` or a opaque-body `EntityId`. The
/// discriminant tag is stable for the v1 storage format; future node
/// kinds (e.g. statement-as-node) widen the tag space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum NodeRef {
    Memory(MemoryId),
    Entity(EntityId),
}

impl NodeRef {
    /// Fixed encoded size: 1 tag byte + 16 id bytes.
    pub const BYTES: usize = 17;

    /// Tag byte (0 = Memory, 1 = Entity). The numbers are part of the
    /// on-disk format; do not renumber.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            NodeRef::Memory(_) => 0,
            NodeRef::Entity(_) => 1,
        }
    }

    /// The 16 id bytes of either variant, big-endian for `MemoryId`
    /// (its wire layout) and raw UUID bytes for `EntityId`.
    #[must_use]
    pub const fn id_bytes(self) -> [u8; 16] {
        match self {
            NodeRef::Memory(m) => m.to_be_bytes(),
            NodeRef::Entity(e) => e.to_bytes(),
        }
    }

    /// Encode as `[tag, id_bytes..]`.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; Self::BYTES] {
        let id = self.id_bytes();
        [
            self.tag(),
            id[0],
            id[1],
            id[2],
            id[3],
            id[4],
            id[5],
            id[6],
            id[7],
            id[8],
            id[9],
            id[10],
            id[11],
            id[12],
            id[13],
            id[14],
            id[15],
        ]
    }

    /// Decode a `NodeRef` from 17 bytes. Unknown tags are rejected so
    /// a v2 binary refuses to misread a hypothetical v3 node kind as
    /// a Memory or Entity.
    pub fn from_bytes(bytes: [u8; Self::BYTES]) -> Result<Self, NodeRefError> {
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[1..17]);
        match bytes[0] {
            0 => Ok(NodeRef::Memory(MemoryId::from_be_bytes(id))),
            1 => Ok(NodeRef::Entity(EntityId::from_bytes(id))),
            tag => Err(NodeRefError::UnknownTag(tag)),
        }
    }
}

/// Errors from decoding a [`NodeRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NodeRefError {
    #[error("unknown NodeRef tag {0}")]
    UnknownTag(u8),
}

impl std::fmt::Display for NodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Lowercase short prefix so logs and debug dumps stay readable
            // without needing to allocate a full id pretty-printer.
            NodeRef::Memory(m) => {
                write!(f, "mem:")?;
                for b in m.to_be_bytes() {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
            NodeRef::Entity(e) => {
                write!(f, "ent:")?;
                for b in e.to_bytes() {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_memory() -> MemoryId {
        MemoryId::pack(0x0102, 0x0304_0506_0708, 0x090A_0B0C)
    }

    fn sample_entity() -> EntityId {
        EntityId::from_bytes([
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
            0x1E, 0x1F,
        ])
    }

    #[test]
    fn node_ref_roundtrip_memory() {
        let nr = NodeRef::Memory(sample_memory());
        let bytes = nr.to_bytes();
        assert_eq!(bytes[0], 0);
        assert_eq!(&bytes[1..17], &sample_memory().to_be_bytes());
        assert_eq!(NodeRef::from_bytes(bytes).unwrap(), nr);
    }

    #[test]
    fn node_ref_unknown_tag_errors() {
        let mut bytes = [0u8; NodeRef::BYTES];
        bytes[0] = 2;
        assert_eq!(NodeRef::from_bytes(bytes), Err(NodeRefError::UnknownTag(2)));
        bytes[0] = 0xFF;
        assert_eq!(
            NodeRef::from_bytes(bytes),
            Err(NodeRefError::UnknownTag(0xFF))
        );
    }

    #[test]
    fn node_ref_display_format() {
        let m = NodeRef::Memory(sample_memory());
        let mut expected = String::from("mem:");
        for b in sample_memory().to_be_bytes() {
            expected.push_str(&format!("{b:02x}"));
        }
        assert_eq!(format!("{m}"), expected);

        let e = NodeRef::Entity(sample_entity());
        let mut expected = String::from("ent:");
        for b in sample_entity().to_bytes() {
            expected.push_str(&format!("{b:02x}"));
        }
        assert_eq!(format!("{e}"), expected);
    }

    proptest! {
        #[test]
        fn node_ref_roundtrip_arbitrary(
            tag in 0u8..=1,
            id in proptest::array::uniform16(any::<u8>()),
        ) {
            let nr = match tag {
                0 => NodeRef::Memory(MemoryId::from_be_bytes(id)),
                _ => NodeRef::Entity(EntityId::from_bytes(id)),
            };
            let bytes = nr.to_bytes();
            prop_assert_eq!(NodeRef::from_bytes(bytes).unwrap(), nr);
        }

        #[test]
        fn node_ref_bytewise_sort_matches_ord(
            tag_a in 0u8..=1,
            id_a in proptest::array::uniform16(any::<u8>()),
            tag_b in 0u8..=1,
            id_b in proptest::array::uniform16(any::<u8>()),
        ) {
            let a = match tag_a {
                0 => NodeRef::Memory(MemoryId::from_be_bytes(id_a)),
                _ => NodeRef::Entity(EntityId::from_bytes(id_a)),
            };
            let b = match tag_b {
                0 => NodeRef::Memory(MemoryId::from_be_bytes(id_b)),
                _ => NodeRef::Entity(EntityId::from_bytes(id_b)),
            };
            let bytes_a = a.to_bytes();
            let bytes_b = b.to_bytes();
            prop_assert_eq!(a.cmp(&b), bytes_a.cmp(&bytes_b));
        }
    }
}
