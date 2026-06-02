//! Recovery apply paths for typed-relation WAL payloads.
//!
//! Covers:
//! - [`RelationLinkPayload`] — create a typed relation (edge + sidecar + evidence).
//! - [`RelationSupersedePayload`] — flip the old sidecar, then write the new one as a fresh create.
//! - [`RelationTombstonePayload`] — flip `tombstoned` / `is_current` on the sidecar; edge stays.
//!
//! Typed relations carry a sidecar row in
//! [`RELATION_METADATA_TABLE`] and an evidence reverse-index row per
//! evidence memory in [`RELATION_BY_EVIDENCE_TABLE`]. Recovery never
//! re-enforces cardinality — the live writer did so before the WAL
//! append; replay is pure projection.

use brain_core::EdgeKindRef;
use brain_storage::recovery::MetadataSinkError;
use brain_storage::wal::payload::{
    RelationLinkPayload, RelationSupersedePayload, RelationTombstonePayload,
};
use redb::{ReadableTable, WriteTransaction};

use crate::db::MetadataDb;
use crate::tables::edge::{self, derived_by, origin, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE};
use crate::tables::relation::{
    RelationMetadata, RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE,
};

use super::transient;

impl MetadataDb {
    pub(super) fn apply_relation_link(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &RelationLinkPayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            write_relation_link(&wtxn, p, timestamp_ns)?;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    pub(super) fn apply_relation_supersede(
        &self,
        lsn: u64,
        timestamp_ns: u64,
        p: &RelationSupersedePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // Flip the old sidecar row first — is_current = 0,
            // superseded_by = new id, valid_to defaults to the new
            // relation's extracted_at when the old row didn't pin one
            // explicitly. The edge row of the old relation stays in
            // place; relation history walks the sidecar chain.
            let old_key = p.old_relation_id.to_bytes();
            let mut sidecar = wtxn
                .open_table(RELATION_METADATA_TABLE)
                .map_err(transient)?;
            let mut old = sidecar
                .get(&old_key)
                .map_err(transient)?
                .map(|g| g.value())
                .ok_or_else(|| {
                    MetadataSinkError::Corruption(format!(
                        "relation_supersede: missing sidecar for {:?}",
                        p.old_relation_id
                    ))
                })?;
            old.superseded_by_bytes = Some(p.new.relation_id.to_bytes());
            if old.valid_to_unix_nanos.is_none() {
                old.valid_to_unix_nanos = Some(timestamp_ns);
            }
            old.is_current = 0;
            sidecar.insert(&old_key, &old).map_err(transient)?;
            drop(sidecar);

            // Replay the new relation insert exactly as a fresh
            // create. The WAL captured the post-supersession version /
            // chain_root / supersedes inline so we just project it.
            write_relation_link(&wtxn, &p.new, timestamp_ns)?;

            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    pub(super) fn apply_relation_tombstone(
        &self,
        lsn: u64,
        p: &RelationTombstonePayload,
    ) -> Result<(), MetadataSinkError> {
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let key = p.relation_id.to_bytes();
            {
                let mut sidecar = wtxn
                    .open_table(RELATION_METADATA_TABLE)
                    .map_err(transient)?;
                let mut row = sidecar
                    .get(&key)
                    .map_err(transient)?
                    .map(|g| g.value())
                    .ok_or_else(|| {
                        MetadataSinkError::Corruption(format!(
                            "relation_tombstone: missing sidecar for {:?}",
                            p.relation_id
                        ))
                    })?;
                row.tombstoned = 1;
                row.tombstoned_at_unix_nanos = Some(p.at_unix_nanos);
                row.is_current = 0;
                sidecar.insert(&key, &row).map_err(transient)?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }
}

/// Replay one [`RelationLinkPayload`] inside the caller's write txn.
/// Writes:
///   1. unified-edge row (and mirror for symmetric self-distinct
///      typed relations),
///   2. sidecar [`RelationMetadata`] keyed by the relation id,
///   3. one row per evidence memory in `RELATION_BY_EVIDENCE_TABLE`.
///
/// Recovery never re-runs cardinality enforcement here — the live
/// writer already enforced the rule before the WAL append; replay is
/// pure projection. Caller commits.
fn write_relation_link(
    wtxn: &WriteTransaction,
    p: &RelationLinkPayload,
    now_unix_nanos: u64,
) -> Result<(), MetadataSinkError> {
    // Schemaless path: the relation type wasn't interned at WAL-append
    // time, so re-resolve it here. Deterministic in LSN order; idempotent.
    let relation_type_id = match &p.relation_type_intern_hint {
        None => p.relation_type_id,
        Some((namespace, name)) => crate::relation::types::relation_type_intern_or_get(
            wtxn,
            namespace,
            name,
            /* first_seen_lsn */ 0,
            now_unix_nanos,
        )
        .map_err(|e| {
            MetadataSinkError::Corruption(format!("relation_type_intern_or_get: {e}"))
        })?,
    };

    // Edge row(s). The auto-mirror split mirrors `relation_ops`:
    // symmetric typed relations write the mirror explicitly here so
    // the `is_symmetric` bit stays sidecar-local. Substrate auto-
    // mirror in `edge::link` is reserved for Builtin kinds and never
    // fires for Typed.
    {
        let mut edges = wtxn.open_table(EDGES_TABLE).map_err(transient)?;
        let mut reverse = wtxn.open_table(EDGES_REVERSE_TABLE).map_err(transient)?;
        let data = EdgeData::new(
            1.0,
            origin::AUTO_DERIVED,
            derived_by::CLIENT,
            now_unix_nanos,
        );
        let kind = EdgeKindRef::Typed(relation_type_id);
        edge::link(
            &mut edges,
            &mut reverse,
            p.from,
            kind,
            p.to,
            p.relation_id.to_bytes(),
            &data,
        )
        .map_err(transient)?;
        if p.is_symmetric && p.from != p.to {
            edge::link(
                &mut edges,
                &mut reverse,
                p.to,
                kind,
                p.from,
                p.relation_id.to_bytes(),
                &data,
            )
            .map_err(transient)?;
        }
    }

    // Sidecar. The WAL payload carries every field we need; the
    // version starts at 1 for a non-supersede create and increments
    // through the supersession chain. `is_current = 1` because a
    // tombstone or supersede arrives as its own WAL record later.
    let evidence_inline: Vec<[u8; 16]> = p.evidence.iter().map(|m| m.to_be_bytes()).collect();
    let chain_root_bytes = if p.supersedes.is_some() {
        p.chain_root.to_bytes()
    } else {
        // A root relation's chain_root equals its own id. The writer
        // sets it that way and the WAL carries it verbatim, but we
        // self-heal here when the payload's `chain_root` is left as
        // the default sentinel.
        if p.chain_root.to_bytes() == [0u8; 16] {
            p.relation_id.to_bytes()
        } else {
            p.chain_root.to_bytes()
        }
    };
    let version = match p.supersedes {
        Some(_) => 2,
        None => 1,
    };
    let meta = RelationMetadata {
        from_tag: p.from.tag(),
        from_bytes: p.from.id_bytes(),
        to_tag: p.to.tag(),
        to_bytes: p.to.id_bytes(),
        relation_type_id: relation_type_id.raw(),
        chain_root_bytes,
        properties_blob: p.properties_blob.clone(),
        version,
        confidence: p.confidence,
        extractor_id: p.extractor_id,
        extracted_at_unix_nanos: now_unix_nanos,
        valid_from_unix_nanos: p.valid_from_unix_nanos,
        valid_to_unix_nanos: p.valid_to_unix_nanos,
        superseded_by_bytes: None,
        supersedes_bytes: p.supersedes.map(|r| r.to_bytes()),
        evidence_inline,
        tombstoned: 0,
        tombstoned_at_unix_nanos: None,
        is_current: 1,
        is_symmetric: u8::from(p.is_symmetric),
        flags: 0,
    };
    {
        let mut t = wtxn
            .open_table(RELATION_METADATA_TABLE)
            .map_err(transient)?;
        t.insert(&p.relation_id.to_bytes(), &meta)
            .map_err(transient)?;
    }

    // Evidence reverse index.
    {
        let mut t = wtxn
            .open_table(RELATION_BY_EVIDENCE_TABLE)
            .map_err(transient)?;
        for mem in &p.evidence {
            t.insert(&(mem.to_be_bytes(), p.relation_id.to_bytes()), &())
                .map_err(transient)?;
        }
    }

    Ok(())
}
