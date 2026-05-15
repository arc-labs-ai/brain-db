//! Entity-op request payloads. Spec §28/00 entity table.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// `ENTITY_CREATE` (0x0130). Spec §28/00.
///
/// `entity_type_id` is the u32 raw form of the registry id (Person =
/// 1 in phase 16; user-declared types arrive with phase 19 schema DSL).
/// `attributes_blob` is opaque — phase 19 typed accessors. `aliases`
/// are stored verbatim, normalized inside the handler.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityCreateRequest {
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    pub request_id: WireUuid,
}

/// `ENTITY_GET` (0x0131).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityGetRequest {
    pub entity_id: WireUuid,
}

/// `ENTITY_UPDATE` (0x0132).
///
/// Carries the **full desired state** for the mutable fields. The
/// handler reads the current row, applies the delta, and writes back
/// inside one redb transaction (per `brain-metadata::entity_ops::entity_update`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityUpdateRequest {
    pub entity_id: WireUuid,
    /// New canonical_name (the handler treats unchanged-vs-rename
    /// using the same comparison as `entity_update`).
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub attributes_blob: Vec<u8>,
    pub request_id: WireUuid,
}

/// `ENTITY_RENAME` (0x0133).
///
/// When `move_to_alias = true` (default semantics per spec §18/02), the
/// old canonical_name is appended to the entity's aliases atomically
/// with the swap. The handler currently always moves to alias
/// (matching `brain-metadata::entity_ops::entity_rename`); the flag is
/// here for forward compat with a "no-trail" mode in later phases.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityRenameRequest {
    pub entity_id: WireUuid,
    pub new_canonical_name: String,
    pub move_to_alias: bool,
    pub request_id: WireUuid,
}

/// `ENTITY_MERGE` (0x0134). Spec §28/01 §7.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityMergeRequest {
    pub survivor: WireUuid,
    pub merged: WireUuid,
    pub confidence: f32,
    pub reason: String,
    pub request_id: WireUuid,
}

/// `ENTITY_UNMERGE` (0x0135). Spec §28/01 §8.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityUnmergeRequest {
    pub merged_entity: WireUuid,
    pub request_id: WireUuid,
}

/// `ENTITY_RESOLVE` (0x0136). Spec §28/01 §9.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityResolveRequest {
    pub candidate_name: String,
    pub context: String,
    /// `0` = no hint; otherwise an EntityTypeId.
    pub entity_type_hint: u32,
    pub allow_create: bool,
    pub request_id: WireUuid,
}

/// `ENTITY_LIST` (0x0137). Spec §28/01 §10.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
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

/// `ENTITY_TOMBSTONE` (0x0138). Spec §28/01 §11.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityTombstoneRequest {
    pub entity_id: WireUuid,
    pub reason: String,
    pub request_id: WireUuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RequestBody;
    use crate::response::ResponseBody;
    use crate::knowledge::{
        EntityCreateResponse, EntityGetResponse, EntityMergeResponse, EntityRenameResponse,
        EntityTombstoneResponse, EntityUnmergeResponse, EntityUpdateResponse, EntityView,
        ResolutionOutcomeWire,
    };
    use crate::opcode::Opcode;

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
    fn knowledge_opcode_byte_assignments() {
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
        assert!(Opcode::EntityCreateReq.is_knowledge());
        assert!(Opcode::EntityCreateReq.is_request());
        assert!(Opcode::EntityCreateResp.is_response());
        assert!(Opcode::EntityMergeReq.is_knowledge());
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
        use crate::knowledge::EntityResolveResponse;
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
                resolved_entity: if matches!(outcome, ResolutionOutcomeWire::Resolved | ResolutionOutcomeWire::Created) {
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
        use crate::knowledge::{EntityListItem, EntityListResponseFrame};
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

