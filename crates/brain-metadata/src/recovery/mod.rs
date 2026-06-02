//! `impl MetadataSink for MetadataDb`.
//!
//! Translates the 15 `WalPayload` variants into redb table writes,
//! one variant per `apply_*` helper. Recovery feeds records in LSN
//! order; the sink commits each `apply` call as its own redb write
//! transaction.
//!
//! ## Module layout
//!
//! Per-variant apply helpers live in family modules. The
//! [`MetadataSink`] impl below dispatches each `WalPayload` variant
//! into the appropriate family function (each implemented as an `impl
//! MetadataDb` block in its own file).
//!
//! - [`memory`] — Encode / Forget / UpdateSalience / UpdateKind / UpdateContext / MigrateEmbedding
//! - [`edge`] — Link / Unlink
//! - [`relation`] — RelationLink / RelationSupersede / RelationTombstone
//! - [`reclaim`] — Reclaim / Consolidate
//! - [`checkpoint`] — CheckpointBegin (inlined here as in-memory) / CheckpointEnd
//!
//! `TxnBegin` / `TxnCommit` / `TxnAbort` and `PhaseBody(_)` are dispatched
//! inline in the match arm below — they have no family module because the
//! sink's only job for them is to advance `next_lsn` (see comments at the
//! match arms).
//!
//! ## Deliberate placeholders (documented in module docs)
//!
//! - `ModelInfo.model_name` is filled with `""` — `EncodePayload`
//!   carries the fingerprint bytes but not the human-readable name.
//!   `ADMIN_REGISTER_MODEL` or the embedding loader fills it later.
//! - `ModelInfo.memory_count_at_fingerprint` stays at 0; the
//!   maintenance worker reconciles it by scanning `memories`.
//! - `MemoryMetadata.edges_out_count` / `edges_in_count` aren't
//!   maintained on Link/Unlink. The maintenance worker reconciles.

use std::sync::atomic::Ordering;

use brain_core::EdgeKind;
use brain_storage::recovery::{MetadataSink, MetadataSinkError};
use brain_storage::wal::payload::{EdgePayload, WalPayload};
use redb::{ReadableTable, WriteTransaction};

use crate::db::MetadataDb;
use crate::tables::edge::{derived_by, EdgeData, EdgeKey};
use crate::tables::next_lsn::NEXT_LSN_TABLE;

pub mod checkpoint;
pub mod edge;
pub mod phases;
pub mod phase_bodies;
pub mod memory;
pub mod reclaim;
pub mod relation;

// ---------------------------------------------------------------------------
// MetadataSink trait impl — single dispatch entry point for recovery.
// ---------------------------------------------------------------------------

impl MetadataSink for MetadataDb {
    fn durable_lsn(&self) -> u64 {
        self.durable_lsn.load(Ordering::Acquire)
    }

