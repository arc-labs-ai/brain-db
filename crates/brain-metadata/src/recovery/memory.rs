//! Recovery apply paths for memory-row WAL payloads.
//!
//! Covers:
//! - [`EncodePayload`] — insert memory + text + idempotency + fingerprint + edges
//! - [`ForgetPayload`] — flag HARD_FORGOTTEN, evict dedup fingerprint
//! - [`UpdateSaliencePayload`] — batched salience writes
//! - [`UpdateKindPayload`] — change a memory's kind
//! - [`UpdateContextPayload`] — change a memory's context_id
//! - [`MigrateEmbeddingPayload`] — swap the embedding fingerprint (re-encode)
//!
//! Every helper opens its own write txn, applies, calls
//! `MetadataDb::bump_next_lsn_in_txn`, then commits.

use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::{
    EncodePayload, ForgetPayload, MigrateEmbeddingPayload, SalienceUpdate, UpdateContextPayload,
    UpdateKindPayload, UpdateSaliencePayload,
};
use redb::ReadableTable;

use crate::db::MetadataDb;
use crate::tables::edge::{self, zero_disambiguator, EDGES_REVERSE_TABLE, EDGES_TABLE};
use crate::tables::fingerprint::{
    content_hash as fp_content_hash, fingerprint_key, FingerprintEntry, FINGERPRINTS_TABLE,
};
use crate::tables::idempotency::{response_kind, IdempotencyEntry, IDEMPOTENCY_TABLE};
use crate::tables::memory::{flags, memory_kind_to_u8, MemoryMetadata, MEMORIES_TABLE};
use crate::tables::model_fingerprint::{ModelInfo, MODEL_FINGERPRINTS_TABLE};
use crate::tables::slot_version::SLOT_VERSIONS_TABLE;
use crate::tables::text::TEXTS_TABLE;

use super::{edge_payload_to_data, transient};

impl MetadataDb {
    pub(super) fn apply_encode(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &EncodePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let memory_id = p.memory_id;
            let slot_id = memory_id.slot();
            let slot_version = memory_id.version();

            // Stamp the dedup back-reference on the memory row when the
            // originating ENCODE opted in. Forget reads it to evict the
            // matching FINGERPRINTS entry in the same write txn.
            let mut mem = MemoryMetadata::new_active(
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
            )
            // Stamp the replayed-from LSN so the rebuilt row carries
            // the same provenance the live writer would have written.
            .with_encoded_at_lsn(lsn)
            // Carry the client-supplied event time through replay so the
            // rebuilt row keeps the same timeline the live write stored.
            .with_occurred_at(p.occurred_at_unix_nanos);
            let content_hash = if p.deduplicate {
                let h = fp_content_hash(&p.text);
                mem.content_hash = Some(h);
                Some(h)
            } else {
                None
            };

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

            // idempotency — populated from the WAL payload so a retry
            // after restart with the same request_id returns the original
            // response bytes; mismatching params surface as Conflict.
            // Persist the replayed-from LSN so a post-restart retry can
            // chain `subscribe --start-lsn=lsn+1` against the same
            // durable position the original write reached.
            {
                let entry = IdempotencyEntry::new(
                    response_kind::ENCODE,
                    Some(memory_id.to_be_bytes()),
                    p.response_payload.clone(),
                    p.request_hash,
                    timestamp_ns,
                    lsn,
                );
                let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).map_err(transient)?;
                t.insert(&<[u8; 16]>::from(p.request_id), &entry)
                    .map_err(transient)?;
            }

            // fingerprints — restore the dedup index for opt-in ENCODEs
            // so future ENCODE+dedup requests for the same text in the
            // same (agent, context) collapse onto the existing memory.
            if let Some(hash) = content_hash {
                let key = fingerprint_key(p.agent_id, p.context_id, &hash);
                let entry = FingerprintEntry::new(memory_id, timestamp_ns);
                let mut t = wtxn.open_table(FINGERPRINTS_TABLE).map_err(transient)?;
                t.insert(&key, &entry).map_err(transient)?;
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
                let mut out = wtxn.open_table(EDGES_TABLE).map_err(transient)?;
                let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).map_err(transient)?;
                for e in &p.edges {
                    let data = edge_payload_to_data(e, timestamp_ns);
                    edge::link(
                        &mut out,
                        &mut rev,
                        e.source,
                        e.kind,
                        e.target,
                        zero_disambiguator(),
                        &data,
                    )
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

    pub(super) fn apply_forget(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &ForgetPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();

            // Update memory: set HARD_FORGOTTEN flag + forgot_at. Capture
            // (agent, context, hash) for the matching FINGERPRINTS row so
            // we can evict it in the same write txn (—
            // the dedup index must never reference a forgotten memory).
            let dedup_key: Option<(brain_core::AgentId, brain_core::ContextId, [u8; 32])> = {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    let captured = mem.content_hash.map(|h| {
                        (
                            brain_core::AgentId::from(mem.agent_id_bytes),
                            brain_core::ContextId(mem.context_id),
                            h,
                        )
                    });
                    mem.flags |= flags::HARD_FORGOTTEN;
                    mem.forgot_at_unix_nanos = Some(timestamp_ns);
                    mem.content_hash = None;
                    t.insert(&key, &mem).map_err(transient)?;
                    captured
                } else {
                    None
                }
            };

            // Evict FINGERPRINTS row in the same txn as the tombstone so
            // a concurrent ENCODE+dedup can't observe a stale hit.
            if let Some((agent, ctx, hash)) = dedup_key {
                let fp_key = fingerprint_key(agent, ctx, &hash);
                let mut t = wtxn.open_table(FINGERPRINTS_TABLE).map_err(transient)?;
                t.remove(&fp_key).map_err(transient)?;
            }

            // Idempotency entry.
            {
                let entry = IdempotencyEntry::new(
                    response_kind::FORGET,
                    Some(key),
                    Vec::new(),
                    [0u8; 32],
                    timestamp_ns,
                    lsn,
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

    pub(super) fn apply_update_salience(
        &self,
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

    pub(super) fn apply_update_kind(
        &self,
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

    pub(super) fn apply_update_context(
        &self,
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

    pub(super) fn apply_migrate_embedding(
        &self,
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
}

/// Apply one [`SalienceUpdate`] inside the caller's already-open
/// memories table. Missing memory rows are silently skipped — the
/// WAL may carry a salience update for a memory that was forgotten
/// in a later record, and we don't want recovery to fail on that.
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
