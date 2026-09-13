//! Recovery apply path for checkpoint pairing.
//!
//! `CheckpointBegin` is handled inline in [`super`]'s dispatcher: it
//! only updates `MetadataDb::pending_checkpoints` (in-memory) and
//! bumps `next_lsn`. No file in this module is needed for that.
//!
//! `apply_checkpoint_end` (this
//! file) pairs an incoming `CheckpointEnd` with its earlier `BEGIN`
//! and writes a [`CheckpointMeta`] row. It also advances the cached
//! `durable_lsn` so future [`MetadataSink::durable_lsn`](
//! brain_storage::recovery::MetadataSink::durable_lsn) calls see the
//! new watermark without re-reading the table.

use std::sync::atomic::Ordering;

use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::CheckpointEndPayload;

use crate::db::MetadataDb;
use crate::storage_version::CURRENT_SCHEMA_VERSION;
use crate::tables::checkpoint::{CheckpointMeta, CHECKPOINTS_TABLE};

use super::transient;

impl MetadataDb {
    pub(super) fn apply_checkpoint_end(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &CheckpointEndPayload,
    ) -> Result<(), MetadataSinkError> {
        // Pair with the matching CheckpointBegin's `started_at`. If
        // no BEGIN was seen (e.g. recovery resumed after a crash that
        // landed between BEGIN and END), use 0 as a sentinel — the
        // row is still useful for `latest()` because `durable_lsn`
        // is authoritative.
        let started_at = self
            .pending_checkpoints
            .lock()
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
        // fetch_max guarantees monotonicity even if another apply
        // path races us (the writer task is single-threaded today,
        // but the atomic stays correct under future evolution).
        self.durable_lsn.fetch_max(p.durable_lsn, Ordering::AcqRel);
        Ok(())
    }
}
