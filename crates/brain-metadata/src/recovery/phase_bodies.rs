//! opaque-body WAL body schemas.
//!
//! `WalPayload::PhaseBody` carries an opaque `body: Vec<u8>` selected by
//! a [`WalRecordKind`](brain_storage::wal::kinds::WalRecordKind). This
//! module defines what those bytes are, per kind, plus the rkyv
//! encode/decode pair for each.
//!
//! The bodies live here (brain-metadata) rather than in brain-ops
//! because they reuse brain-metadata row types as their payload, and
//! recovery — which replays these bodies into redb — already depends on
//! brain-metadata. brain-core ids are serde-only (not rkyv), so every
//! body stores ids in their byte / int forms (`[u8; 16]`, `u32`), the
//! same shape the redb rows use.
//!
//! ## Body selection by kind
//!
//! | `WalRecordKind`      | body type             |
//! |----------------------|-----------------------|
//! | `EntityCreate`       | [`EntityMetadata`]    |
//! | `EntityUpdate`       | [`EntityUpdateBody`]  |
//! | `EntityMerge`        | [`EntityMergeBody`]   |
//! | `EntityTombstone`    | [`EntityTombstoneBody`] |
//! | `StatementCreate`    | [`StatementMetadata`] |
//! | `StatementSupersede` | [`StatementSupersedeBody`] |
//! | `StatementTombstone` | [`StatementTombstoneBody`] |
//! | `SchemaUpdate`       | [`SchemaUpdateBody`]  |
//!
//! Create ops reuse the existing rkyv row types directly because those
//! rows already carry every derived/allocated field and convert to/from
//! the brain-core domain type via `From` / projection helpers. Mutation
//! ops carry the arguments their corresponding helper call needs.
//!
//! Relations (`RelationCreate` / `RelationSupersede` / `RelationTombstone`,
//! `0x30..=0x32`) are NOT defined here — they ride first-class typed
//! `WalPayload` variants (`RelationLink` / `RelationSupersede` /
//! `RelationTombstone`). `Audit` (`0x50`) is not defined yet.

use crate::tables::entity::EntityMetadata;
use crate::tables::statement::StatementMetadata;

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// Failure decoding a opaque-body WAL body. A decode failure during
/// recovery is corruption: the bytes were CRC-verified at the framing
/// layer, so a validation miss here means the body schema and the
/// on-disk encoding disagree.
#[derive(thiserror::Error, Debug)]
pub enum PhaseBodyError {
    /// rkyv `check_bytes` validation rejected the archived root, or the
    /// subsequent deserialize failed.
    #[error("typed-graph body failed rkyv validation: {0}")]
    Decode(String),
}

// ---------------------------------------------------------------------------
// Create bodies — alias the existing row types.
// ---------------------------------------------------------------------------

/// `EntityCreate` (0x10) body: the full entity row. Recovery replays it
/// via `entity_put(Entity::from(&meta))`.
pub type EntityCreateBody = EntityMetadata;

/// `StatementCreate` (0x20) body: the statement row plus the schemaless
/// intern hint.
///
/// The row reuses [`StatementMetadata`]'s rkyv encoding (which already
/// carries subject / object / evidence / the four timestamps). The
/// predicate is special: on the schemaless write path the predicate id is
/// not resolved until the apply wtxn (which opens *after* the WAL append),
/// so `meta.predicate_id` holds the pre-intern placeholder and
/// `predicate_intern_hint` is `Some((namespace, name))`. Recovery
/// re-interns — deterministic because replay is LSN-ordered and
/// `predicate_intern_or_get` is idempotent — overrides the predicate, and
/// stamps `IMPLICIT_PREDICATE`, mirroring the live apply path. `None`
/// means the predicate in `meta` is authoritative (strict path).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct StatementCreateBody {
    pub meta: StatementMetadata,
    pub predicate_intern_hint: Option<(String, String)>,
}

// ---------------------------------------------------------------------------
// Mutation bodies.
// ---------------------------------------------------------------------------

