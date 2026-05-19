//! Hand-written entity SDK types — phase 16.8.1.
//!
//! Defines:
//!
//! - [`BrainEntityType`] — the trait phase 19's derive macro will
//!   auto-implement. The hand-written [`Person`] impl in this file is
//!   the v1 reference.
//! - [`Person`] — built-in entity type seeded by
//!   `brain-core::EntityType::PERSON_ID = 1`.
//! - [`PersonAttributes`] — typed accessor for Person's attribute
//!   slots (email / role / team), with rkyv round-trip to the wire's
//!   opaque `attributes_blob`.
//! - [`EntityHandle<T>`] — the value type returned by every read /
//!   mutate operation.

use brain_core::EntityId;
use brain_protocol::knowledge::EntityView;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

// ---------------------------------------------------------------------------
// BrainEntityType.
// ---------------------------------------------------------------------------

/// Mapping between a user Rust type (`Person`, `Project`, ...) and the
/// wire-level `entity_type_id` + attribute encoding.
///
/// Phase 16.8 implements this by hand for [`Person`]. Phase 19's
/// `#[derive(BrainEntity)]` derives it from the struct's fields and
/// schema-DSL declarations.
///
/// All implementors must be ZSTs in practice — the trait carries the
/// **type-level** metadata, the values flow through `Self::Attributes`.
pub trait BrainEntityType: Sized + Send + Sync + 'static {
    /// The wire `entity_type_id`. For built-in `Person` this is `1`
    /// (seeded by `MetadataDb::open`). User-declared types from
    /// phase 19's schema DSL get monotonically-increasing ids ≥ 2.
    const ENTITY_TYPE_ID: u32;

    /// Human-readable name. Used for diagnostics + phase 19 schema
    /// upload.
    const TYPE_NAME: &'static str;

    /// Typed attribute value type. Defined by the impl; phase 19's
    /// macro derives this from the struct's named fields.
    type Attributes: Clone + Default + Send + Sync + 'static;

    /// Encode `attrs` to the wire's opaque `attributes_blob`.
    ///
    /// Returns an empty `Vec` when `attrs == Self::Attributes::default()`
    /// — the substrate distinguishes "no attributes set" from "empty
    /// blob", but the wire treats them as equivalent for storage.
    fn encode_attributes(attrs: &Self::Attributes) -> Vec<u8>;

    /// Decode a wire `attributes_blob` to typed attributes. Empty
    /// input → `Default::default()`. Malformed input → also default
    /// (corrupted blobs surface as `EntityHandleFromViewError` from
    /// the caller, not via this trait).
    fn decode_attributes(blob: &[u8]) -> Self::Attributes;
}

// ---------------------------------------------------------------------------
// Person.
// ---------------------------------------------------------------------------

/// Built-in `Person` entity type. Spec §18/00 §"Entity types".
///
/// The struct is a zero-sized marker; instances are constructed at the
/// type level via `client.entity::<Person>()`. Per-entity values live
/// in [`PersonAttributes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Person;

/// Typed accessor for Person's attribute slots. Mirrors the spec
/// §18/00 "Person" example:
///
/// ```text
/// define entity_type Person {
///     attributes {
///         email:       text optional unique
///         role:        text optional
///         team:        text optional
///         timezone:    text optional
///     }
/// }
/// ```
///
/// `timezone` is included in the wire shape but not exposed on the
/// SDK helper struct — phase 19's macro auto-adds attribute fields
/// from the schema. For 16.8 the three most-commonly-needed slots are
/// surfaced; additional attributes round-trip through the wire blob
/// transparently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonAttributes {
    pub email: Option<String>,
    pub role: Option<String>,
    pub team: Option<String>,
    pub timezone: Option<String>,
}

impl BrainEntityType for Person {
    /// Person is `EntityType::PERSON_ID` from `brain-core`, seeded by
    /// `MetadataDb::open` (phase 16.1).
    const ENTITY_TYPE_ID: u32 = 1;
    const TYPE_NAME: &'static str = "Person";
    type Attributes = PersonAttributes;

    fn encode_attributes(attrs: &PersonAttributes) -> Vec<u8> {
        if attrs == &PersonAttributes::default() {
            return Vec::new();
        }
        let wire = PersonAttributesWire::from(attrs);
        // rkyv's archive infallibly produces a Vec<u8>.
        let bytes = rkyv::to_bytes::<_, 256>(&wire).expect(
            "invariant: PersonAttributesWire serialization cannot fail for in-memory bounded data",
        );
        bytes.into_vec()
    }

