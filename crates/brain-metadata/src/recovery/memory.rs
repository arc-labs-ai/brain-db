//! Recovery apply paths for memory-row WAL payloads.
//!
//! Covers:
//! - [`EncodePayload`] — insert memory + text + idempotency + fingerprint + edges
//! - [`ForgetPayload`] — tombstone (clear ACTIVE + stamp tombstoned_at),
//!   drop timeline + dedup fingerprint; hard mode also sets HARD_FORGOTTEN
//!   and purges text + artifact + vector (converges with the live apply)
//! - [`UpdateSaliencePayload`] — batched salience writes
//! - [`UpdateKindPayload`] — change a memory's kind
//! - [`UpdateSessionPayload`] — change a memory's session_id
//! - [`MigrateEmbeddingPayload`] — swap the embedding fingerprint (re-encode)
//!
//! Every helper opens its own write txn, applies, calls
//! `MetadataDb::bump_next_lsn_in_txn`, then commits.

use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::{
    EncodePayload, ForgetMode, ForgetPayload, MigrateEmbeddingPayload, RestorePayload,
    SalienceUpdate, UpdateKindPayload, UpdateSaliencePayload, UpdateSessionPayload,
};
use redb::ReadableTable;

use crate::db::MetadataDb;
use crate::tables::edge::{self, zero_disambiguator, EDGES_REVERSE_TABLE, EDGES_TABLE};
use crate::tables::fingerprint::{
    content_hash as fp_content_hash, fingerprint_key, FingerprintEntry, FINGERPRINTS_TABLE,
};
use crate::tables::idempotency::{response_kind, IdempotencyEntry, IDEMPOTENCY_TABLE};
use crate::tables::memory::{
    flags, memory_kind_to_u8, space_timeline_key, MemoryMetadata, MEMORIES_BY_SPACE_TIMELINE_TABLE,
    MEMORIES_TABLE,
};
use crate::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use crate::tables::memory_vector::MEMORY_VECTORS_TABLE;
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
            // The owning namespace rides the WAL Encode payload, so recovery
            // rebuilds the row under its real tenant — never the SYSTEM
            // fallback — and cross-tenant isolation survives a restart.
            let mut mem = MemoryMetadata::new_active(
                memory_id,
                p.namespace_id,
                p.space_id,
                p.session_id,
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
            // same (space, session) collapse onto the existing memory.
            if let Some(hash) = content_hash {
                let key = fingerprint_key(p.space_id, p.session_id, &hash);
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

            // Registry — the same implicit space/session upsert the live
            // apply path runs, so the derived registry rows survive a crash
            // and replay cleanly (idempotent bump on re-replay).
            crate::registry::touch_on_write(
                &wtxn,
                p.namespace_id.raw(),
                p.space_id.into(),
                // The ENCODE WAL payload does not carry the human space
                // string; an implicit space's string is restored from its
                // own SpaceCreate record (if any). The registry is derived,
                // recomputable view state, so an empty string here is a
                // display-only gap the counter-reconcile worker can heal.
                "",
                p.session_id.raw(),
                timestamp_ns,
            )
            .map_err(|e| MetadataSinkError::Corruption(format!("registry touch: {e}")))?;

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
            let is_hard = p.mode == ForgetMode::Hard;

            // Converge on the exact redb end-state the live tombstone apply
            // produces (crates/brain-ops apply_tombstone_memory), so replay is
            // idempotent and indistinguishable from the live write:
            //
            //   Both modes: clear ACTIVE, stamp tombstoned_at, drop the
            //   timeline-index entry, evict the dedup FINGERPRINTS row.
            //   Hard mode only: additionally set HARD_FORGOTTEN and purge the
            //   text row + write-artifact bundle (vector + derived graph).
            //
            // The prior implementation set HARD_FORGOTTEN + forgot_at
            // unconditionally and never cleared ACTIVE — a crash before
            // checkpoint replayed the FORGET but left the row ACTIVE, so every
            // is_active()-filtered read resurrected the forgotten memory and
            // the never-stamped tombstoned_at blocked slot reclamation.
            //
            // Capture the timeline/fingerprint coordinates while the row is in
            // hand so the follow-up index deletes run in this same write txn.
            struct RowCoords {
                namespace_id: u32,
                space_id_bytes: [u8; 16],
                created_at_unix_nanos: u64,
                session_id: u64,
                memory_id_bytes: [u8; 16],
                content_hash: Option<[u8; 32]>,
            }
            let coords: Option<RowCoords> = {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    let captured = RowCoords {
                        namespace_id: mem.namespace_id,
                        space_id_bytes: mem.space_id_bytes,
                        created_at_unix_nanos: mem.created_at_unix_nanos,
                        session_id: mem.session_id,
                        memory_id_bytes: mem.memory_id_bytes,
                        content_hash: mem.content_hash,
                    };
                    mem.flags &= !flags::ACTIVE;
                    mem.tombstoned_at_unix_nanos = Some(timestamp_ns);
                    if is_hard {
                        mem.flags |= flags::HARD_FORGOTTEN;
                        mem.forgot_at_unix_nanos = Some(timestamp_ns);
                    }
                    t.insert(&key, &mem).map_err(transient)?;
                    Some(captured)
                } else {
                    None
                }
            };

            if let Some(c) = coords {
                // Drop the timeline-index entry — a tombstoned memory must not
                // surface as a temporal predecessor for future encodes.
                {
                    let mut t = wtxn
                        .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
                        .map_err(transient)?;
                    let tkey = space_timeline_key(
                        c.namespace_id,
                        c.space_id_bytes,
                        c.created_at_unix_nanos,
                        c.session_id,
                        c.memory_id_bytes,
                    );
                    let _ = t.remove(tkey.as_slice()).map_err(transient)?;
                }

                // Evict the dedup FINGERPRINTS row in the same txn as the
                // tombstone so a re-encode of the same text can't fold into
                // the dead memory. Live apply evicts it for both soft and hard
                // FORGET, so recovery does too.
                if let Some(hash) = c.content_hash {
                    let fp_key = fingerprint_key(
                        brain_core::SpaceId::from(c.space_id_bytes),
                        brain_core::SessionId(c.session_id),
                        &hash,
                    );
                    let mut t = wtxn.open_table(FINGERPRINTS_TABLE).map_err(transient)?;
                    let _ = t.remove(&fp_key).map_err(transient)?;
                }

                // Hard FORGET purges recoverable plaintext-derived data at
                // rest: the text row + the write-artifact bundle (embedding
                // vector + derived graph) + the raw by-id vector row. Soft
                // FORGET keeps them until slot reclamation runs after grace.
                if is_hard {
                    {
                        let mut t = wtxn.open_table(TEXTS_TABLE).map_err(transient)?;
                        let _ = t.remove(&key).map_err(transient)?;
                    }
                    {
                        let mut t = wtxn.open_table(MEMORY_ARTIFACTS_TABLE).map_err(transient)?;
                        let _ = t.remove(&key).map_err(transient)?;
                    }
                    {
                        let mut t = wtxn.open_table(MEMORY_VECTORS_TABLE).map_err(transient)?;
                        let _ = t.remove(&key).map_err(transient)?;
                    }
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

    pub(super) fn apply_restore_memory(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &RestorePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();

            // Invert exactly what the soft-FORGET redb end-state produced
            // (see `apply_forget`): re-set ACTIVE, drop `tombstoned_at`,
            // re-insert the timeline-index entry, and re-insert the dedup
            // FINGERPRINTS row. All of these are idempotent, so a re-replay
            // after a crash between commit and checkpoint is a structural
            // no-op — restoring an already-active memory changes nothing.
            //
            // A hard-forgotten row is never the subject of a RestoreMemory
            // record (hard forget is irreversible and purges the text +
            // artifact), so if we ever see one here we leave it untouched:
            // resurrecting the flags over purged data would be a zombie.
            struct RowCoords {
                namespace_id: u32,
                space_id_bytes: [u8; 16],
                created_at_unix_nanos: u64,
                session_id: u64,
                memory_id_bytes: [u8; 16],
                content_hash: Option<[u8; 32]>,
            }
            let coords: Option<RowCoords> = {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                match existing {
                    Some(mut mem) if mem.flags & flags::HARD_FORGOTTEN == 0 => {
                        let captured = RowCoords {
                            namespace_id: mem.namespace_id,
                            space_id_bytes: mem.space_id_bytes,
                            created_at_unix_nanos: mem.created_at_unix_nanos,
                            session_id: mem.session_id,
                            memory_id_bytes: mem.memory_id_bytes,
                            content_hash: mem.content_hash,
                        };
                        mem.flags |= flags::ACTIVE;
                        mem.tombstoned_at_unix_nanos = None;
                        t.insert(&key, &mem).map_err(transient)?;
                        Some(captured)
                    }
                    _ => None,
                }
            };

            if let Some(c) = coords {
                // Re-insert the timeline-index entry so the restored memory
                // is once again a temporal predecessor for future encodes.
                {
                    let mut t = wtxn
                        .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
                        .map_err(transient)?;
                    let tkey = space_timeline_key(
                        c.namespace_id,
                        c.space_id_bytes,
                        c.created_at_unix_nanos,
                        c.session_id,
                        c.memory_id_bytes,
                    );
                    t.insert(tkey.as_slice(), ()).map_err(transient)?;
                }

                // Restore the dedup FINGERPRINTS row the forget evicted, so a
                // re-encode of the same text once again folds onto this
                // memory. Only present when the original encode opted in.
                if let Some(hash) = c.content_hash {
                    let fp_key = fingerprint_key(
                        brain_core::SpaceId::from(c.space_id_bytes),
                        brain_core::SessionId(c.session_id),
                        &hash,
                    );
                    let entry = FingerprintEntry::new(p.memory_id, timestamp_ns);
                    let mut t = wtxn.open_table(FINGERPRINTS_TABLE).map_err(transient)?;
                    t.insert(&fp_key, &entry).map_err(transient)?;
                }
            }

            // Idempotency entry.
            {
                let entry = IdempotencyEntry::new(
                    response_kind::RESTORE,
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

    pub(super) fn apply_update_session(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &UpdateSessionPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.memory_id.to_be_bytes();
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).map_err(transient)?;
                let existing = t.get(&key).map_err(transient)?.map(|a| a.value());
                if let Some(mut mem) = existing {
                    mem.session_id = p.new_session_id.raw();
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