/// `EntityUpdate` (0x11) body. Mirrors the `entity_update` helper's
/// post-update state inputs: the new canonical name, the full alias
/// list, the opaque attributes blob, and the update timestamp.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityUpdateBody {
    pub id: [u8; 16],
    pub canonical_name: String,
    pub aliases: Vec<String>,
    /// rkyv-encoded `BTreeMap<String, Value>` blob, carried opaquely.
    pub attributes_blob: Vec<u8>,
    pub at_unix_nanos: u64,
}

/// `EntityTombstone` (0x13) body. Mirrors the `entity_tombstone`
/// helper's inputs.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityTombstoneBody {
    pub id: [u8; 16],
    pub at_unix_nanos: u64,
}

/// `EntityRename` (0x14) body. Mirrors `entity_rename`'s inputs: the
/// rename phase carries only the new canonical name; aliases and
/// attributes are untouched (entity_rename applies the alias-trail policy
/// itself, moving the old canonical into aliases).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityRenameBody {
    pub id: [u8; 16],
    pub new_canonical_name: String,
    pub at_unix_nanos: u64,
}

/// `EntityUnmerge` (0x15) body. Mirrors `unmerge_entity`'s inputs.
/// `actor_kind`: `0` = System (`actor_agent` is `[0; 16]`), `1` = Agent.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityUnmergeBody {
    pub merged: [u8; 16],
    pub actor_kind: u8,
    pub actor_agent: [u8; 16],
    pub at_unix_nanos: u64,
}

/// `EntityMerge` (0x12) body. Mirrors the `merge_entity` helper's
/// inputs. `actor_kind` encodes `MergeActor`: `0` = `System` (and
/// `actor_agent` is `[0; 16]`), `1` = `Agent(actor_agent)`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityMergeBody {
    pub source: [u8; 16],
    pub target: [u8; 16],
    pub retain_aliases: bool,
    pub retain_attributes: bool,
    pub at_unix_nanos: u64,
    pub confidence: f32,
    pub reason: String,
    /// `0` = System, `1` = Agent. See [`MergeActor`](crate::entity::merge::MergeActor).
    pub actor_kind: u8,
    /// `[0; 16]` when `actor_kind == 0` (System).
    pub actor_agent: [u8; 16],
    pub grace_seconds: u64,
}

/// `StatementSupersede` (0x21) body. The new statement row plus the id
/// of the statement it replaces and the supersession timestamp.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct StatementSupersedeBody {
    pub old_id: [u8; 16],
    pub new: StatementMetadata,
    pub at_unix_nanos: u64,
}

/// `StatementTombstone` (0x22) body. The statement id, the tombstone
/// reason byte (see `tables::statement::tombstone_reason`), and the
/// tombstone timestamp.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct StatementTombstoneBody {
    pub id: [u8; 16],
    pub reason: u8,
    pub at_unix_nanos: u64,
}

/// `SchemaUpdate` (0x40) body. The schema DSL source text and its
/// upload timestamp. Recovery re-parses, validates, and re-applies the
/// declaration the same way the live `SCHEMA_UPLOAD` apply path does —
/// storing the source rather than a pre-parsed form keeps replay
/// authoritative against the parser that the running binary ships.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SchemaUpdateBody {
    /// Namespace + version this upload assigned. Carried so recovery can
    /// skip re-applying an already-present (namespace, version) — without
    /// it, `schema_upload`'s auto-increment would mint a duplicate on
    /// re-replay.
    pub namespace: String,
    pub version: u32,
    /// Schema DSL source text, as UTF-8 bytes. Recovery re-parses +
    /// re-validates + re-uploads (the version it recomputes matches
    /// `version` because replay reconstructs schema state in LSN order).
    pub blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

/// Extractor enable/disable toggle. There is no dedicated
/// `WalRecordKind` for extractor toggles — the only opaque-body kind
/// that could carry this is `Audit` (0x50). This body is defined so the
/// schema is ready, but wiring it to a kind is deferred to the work that
/// decides whether toggles ride `Audit` or get their own kind.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractorToggleBody {
    pub id: u32,
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Encode / decode.
// ---------------------------------------------------------------------------

/// rkyv scratch-space size for body serialization. Bodies are small
/// (a row plus a handful of scalars); 1 KiB covers the common case
/// without a heap fallback. rkyv grows the scratch on its own if a body
/// (e.g. a large alias list or attributes blob) exceeds this.
const SCRATCH: usize = 1024;

