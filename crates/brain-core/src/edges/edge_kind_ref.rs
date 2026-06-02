//! `EdgeKindRef` — a discriminated edge label spanning the substrate's
//! closed [`EdgeKind`] vocabulary, the reserved `Mentions` slot for
//! `Memory →(mentions)→ Entity` edges, and open-vocabulary typed
//! relations keyed by [`RelationTypeId`].
//!
//! The unified edge table keys every row by
//! `(NodeRef from, EdgeKindRef kind, NodeRef to, disambiguator)`. The
//! kind component is variable-length so a prefix scan on `(from, *, *)`
//! returns substrate edges (the smallest tag) first, then mentions,
//! then typed relations — no-schema deployments never pay decode
//! cost for the typed-relation suffix.
//!
//! ## Why `Typed` carries `RelationTypeId`, not `RelationId`
//!
//! Cardinality probes (the dominant write-side traversal) walk every
//! current edge of a given `RelationTypeId` from a single entity. Keying
//! the table by `RelationTypeId` makes that an O(matches) prefix scan;
//! keying by `RelationId` would force a full scan + sidecar lookup per
//! candidate. The per-relation identity moves into the `EdgeKey`'s
//! disambiguator suffix so multiple relations of the same type between
//! the same `(from, to)` pair don't collide on the key.
//!
//! ## Byte layout (stable)
//!
//! ```text
//!   tag 0 (Builtin) :  [0x00, edge_kind: u8]                        (2 bytes)
//!   tag 1 (Mentions):  [0x01]                                       (1 byte)
//!   tag 2 (Typed)   :  [0x02, relation_type_id: u32 LE]             (5 bytes)
//! ```
//!
//! Sort order: `Builtin < Mentions < Typed`. Tags `3..=255` are reserved.

use serde::{Deserialize, Serialize};

use crate::edges::edge::EdgeKind;
use crate::ids::RelationTypeId;

/// A reference to an edge label. The substrate uses [`EdgeKind`]; the
/// phase bodies uses typed relations keyed by [`RelationTypeId`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum EdgeKindRef {
    /// One of the eight substrate edge kinds.
    Builtin(EdgeKind),
    /// `Memory →(mentions)→ Entity`. Reserved for the extractor
    /// pipeline; the writer for this kind lands with the mention
    /// extractor.
    Mentions,
    /// Open-vocabulary typed relation. Carries the *type* id so the
    /// unified edge table prefix-scans by `(from, Typed(rt_id), *)`
    /// answer cardinality probes in O(matches). The per-relation id
    /// lives in the `EdgeKey`'s disambiguator field.
    Typed(RelationTypeId),
}

impl EdgeKindRef {
    /// Maximum encoded byte length across all variants. Used by callers
    /// pre-sizing a buffer.
    pub const MAX_BYTES: usize = 5;

    /// Encoded byte length: 1 (tag) + variant payload.
    #[must_use]
    pub fn encoded_len(self) -> usize {
        match self {
            EdgeKindRef::Builtin(_) => 2,
            EdgeKindRef::Mentions => 1,
            EdgeKindRef::Typed(_) => 5,
        }
    }

    /// Append the encoded bytes to `out`.
    pub fn encode_into(self, out: &mut Vec<u8>) {
        match self {
            EdgeKindRef::Builtin(k) => {
                out.push(0);
                out.push(k as u8);
            }
            EdgeKindRef::Mentions => {
                out.push(1);
            }
            EdgeKindRef::Typed(id) => {
                out.push(2);
                // Big-endian so the prefix-scan sort order matches the
                // numerical ordering callers expect when grouping by
                // relation type.
                out.extend_from_slice(&id.raw().to_be_bytes());
            }
        }
    }