    fn decode_attributes(blob: &[u8]) -> PersonAttributes {
        if blob.is_empty() {
            return PersonAttributes::default();
        }
        // Malformed blob → default (corruption surfaces upstream as
        // an EntityHandleFromViewError once the caller turns the View
        // into a Handle).
        match rkyv::check_archived_root::<PersonAttributesWire>(blob) {
            Ok(archived) => {
                let wire: PersonAttributesWire = archived
                    .deserialize(&mut rkyv::Infallible)
                    .expect("invariant: rkyv Infallible deserialize");
                PersonAttributes::from(&wire)
            }
            Err(_) => PersonAttributes::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// PersonAttributesWire — rkyv-archived shape behind the wire blob.
// ---------------------------------------------------------------------------

/// rkyv-archived representation behind Person's `attributes_blob`.
///
/// Phase 19's schema DSL replaces this hand-written shape with a
/// schema-driven encoding. For now: a fixed Optional-string record
/// for the four well-known Person slots. Forward-compatible: adding a
/// new field at the bottom is a non-breaking rkyv-shape change as
/// long as the field type is `Option<T>` (rkyv 0.7's `Option<String>`
/// archives reliably with `check_bytes`).
#[derive(Archive, RkyvSerialize, RkyvDeserialize, Clone, Debug, PartialEq, Eq, Default)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
struct PersonAttributesWire {
    email: Option<String>,
    role: Option<String>,
    team: Option<String>,
    timezone: Option<String>,
}

impl From<&PersonAttributes> for PersonAttributesWire {
    fn from(p: &PersonAttributes) -> Self {
        Self {
            email: p.email.clone(),
            role: p.role.clone(),
            team: p.team.clone(),
            timezone: p.timezone.clone(),
        }
    }
}

impl From<&PersonAttributesWire> for PersonAttributes {
    fn from(p: &PersonAttributesWire) -> Self {
        Self {
            email: p.email.clone(),
            role: p.role.clone(),
            team: p.team.clone(),
            timezone: p.timezone.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// EntityHandle<T>.
// ---------------------------------------------------------------------------

/// Value returned by every read / mutate entity operation.
///
/// Generic over the entity type so callers get typed attributes
/// without re-decoding. Constructed via [`EntityHandle::from_view`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityHandle<T: BrainEntityType>
where
    T::Attributes: PartialEq + Eq,
{
    pub id: EntityId,
    pub canonical_name: String,
    pub normalized_name: String,
    pub aliases: Vec<String>,
    pub attributes: T::Attributes,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    /// `Some(_)` if this entity has been merged into another. Queries
    /// through this entity transparently follow the redirect on the
    /// server side; clients see this field populated.
    pub merged_into: Option<EntityId>,
    pub embedding_version: u32,
    pub flags: u32,
}

impl<T: BrainEntityType> EntityHandle<T>
where
    T::Attributes: PartialEq + Eq,
{
    /// `true` if the entity is tombstoned (`flags & TOMBSTONED`).
    /// Spec §18/05.
    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        self.flags & TOMBSTONED_FLAG != 0
    }

    /// `true` if the entity has been merged into another.
    #[must_use]
    pub fn is_merged(&self) -> bool {
        self.merged_into.is_some()
    }

    /// Build from a wire [`EntityView`]. Errors if the view's
    /// `entity_type_id` doesn't match `T::ENTITY_TYPE_ID` — the caller
    /// asked for the wrong type.
    pub fn from_view(view: EntityView) -> Result<Self, EntityHandleFromViewError> {
        if view.entity_type_id != T::ENTITY_TYPE_ID {
            return Err(EntityHandleFromViewError::TypeMismatch {
                expected: T::ENTITY_TYPE_ID,
                actual: view.entity_type_id,
            });
        }
        let attributes = T::decode_attributes(&view.attributes_blob);
        let merged_into = if view.merged_into == [0u8; 16] {
            None
        } else {
            Some(EntityId::from(view.merged_into))
        };
        Ok(Self {
            id: EntityId::from(view.entity_id),
            canonical_name: view.canonical_name,
            normalized_name: view.normalized_name,
            aliases: view.aliases,
            attributes,
            mention_count: view.mention_count,
            created_at_unix_nanos: view.created_at_unix_nanos,
            updated_at_unix_nanos: view.updated_at_unix_nanos,
            merged_into,
            embedding_version: view.embedding_version,
            flags: view.flags,
        })
    }
}

/// Bit 0 of `EntityView::flags` — entity is tombstoned. Mirrors
/// `brain-metadata::tables::knowledge::entity::flags::TOMBSTONED` so
/// the SDK doesn't need to depend on brain-metadata.
const TOMBSTONED_FLAG: u32 = 1 << 0;

/// Errors converting an [`EntityView`] into an [`EntityHandle<T>`].
#[derive(Debug, thiserror::Error)]
pub enum EntityHandleFromViewError {
    /// The server returned a view with a different `entity_type_id`
    /// than the requested generic `T`. Programmer error — usually the
    /// caller asked `client.entity::<Person>().get(id)` for an id that
    /// belongs to a different type.
    #[error("entity_type_id mismatch: expected {expected} (for type T), got {actual}")]
    TypeMismatch { expected: u32, actual: u32 },
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_view(entity_type_id: u32, attrs_blob: Vec<u8>) -> EntityView {
        EntityView {
            entity_id: [7u8; 16],
            entity_type_id,
            canonical_name: "Alice".into(),
            normalized_name: "alice".into(),
            aliases: vec!["A.".into()],
            attributes_blob: attrs_blob,
            mention_count: 3,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            updated_at_unix_nanos: 1_700_000_001_000_000_000,
            merged_into: [0u8; 16],
            embedding_version: 0,
            flags: 0,
        }
    }

    #[test]
    fn person_constants_match_built_in_id() {
        assert_eq!(Person::ENTITY_TYPE_ID, 1);
        assert_eq!(Person::TYPE_NAME, "Person");
    }

    #[test]
    fn default_attributes_encode_to_empty_blob() {
        let attrs = PersonAttributes::default();
        let blob = Person::encode_attributes(&attrs);
        assert!(blob.is_empty());
    }

    #[test]
    fn empty_blob_decodes_to_default_attributes() {
        let attrs = Person::decode_attributes(&[]);
        assert_eq!(attrs, PersonAttributes::default());
    }

    #[test]
    fn corrupt_blob_decodes_to_default() {
        let attrs = Person::decode_attributes(&[0xFFu8; 8]);
        assert_eq!(attrs, PersonAttributes::default());
    }

    #[test]
    fn attribute_round_trip_all_set() {
        let original = PersonAttributes {
            email: Some("alice@example.com".into()),
            role: Some("Engineer".into()),
            team: Some("Platform".into()),
            timezone: Some("America/New_York".into()),
        };
        let blob = Person::encode_attributes(&original);
        assert!(!blob.is_empty());
        let decoded = Person::decode_attributes(&blob);
        assert_eq!(decoded, original);
    }

    #[test]
    fn attribute_round_trip_partial() {
        let original = PersonAttributes {
            email: Some("alice@example.com".into()),
            role: None,
            team: Some("Platform".into()),
            timezone: None,
        };
        let blob = Person::encode_attributes(&original);
        let decoded = Person::decode_attributes(&blob);
        assert_eq!(decoded, original);
    }

    #[test]
    fn handle_from_view_happy_path() {
        let attrs = PersonAttributes {
            email: Some("a@b".into()),
            role: Some("Eng".into()),
            team: None,
            timezone: None,
        };
        let blob = Person::encode_attributes(&attrs);
        let view = sample_view(Person::ENTITY_TYPE_ID, blob);

        let handle: EntityHandle<Person> = EntityHandle::from_view(view).unwrap();
        assert_eq!(handle.id, EntityId::from([7u8; 16]));
        assert_eq!(handle.canonical_name, "Alice");
        assert_eq!(handle.normalized_name, "alice");
        assert_eq!(handle.aliases, vec!["A.".to_string()]);
        assert_eq!(handle.attributes, attrs);
        assert_eq!(handle.mention_count, 3);
        assert_eq!(handle.merged_into, None);
        assert!(!handle.is_tombstoned());
        assert!(!handle.is_merged());
    }

    #[test]
    fn handle_from_view_rejects_type_mismatch() {
        let view = sample_view(999, Vec::new());
        let err = EntityHandle::<Person>::from_view(view).unwrap_err();
        match err {
            EntityHandleFromViewError::TypeMismatch { expected, actual } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 999);
            }
        }
    }

    #[test]
    fn handle_from_view_reports_tombstoned() {
        let mut view = sample_view(Person::ENTITY_TYPE_ID, Vec::new());
        view.flags |= 1;
        let handle: EntityHandle<Person> = EntityHandle::from_view(view).unwrap();
        assert!(handle.is_tombstoned());
    }

    #[test]
    fn handle_from_view_reports_merged() {
        let mut view = sample_view(Person::ENTITY_TYPE_ID, Vec::new());
        view.merged_into = [9u8; 16];
        let handle: EntityHandle<Person> = EntityHandle::from_view(view).unwrap();
        assert!(handle.is_merged());
        assert_eq!(handle.merged_into, Some(EntityId::from([9u8; 16])));
    }
}