/// Encode any typed-graph body. rkyv serialization of an owned, fully
/// constructed value is infallible.
macro_rules! body_codec {
    ($encode:ident, $decode:ident, $ty:ty) => {
        #[doc = concat!("Encode a [`", stringify!($ty), "`] to its WAL body bytes.")]
        #[must_use]
        pub fn $encode(body: &$ty) -> Vec<u8> {
            rkyv::to_bytes::<_, SCRATCH>(body)
                .expect("invariant: typed-graph body rkyv encode is infallible")
                .into_vec()
        }

        #[doc = concat!("Decode a [`", stringify!($ty), "`] from WAL body bytes.")]
        ///
        /// # Errors
        ///
        /// Returns [`PhaseBodyError::Decode`] if the bytes fail
        /// rkyv `check_bytes` validation or the deserialize fails.
        pub fn $decode(bytes: &[u8]) -> Result<$ty, PhaseBodyError> {
            use rkyv::Deserialize;
            // rkyv's `check_archived_root` resolves the archived root from
            // the *end* of the buffer and follows relative pointers that
            // assume the buffer is aligned to the archive's alignment. The
            // encode side returns an `AlignedVec`, but a WAL record body is
            // a `&[u8]` slice at an arbitrary offset inside the larger
            // record buffer — almost never aligned. Validating that slice
            // directly trips "pointer out of bounds". Copy into an
            // `AlignedVec` so the relative-pointer arithmetic is sound.
            let mut aligned = rkyv::AlignedVec::with_capacity(bytes.len());
            aligned.extend_from_slice(bytes);
            let archived = rkyv::check_archived_root::<$ty>(&aligned)
                .map_err(|e| PhaseBodyError::Decode(e.to_string()))?;
            archived
                .deserialize(&mut rkyv::Infallible)
                .map_err(|e| PhaseBodyError::Decode(format!("{e:?}")))
        }
    };
}

