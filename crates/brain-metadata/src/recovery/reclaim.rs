//! Recovery apply paths for slot-recycling and consolidation payloads.
//!
//! Covers:
//! - [`ReclaimPayload`] — slot recycle: delete memory + text, bump slot_version.
//! - [`ConsolidatePayload`] — insert a freshly-summarised Consolidated
//!   memory row (the originating sources stay live until their own
//!   tombstone records run).
//!
//! Both share the property that they leave the arena slot in a new
//! version; recovery just lays down the post-state.

use brain_core::{AgentId, ContextId, MemoryKind};
use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::{ConsolidatePayload, ReclaimPayload};

use crate::db::MetadataDb;
use crate::tables::memory::{flags, memory_kind_to_u8, MemoryMetadata, MEMORIES_TABLE};
use crate::tables::slot_version::SLOT_VERSIONS_TABLE;
use crate::tables::text::TEXTS_TABLE;

use super::transient;

impl MetadataDb {
    pub(super) fn apply_reclaim(
        &self,
        lsn: u64,
        p: &ReclaimPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // O(1) delete: `ReclaimPayload.memory_id` carries the row's
            // primary key directly, so no scan is needed.
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

    pub(super) fn apply_consolidate(
        &self,
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
            // context_id aren't in the payload. Consolidation is
            // agent-scoped — every source shares an agent. Storage uses
            // the brain-core NULL sentinels here; the wire layer
            // populates these via a richer payload. This is a pure
            // recovery-time fill.
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
                // Consolidated rows synthesise content from sources; they
                // carry no client-supplied event time.
                occurred_at_unix_nanos: None,
                salience: 0.5,
                salience_initial: 0.5,
                access_count: 0,
                embedding_model_fp: p.embedding_model_fp,
                flags: flags::ACTIVE,
                edges_out_count: 0,
                edges_in_count: 0,
                // WAL-recovered Consolidated rows have no dedup
                // back-reference — consolidation never opts into
                // fingerprint dedup (see consolidation.rs).
                content_hash: None,
                // Recovery threads the WAL LSN through so a future
                // RECALL of a consolidated row can resume subscribe
                // from the moment consolidation ran.
                encoded_at_lsn: lsn,
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
}