    fn apply(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        payload: &WalPayload,
    ) -> Result<(), MetadataSinkError> {
        match payload {
            WalPayload::Encode(p) => self.apply_encode(lsn, timestamp_ns, p),
            WalPayload::Forget(p) => self.apply_forget(lsn, timestamp_ns, p),
            WalPayload::Link(p) => self.apply_link(lsn, timestamp_ns, p),
            WalPayload::Unlink(p) => self.apply_unlink(lsn, timestamp_ns, p),
            WalPayload::UpdateSalience(p) => self.apply_update_salience(lsn, p),
            WalPayload::Reclaim(p) => self.apply_reclaim(lsn, p),
            WalPayload::Consolidate(p) => self.apply_consolidate(lsn, timestamp_ns, p),
            WalPayload::UpdateKind(p) => self.apply_update_kind(lsn, timestamp_ns, p),
            WalPayload::UpdateContext(p) => self.apply_update_context(lsn, timestamp_ns, p),
            WalPayload::MigrateEmbedding(p) => self.apply_migrate_embedding(lsn, p),
            WalPayload::CheckpointBegin(p) => {
                // In-memory state only; no persistent write.
                self.pending_checkpoints
                    .lock()
                    .insert(p.checkpoint_id, p.started_at_unix_nanos);
                self.bump_next_lsn(lsn)
            }
            WalPayload::CheckpointEnd(p) => self.apply_checkpoint_end(lsn, timestamp_ns, p),
            WalPayload::TxnBegin(_) | WalPayload::TxnCommit(_) | WalPayload::TxnAbort(_) => {
                // Txn brackets are no-ops at this sink. The recovery
                // driver (brain_storage::recovery::recover) already
                // buffers records between a matching Begin/Commit pair
                // and only invokes apply() on the committed members;
                // aborted transactions never reach the sink. The sink's
                // only remaining duty for a bracket record is keeping
                // the next_lsn watermark monotonic.
                self.bump_next_lsn(lsn)
            }
            WalPayload::PhaseBody(record) => {
                use brain_storage::wal::kinds::WalRecordKind;
                match record.kind {
                    WalRecordKind::EntityCreate => self.apply_entity_create(lsn, &record.body),
                    WalRecordKind::EntityUpdate => self.apply_entity_update(lsn, &record.body),
                    WalRecordKind::EntityRename => self.apply_entity_rename(lsn, &record.body),
                    WalRecordKind::EntityMerge => self.apply_entity_merge(lsn, &record.body),
                    WalRecordKind::EntityUnmerge => self.apply_entity_unmerge(lsn, &record.body),
                    WalRecordKind::EntityTombstone => {
                        self.apply_entity_tombstone(lsn, &record.body)
                    }
                    WalRecordKind::StatementCreate => {
                        self.apply_statement_create(lsn, &record.body)
                    }
                    WalRecordKind::StatementSupersede => {
                        self.apply_statement_supersede(lsn, &record.body)
                    }
                    WalRecordKind::StatementTombstone => {
                        self.apply_statement_tombstone(lsn, &record.body)
                    }
                    WalRecordKind::SchemaUpdate => self.apply_schema_update(lsn, &record.body),
                    WalRecordKind::ExtractorToggle => {
                        self.apply_extractor_toggle(lsn, &record.body)
                    }
                    // Other typed-graph kinds aren't WAL-mapped on the write
                    // side yet (durability still rides the redb commit for
                    // them); bump next_lsn so checkpointing and
                    // replay-from-LSN stay correct as families land.
                    _ => self.bump_next_lsn(lsn),
                }
            }
            WalPayload::RelationLink(p) => self.apply_relation_link(lsn, timestamp_ns, p),
            WalPayload::RelationSupersede(p) => self.apply_relation_supersede(lsn, timestamp_ns, p),
            WalPayload::RelationTombstone(p) => self.apply_relation_tombstone(lsn, p),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by every family module.
// ---------------------------------------------------------------------------

impl MetadataDb {
    /// Apply a `next_lsn[()] = max(current, lsn + 1)` update inside an
    /// existing write transaction.
    pub(super) fn bump_next_lsn_in_txn(
        &self,
        wtxn: &WriteTransaction,
        lsn: u64,
    ) -> Result<(), MetadataSinkError> {
        let mut t = wtxn.open_table(NEXT_LSN_TABLE).map_err(transient)?;
        let current = t.get(&()).map_err(transient)?.map_or(0, |a| a.value());
        let next = lsn.saturating_add(1).max(current);
        t.insert(&(), &next).map_err(transient)?;
        Ok(())
    }

    /// Bump `next_lsn` in its own transaction. Used by variants whose
    /// apply has no other table writes (TxnBegin/Commit/Abort,
    /// CheckpointBegin).
    pub(super) fn bump_next_lsn(&self, lsn: u64) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        wtxn.commit().map_err(transient)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free helpers shared across family modules.
// ---------------------------------------------------------------------------

/// Project an [`EdgePayload`] (the WAL-side, payload-flavoured shape)
/// to an [`EdgeData`] row (the redb-side, table-flavoured shape).
/// Used by the Encode and Link recovery paths — every WAL-replayed
/// edge is `derived_by::CLIENT` because recovery only sees what the
/// live writer recorded.
pub(super) fn edge_payload_to_data(e: &EdgePayload, timestamp_ns: u64) -> EdgeData {
    EdgeData::new(e.weight, e.origin as u8, derived_by::CLIENT, timestamp_ns)
}

/// Wrap any redb / storage error as [`MetadataSinkError::Transient`].
/// Every redb call inside an apply path runs through this — recovery
/// surfaces the same error taxonomy regardless of which family wrote
/// the offending row.
pub(super) fn transient<E: std::fmt::Display>(e: E) -> MetadataSinkError {
    MetadataSinkError::Transient(format!("{e}"))
}

// ---------------------------------------------------------------------------
// Suppress unused-import warning for types referenced only in the docs/
// match arms above.
// ---------------------------------------------------------------------------

const _: fn() = || {
    let _: Option<EdgeKey> = None;
    let _: Option<EdgeKind> = None;
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::storage_version::CURRENT_SCHEMA_VERSION;
    use crate::tables::checkpoint::CHECKPOINTS_TABLE;
    use crate::tables::edge::{EDGES_REVERSE_TABLE, EDGES_TABLE};
    use crate::tables::idempotency::{response_kind, IDEMPOTENCY_TABLE};
    use crate::tables::memory::{flags, memory_kind_to_u8, MEMORIES_TABLE};
    use crate::tables::model_fingerprint::MODEL_FINGERPRINTS_TABLE;
    use crate::tables::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};
    use crate::tables::slot_version::SLOT_VERSIONS_TABLE;
    use crate::tables::text::TEXTS_TABLE;
    use brain_core::{AgentId, ContextId, EdgeKind, EdgeOrigin, MemoryId, MemoryKind, RequestId};
    use brain_storage::wal::payload::{
        CheckpointBeginPayload, CheckpointEndPayload, EdgePayload, EncodePayload, ForgetMode,
        ForgetPayload, ForgetReason, LinkPayload, MigrateEmbeddingPayload, ReclaimPayload,
        SalienceReason, SalienceUpdate, TxnBeginPayload, UnlinkPayload, UpdateContextPayload,
        UpdateKindPayload, UpdateSaliencePayload, WalPayload,
    };
    use std::path::PathBuf;

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sink.redb")
    }

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn rid(byte: u8) -> RequestId {
        let mut b = [0u8; 16];
        b[15] = byte;
        RequestId::from(b)
    }

    fn mid(slot: u64, version: u32) -> MemoryId {
        MemoryId::pack(1, slot, version)
    }

    fn sample_encode(slot: u64, byte: u8) -> EncodePayload {
        EncodePayload {
            memory_id: mid(slot, 1),
            request_id: rid(byte),
            agent_id: aid(byte),
            context_id: ContextId(42),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: [byte; 16],
            text: format!("text for memory {byte}"),
            vector: vec![0.0; 384],
            edges: Vec::new(),
            request_hash: [byte; 32],
            response_payload: vec![],
            deduplicate: false,
        }
    }

    const TS: u64 = 1_700_000_000_000_000_000;

    // ---------- durable_lsn ----------

    #[test]
    fn durable_lsn_fresh_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(db_path(&dir)).unwrap();
        assert_eq!(db.durable_lsn(), 0);
    }

    #[test]
    fn durable_lsn_persists_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        {
            let mut db = MetadataDb::open(&path).unwrap();
            db.apply(
                10,
                TS,
                &WalPayload::CheckpointBegin(CheckpointBeginPayload {
                    checkpoint_id: 1,
                    started_at_unix_nanos: TS,
                }),
            )
            .unwrap();
            db.apply(
                11,
                TS + 1000,
                &WalPayload::CheckpointEnd(CheckpointEndPayload {
                    checkpoint_id: 1,
                    durable_lsn: 100,
                    arena_capacity: 1024,
                }),
            )
            .unwrap();
            assert_eq!(db.durable_lsn(), 100);
        }
        let db = MetadataDb::open(&path).unwrap();
        assert_eq!(db.durable_lsn(), 100);
    }

