//! `impl MetadataSink for MetadataDb`.
//!
//! Translates the 15 `WalPayload` variants into redb table writes,
//! one variant per `apply_*` helper. Recovery feeds records in LSN
//! order; the sink commits each `apply` call as its own redb write
//! transaction.
//!
//! Spec references:
//! - `spec/05_storage_arena_wal/08_recovery.md` — the recovery contract.
//! - `spec/07_metadata_graph/08_transactions.md` §11 — multi-table atomic writes.
//! - `spec/05_storage_arena_wal/09_checkpointing.md` §2, §12.1 — checkpoint pairing.
//! - `spec/07_metadata_graph/06_idempotency.md` §17 — which ops record idempotency.
//! - `spec/02_data_model/03_identifiers.md` §2.3 — MemoryId stability under reclaim.
//!
//! ## Deliberate placeholders (documented in module docs)
//!
//! - `IdempotencyEntry.request_hash` is filled with zeros during
//!   recovery. Spec §07/06 §5's conflict-detection hash is computed
//!   from canonicalised request bytes which the WAL doesn't carry; the
//!   wire layer (Phase 9) populates the hash on live requests.
//! - `ModelInfo.model_name` is filled with `""` — `EncodePayload`
//!   carries the fingerprint bytes but not the human-readable name.
//!   `ADMIN_REGISTER_MODEL` (Phase 9) or the embedding loader fills
//!   it later.
//! - `ModelInfo.memory_count_at_fingerprint` stays at 0; the Phase 8
//!   maintenance worker reconciles it by scanning `memories`.
//! - `MemoryMetadata.edges_out_count` / `edges_in_count` aren't
//!   maintained on Link/Unlink. Phase 8 worker reconciles.

use brain_core::{AgentId, ContextId, EdgeKind, MemoryKind};
use brain_storage::recovery::{MetadataSink, MetadataSinkError};
use brain_storage::wal::payload::{
    ConsolidatePayload, EdgePayload, EncodePayload, ForgetPayload, LinkPayload,
    MigrateEmbeddingPayload, ReclaimPayload, SalienceUpdate, UnlinkPayload, UpdateContextPayload,
    UpdateKindPayload, UpdateSaliencePayload, WalPayload,
};
use redb::{ReadableTable, WriteTransaction};

