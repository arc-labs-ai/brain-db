//! Entity-op request payloads.

use crate::envelope::request::WireUuid;

/// `ENTITY_CREATE` (0x0130).
///
/// `entity_type_id` is the u32 raw form of the registry id. Built-in
/// types (Person = 1, etc.) ship pre-registered; user-declared types
/// arrive via the schema DSL. `attributes_blob` is opaque (typed
/// accessors are a follow-up). `aliases` are stored verbatim,
/// normalized inside the handler.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityCreateRequest {
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_GET` (0x0131).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityGetRequest {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
}

/// `ENTITY_UPDATE` (0x0132).
///
/// Carries the **full desired state** for the mutable fields. The
/// handler reads the current row, applies the delta, and writes back
/// inside one redb transaction (per `brain-metadata::entity_ops::entity_update`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityUpdateRequest {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
    /// New canonical_name (the handler treats unchanged-vs-rename
    /// using the same comparison as `entity_update`).
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_RENAME` (0x0133).
///
/// When `move_to_alias = true` (default semantics), the
/// old canonical_name is appended to the entity's aliases atomically
/// with the swap. The handler currently always moves to alias
/// (matching `brain-metadata::entity_ops::entity_rename`); the flag is
/// here for forward compat with a future "no-trail" mode.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityRenameRequest {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
    pub new_canonical_name: String,
    pub move_to_alias: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_MERGE` (0x0134).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityMergeRequest {
    #[serde(with = "serde_bytes")]
    pub survivor: WireUuid,
    #[serde(with = "serde_bytes")]
    pub merged: WireUuid,
    pub confidence: f32,
    pub reason: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_UNMERGE` (0x0135).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityUnmergeRequest {
    #[serde(with = "serde_bytes")]
    pub merged_entity: WireUuid,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_RESOLVE` (0x0136).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityResolveRequest {
    pub candidate_name: String,
    pub context: String,
    /// `0` = no hint; otherwise an EntityTypeId.
    pub entity_type_hint: u32,
    pub allow_create: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

/// `ENTITY_LIST` (0x0137).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityListRequest {
    /// `0` = no filter; otherwise an EntityTypeId.
    pub entity_type_id: u32,
    /// Empty = no filter.
    pub name_prefix: String,
    pub mention_count_min: u32,
    pub include_tombstoned: bool,
    pub include_merged: bool,
    /// 1..=1000.
    pub limit: u32,
    /// Empty on first page; opaque continuation token from a previous
    /// response's tail.
    pub cursor: Vec<u8>,
}

/// `ENTITY_TOMBSTONE` (0x0138).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityTombstoneRequest {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
    pub reason: String,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::opcode::Opcode;
    use crate::envelope::request::RequestBody;
    use crate::envelope::response::ResponseBody;

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_view() -> EntityView {
        EntityView {
            entity_id: sample_uuid(7),
            entity_type_id: 1,
            canonical_name: "Alice".into(),
            normalized_name: "alice".into(),
            aliases: vec!["A.".into()],
            attributes_blob: b"x".to_vec(),
            mention_count: 3,
            created_at_unix_nanos: 1_700_000_000_000_000_000,
            updated_at_unix_nanos: 1_700_000_001_000_000_000,
            merged_into: [0; 16],
            embedding_version: 1,
            flags: 0,
        }
    }

    fn req_round_trip(body: RequestBody) {
        let bytes = body.encode();
        let decoded = RequestBody::decode(body.opcode(), &bytes).expect("decode req");
        assert_eq!(decoded, body);
    }

    fn resp_round_trip(body: ResponseBody) {
        let bytes = body.encode();
        let decoded = ResponseBody::decode(body.opcode(), &bytes).expect("decode resp");
        assert_eq!(decoded, body);
    }

    #[test]
    fn entity_create_request_roundtrip() {
        req_round_trip(RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: 1,
            canonical_name: "Alice".into(),
            aliases: vec!["A.".into(), "Alyss".into()],
            attributes_blob: b"bio=astronaut".to_vec(),
            request_id: sample_uuid(1),
        }));
    }

    #[test]
    fn entity_get_request_roundtrip() {
        req_round_trip(RequestBody::EntityGet(EntityGetRequest {
            entity_id: sample_uuid(2),
        }));
    }

    #[test]
    fn entity_update_request_roundtrip() {
        req_round_trip(RequestBody::EntityUpdate(EntityUpdateRequest {
            entity_id: sample_uuid(3),
            canonical_name: "Alice Cooper".into(),
            aliases: vec![],
            attributes_blob: Vec::new(),
            request_id: sample_uuid(4),
        }));
    }

    #[test]
    fn entity_rename_request_roundtrip_both_flags() {
        for flag in [true, false] {
            req_round_trip(RequestBody::EntityRename(EntityRenameRequest {
                entity_id: sample_uuid(5),
                new_canonical_name: "Bob".into(),
                move_to_alias: flag,
                request_id: sample_uuid(6),
            }));
        }
    }

    #[test]
    fn entity_responses_roundtrip() {
        resp_round_trip(ResponseBody::EntityCreate(EntityCreateResponse {
            entity_id: sample_uuid(10),
        }));
        resp_round_trip(ResponseBody::EntityGet(EntityGetResponse {
            entity: sample_view(),
        }));
        resp_round_trip(ResponseBody::EntityUpdate(EntityUpdateResponse {
            entity: sample_view(),
        }));
        resp_round_trip(ResponseBody::EntityRename(EntityRenameResponse {
            entity: sample_view(),
        }));
    }

    #[test]
    fn graph_opcode_byte_assignments() {
        assert_eq!(Opcode::EntityCreateReq.as_u16(), 0x0130);
        assert_eq!(Opcode::EntityCreateResp.as_u16(), 0x01B0);
        assert_eq!(Opcode::EntityGetReq.as_u16(), 0x0131);
        assert_eq!(Opcode::EntityGetResp.as_u16(), 0x01B1);
        assert_eq!(Opcode::EntityUpdateReq.as_u16(), 0x0132);
        assert_eq!(Opcode::EntityUpdateResp.as_u16(), 0x01B2);
        assert_eq!(Opcode::EntityRenameReq.as_u16(), 0x0133);
        assert_eq!(Opcode::EntityRenameResp.as_u16(), 0x01B3);
        assert_eq!(Opcode::EntityMergeReq.as_u16(), 0x0134);
        assert_eq!(Opcode::EntityMergeResp.as_u16(), 0x01B4);
        assert_eq!(Opcode::EntityUnmergeReq.as_u16(), 0x0135);
        assert_eq!(Opcode::EntityUnmergeResp.as_u16(), 0x01B5);
        assert_eq!(Opcode::EntityResolveReq.as_u16(), 0x0136);
        assert_eq!(Opcode::EntityResolveResp.as_u16(), 0x01B6);
        assert_eq!(Opcode::EntityListReq.as_u16(), 0x0137);
        assert_eq!(Opcode::EntityListResp.as_u16(), 0x01B7);
        assert_eq!(Opcode::EntityTombstoneReq.as_u16(), 0x0138);
        assert_eq!(Opcode::EntityTombstoneResp.as_u16(), 0x01B8);

        assert_eq!(Opcode::EntityCreateReq.namespace(), 0x01);
        assert!(Opcode::EntityCreateReq.is_typed_graph());
        assert!(Opcode::EntityCreateReq.is_request());
        assert!(Opcode::EntityCreateResp.is_response());
        assert!(Opcode::EntityMergeReq.is_typed_graph());
        assert!(Opcode::EntityResolveReq.is_request());
        assert!(Opcode::EntityListResp.is_response());
    }

    // 16.7.3 — round-trip the new request/response shapes.

    #[test]
    fn entity_merge_request_roundtrip() {
        req_round_trip(RequestBody::EntityMerge(EntityMergeRequest {
            survivor: sample_uuid(10),
            merged: sample_uuid(11),
            confidence: 0.91,
            reason: "duplicate".into(),
            request_id: sample_uuid(12),
        }));
    }

    #[test]
    fn entity_unmerge_request_roundtrip() {
        req_round_trip(RequestBody::EntityUnmerge(EntityUnmergeRequest {
            merged_entity: sample_uuid(13),
            request_id: sample_uuid(14),
        }));
    }

    #[test]
    fn entity_resolve_request_roundtrip_both_create_flags() {
        for flag in [true, false] {
            req_round_trip(RequestBody::EntityResolve(EntityResolveRequest {
                candidate_name: "Priya".into(),
                context: "in the engineering team".into(),
                entity_type_hint: 1,
                allow_create: flag,
                request_id: sample_uuid(15),
            }));
        }
    }

    #[test]
    fn entity_list_request_roundtrip() {
        req_round_trip(RequestBody::EntityList(EntityListRequest {
            entity_type_id: 1,
            name_prefix: "pri".into(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 50,
            cursor: Vec::new(),
        }));
    }

    #[test]
    fn entity_tombstone_request_roundtrip() {
        req_round_trip(RequestBody::EntityTombstone(EntityTombstoneRequest {
            entity_id: sample_uuid(16),
            reason: "obsolete".into(),
            request_id: sample_uuid(17),
        }));
    }

    #[test]
    fn entity_merge_response_roundtrip() {
        resp_round_trip(ResponseBody::EntityMerge(EntityMergeResponse {
            audit_id: sample_uuid(20),
            grace_period_seconds: 7 * 24 * 60 * 60,
        }));
    }

    #[test]
    fn entity_unmerge_response_roundtrip() {
        resp_round_trip(ResponseBody::EntityUnmerge(EntityUnmergeResponse {
            restored_entity_id: sample_uuid(21),
        }));
    }

    #[test]
    fn entity_resolve_response_roundtrip_all_outcomes() {
        use crate::ops::entity::EntityResolveResponse;
        for outcome in [
            ResolutionOutcomeWire::Resolved,
            ResolutionOutcomeWire::Created,
            ResolutionOutcomeWire::Ambiguous,
            ResolutionOutcomeWire::NotFound,
        ] {
            resp_round_trip(ResponseBody::EntityResolve(EntityResolveResponse {
                outcome,
                tier: 1,
                confidence: 0.9,
                resolved_entity: if matches!(
                    outcome,
                    ResolutionOutcomeWire::Resolved | ResolutionOutcomeWire::Created
                ) {
                    sample_uuid(30)
                } else {
                    [0; 16]
                },
                candidate_ids: if matches!(outcome, ResolutionOutcomeWire::Ambiguous) {
                    vec![sample_uuid(31), sample_uuid(32)]
                } else {
                    Vec::new()
                },
                audit_id: if matches!(outcome, ResolutionOutcomeWire::Ambiguous) {
                    sample_uuid(33)
                } else {
                    [0; 16]
                },
            }));
        }
    }

    #[test]
    fn entity_list_response_roundtrip_intermediate_and_final() {
        // Intermediate frame: items present, not final.
        let intermediate = EntityListResponseFrame {
            items: vec![EntityListItem {
                entity: sample_view(),
            }],
            next_cursor: Vec::new(),
            cumulative_count: 1,
            is_final: false,
        };
        assert!(!intermediate.is_final());
        resp_round_trip(ResponseBody::EntityList(intermediate));

        // Final frame: may carry items + cursor + EOS-flag-equivalent body bit.
        let final_frame = EntityListResponseFrame {
            items: vec![EntityListItem {
                entity: sample_view(),
            }],
            next_cursor: vec![0xAB, 0xCD],
            cumulative_count: 42,
            is_final: true,
        };
        assert!(final_frame.is_final());
        resp_round_trip(ResponseBody::EntityList(final_frame));
    }

    #[test]
    fn entity_tombstone_response_roundtrip() {
        resp_round_trip(ResponseBody::EntityTombstone(EntityTombstoneResponse {
            tombstoned_at_unix_nanos: 1_700_000_000_000_000_000,
        }));
    }
}

// ============================================================
// Response payloads
// ============================================================

/// Read-side view of an entity. Mirrors `brain_core::Entity` but uses
/// wire-domain primitives (`[u8; 16]` for the entity id, `u32` for the
/// type id) so the wire types stay decoupled from `brain-core`
/// value types.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityView {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub normalized_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    /// `[0; 16]` when not merged (the wire form avoids `Option<[u8; 16]>`;
    /// consumers treat all-zero as None).
    #[serde(with = "serde_bytes")]
    pub merged_into: WireUuid,
    pub embedding_version: u32,
    pub flags: u32,
}

/// Reply to `ENTITY_CREATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityCreateResponse {
    #[serde(with = "serde_bytes")]
    pub entity_id: WireUuid,
}

/// Reply to `ENTITY_GET`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityGetResponse {
    pub entity: EntityView,
}

