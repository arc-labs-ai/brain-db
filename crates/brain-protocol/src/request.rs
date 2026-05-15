//! Request-frame payload codecs.
//!
//! One variant of [`RequestBody`] per server-bound opcode in spec §03/07.
//! Structured fields are encoded with [rkyv] 0.7; raw vector blobs (per
//! `ENCODE_VECTOR_DIRECT_REQ` and `RECALL_REQ` with a pre-supplied cue
//! vector) live in the trailing raw section of the payload and are
//! composed at the [`crate::Frame`] layer — they are *not* part of the
//! rkyv-encoded bytes this module produces.
//!
//! ## Wire-domain types
//!
//! Each request struct uses raw representations (`u128` for `MemoryId`,
//! `[u8; 16]` for UUID-shaped IDs, `u8`-mapped enums) so the
//! `rkyv::Archive` derive can fire without coupling `brain-core` value
//! types to rkyv. Conversion between these wire types and `brain_core`
//! domain types is the operation handler's responsibility (later phases).
//!
//! [rkyv]: https://docs.rs/rkyv/0.7

// `PlanState` and `ObservationInput` use `By*` variant naming that mirrors
// the spec's discriminator phrasing. rkyv's `Archive` derive generates a
// parallel `ArchivedPlanState` whose variant names are inherited; clippy
// 1.95+ flags both the source and the macro-generated copy and the
// per-item `#[allow]` doesn't always reach the macro expansion path. The
// module-level allow covers both without spreading attribute noise.
#![allow(clippy::enum_variant_names)]

use crate::error::ProtocolError;
use crate::handshake::{AuthPayload, HelloPayload};
use crate::opcode::Opcode;
use crate::rkyv_codec::{from_rkyv_bytes, to_rkyv_bytes};

// ---------------------------------------------------------------------------
// Helper aliases for spec-domain primitive types as carried on the wire.
// ---------------------------------------------------------------------------

/// 16-byte UUID-shaped identifier (`AgentId`, `RequestId`, `TxnId`).
/// Matches the on-the-wire byte layout described in spec §07.
pub type WireUuid = [u8; 16];

/// Wire-side `ContextId` (spec §02/03 §4 + §8 — 8 bytes / `u64`).
pub type WireContextId = u64;

/// Packed `MemoryId` per spec §02/03 §2.1 (shard 16 + slot 48 +
/// version 32 + reserved 32, all rolled into a `u128`).
pub type WireMemoryId = u128;

// Per-op-family request payload structs live in `requests/`. Re-exported
// here so external callers continue to address them as
// `brain_protocol::request::EncodeRequest` etc.
pub use crate::requests::*;

/// One variant per server-bound opcode in spec §03/07. The variant carries
/// the rkyv-archivable structured payload; raw vector blobs (for opcodes
/// that include them) are appended by the [`crate::Frame`] layer as the
/// trailing raw section, not by this enum.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestBody {
    /// Spec §06 §2 — opening handshake frame (connection-level, stream 0).
    Hello(HelloPayload),
    /// Spec §06 §4 — authentication frame following WELCOME.
    Auth(AuthPayload),
    Encode(EncodeRequest),
    EncodeVectorDirect(EncodeVectorDirectRequest),
    Recall(RecallRequest),
    Plan(PlanRequest),
    Reason(ReasonRequest),
    Forget(ForgetRequest),
    Link(LinkRequest),
    Unlink(UnlinkRequest),
    Subscribe(SubscribeRequest),
    Unsubscribe(UnsubscribeRequest),
    TxnBegin(TxnBeginRequest),
    TxnCommit(TxnCommitRequest),
    TxnAbort(TxnAbortRequest),
    CancelStream(CancelStreamRequest),
    Ping(PingRequest),
    ClientPong(ClientPongRequest),
    Bye(ByeRequest),
    AdminStats(AdminStatsRequest),
    AdminSnapshot(AdminSnapshotRequest),
    AdminRestore(AdminRestoreRequest),
    AdminIntegrityCheck(AdminIntegrityCheckRequest),
    AdminMigrateEmbeddings(AdminMigrateEmbeddingsRequest),
    AdminCreateContext(AdminCreateContextRequest),
    AdminRenameContext(AdminRenameContextRequest),
    AdminMoveMemory(AdminMoveMemoryRequest),
    AdminReclassify(AdminReclassifyRequest),
    AdminListTombstoned(AdminListTombstonedRequest),

    // Knowledge namespace (spec §28/00). 16.6c landed CREATE/GET/UPDATE/
    // RENAME; 16.7 adds MERGE/UNMERGE/RESOLVE/LIST/TOMBSTONE. Statement /
    // relation / query / admin opcodes follow in phases 17-24.
    EntityCreate(crate::knowledge::EntityCreateRequest),
    EntityGet(crate::knowledge::EntityGetRequest),
    EntityUpdate(crate::knowledge::EntityUpdateRequest),
    EntityRename(crate::knowledge::EntityRenameRequest),
    EntityMerge(crate::knowledge::EntityMergeRequest),
    EntityUnmerge(crate::knowledge::EntityUnmergeRequest),
    EntityResolve(crate::knowledge::EntityResolveRequest),
    EntityList(crate::knowledge::EntityListRequest),
    EntityTombstone(crate::knowledge::EntityTombstoneRequest),
}