use crate::db::MetadataDb;
use crate::schema::CURRENT_SCHEMA_VERSION;
use crate::tables::checkpoint::{CheckpointMeta, CHECKPOINTS_TABLE};
use crate::tables::edge::{self, EdgeData, EdgeKey, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use crate::tables::idempotency::{response_kind, IdempotencyEntry, IDEMPOTENCY_TABLE};
use crate::tables::memory::{flags, memory_kind_to_u8, MemoryMetadata, MEMORIES_TABLE};
use crate::tables::model_fingerprint::{ModelInfo, MODEL_FINGERPRINTS_TABLE};
use crate::tables::next_lsn::NEXT_LSN_TABLE;
use crate::tables::slot_version::SLOT_VERSIONS_TABLE;
use crate::tables::text::TEXTS_TABLE;

// ---------------------------------------------------------------------------
// MetadataSink trait impl.
// ---------------------------------------------------------------------------

impl MetadataSink for MetadataDb {
    fn durable_lsn(&self) -> u64 {
        self.durable_lsn
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
                    .insert(p.checkpoint_id, p.started_at_unix_nanos);
                self.bump_next_lsn(lsn)
            }
            WalPayload::CheckpointEnd(p) => self.apply_checkpoint_end(lsn, timestamp_ns, p),
            WalPayload::TxnBegin(_) | WalPayload::TxnCommit(_) | WalPayload::TxnAbort(_) => {
                // Recovery (brain_storage::recovery::recover) already
                // buffers and applies bracketed records atomically. The
                // sink only sees committed records; brackets themselves
                // are no-ops here.
                self.bump_next_lsn(lsn)
            }
            WalPayload::Knowledge(_) => {
                // Knowledge-layer records (spec §26) — substrate sink
                // ignores these. Phases 16+ supply their own sinks for
                // entity / statement / relation / schema / audit
                // hydration. We still advance the LSN watermark so
                // checkpointing is correct.
                self.bump_next_lsn(lsn)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-variant apply helpers.
// ---------------------------------------------------------------------------

impl MetadataDb {
    fn apply_encode(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &EncodePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let memory_id = p.memory_id;
            let slot_id = memory_id.slot();
            let slot_version = memory_id.version();

            let mem = MemoryMetadata::new_active(
                memory_id,
                p.agent_id,
                p.context_id,
                slot_id,
                slot_version,
                p.kind,
                p.embedding_model_fp,
                p.salience_initial,
                u32::try_from(p.text.len()).unwrap_or(u32::MAX),
                timestamp_ns,
            );

            // memories
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                t.insert(&memory_id.to_be_bytes(), &mem)
                    .map_err(transient)?;
            }

            // texts
            {
                let mut t = wtxn.open_table(TEXTS_TABLE).map_err(transient)?;
                t.insert(&memory_id.to_be_bytes(), p.text.as_bytes())
                    .map_err(transient)?;
            }

            // idempotency
            {
                let entry = IdempotencyEntry::new(
                    response_kind::ENCODE,
                    Some(memory_id.to_be_bytes()),
                    /* response_payload */ Vec::new(),
                    /* request_hash */ [0u8; 32],
                    timestamp_ns,
                );
                let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).map_err(transient)?;
                t.insert(&<[u8; 16]>::from(p.request_id), &entry)
                    .map_err(transient)?;
            }

            // model_fingerprints — insert if absent.
            {
                let mut t = wtxn
                    .open_table(MODEL_FINGERPRINTS_TABLE)
                    .map_err(transient)?;
                if t.get(&p.embedding_model_fp).map_err(transient)?.is_none() {
                    let info = ModelInfo::new(String::new(), timestamp_ns);
                    t.insert(&p.embedding_model_fp, &info).map_err(transient)?;
                }
            }

            // edges
            if !p.edges.is_empty() {
                let mut out = wtxn.open_table(EDGES_OUT_TABLE).map_err(transient)?;
                let mut in_ = wtxn.open_table(EDGES_IN_TABLE).map_err(transient)?;
                for e in &p.edges {
                    let data = edge_payload_to_data(e, timestamp_ns);
                    edge::link(&mut out, &mut in_, e.source, e.kind, e.target, &data)
                        .map_err(transient)?;
                }
            }

            // slot_versions — direct insert with the WAL-recorded version
            // (recovery replays the version verbatim; we don't use the
            // `increment` helper).
            {
                let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).map_err(transient)?;
                t.insert(&slot_id, &slot_version).map_err(transient)?;
            }

            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_forget(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &ForgetPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();

            // Update memory: set HARD_FORGOTTEN flag + forgot_at.
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    mem.flags |= flags::HARD_FORGOTTEN;
                    mem.forgot_at_unix_nanos = Some(timestamp_ns);
                    t.insert(&key, &mem).map_err(transient)?;
                }
            }

            // Idempotency entry.
            {
                let entry = IdempotencyEntry::new(
                    response_kind::FORGET,
                    Some(key),
                    Vec::new(),
                    [0u8; 32],
                    timestamp_ns,
                );
                let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).map_err(transient)?;
                t.insert(&<[u8; 16]>::from(p.request_id), &entry)
                    .map_err(transient)?;
            }

            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_link(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &LinkPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let data = EdgeData::new(
                p.weight,
                p.origin as u8,
                edge::derived_by::CLIENT,
                timestamp_ns,
            );
            {
                let mut out = wtxn.open_table(EDGES_OUT_TABLE).map_err(transient)?;
                let mut in_ = wtxn.open_table(EDGES_IN_TABLE).map_err(transient)?;
                edge::link(&mut out, &mut in_, p.source, p.edge_kind, p.target, &data)
                    .map_err(transient)?;
            }

            // No RequestId in LinkPayload, so no idempotency entry.
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_unlink(
        &mut self,
        lsn: u64,
        _timestamp_ns: u64,
        p: &UnlinkPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            {
                let mut out = wtxn.open_table(EDGES_OUT_TABLE).map_err(transient)?;
                let mut in_ = wtxn.open_table(EDGES_IN_TABLE).map_err(transient)?;
                edge::unlink(&mut out, &mut in_, p.source, p.edge_kind, p.target)
                    .map_err(transient)?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_update_salience(
        &mut self,
        lsn: u64,
        p: &UpdateSaliencePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
            for u in &p.updates {
                update_one_salience(&mut t, u)?;
            }
            drop(t);
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_reclaim(&mut self, lsn: u64, p: &ReclaimPayload) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // O(1) delete: `ReclaimPayload.memory_id` carries the row's
            // primary key directly (SD-3.11-3 supersedes the deferred
            // SD-3.11-2 scan path).
            let key = p.memory_id.to_be_bytes();
            {
                let mut mems = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                mems.remove(&key).map_err(transient)?;
            }
            {
                let mut texts = wtxn.open_table(TEXTS_TABLE).map_err(transient)?;
                texts.remove(&key).map_err(transient)?;
            }

            // Advance slot_versions to the WAL's recorded new_version.
            {
                let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).map_err(transient)?;
                t.insert(&p.slot_id, &p.new_version).map_err(transient)?;
            }

            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_consolidate(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &ConsolidatePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let memory_id = p.new_memory_id;
            let slot_id = memory_id.slot();
            let slot_version = memory_id.version();

            // For Consolidated memories: the source-derived agent_id /
            // context_id aren't in the payload. Spec §11/02 describes
            // consolidation as agent-scoped — every source shares an
            // agent. v1 storage uses the brain-core NULL sentinels;
            // wire layer (Phase 9) populates these via a richer
            // payload. SD-3.11-3-style placeholder, but doesn't need a
            // logged deviation since it's pure recovery-time fill.
            let mem = MemoryMetadata {
                memory_id_bytes: memory_id.to_be_bytes(),
                agent_id_bytes: <[u8; 16]>::from(AgentId::default()),
                context_id: ContextId::default().raw(),
                slot_id,
                slot_version,
                kind: memory_kind_to_u8(MemoryKind::Consolidated),
                text_size: u32::try_from(p.text.len()).unwrap_or(u32::MAX),
                created_at_unix_nanos: timestamp_ns,
                last_accessed_at_unix_nanos: timestamp_ns,
                forgot_at_unix_nanos: None,
                tombstoned_at_unix_nanos: None,
                consolidated_at_unix_nanos: Some(timestamp_ns),
                salience: 0.5,
                salience_initial: 0.5,
                access_count: 0,
                embedding_model_fp: p.embedding_model_fp,
                flags: flags::ACTIVE,
                edges_out_count: 0,
                edges_in_count: 0,
            };

            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                t.insert(&memory_id.to_be_bytes(), &mem)
                    .map_err(transient)?;
            }
            {
                let mut t = wtxn.open_table(TEXTS_TABLE).map_err(transient)?;
                t.insert(&memory_id.to_be_bytes(), p.text.as_bytes())
                    .map_err(transient)?;
            }
            {
                let mut t = wtxn.open_table(SLOT_VERSIONS_TABLE).map_err(transient)?;
                t.insert(&slot_id, &slot_version).map_err(transient)?;
            }

            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_update_kind(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &UpdateKindPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    mem.kind = memory_kind_to_u8(p.new_kind);
                    t.insert(&key, &mem).map_err(transient)?;
                }
            }
            // No RequestId in UpdateKindPayload — skip idempotency table.
            let _ = timestamp_ns; // currently unused; reserved for future audit
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_update_context(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &UpdateContextPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    mem.context_id = p.new_context_id.raw();
                    t.insert(&key, &mem).map_err(transient)?;
                }
            }
            let _ = timestamp_ns;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_migrate_embedding(
        &mut self,
        lsn: u64,
        p: &MigrateEmbeddingPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    mem.embedding_model_fp = p.new_fingerprint;
                    t.insert(&key, &mem).map_err(transient)?;
                }
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    fn apply_checkpoint_end(
        &mut self,
        lsn: u64,
        timestamp_ns: u64,
        p: &brain_storage::wal::payload::CheckpointEndPayload,
    ) -> Result<(), MetadataSinkError> {
        // Pair with the matching CheckpointBegin's `started_at`. If
        // no BEGIN was seen (e.g. recovery resumed after a crash that
        // landed between BEGIN and END), use 0 as a sentinel — the
        // row is still useful for `latest()` because `durable_lsn`
        // is authoritative.
        let started_at = self
            .pending_checkpoints
            .remove(&p.checkpoint_id)
            .unwrap_or(0);

        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let meta = CheckpointMeta::new(
                p.checkpoint_id,
                p.durable_lsn,
                p.arena_capacity,
                u64::from(CURRENT_SCHEMA_VERSION),
                started_at,
                timestamp_ns,
            );
            {
                let mut t = wtxn.open_table(CHECKPOINTS_TABLE).map_err(transient)?;
                t.insert(&p.checkpoint_id, &meta).map_err(transient)?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;

        // Advance cached durable_lsn (also returned by durable_lsn()).
        self.durable_lsn = self.durable_lsn.max(p.durable_lsn);
        Ok(())
    }

    // ---- Helpers --------------------------------------------------------

    /// Apply a `next_lsn[()] = max(current, lsn + 1)` update inside an
    /// existing write transaction.
    fn bump_next_lsn_in_txn(
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
    fn bump_next_lsn(&mut self, lsn: u64) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        wtxn.commit().map_err(transient)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free helpers.
// ---------------------------------------------------------------------------

fn edge_payload_to_data(e: &EdgePayload, timestamp_ns: u64) -> EdgeData {
    EdgeData::new(
        e.weight,
        e.origin as u8,
        edge::derived_by::CLIENT,
        timestamp_ns,
    )
}

fn update_one_salience(
    t: &mut redb::Table<'_, [u8; 16], MemoryMetadata>,
    u: &SalienceUpdate,
) -> Result<(), MetadataSinkError> {
    let key = u.memory_id.to_be_bytes();
    let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
    if let Some(mut mem) = existing {
        mem.salience = u.new_salience;
        t.insert(&key, &mem).map_err(transient)?;
    }
    Ok(())
}

fn transient<E: std::fmt::Display>(e: E) -> MetadataSinkError {
    MetadataSinkError::Transient(format!("{e}"))
}

// ---------------------------------------------------------------------------
// Suppress unused-import warning for types referenced only in the docs/
// match arms above. (`EdgeKey` is conditionally referenced via the
// edge::link signature; `EdgeKind` is used in payload conversion.)
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
        p.edges = vec![
            EdgePayload {
                source: p.memory_id,
                target: mid(2, 1),
                kind: EdgeKind::Caused,
                weight: 0.8,
                origin: EdgeOrigin::Explicit,
            },
            EdgePayload {
                source: p.memory_id,
                target: mid(3, 1),
                kind: EdgeKind::SimilarTo,
                weight: 0.6,
                origin: EdgeOrigin::AutoDerived,
            },
        ];
        db.apply(1, TS, &WalPayload::Encode(p)).unwrap();

        let rtxn = db.read_txn().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        // 1 Caused + 2 SimilarTo (direct + mirror) = 3 rows in edges_out.
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
                source: mid(1, 1),
                target: mid(2, 1),
                edge_kind: EdgeKind::Caused,
                weight: 0.9,
                origin: EdgeOrigin::Explicit,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        assert_eq!(out.iter().unwrap().count(), 1);
        assert_eq!(in_.iter().unwrap().count(), 1);
    }

    #[test]
    fn unlink_removes_both_edges() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = MetadataDb::open(db_path(&dir)).unwrap();
        let src = mid(1, 1);
        let tgt = mid(2, 1);
        db.apply(
            1,
            TS,
            &WalPayload::Link(LinkPayload {
                source: src,
                target: tgt,
                edge_kind: EdgeKind::Caused,
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
                edge_kind: EdgeKind::Caused,
                edge_seq: 0,
            }),
        )
        .unwrap();

        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            rtxn.open_table(EDGES_OUT_TABLE)
                .unwrap()
                .iter()
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            rtxn.open_table(EDGES_IN_TABLE)
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
        // memories table should be absent (no domain writes).
        match rtxn.open_table(MEMORIES_TABLE) {
            Ok(t) => assert_eq!(t.iter().unwrap().count(), 0),
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => panic!("unexpected: {e:?}"),
        }
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
}
