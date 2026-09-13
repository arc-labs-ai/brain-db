//! Recovery apply paths for substrate-edge WAL payloads.
//!
//! Covers:
//! - [`LinkPayload`] — insert a single substrate edge (and reverse).
//! - [`UnlinkPayload`] — delete a single substrate edge (and reverse).
//!
//! Typed-relation edges (sidecar-bearing) live in
//! [`super::relation`]; this file is only for plain substrate edges
//! written by `LINK` / `UNLINK` ops.

use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::{LinkPayload, UnlinkPayload};

use crate::db::MetadataDb;
use crate::tables::edge::{self, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};

use super::transient;

impl MetadataDb {
    pub(super) fn apply_link(
        &self,
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
                let mut out = wtxn.open_table(EDGES_TABLE).map_err(transient)?;
                let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).map_err(transient)?;
                edge::link(
                    &mut out,
                    &mut rev,
                    p.source,
                    p.edge_kind,
                    p.target,
                    zero_disambiguator(),
                    &data,
                )
                .map_err(transient)?;
            }

            // No RequestId in LinkPayload, so no idempotency entry.
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    pub(super) fn apply_unlink(
        &self,
        lsn: u64,
        _timestamp_ns: u64,
        p: &UnlinkPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            {
                let mut out = wtxn.open_table(EDGES_TABLE).map_err(transient)?;
                let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).map_err(transient)?;
                edge::unlink(
                    &mut out,
                    &mut rev,
                    p.source,
                    p.edge_kind,
                    p.target,
                    zero_disambiguator(),
                )
                .map_err(transient)?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }
}