body_codec!(encode_entity_create, decode_entity_create, EntityCreateBody);
body_codec!(encode_entity_update, decode_entity_update, EntityUpdateBody);
body_codec!(encode_entity_merge, decode_entity_merge, EntityMergeBody);
body_codec!(
    encode_entity_tombstone,
    decode_entity_tombstone,
    EntityTombstoneBody
);
body_codec!(encode_entity_rename, decode_entity_rename, EntityRenameBody);
body_codec!(
    encode_entity_unmerge,
    decode_entity_unmerge,
    EntityUnmergeBody
);
body_codec!(
    encode_statement_create,
    decode_statement_create,
    StatementCreateBody
);
body_codec!(
    encode_statement_supersede,
    decode_statement_supersede,
    StatementSupersedeBody
);
body_codec!(
    encode_statement_tombstone,
    decode_statement_tombstone,
    StatementTombstoneBody
);
body_codec!(encode_schema_update, decode_schema_update, SchemaUpdateBody);
body_codec!(
    encode_extractor_toggle,
    decode_extractor_toggle,
    ExtractorToggleBody
);

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::entity::EntityMetadata;
    use crate::tables::statement::{metadata_from_statement, tombstone_reason};
    use brain_core::{
        EntityId, EntityTypeId, EvidenceRef, ExtractorId, PredicateId, Statement, StatementKind,
        StatementObject, SubjectRef,
    };
    use smallvec::SmallVec;

    fn sample_entity_metadata() -> EntityMetadata {
        let mut m = EntityMetadata::new_active(
            EntityId::new(),
            EntityTypeId::from(1),
            "Priya Patel".into(),
            "priya patel".into(),
            1_700_000_000_000_000_000,
        );
        m.add_alias("priya".into());
        m.add_alias("p. patel".into());
        m.attributes_blob = vec![0xDE, 0xAD, 0xBE, 0xEF];
        m.mention_count = 9;
        m
    }

    fn sample_statement_metadata() -> StatementMetadata {
        let s = Statement::new_root(
            brain_core::StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(EntityId::new()),
            PredicateId::from(7),
            StatementObject::Value(brain_core::StatementValue::Text("blue".into())),
            0.88,
            EvidenceRef::Inline(Box::new(SmallVec::new())),
            ExtractorId::from(3),
            1_700_000_000_000_000_000,
            1,
        );
        metadata_from_statement(&s)
    }

    #[test]
    fn entity_create_body_round_trips() {
        let body = sample_entity_metadata();
        let bytes = encode_entity_create(&body);
        let got = decode_entity_create(&bytes).unwrap();
        assert_eq!(got, body);
        assert_eq!(got.aliases.len(), 2);
        assert_eq!(got.attributes_blob, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn entity_update_body_round_trips() {
        let body = EntityUpdateBody {
            id: EntityId::new().to_bytes(),
            canonical_name: "Priya P. Patel".into(),
            aliases: vec!["priya".into(), "p patel".into(), "priya p".into()],
            attributes_blob: vec![1, 2, 3, 4, 5],
            at_unix_nanos: 1_700_000_000_000_000_111,
        };
        let bytes = encode_entity_update(&body);
        let got = decode_entity_update(&bytes).unwrap();
        assert_eq!(got, body);
        assert_eq!(got.aliases.len(), 3);
    }

    #[test]
    fn entity_tombstone_body_round_trips() {
        let body = EntityTombstoneBody {
            id: EntityId::new().to_bytes(),
            at_unix_nanos: 1_700_000_000_000_000_222,
        };
        let bytes = encode_entity_tombstone(&body);
        let got = decode_entity_tombstone(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn entity_rename_body_round_trips() {
        let body = EntityRenameBody {
            id: EntityId::new().to_bytes(),
            new_canonical_name: "Priya P. Patel".into(),
            at_unix_nanos: 1_700_000_000_000_000_321,
        };
        let bytes = encode_entity_rename(&body);
        let got = decode_entity_rename(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn entity_unmerge_body_round_trips() {
        let body = EntityUnmergeBody {
            merged: EntityId::new().to_bytes(),
            actor_kind: 1,
            actor_agent: [3u8; 16],
            at_unix_nanos: 1_700_000_000_000_000_456,
        };
        let bytes = encode_entity_unmerge(&body);
        let got = decode_entity_unmerge(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn entity_merge_body_round_trips_agent_actor() {
        let body = EntityMergeBody {
            source: EntityId::new().to_bytes(),
            target: EntityId::new().to_bytes(),
            retain_aliases: true,
            retain_attributes: false,
            at_unix_nanos: 1_700_000_000_000_000_333,
            confidence: 0.95,
            reason: "duplicate detected by resolver".into(),
            actor_kind: 1,
            actor_agent: [7u8; 16],
            grace_seconds: 7 * 24 * 60 * 60,
        };
        let bytes = encode_entity_merge(&body);
        let got = decode_entity_merge(&bytes).unwrap();
        assert_eq!(got, body);
        assert!(got.retain_aliases);
        assert!(!got.retain_attributes);
    }

    #[test]
    fn entity_merge_body_round_trips_system_actor() {
        let body = EntityMergeBody {
            source: EntityId::new().to_bytes(),
            target: EntityId::new().to_bytes(),
            retain_aliases: false,
            retain_attributes: true,
            at_unix_nanos: 1_700_000_000_000_000_444,
            confidence: 0.7,
            reason: String::new(),
            actor_kind: 0,
            actor_agent: [0u8; 16],
            grace_seconds: 0,
        };
        let bytes = encode_entity_merge(&body);
        let got = decode_entity_merge(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn statement_create_body_round_trips() {
        let body = StatementCreateBody {
            meta: sample_statement_metadata(),
            predicate_intern_hint: Some(("brain".into(), "likes".into())),
        };
        let bytes = encode_statement_create(&body);
        let got = decode_statement_create(&bytes).unwrap();
        assert_eq!(got, body);
        assert!(got.predicate_intern_hint.is_some());
    }

    #[test]
    fn statement_supersede_body_round_trips() {
        let body = StatementSupersedeBody {
            old_id: brain_core::StatementId::new().to_bytes(),
            new: sample_statement_metadata(),
            at_unix_nanos: 1_700_000_000_000_000_555,
        };
        let bytes = encode_statement_supersede(&body);
        let got = decode_statement_supersede(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn statement_tombstone_body_round_trips() {
        let body = StatementTombstoneBody {
            id: brain_core::StatementId::new().to_bytes(),
            reason: tombstone_reason::USER_REQUEST,
            at_unix_nanos: 1_700_000_000_000_000_666,
        };
        let bytes = encode_statement_tombstone(&body);
        let got = decode_statement_tombstone(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn schema_update_body_round_trips() {
        let body = SchemaUpdateBody {
            namespace: "acme".into(),
            version: 3,
            blob: b"entity Person { name: text }".to_vec(),
            created_at_unix_nanos: 1_700_000_000_000_000_777,
        };
        let bytes = encode_schema_update(&body);
        let got = decode_schema_update(&bytes).unwrap();
        assert_eq!(got, body);
        assert_eq!(got.blob, b"entity Person { name: text }".to_vec());
        assert_eq!(got.version, 3);
    }

    #[test]
    fn extractor_toggle_body_round_trips() {
        let body = ExtractorToggleBody {
            id: 42,
            enabled: true,
        };
        let bytes = encode_extractor_toggle(&body);
        let got = decode_extractor_toggle(&bytes).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn decode_rejects_garbage_bytes() {
        let err = decode_entity_tombstone(&[0xFF, 0x00, 0x13, 0x37]);
        assert!(matches!(err, Err(PhaseBodyError::Decode(_))));
    }

    /// Regression: during recovery the body is a `&[u8]` slice carved out
    /// of a larger WAL record buffer, so it lands on an arbitrary
    /// (almost-never-aligned) address. Decoding such a slice directly used
    /// to trip rkyv's "pointer out of bounds" because `check_archived_root`
    /// assumes the buffer is aligned to the archive's alignment. The decode
    /// now copies into an `AlignedVec` first. This test reproduces the
    /// recovery layout by placing each encoded body at a deliberately odd
    /// offset inside a backing buffer and decoding from the misaligned
    /// sub-slice.
    fn decode_from_misaligned<T, F>(bytes: &[u8], decode: F) -> T
    where
        F: Fn(&[u8]) -> Result<T, PhaseBodyError>,
    {
        // Prefix with 1 byte to force an odd start offset, then decode the
        // tail sub-slice — the same shape a WAL body slice has.
        let mut backing = Vec::with_capacity(bytes.len() + 1);
        backing.push(0xAB);
        backing.extend_from_slice(bytes);
        decode(&backing[1..]).expect("misaligned decode must succeed")
    }

    #[test]
    fn entity_create_body_decodes_from_misaligned_slice() {
        let body = sample_entity_metadata();
        let bytes = encode_entity_create(&body);
        let got = decode_from_misaligned(&bytes, decode_entity_create);
        assert_eq!(got, body);
    }

    #[test]
    fn statement_create_body_decodes_from_misaligned_slice() {
        let body = StatementCreateBody {
            meta: sample_statement_metadata(),
            predicate_intern_hint: Some(("brain".into(), "likes".into())),
        };
        let bytes = encode_statement_create(&body);
        let got = decode_from_misaligned(&bytes, decode_statement_create);
        assert_eq!(got, body);
    }

    #[test]
    fn entity_merge_body_decodes_from_misaligned_slice() {
        let body = EntityMergeBody {
            source: EntityId::new().to_bytes(),
            target: EntityId::new().to_bytes(),
            retain_aliases: true,
            retain_attributes: false,
            at_unix_nanos: 1_700_000_000_000_000_333,
            confidence: 0.95,
            reason: "duplicate detected by resolver".into(),
            actor_kind: 1,
            actor_agent: [7u8; 16],
            grace_seconds: 7 * 24 * 60 * 60,
        };
        let bytes = encode_entity_merge(&body);
        let got = decode_from_misaligned(&bytes, decode_entity_merge);
        assert_eq!(got, body);
    }

    #[test]
    fn schema_update_body_decodes_from_misaligned_slice() {
        let body = SchemaUpdateBody {
            namespace: "acme".into(),
            version: 3,
            blob: b"entity Person { name: text }".to_vec(),
            created_at_unix_nanos: 1_700_000_000_000_000_777,
        };
        let bytes = encode_schema_update(&body);
        let got = decode_from_misaligned(&bytes, decode_schema_update);
        assert_eq!(got, body);
    }
}