impl RequestBody {
    /// The opcode this body corresponds to.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Hello(_) => Opcode::Hello,
            Self::Auth(_) => Opcode::Auth,
            Self::Encode(_) => Opcode::EncodeReq,
            Self::EncodeVectorDirect(_) => Opcode::EncodeVectorDirectReq,
            Self::Recall(_) => Opcode::RecallReq,
            Self::Plan(_) => Opcode::PlanReq,
            Self::Reason(_) => Opcode::ReasonReq,
            Self::Forget(_) => Opcode::ForgetReq,
            Self::Link(_) => Opcode::LinkReq,
            Self::Unlink(_) => Opcode::UnlinkReq,
            Self::Subscribe(_) => Opcode::SubscribeReq,
            Self::Unsubscribe(_) => Opcode::UnsubscribeReq,
            Self::TxnBegin(_) => Opcode::TxnBegin,
            Self::TxnCommit(_) => Opcode::TxnCommit,
            Self::TxnAbort(_) => Opcode::TxnAbort,
            Self::CancelStream(_) => Opcode::CancelStream,
            Self::Ping(_) => Opcode::Ping,
            Self::ClientPong(_) => Opcode::ClientPong,
            Self::Bye(_) => Opcode::Bye,
            Self::AdminStats(_) => Opcode::AdminStatsReq,
            Self::AdminSnapshot(_) => Opcode::AdminSnapshotReq,
            Self::AdminRestore(_) => Opcode::AdminRestoreReq,
            Self::AdminIntegrityCheck(_) => Opcode::AdminIntegrityCheckReq,
            Self::AdminMigrateEmbeddings(_) => Opcode::AdminMigrateEmbeddingsReq,
            Self::AdminCreateContext(_) => Opcode::AdminCreateContextReq,
            Self::AdminRenameContext(_) => Opcode::AdminRenameContextReq,
            Self::AdminMoveMemory(_) => Opcode::AdminMoveMemoryReq,
            Self::AdminReclassify(_) => Opcode::AdminReclassifyReq,
            Self::AdminListTombstoned(_) => Opcode::AdminListTombstonedReq,
            Self::EntityCreate(_) => Opcode::EntityCreateReq,
            Self::EntityGet(_) => Opcode::EntityGetReq,
            Self::EntityUpdate(_) => Opcode::EntityUpdateReq,
            Self::EntityRename(_) => Opcode::EntityRenameReq,
            Self::EntityMerge(_) => Opcode::EntityMergeReq,
            Self::EntityUnmerge(_) => Opcode::EntityUnmergeReq,
            Self::EntityResolve(_) => Opcode::EntityResolveReq,
            Self::EntityList(_) => Opcode::EntityListReq,
            Self::EntityTombstone(_) => Opcode::EntityTombstoneReq,
        }
    }

    /// Encode the structured body to bytes via rkyv. The returned vector
    /// is suitable for placement in a [`crate::Frame::payload`]; vector
    /// blobs (where this opcode supports them) are appended by callers.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello(r) => to_rkyv_bytes(r),
            Self::Auth(r) => to_rkyv_bytes(r),
            Self::Encode(r) => to_rkyv_bytes(r),
            Self::EncodeVectorDirect(r) => to_rkyv_bytes(r),
            Self::Recall(r) => to_rkyv_bytes(r),
            Self::Plan(r) => to_rkyv_bytes(r),
            Self::Reason(r) => to_rkyv_bytes(r),
            Self::Forget(r) => to_rkyv_bytes(r),
            Self::Link(r) => to_rkyv_bytes(r),
            Self::Unlink(r) => to_rkyv_bytes(r),
            Self::Subscribe(r) => to_rkyv_bytes(r),
            Self::Unsubscribe(r) => to_rkyv_bytes(r),
            Self::TxnBegin(r) => to_rkyv_bytes(r),
            Self::TxnCommit(r) => to_rkyv_bytes(r),
            Self::TxnAbort(r) => to_rkyv_bytes(r),
            Self::CancelStream(r) => to_rkyv_bytes(r),
            Self::Ping(r) => to_rkyv_bytes(r),
            Self::ClientPong(r) => to_rkyv_bytes(r),
            Self::Bye(r) => to_rkyv_bytes(r),
            Self::AdminStats(r) => to_rkyv_bytes(r),
            Self::AdminSnapshot(r) => to_rkyv_bytes(r),
            Self::AdminRestore(r) => to_rkyv_bytes(r),
            Self::AdminIntegrityCheck(r) => to_rkyv_bytes(r),
            Self::AdminMigrateEmbeddings(r) => to_rkyv_bytes(r),
            Self::AdminCreateContext(r) => to_rkyv_bytes(r),
            Self::AdminRenameContext(r) => to_rkyv_bytes(r),
            Self::AdminMoveMemory(r) => to_rkyv_bytes(r),
            Self::AdminReclassify(r) => to_rkyv_bytes(r),
            Self::AdminListTombstoned(r) => to_rkyv_bytes(r),
            Self::EntityCreate(r) => to_rkyv_bytes(r),
            Self::EntityGet(r) => to_rkyv_bytes(r),
            Self::EntityUpdate(r) => to_rkyv_bytes(r),
            Self::EntityRename(r) => to_rkyv_bytes(r),
            Self::EntityMerge(r) => to_rkyv_bytes(r),
            Self::EntityUnmerge(r) => to_rkyv_bytes(r),
            Self::EntityResolve(r) => to_rkyv_bytes(r),
            Self::EntityList(r) => to_rkyv_bytes(r),
            Self::EntityTombstone(r) => to_rkyv_bytes(r),
        }
    }

    /// Decode `bytes` as the request body for the given server-bound
    /// `opcode`. Returns [`ProtocolError::UnknownOpcode`] for opcodes that
    /// don't carry a request body (responses, error frames).
    pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(match opcode {
            Opcode::Hello => Self::Hello(from_rkyv_bytes(bytes)?),
            Opcode::Auth => Self::Auth(from_rkyv_bytes(bytes)?),
            Opcode::EncodeReq => Self::Encode(from_rkyv_bytes(bytes)?),
            Opcode::EncodeVectorDirectReq => Self::EncodeVectorDirect(from_rkyv_bytes(bytes)?),
            Opcode::RecallReq => Self::Recall(from_rkyv_bytes(bytes)?),
            Opcode::PlanReq => Self::Plan(from_rkyv_bytes(bytes)?),
            Opcode::ReasonReq => Self::Reason(from_rkyv_bytes(bytes)?),
            Opcode::ForgetReq => Self::Forget(from_rkyv_bytes(bytes)?),
            Opcode::LinkReq => Self::Link(from_rkyv_bytes(bytes)?),
            Opcode::UnlinkReq => Self::Unlink(from_rkyv_bytes(bytes)?),
            Opcode::SubscribeReq => Self::Subscribe(from_rkyv_bytes(bytes)?),
            Opcode::UnsubscribeReq => Self::Unsubscribe(from_rkyv_bytes(bytes)?),
            Opcode::TxnBegin => Self::TxnBegin(from_rkyv_bytes(bytes)?),
            Opcode::TxnCommit => Self::TxnCommit(from_rkyv_bytes(bytes)?),
            Opcode::TxnAbort => Self::TxnAbort(from_rkyv_bytes(bytes)?),
            Opcode::CancelStream => Self::CancelStream(from_rkyv_bytes(bytes)?),
            Opcode::Ping => Self::Ping(from_rkyv_bytes(bytes)?),
            Opcode::ClientPong => Self::ClientPong(from_rkyv_bytes(bytes)?),
            Opcode::Bye => Self::Bye(from_rkyv_bytes(bytes)?),
            Opcode::AdminStatsReq => Self::AdminStats(from_rkyv_bytes(bytes)?),
            Opcode::AdminSnapshotReq => Self::AdminSnapshot(from_rkyv_bytes(bytes)?),
            Opcode::AdminRestoreReq => Self::AdminRestore(from_rkyv_bytes(bytes)?),
            Opcode::AdminIntegrityCheckReq => Self::AdminIntegrityCheck(from_rkyv_bytes(bytes)?),
            Opcode::AdminMigrateEmbeddingsReq => {
                Self::AdminMigrateEmbeddings(from_rkyv_bytes(bytes)?)
            }
            Opcode::AdminCreateContextReq => Self::AdminCreateContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminRenameContextReq => Self::AdminRenameContext(from_rkyv_bytes(bytes)?),
            Opcode::AdminMoveMemoryReq => Self::AdminMoveMemory(from_rkyv_bytes(bytes)?),
            Opcode::AdminReclassifyReq => Self::AdminReclassify(from_rkyv_bytes(bytes)?),
            Opcode::AdminListTombstonedReq => Self::AdminListTombstoned(from_rkyv_bytes(bytes)?),
            Opcode::EntityCreateReq => Self::EntityCreate(from_rkyv_bytes(bytes)?),
            Opcode::EntityGetReq => Self::EntityGet(from_rkyv_bytes(bytes)?),
            Opcode::EntityUpdateReq => Self::EntityUpdate(from_rkyv_bytes(bytes)?),
            Opcode::EntityRenameReq => Self::EntityRename(from_rkyv_bytes(bytes)?),
            Opcode::EntityMergeReq => Self::EntityMerge(from_rkyv_bytes(bytes)?),
            Opcode::EntityUnmergeReq => Self::EntityUnmerge(from_rkyv_bytes(bytes)?),
            Opcode::EntityResolveReq => Self::EntityResolve(from_rkyv_bytes(bytes)?),
            Opcode::EntityListReq => Self::EntityList(from_rkyv_bytes(bytes)?),
            Opcode::EntityTombstoneReq => Self::EntityTombstone(from_rkyv_bytes(bytes)?),
            other => return Err(ProtocolError::UnknownOpcode(other.as_u16())),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a `RequestBody` through encode → decode and assert
    /// equality. Used by every per-variant test.
    fn round_trip(body: RequestBody) {
        let bytes = body.encode();
        let decoded = RequestBody::decode(body.opcode(), &bytes)
            .unwrap_or_else(|e| panic!("decode failed for {:?}: {e}", body.opcode()));
        assert_eq!(decoded, body);
    }

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_memory_id() -> WireMemoryId {
        // Equivalent of `MemoryId::pack(7, 0x12_3456, 42)`.
        ((7u128) << 72) | ((42u128) << 56) | 0x12_3456_u128
    }

    #[test]
    fn encode_round_trips() {
        round_trip(RequestBody::Encode(EncodeRequest {
            text: "hello brain".into(),
            context_id: 1_u64,
            kind: MemoryKindWire::Episodic,
            salience_hint: 0.25,
            edges: vec![EdgeRequest {
                target: sample_memory_id(),
                kind: EdgeKindWire::Caused,
                weight: 0.9,
            }],
            request_id: sample_uuid(2),
            txn_id: Some(sample_uuid(3)),
            deduplicate: true,
        }));
    }

    #[test]
    fn encode_vector_direct_round_trips() {
        round_trip(RequestBody::EncodeVectorDirect(EncodeVectorDirectRequest {
            text: "vec direct".into(),
            vector_offset: 200,
            vector_dim: 384,
            model_fingerprint: sample_uuid(4),
            context_id: 5_u64,
            kind: MemoryKindWire::Semantic,
            salience_hint: 0.5,
            edges: vec![],
            request_id: sample_uuid(6),
            txn_id: None,
        }));
    }

    #[test]
    fn recall_round_trips() {
        round_trip(RequestBody::Recall(RecallRequest {
            cue_text: "what about budgets".into(),
            cue_vector_offset: 0,
            cue_vector_dim: 0,
            top_k: 10,
            confidence_threshold: 0.3,
            context_filter: Some(vec![1_u64, 2_u64]),
            age_bound_unix_nanos: Some(1_700_000_000_000_000_000),
            kind_filter: Some(vec![MemoryKindWire::Episodic, MemoryKindWire::Semantic]),
            salience_floor: 0.1,
            strategy_hint: Some(RecallStrategy::Hybrid),
            include_vectors: false,
            include_edges: true,
            request_id: Some(sample_uuid(7)),
            txn_id: None,
        }));
    }

    #[test]
    fn plan_round_trips_with_each_state_variant() {
        for start in [
            PlanState::ByMemoryId(sample_memory_id()),
            PlanState::ByText("origin".into()),
            PlanState::ByVector {
                offset: 16,
                dim: 384,
            },
        ] {
            round_trip(RequestBody::Plan(PlanRequest {
                start: start.clone(),
                goal: PlanState::ByText("destination".into()),
                budget: PlanBudget {
                    max_steps: 10,
                    max_wall_time_ms: 1_000,
                    max_branches_explored: 100,
                },
                strategy_hint: Some(PlanStrategy::AStar),
                context_filter: None,
                request_id: None,
                txn_id: None,
            }));
        }
    }

    #[test]
    fn reason_round_trips_with_each_observation_variant() {
        for obs in [
            ObservationInput::ByMemoryId(sample_memory_id()),
            ObservationInput::ByText("an event".into()),
        ] {
            round_trip(RequestBody::Reason(ReasonRequest {
                observation: obs,
                depth: 5,
                confidence_threshold: 0.4,
                context_filter: None,
                max_inferences: 50,
                budget_wall_time_ms: 5_000,
                request_id: None,
                txn_id: None,
            }));
        }
    }

    #[test]
    fn forget_round_trips() {
        for mode in [ForgetMode::Soft, ForgetMode::Hard] {
            round_trip(RequestBody::Forget(ForgetRequest {
                memory_id: sample_memory_id(),
                mode,
                request_id: sample_uuid(8),
                txn_id: None,
            }));
        }
    }

    #[test]
    fn subscribe_round_trips() {
        round_trip(RequestBody::Subscribe(SubscribeRequest {
            filter: SubscriptionFilter {
                contexts: Some(vec![9_u64]),
                kinds: None,
                similar_to: Some(SimilarityFilter {
                    reference_memory_id: sample_memory_id(),
                    threshold: 0.85,
                }),
            },
            include_history: true,
            from_lsn: Some(42),
            max_inflight: 16,
        }));
    }

    #[test]
    fn unsubscribe_round_trips() {
        round_trip(RequestBody::Unsubscribe(UnsubscribeRequest {
            target_stream_id: 7,
        }));
    }

    #[test]
    fn txn_lifecycle_round_trips() {
        let id = sample_uuid(10);
        round_trip(RequestBody::TxnBegin(TxnBeginRequest {
            txn_id: id,
            timeout_seconds: 60,
        }));
        round_trip(RequestBody::TxnCommit(TxnCommitRequest { txn_id: id }));
        round_trip(RequestBody::TxnAbort(TxnAbortRequest { txn_id: id }));
    }

    #[test]
    fn cancel_stream_round_trips() {
        for reason in [
            CancellationReason::ClientUnneeded,
            CancellationReason::Timeout,
            CancellationReason::Other("downstream cancelled".into()),
        ] {
            round_trip(RequestBody::CancelStream(CancelStreamRequest {
                target_stream_id: 9,
                reason,
            }));
        }
    }

    #[test]
    fn keepalive_and_bye_round_trip() {
        round_trip(RequestBody::Ping(PingRequest {
            client_timestamp_unix_nanos: 123_456_789,
        }));
        round_trip(RequestBody::ClientPong(ClientPongRequest {
            server_timestamp_unix_nanos: 1,
            client_timestamp_unix_nanos: 2,
        }));
        round_trip(RequestBody::Bye(ByeRequest {
            reason: Some("done".into()),
        }));
        round_trip(RequestBody::Bye(ByeRequest { reason: None }));
    }

    #[test]
    fn admin_round_trips() {
        round_trip(RequestBody::AdminStats(AdminStatsRequest {
            detail: StatsDetail::PerShard,
        }));
        round_trip(RequestBody::AdminSnapshot(AdminSnapshotRequest {
            snapshot_name: "nightly".into(),
            target_path: Some("/var/brain/snapshots/2026-05-10".into()),
            include_wal: true,
            request_id: sample_uuid(11),
        }));
        round_trip(RequestBody::AdminRestore(AdminRestoreRequest {
            snapshot_name: "nightly".into(),
            target_shard: Some(2),
            request_id: sample_uuid(12),
        }));
        round_trip(RequestBody::AdminIntegrityCheck(
            AdminIntegrityCheckRequest {
                scope: CheckScope::PerShard(vec![0, 1, 2]),
                repair_if_possible: false,
            },
        ));
        round_trip(RequestBody::AdminIntegrityCheck(
            AdminIntegrityCheckRequest {
                scope: CheckScope::QuickSample,
                repair_if_possible: true,
            },
        ));
        round_trip(RequestBody::AdminMigrateEmbeddings(
            AdminMigrateEmbeddingsRequest {
                target_model: ModelIdentifier {
                    name: "bge-large-en-v1.5".into(),
                    fingerprint: sample_uuid(13),
                },
                batch_size: 100,
                rate_limit_qps: 0,
            },
        ));
        round_trip(RequestBody::AdminCreateContext(AdminCreateContextRequest {
            name: "personal".into(),
            description: "personal notes".into(),
            request_id: sample_uuid(14),
        }));
        round_trip(RequestBody::AdminRenameContext(AdminRenameContextRequest {
            context_id: 15_u64,
            new_name: "renamed".into(),
        }));
        round_trip(RequestBody::AdminMoveMemory(AdminMoveMemoryRequest {
            memory_id: sample_memory_id(),
            new_context_id: 16_u64,
        }));
        round_trip(RequestBody::AdminReclassify(AdminReclassifyRequest {
            memory_id: sample_memory_id(),
            new_kind: MemoryKindWire::Consolidated,
        }));
        round_trip(RequestBody::AdminListTombstoned(
            AdminListTombstonedRequest {
                context_id: Some(17_u64),
                max_age_seconds: 3600,
                limit: 100,
            },
        ));
    }

    #[test]
    fn handshake_request_bodies_round_trip() {
        use crate::handshake::{
            AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload, MtlsClaim,
        };

        for body in [
            RequestBody::Hello(HelloPayload {
                client_id: "brain-rust-sdk/0.5.0".into(),
                supported_versions: vec![1],
                capabilities: HelloCapabilities {
                    streaming: true,
                    compression_zstd: false,
                    server_push: false,
                },
                client_session_token: None,
            }),
            RequestBody::Auth(AuthPayload {
                method: AuthMethod::Token,
                agent_id: sample_uuid(11),
                credentials: AuthCredentials::Token(b"opaque".to_vec()),
            }),
            RequestBody::Auth(AuthPayload {
                method: AuthMethod::Mtls,
                agent_id: sample_uuid(12),
                credentials: AuthCredentials::Mtls(MtlsClaim {
                    cert_fingerprint: [9u8; 32],
                    asserted_subject: "CN=client".into(),
                }),
            }),
            RequestBody::Auth(AuthPayload {
                method: AuthMethod::None,
                agent_id: sample_uuid(13),
                credentials: AuthCredentials::None,
            }),
        ] {
            let bytes = body.encode();
            let decoded = RequestBody::decode(body.opcode(), &bytes).unwrap();
            assert_eq!(decoded, body);
        }
    }

    #[test]
    fn opcode_matches_variant() {
        // Cross-check that every variant reports its expected opcode.
        let cases: &[(RequestBody, Opcode)] = &[
            (
                RequestBody::Ping(PingRequest {
                    client_timestamp_unix_nanos: 0,
                }),
                Opcode::Ping,
            ),
            (RequestBody::Bye(ByeRequest { reason: None }), Opcode::Bye),
            (
                RequestBody::Unsubscribe(UnsubscribeRequest {
                    target_stream_id: 0,
                }),
                Opcode::UnsubscribeReq,
            ),
        ];
        for (body, opcode) in cases {
            assert_eq!(body.opcode(), *opcode);
        }
    }

    #[test]
    fn decode_with_response_opcode_returns_unknown() {
        // Response opcodes don't carry request bodies. Feeding one to
        // `RequestBody::decode` must error rather than panic.
        let any_bytes = vec![0u8; 8];
        let err = RequestBody::decode(Opcode::EncodeResp, &any_bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownOpcode(_)));
    }

    #[test]
    fn decode_garbage_returns_malformed() {
        let garbage = vec![0xAAu8; 64];
        let err = RequestBody::decode(Opcode::EncodeReq, &garbage).unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedPayload(_)));
    }
}