    /// Encode into a fresh `Vec<u8>`. Convenience for tests and one-off
    /// callers.
    #[must_use]
    pub fn to_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }

    /// Decode the leading variable-length `EdgeKindRef` from `bytes` and
    /// return it together with the byte count consumed.
    pub fn decode_from(bytes: &[u8]) -> Result<(Self, usize), EdgeKindRefError> {
        let Some((&tag, rest)) = bytes.split_first() else {
            return Err(EdgeKindRefError::Short(0));
        };
        match tag {
            0 => {
                let Some(&kb) = rest.first() else {
                    return Err(EdgeKindRefError::Short(1));
                };
                let kind = edge_kind_from_u8(kb).ok_or(EdgeKindRefError::InvalidEdgeKind(kb))?;
                Ok((EdgeKindRef::Builtin(kind), 2))
            }
            1 => Ok((EdgeKindRef::Mentions, 1)),
            2 => {
                if rest.len() < 4 {
                    return Err(EdgeKindRefError::Short(rest.len() + 1));
                }
                let mut id = [0u8; 4];
                id.copy_from_slice(&rest[..4]);
                Ok((
                    EdgeKindRef::Typed(RelationTypeId::from(u32::from_be_bytes(id))),
                    5,
                ))
            }
            other => Err(EdgeKindRefError::UnknownTag(other)),
        }
    }
}

/// Errors from decoding an [`EdgeKindRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EdgeKindRefError {
    #[error("unknown EdgeKindRef tag {0}")]
    UnknownTag(u8),
    #[error("EdgeKindRef::Builtin: invalid EdgeKind {0}")]
    InvalidEdgeKind(u8),
    #[error("EdgeKindRef short read (got {0} bytes)")]
    Short(usize),
}

impl std::fmt::Display for EdgeKindRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EdgeKindRef::Builtin(k) => write!(f, "Builtin({k:?})"),
            EdgeKindRef::Mentions => write!(f, "Mentions"),
            EdgeKindRef::Typed(id) => write!(f, "Typed({})", id.raw()),
        }
    }
}