    // ---------- Encode ----------

    #[test]
    fn encode_writes_all_expected_tables() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let p = sample_encode(7, 7);
        let id_bytes = p.memory_id.to_be_bytes();
        let fp = p.embedding_model_fp;
        let req_bytes = <[u8; 16]>::from(p.request_id);
        let slot_id = p.memory_id.slot();
        let slot_version = p.memory_id.version();
        let expected_text = p.text.clone();

        db.apply(1, TS, &WalPayload::Encode(p)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&id_bytes)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.kind, memory_kind_to_u8(MemoryKind::Episodic));
        assert_eq!(m.slot_id, slot_id);
        assert_eq!(m.slot_version, slot_version);

        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        assert_eq!(
            t.get(&id_bytes).unwrap().unwrap().value(),
            expected_text.as_bytes()
        );

        let i = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let entry = i.get(&req_bytes).unwrap().unwrap().value();
        assert_eq!(entry.response_kind, response_kind::ENCODE);
        assert_eq!(entry.memory_id_bytes, Some(id_bytes));

        let f = rtxn.open_table(MODEL_FINGERPRINTS_TABLE).unwrap();
        assert!(f.get(&fp).unwrap().is_some());

        let s = rtxn.open_table(SLOT_VERSIONS_TABLE).unwrap();
        assert_eq!(s.get(&slot_id).unwrap().unwrap().value(), slot_version);

        let n = rtxn.open_table(NEXT_LSN_TABLE).unwrap();
        assert_eq!(n.get(&()).unwrap().unwrap().value(), 2);
    }

    #[test]
    fn encode_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let p = sample_encode(5, 5);
        db.apply(1, TS, &WalPayload::Encode(p.clone())).unwrap();
        db.apply(1, TS, &WalPayload::Encode(p.clone())).unwrap();

        // Just one memory row.
        let rtxn = db.read_txn().unwrap();
        let mems = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let count: u64 = mems.iter().unwrap().count() as u64;
        assert_eq!(count, 1);
    }

    #[test]
    fn encode_with_multiple_edges_writes_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let mut p = sample_encode(1, 1);
        let mem_node = brain_core::NodeRef::Memory(p.memory_id);
        p.edges = vec![
            EdgePayload {
                source: mem_node,
                target: brain_core::NodeRef::Memory(mid(2, 1)),
                kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: 0.8,
                origin: EdgeOrigin::Explicit,
            },
            EdgePayload {
                source: mem_node,
                target: brain_core::NodeRef::Memory(mid(3, 1)),
                kind: brain_core::EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                weight: 0.6,
                origin: EdgeOrigin::AutoDerived,
            },
        ];
        db.apply(1, TS, &WalPayload::Encode(p)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let out = rtxn.open_table(EDGES_TABLE).unwrap();
        // 1 Caused + 2 SimilarTo (direct + mirror) = 3 rows in EDGES_TABLE.
        let count: u64 = out.iter().unwrap().count() as u64;
        assert_eq!(count, 3);
    }

    // ---------- Forget ----------

    #[test]
    fn forget_marks_memory_tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(1, 1);
        let id = enc.memory_id;
        let key = id.to_be_bytes();
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::Forget(ForgetPayload {
                memory_id: id,
                request_id: rid(2),
                agent_id: brain_core::AgentId::default(),
                mode: ForgetMode::Soft,
                reason: ForgetReason::ClientRequest,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&key)
            .unwrap()
            .unwrap()
            .value();
        assert_ne!(m.flags & flags::HARD_FORGOTTEN, 0);
        assert_eq!(m.forgot_at_unix_nanos, Some(TS + 1));
    }

    // ---------- Link / Unlink ----------

    #[test]
    fn link_writes_both_edge_tables() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        db.apply(
            1,
            TS,
            &WalPayload::Link(LinkPayload {
                source: brain_core::NodeRef::Memory(mid(1, 1)),
                target: brain_core::NodeRef::Memory(mid(2, 1)),
                edge_kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: 0.9,
                origin: EdgeOrigin::Explicit,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let out = rtxn.open_table(EDGES_TABLE).unwrap();
        let rev = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(out.iter().unwrap().count(), 1);
        assert_eq!(rev.iter().unwrap().count(), 1);
    }

    #[test]
    fn unlink_removes_both_edges() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let src = brain_core::NodeRef::Memory(mid(1, 1));
        let tgt = brain_core::NodeRef::Memory(mid(2, 1));
        db.apply(
            1,
            TS,
            &WalPayload::Link(LinkPayload {
                source: src,
                target: tgt,
                edge_kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                weight: 0.9,
                origin: EdgeOrigin::Explicit,
            }),
        )
        .unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::Unlink(UnlinkPayload {
                source: src,
                target: tgt,
                edge_kind: brain_core::EdgeKindRef::Builtin(EdgeKind::Caused),
                edge_seq: 0,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            rtxn.open_table(EDGES_TABLE)
                .unwrap()
                .iter()
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            rtxn.open_table(EDGES_REVERSE_TABLE)
                .unwrap()
                .iter()
                .unwrap()
                .count(),
            0
        );
    }

    // ---------- UpdateSalience ----------

    #[test]
    fn update_salience_changes_memory_salience() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(1, 1);
        let id = enc.memory_id;
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::UpdateSalience(UpdateSaliencePayload {
                updates: vec![SalienceUpdate {
                    memory_id: id,
                    new_salience: 0.95,
                    reason: SalienceReason::Access,
                }],
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value();
        assert!((m.salience - 0.95).abs() < 1e-6);
    }

    // ---------- Reclaim ----------

    #[test]
    fn reclaim_advances_slot_version_and_deletes_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(5, 5);
        let memory_id = enc.memory_id;
        let slot_id = memory_id.slot();
        let old_version = memory_id.version();
        let key = memory_id.to_be_bytes();
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::Reclaim(ReclaimPayload {
                slot_id,
                old_version,
                new_version: old_version + 1,
                memory_id,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        assert!(rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&key)
            .unwrap()
            .is_none());
        assert!(rtxn
            .open_table(TEXTS_TABLE)
            .unwrap()
            .get(&key)
            .unwrap()
            .is_none());
        assert_eq!(
            rtxn.open_table(SLOT_VERSIONS_TABLE)
                .unwrap()
                .get(&slot_id)
                .unwrap()
                .unwrap()
                .value(),
            old_version + 1
        );
    }

    // ---------- Consolidate ----------

    #[test]
    fn consolidate_inserts_new_memory() {
        use brain_storage::wal::payload::ConsolidatePayload;
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let new_id = mid(100, 1);
        db.apply(
            1,
            TS,
            &WalPayload::Consolidate(ConsolidatePayload {
                new_memory_id: new_id,
                source_memory_ids: vec![mid(1, 1), mid(2, 1)],
                text: "consolidated summary".to_string(),
                vector: vec![0.0; 384],
                embedding_model_fp: [0xCC; 16],
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&new_id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.kind, memory_kind_to_u8(MemoryKind::Consolidated));
        assert_eq!(m.consolidated_at_unix_nanos, Some(TS));
    }

    // ---------- UpdateKind / UpdateContext ----------

    #[test]
    fn update_kind_changes_memory_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(1, 1);
        let id = enc.memory_id;
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::UpdateKind(UpdateKindPayload {
                memory_id: id,
                new_kind: MemoryKind::Semantic,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.kind, memory_kind_to_u8(MemoryKind::Semantic));
    }

    #[test]
    fn update_context_changes_memory_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(1, 1);
        let id = enc.memory_id;
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::UpdateContext(UpdateContextPayload {
                memory_id: id,
                new_context_id: ContextId(999),
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.context_id, 999);
    }

    // ---------- MigrateEmbedding ----------

    #[test]
    fn migrate_embedding_changes_memory_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let enc = sample_encode(1, 1);
        let id = enc.memory_id;
        let new_fp = [0x99; 16];
        db.apply(1, TS, &WalPayload::Encode(enc)).unwrap();
        db.apply(
            2,
            TS + 1,
            &WalPayload::MigrateEmbedding(MigrateEmbeddingPayload {
                memory_id: id,
                old_fingerprint: [1; 16],
                new_fingerprint: new_fp,
                new_vector: vec![0.0; 384],
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(MEMORIES_TABLE)
            .unwrap()
            .get(&id.to_be_bytes())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.embedding_model_fp, new_fp);
    }

    // ---------- Checkpoint pairing ----------

    #[test]
    fn checkpoint_end_writes_meta_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        db.apply(
            1,
            TS,
            &WalPayload::CheckpointBegin(CheckpointBeginPayload {
                checkpoint_id: 7,
                started_at_unix_nanos: TS,
            }),
        )
        .unwrap();
        db.apply(
            2,
            TS + 5000,
            &WalPayload::CheckpointEnd(CheckpointEndPayload {
                checkpoint_id: 7,
                durable_lsn: 1,
                arena_capacity: 1024,
            }),
        )
        .unwrap();

        assert_eq!(db.durable_lsn(), 1);

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(CHECKPOINTS_TABLE)
            .unwrap()
            .get(&7u64)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.checkpoint_id, 7);
        assert_eq!(m.durable_lsn, 1);
        assert_eq!(m.started_at_unix_nanos, TS);
        assert_eq!(m.completed_at_unix_nanos, TS + 5000);
        assert_eq!(
            m.metadata_version_at_checkpoint,
            u64::from(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn checkpoint_end_without_begin_uses_zero_started_at() {
        // E.g. recovery restarts after a crash that landed between
        // BEGIN and END; the BEGIN was applied before the crash, but
        // pending_checkpoints is in-memory only and didn't survive.
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        db.apply(
            1,
            TS + 5000,
            &WalPayload::CheckpointEnd(CheckpointEndPayload {
                checkpoint_id: 9,
                durable_lsn: 50,
                arena_capacity: 1024,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let m = rtxn
            .open_table(CHECKPOINTS_TABLE)
            .unwrap()
            .get(&9u64)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(m.started_at_unix_nanos, 0);
        assert_eq!(m.completed_at_unix_nanos, TS + 5000);
        assert_eq!(m.durable_lsn, 50);
    }

    // ---------- Txn no-ops ----------

    #[test]
    fn txn_records_are_noops_except_next_lsn() {
        use brain_core::TxnId;
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let mut tid_bytes = [0u8; 16];
        tid_bytes[15] = 0x55;
        let txn_id = TxnId::from(tid_bytes);
        db.apply(
            1,
            TS,
            &WalPayload::TxnBegin(TxnBeginPayload {
                txn_id,
                expected_record_count: 2,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        // memories table is materialized at MetadataDb::open but empty
        // (no domain writes happened).
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert_eq!(t.iter().unwrap().count(), 0);
        // next_lsn should be 2.
        assert_eq!(
            rtxn.open_table(NEXT_LSN_TABLE)
                .unwrap()
                .get(&())
                .unwrap()
                .unwrap()
                .value(),
            2
        );
    }

    // ---------- next_lsn tracking ----------

    #[test]
    fn next_lsn_tracks_max_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let p = sample_encode(1, 1);
        // Apply LSNs out of monotonic order: 3, 5, 4, 7, 6.
        for lsn in [3u64, 5, 4, 7, 6] {
            db.apply(lsn, TS + lsn, &WalPayload::Encode(p.clone()))
                .unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let v = rtxn
            .open_table(NEXT_LSN_TABLE)
            .unwrap()
            .get(&())
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(v, 8);
    }

    // ---------- RelationLink / Supersede / Tombstone ----------

    use brain_core::{EntityId, RelationId, RelationTypeId};
    use brain_storage::wal::payload::{
        RelationLinkPayload, RelationSupersedePayload, RelationTombstonePayload,
    };

    fn ent(byte: u8) -> EntityId {
        let mut b = [0u8; 16];
        b[15] = byte;
        EntityId::from(b)
    }

    fn relid(byte: u8) -> RelationId {
        let mut b = [0u8; 16];
        b[0] = 0xA0;
        b[15] = byte;
        RelationId::from(b)
    }

    fn sample_relation_link(rid_byte: u8, from: u8, to: u8) -> RelationLinkPayload {
        RelationLinkPayload {
            relation_id: relid(rid_byte),
            from: brain_core::NodeRef::Entity(ent(from)),
            to: brain_core::NodeRef::Entity(ent(to)),
            relation_type_id: RelationTypeId::from(101),
            chain_root: relid(rid_byte),
            confidence: 0.92,
            valid_from_unix_nanos: Some(TS),
            valid_to_unix_nanos: None,
            supersedes: None,
            evidence: vec![mid(50, 1), mid(51, 1)],
            extractor_id: 7,
            is_symmetric: false,
            properties_blob: vec![1, 2, 3],
            agent_id: aid(1),
            relation_type_intern_hint: None,
        }
    }

    #[test]
    fn relation_link_writes_unified_edge_plus_sidecar_plus_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let p = sample_relation_link(1, 2, 3);
        let rid_bytes = p.relation_id.to_bytes();
        db.apply(1, TS, &WalPayload::RelationLink(p.clone()))
            .unwrap();

        let rtxn = db.read_txn().unwrap();
        // Unified edge row.
        let edges = rtxn.open_table(EDGES_TABLE).unwrap();
        let edge_count = edges.iter().unwrap().count();
        assert_eq!(edge_count, 1, "asymmetric typed relation: one edge row");
        let reverse = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(reverse.iter().unwrap().count(), 1);

        // Sidecar.
        let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let meta = sidecar.get(&rid_bytes).unwrap().unwrap().value();
        assert_eq!(meta.relation_type_id, 101);
        assert!((meta.confidence - 0.92).abs() < 1e-6);
        assert_eq!(meta.is_current, 1);
        assert_eq!(meta.tombstoned, 0);
        assert_eq!(meta.evidence_inline.len(), 2);

        // Evidence reverse index — one row per evidence memory.
        let by_ev = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
        assert_eq!(by_ev.iter().unwrap().count(), 2);
    }

    #[test]
    fn relation_link_symmetric_mirrors_unified_edge() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let mut p = sample_relation_link(5, 7, 9);
        p.is_symmetric = true;
        db.apply(1, TS, &WalPayload::RelationLink(p.clone()))
            .unwrap();

        let rtxn = db.read_txn().unwrap();
        let edges = rtxn.open_table(EDGES_TABLE).unwrap();
        // Symmetric typed: two rows (forward + mirror) in EDGES_TABLE.
        // The mirror is written explicitly by relation_ops/sink so the
        // `is_symmetric` bit stays sidecar-local.
        assert_eq!(edges.iter().unwrap().count(), 2);
    }

    #[test]
    fn relation_supersede_flips_old_and_inserts_new() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let old = sample_relation_link(1, 2, 3);
        let old_id = old.relation_id;
        db.apply(1, TS, &WalPayload::RelationLink(old.clone()))
            .unwrap();

        let mut new_p = sample_relation_link(2, 2, 4);
        new_p.supersedes = Some(old_id);
        new_p.chain_root = old_id;
        let new_id = new_p.relation_id;
        db.apply(
            2,
            TS + 1_000,
            &WalPayload::RelationSupersede(RelationSupersedePayload {
                old_relation_id: old_id,
                new: new_p,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let old_meta = sidecar.get(&old_id.to_bytes()).unwrap().unwrap().value();
        assert_eq!(old_meta.is_current, 0);
        assert_eq!(old_meta.superseded_by_bytes, Some(new_id.to_bytes()));
        assert_eq!(old_meta.valid_to_unix_nanos, Some(TS + 1_000));

        let new_meta = sidecar.get(&new_id.to_bytes()).unwrap().unwrap().value();
        assert_eq!(new_meta.is_current, 1);
        assert_eq!(new_meta.supersedes_bytes, Some(old_id.to_bytes()));
        assert_eq!(new_meta.chain_root_bytes, old_id.to_bytes());
        assert_eq!(new_meta.version, 2);
    }

    #[test]
    fn relation_tombstone_flips_sidecar_bits_but_keeps_edge() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let p = sample_relation_link(1, 2, 3);
        let rid_bytes = p.relation_id.to_bytes();
        db.apply(1, TS, &WalPayload::RelationLink(p.clone()))
            .unwrap();

        db.apply(
            2,
            TS + 2_000,
            &WalPayload::RelationTombstone(RelationTombstonePayload {
                relation_id: p.relation_id,
                reason: "test".into(),
                at_unix_nanos: TS + 2_000,
                agent_id: aid(9),
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let sidecar = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let meta = sidecar.get(&rid_bytes).unwrap().unwrap().value();
        assert_eq!(meta.tombstoned, 1);
        assert_eq!(meta.tombstoned_at_unix_nanos, Some(TS + 2_000));
        assert_eq!(meta.is_current, 0);

        // The edge row stays — tombstone is a sidecar property.
        let edges = rtxn.open_table(EDGES_TABLE).unwrap();
        assert_eq!(edges.iter().unwrap().count(), 1);
    }

    #[test]
    fn relation_tombstone_for_missing_sidecar_errors_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        // RELATION_METADATA_TABLE is created lazily on first write.
        // Seed an unrelated relation so the table exists, then issue
        // a tombstone against an id we never wrote.
        db.apply(
            1,
            TS,
            &WalPayload::RelationLink(sample_relation_link(1, 2, 3)),
        )
        .unwrap();

        let err = db
            .apply(
                2,
                TS + 1,
                &WalPayload::RelationTombstone(RelationTombstonePayload {
                    relation_id: relid(99),
                    reason: "ghost".into(),
                    at_unix_nanos: TS + 1,
                    agent_id: aid(9),
                }),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                brain_storage::recovery::MetadataSinkError::Corruption(_)
            ),
            "expected Corruption, got {err:?}"
        );
    }
}