/// Reply to `ENTITY_UPDATE`. Carries the post-update view for the
/// client's convenience (avoids a follow-up `ENTITY_GET`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityUpdateResponse {
    pub entity: EntityView,
}

/// Reply to `ENTITY_RENAME`. Carries the post-rename view.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityRenameResponse {
    pub entity: EntityView,
}

/// Reply to `ENTITY_MERGE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityMergeResponse {
    /// MergeId (the audit row id), not an EntityId.
    #[serde(with = "serde_bytes")]
    pub audit_id: WireUuid,
    pub grace_period_seconds: u64,
}

/// Reply to `ENTITY_UNMERGE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityUnmergeResponse {
    #[serde(with = "serde_bytes")]
    pub restored_entity_id: WireUuid,
}

/// `ResolutionOutcome` wire enum — mirrors `brain_core::ResolutionOutcome`
/// but flattened to a u8 for wire simplicity.
///
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum ResolutionOutcomeWire {
    Resolved = 1,
    Created = 2,
    Ambiguous = 3,
    NotFound = 4,
}

/// Reply to `ENTITY_RESOLVE`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityResolveResponse {
    pub outcome: ResolutionOutcomeWire,
    /// Which tier resolved (1..=5; 0 if unresolved).
    pub tier: u8,
    pub confidence: f32,
    /// Populated when outcome == Resolved or Created (single id).
    /// `[0; 16]` for Ambiguous / NotFound.
    #[serde(with = "serde_bytes")]
    pub resolved_entity: WireUuid,
    /// Populated when outcome == Ambiguous; ranked by score.
    #[serde(with = "crate::codec::cbor::vec_byte_array16")]
    pub candidate_ids: Vec<WireUuid>,
    /// `[0; 16]` unless an ambiguity audit was written.
    #[serde(with = "serde_bytes")]
    pub audit_id: WireUuid,
}

/// One entity in an `ENTITY_LIST` response batch.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityListItem {
    pub entity: EntityView,
}

/// Response body for `ENTITY_LIST` (`0x01B7`). Carries one or more
/// `EntityListItem`s per frame; `is_final = true` on the last frame.
/// Mirrors `RecallResponseFrame`'s streaming shape.
///
/// v1 emits a single frame with `is_final = true` carrying the entire
/// snapshot. A later cut splits this into per-batch streaming.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityListResponseFrame {
    pub items: Vec<EntityListItem>,
    /// Empty on intermediate frames; populated only on the final
    /// frame. Empty `next_cursor` on the final frame means "exhausted";
    /// non-empty means "more pages available, resume with this".
    pub next_cursor: Vec<u8>,
    /// Cumulative count of items emitted across all frames in this
    /// stream so far.
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl EntityListResponseFrame {
    /// True for the final tail frame; false for per-batch intermediate
    /// frames. Mirrors the body-side `is_final` signal used by other
    /// streaming responses.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// Reply to `ENTITY_TOMBSTONE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}