fn edge_kind_from_u8(b: u8) -> Option<EdgeKind> {
    Some(match b {
        0 => EdgeKind::Caused,
        1 => EdgeKind::FollowedBy,
        2 => EdgeKind::DerivedFrom,
        3 => EdgeKind::SimilarTo,
        4 => EdgeKind::Contradicts,
        5 => EdgeKind::Supports,
        6 => EdgeKind::References,
        7 => EdgeKind::PartOf,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ALL_BUILTIN: [EdgeKind; 8] = [
        EdgeKind::Caused,
        EdgeKind::FollowedBy,
        EdgeKind::DerivedFrom,
        EdgeKind::SimilarTo,
        EdgeKind::Contradicts,
        EdgeKind::Supports,
        EdgeKind::References,
        EdgeKind::PartOf,
    ];

    #[test]
    fn edge_kind_ref_builtin_roundtrip() {
        for k in ALL_BUILTIN {
            let kr = EdgeKindRef::Builtin(k);
            let bytes = kr.to_bytes();
            assert_eq!(bytes.len(), 2);
            assert_eq!(bytes[0], 0);
            assert_eq!(bytes[1], k as u8);
            let (decoded, consumed) = EdgeKindRef::decode_from(&bytes).unwrap();
            assert_eq!(decoded, kr);
            assert_eq!(consumed, 2);
        }
    }

    #[test]
    fn edge_kind_ref_mentions_roundtrip() {
        let kr = EdgeKindRef::Mentions;
        let bytes = kr.to_bytes();
        assert_eq!(bytes, vec![1]);
        let (decoded, consumed) = EdgeKindRef::decode_from(&bytes).unwrap();
        assert_eq!(decoded, kr);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn edge_kind_ref_typed_roundtrip() {
        for raw in [0u32, 1, 42, 0x1234_5678, u32::MAX] {
            let kr = EdgeKindRef::Typed(RelationTypeId::from(raw));
            let bytes = kr.to_bytes();
            assert_eq!(bytes.len(), 5);
            assert_eq!(bytes[0], 2);
            assert_eq!(&bytes[1..5], &raw.to_be_bytes());
            let (decoded, consumed) = EdgeKindRef::decode_from(&bytes).unwrap();
            assert_eq!(decoded, kr);
            assert_eq!(consumed, 5);
        }
    }

    #[test]
    fn edge_kind_ref_unknown_tag_errors() {
        for tag in [3u8, 0x10, 0xFF] {
            let bytes = [tag];
            assert_eq!(
                EdgeKindRef::decode_from(&bytes),
                Err(EdgeKindRefError::UnknownTag(tag))
            );
        }
    }

    #[test]
    fn edge_kind_ref_builtin_invalid_kind_errors() {
        let bytes = [0u8, 99];
        assert_eq!(
            EdgeKindRef::decode_from(&bytes),
            Err(EdgeKindRefError::InvalidEdgeKind(99))
        );
    }

    #[test]
    fn edge_kind_ref_typed_short_read_errors() {
        // Tag present, but the 4-byte type id is truncated.
        for n in 1..5 {
            let mut bytes = vec![2u8];
            bytes.extend(std::iter::repeat_n(0u8, n - 1));
            match EdgeKindRef::decode_from(&bytes) {
                Err(EdgeKindRefError::Short(_)) => {}
                other => panic!("expected Short for {n}-byte payload, got {other:?}"),
            }
        }
    }

    #[test]
    fn edge_kind_ref_empty_input_errors() {
        assert_eq!(
            EdgeKindRef::decode_from(&[]),
            Err(EdgeKindRefError::Short(0))
        );
    }

    #[test]
    fn edge_kind_ref_builtin_short_read_errors() {
        let bytes = [0u8];
        assert_eq!(
            EdgeKindRef::decode_from(&bytes),
            Err(EdgeKindRefError::Short(1))
        );
    }

    #[test]
    fn builtin_lt_mentions_lt_typed() {
        let b = EdgeKindRef::Builtin(EdgeKind::PartOf);
        let m = EdgeKindRef::Mentions;
        let t = EdgeKindRef::Typed(RelationTypeId::from(0));
        assert!(b < m);
        assert!(m < t);
        assert!(b < t);
    }

    #[test]
    fn encoded_bytes_sort_matches_ord_for_cross_variants() {
        let b = EdgeKindRef::Builtin(EdgeKind::PartOf).to_bytes();
        let m = EdgeKindRef::Mentions.to_bytes();
        let t = EdgeKindRef::Typed(RelationTypeId::from(0)).to_bytes();
        assert!(b.as_slice() < m.as_slice());
        assert!(m.as_slice() < t.as_slice());
    }

    proptest! {
        #[test]
        fn edge_kind_ref_roundtrip_arbitrary(
            tag in 0u8..=2,
            kind_byte in 0u8..=7,
            type_raw in any::<u32>(),
        ) {
            let kr = match tag {
                0 => EdgeKindRef::Builtin(edge_kind_from_u8(kind_byte).unwrap()),
                1 => EdgeKindRef::Mentions,
                _ => EdgeKindRef::Typed(RelationTypeId::from(type_raw)),
            };
            let bytes = kr.to_bytes();
            let (decoded, consumed) = EdgeKindRef::decode_from(&bytes).unwrap();
            prop_assert_eq!(decoded, kr);
            prop_assert_eq!(consumed, bytes.len());
        }

        #[test]
        fn edge_kind_ref_bytewise_sort_matches_ord(
            tag_a in 0u8..=2,
            tag_b in 0u8..=2,
            kind_a in 0u8..=7,
            kind_b in 0u8..=7,
            type_a in any::<u32>(),
            type_b in any::<u32>(),
        ) {
            let a = match tag_a {
                0 => EdgeKindRef::Builtin(edge_kind_from_u8(kind_a).unwrap()),
                1 => EdgeKindRef::Mentions,
                _ => EdgeKindRef::Typed(RelationTypeId::from(type_a)),
            };
            let b = match tag_b {
                0 => EdgeKindRef::Builtin(edge_kind_from_u8(kind_b).unwrap()),
                1 => EdgeKindRef::Mentions,
                _ => EdgeKindRef::Typed(RelationTypeId::from(type_b)),
            };
            let bytes_a = a.to_bytes();
            let bytes_b = b.to_bytes();
            prop_assert_eq!(a.cmp(&b), bytes_a.as_slice().cmp(bytes_b.as_slice()));
        }
    }
}
