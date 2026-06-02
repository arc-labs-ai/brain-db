//! Recovery apply paths for opaque-body WAL payloads carried in the
//! [`WalPayload::PhaseBody`](brain_storage::wal::payload::WalPayload::PhaseBody)
//! envelope.
//!
//! Each method decodes the body (selected by the record's
//! [`WalRecordKind`](brain_storage::wal::kinds::WalRecordKind)), then calls
//! the same brain-metadata helper the live writer used, so replay and the
//! original write converge on identical redb state. Each opens its own
//! write transaction and advances `next_lsn`, matching the relation
//! recovery paths.
//!
//! Replay is idempotent. Recovery re-runs every record above the
//! checkpoint `durable_lsn`; a crash mid-replay (before the next
//! checkpoint) re-applies the same records on restart. Where a helper
//! is not naturally re-runnable, the apply method guards on current
//! state (see `apply_entity_create`).

use brain_core::{Entity, EntityAttributes, EntityId, StatementId, TombstoneReason};
use brain_storage::recovery::MetadataSinkError;
use redb::{ReadableTable, WriteTransaction};

use crate::db::MetadataDb;
use crate::entity::merge::{merge_entity, unmerge_entity, MergeActor};
use crate::entity::ops::{
    entity_get_inside_wtxn, entity_put, entity_rename, entity_tombstone, entity_update,
    normalize_name,
};
use crate::extractor::ops::extractor_set_enabled;
use crate::recovery::phase_bodies::{
    decode_entity_create, decode_entity_merge, decode_entity_rename, decode_entity_tombstone,
    decode_entity_unmerge, decode_entity_update, decode_extractor_toggle, decode_schema_update,
    decode_statement_create, decode_statement_supersede, decode_statement_tombstone,
};
use crate::schema::store::{schema_get, schema_upload};
use brain_protocol::schema::{parse_schema, validate};
use crate::schema::predicate::predicate_intern_or_get;
use crate::statement::{statement_create, statement_supersede, statement_tombstone};
use crate::tables::statement::{
    statement_flags, statement_from_metadata, StatementMetadata, STATEMENTS_TABLE,
};

use super::transient;

impl MetadataDb {
    /// Replay an `EntityCreate` body: reconstruct the entity row and
    /// `entity_put` it (rebuilding the primary row + canonical-name,
    /// alias, and trigram indexes).
    pub(super) fn apply_entity_create(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let meta = decode_entity_create(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_create decode: {e}")))?;
        let entity = Entity::from(&meta);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // `entity_put` rejects a duplicate canonical name, so a naive
            // re-apply of an already-recovered create would error. On a
            // re-replay (crash between this commit and the next
            // checkpoint) the row is already present — skip it. In correct
            // LSN-ordered first-replay the row is absent, so this is a
            // no-op guard there.
            if entity_get_inside_wtxn(&wtxn, entity.id)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_create lookup: {e}")))?
                .is_none()
            {
                entity_put(&wtxn, &entity)
                    .map_err(|e| MetadataSinkError::Corruption(format!("entity_put: {e}")))?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `EntityTombstone` body via `entity_tombstone`.
    ///
    /// Idempotent on re-replay: the row persists after a tombstone (the
    /// flag is set, indexes are torn down), so re-running tears down
    /// already-absent index keys (a redb `remove` no-op) and re-sets the
    /// flag. A genuinely missing row is corruption — in correct
    /// LSN-ordered replay the lower-LSN create ran first — and surfaces
    /// as a fail-stop error rather than being silently skipped.
    pub(super) fn apply_entity_tombstone(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_entity_tombstone(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_tombstone decode: {e}")))?;
        let id = EntityId::from(b.id);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            entity_tombstone(&wtxn, id, b.at_unix_nanos)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_tombstone: {e}")))?;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `EntityUpdate` body: re-read the current row, apply the
    /// new canonical / aliases / attributes, then `entity_update`.
    pub(super) fn apply_entity_update(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_entity_update(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_update decode: {e}")))?;
        let id = EntityId::from(b.id);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let current = entity_get_inside_wtxn(&wtxn, id)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_update lookup: {e}")))?
                .ok_or_else(|| {
                    MetadataSinkError::Corruption(format!("entity_update: entity {id:?} absent"))
                })?;
            // Idempotent re-replay: `entity_update`'s alias-delta + canonical
            // index churn isn't naturally re-runnable, so skip when the row's
            // updated_at already reflects this (or a later) update.
            if current.updated_at_unix_nanos < b.at_unix_nanos {
                let mut next = current;
                next.canonical_name = b.canonical_name.clone();
                next.normalized_name = normalize_name(&b.canonical_name);
                next.aliases = b.aliases.clone();
                next.attributes = EntityAttributes::from(b.attributes_blob.clone());
                entity_update(&wtxn, &next, b.at_unix_nanos)
                    .map_err(|e| MetadataSinkError::Corruption(format!("entity_update: {e}")))?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay a `SchemaUpdate` body: re-parse + re-validate the DSL blob,
    /// then `schema_upload`. Idempotent: skips when (namespace, version)
    /// already exists (otherwise `schema_upload`'s auto-increment would
    /// mint a duplicate version on re-replay).
    pub(super) fn apply_schema_update(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_schema_update(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("schema_update decode: {e}")))?;
        let already = {
            let rtxn = self.read_txn().map_err(transient)?;
            schema_get(&rtxn, &b.namespace, b.version)
                .map_err(|e| MetadataSinkError::Corruption(format!("schema_get: {e}")))?
                .is_some()
        };
        if already {
            return self.bump_next_lsn(lsn);
        }
        let source = std::str::from_utf8(&b.blob)
            .map_err(|e| MetadataSinkError::Corruption(format!("schema blob not UTF-8: {e}")))?;
        let parsed = parse_schema(source)
            .map_err(|e| MetadataSinkError::Corruption(format!("schema re-parse: {e:?}")))?;
        let validated = validate(&parsed)
            .map_err(|errs| MetadataSinkError::Corruption(format!("schema re-validate: {errs:?}")))?;
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            schema_upload(&wtxn, &validated, b.created_at_unix_nanos)
                .map_err(|e| MetadataSinkError::Corruption(format!("schema_upload: {e}")))?;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `EntityMerge` body via `merge_entity` (survivor=target,
    /// merged=source), guarded by `merged_into` for re-replay safety.
    pub(super) fn apply_entity_merge(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_entity_merge(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_merge decode: {e}")))?;
        let survivor = EntityId::from(b.target);
        let merged = EntityId::from(b.source);
        let actor = if b.actor_kind == 0 {
            MergeActor::System
        } else {
            MergeActor::Agent(b.actor_agent)
        };
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // Idempotent re-replay: skip if `merged` already points at
            // `survivor` (the merge already ran).
            let already = entity_get_inside_wtxn(&wtxn, merged)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_merge lookup: {e}")))?
                .and_then(|e| e.merged_into)
                == Some(survivor);
            if !already {
                merge_entity(
                    &wtxn,
                    survivor,
                    merged,
                    b.confidence,
                    b.reason.clone(),
                    actor,
                    b.grace_seconds,
                    b.at_unix_nanos,
                )
                .map_err(|e| MetadataSinkError::Corruption(format!("merge_entity: {e}")))?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `EntityUnmerge` body via `unmerge_entity` (guarded by
    /// `merged_into` so a re-replay of an already-unmerged entity skips).
    pub(super) fn apply_entity_unmerge(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_entity_unmerge(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_unmerge decode: {e}")))?;
        let merged = EntityId::from(b.merged);
        let actor = if b.actor_kind == 0 {
            MergeActor::System
        } else {
            MergeActor::Agent(b.actor_agent)
        };
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let still_merged = entity_get_inside_wtxn(&wtxn, merged)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_unmerge lookup: {e}")))?
                .and_then(|e| e.merged_into)
                .is_some();
            if still_merged {
                unmerge_entity(&wtxn, merged, actor, b.at_unix_nanos)
                    .map_err(|e| MetadataSinkError::Corruption(format!("unmerge_entity: {e}")))?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `ExtractorToggle` body via `extractor_set_enabled`
    /// (idempotent — setting the flag to the same value is a no-op).
    pub(super) fn apply_extractor_toggle(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_extractor_toggle(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("extractor_toggle decode: {e}")))?;
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            extractor_set_enabled(&wtxn, brain_core::ExtractorId::from(b.id), b.enabled)
                .map_err(|e| MetadataSinkError::Corruption(format!("extractor_set_enabled: {e}")))?;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay an `EntityRename` body via `entity_rename` (alias-trail).
    pub(super) fn apply_entity_rename(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_entity_rename(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("entity_rename decode: {e}")))?;
        let id = EntityId::from(b.id);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            let current = entity_get_inside_wtxn(&wtxn, id)
                .map_err(|e| MetadataSinkError::Corruption(format!("entity_rename lookup: {e}")))?
                .ok_or_else(|| {
                    MetadataSinkError::Corruption(format!("entity_rename: entity {id:?} absent"))
                })?;
            // Idempotent re-replay: skip if already renamed to this name
            // (entity_rename's alias-trail move isn't re-runnable).
            if current.canonical_name != b.new_canonical_name {
                entity_rename(&wtxn, id, b.new_canonical_name.clone(), b.at_unix_nanos)
                    .map_err(|e| MetadataSinkError::Corruption(format!("entity_rename: {e}")))?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay a `StatementCreate` body. Re-resolves the predicate when the
    /// schemaless intern hint is present (deterministic in LSN order),
    /// then `statement_create`s the row and stamps `IMPLICIT_PREDICATE` —
    /// mirroring the live apply path.
    pub(super) fn apply_statement_create(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_statement_create(body)
            .map_err(|e| MetadataSinkError::Corruption(format!("statement_create decode: {e}")))?;
        let mut s = statement_from_metadata(&b.meta).ok_or_else(|| {
            MetadataSinkError::Corruption(
                "statement_create: statement_from_metadata returned None".into(),
            )
        })?;
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // Idempotent re-replay: skip if the row is already present.
            let already = {
                let t = wtxn.open_table(STATEMENTS_TABLE).map_err(transient)?;
                let g = t.get(&s.id.to_bytes()).map_err(transient)?;
                g.is_some()
            };
            if !already {
                if let Some((namespace, name)) = &b.predicate_intern_hint {
                    let pid = predicate_intern_or_get(
                        &wtxn,
                        namespace,
                        name,
                        /* first_seen_lsn */ 0,
                        s.extracted_at_unix_nanos,
                    )
                    .map_err(|e| {
                        MetadataSinkError::Corruption(format!("predicate_intern_or_get: {e}"))
                    })?;
                    s.predicate = pid;
                }
                statement_create(&wtxn, &s, s.extracted_at_unix_nanos).map_err(|e| {
                    MetadataSinkError::Corruption(format!("statement_create: {e}"))
                })?;
                if b.predicate_intern_hint.is_some() {
                    stamp_implicit_predicate(&wtxn, s.id)?;
                }
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay a `StatementSupersede` body: flip the old row's chain and
    /// write the new statement, via `statement_supersede`.
    pub(super) fn apply_statement_supersede(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_statement_supersede(body).map_err(|e| {
            MetadataSinkError::Corruption(format!("statement_supersede decode: {e}"))
        })?;
        let new_s = statement_from_metadata(&b.new).ok_or_else(|| {
            MetadataSinkError::Corruption(
                "statement_supersede: statement_from_metadata returned None".into(),
            )
        })?;
        let old_id = StatementId::from(b.old_id);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            // Idempotent re-replay: the new statement already existing
            // means the supersession (including the old row's flip) already
            // ran.
            let already = {
                let t = wtxn.open_table(STATEMENTS_TABLE).map_err(transient)?;
                let g = t.get(&new_s.id.to_bytes()).map_err(transient)?;
                g.is_some()
            };
            if !already {
                statement_supersede(&wtxn, old_id, &new_s, b.at_unix_nanos).map_err(|e| {
                    MetadataSinkError::Corruption(format!("statement_supersede: {e}"))
                })?;
            }
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }

    /// Replay a `StatementTombstone` body via `statement_tombstone`.
    pub(super) fn apply_statement_tombstone(
        &self,
        lsn: u64,
        body: &[u8],
    ) -> Result<(), MetadataSinkError> {
        let b = decode_statement_tombstone(body).map_err(|e| {
            MetadataSinkError::Corruption(format!("statement_tombstone decode: {e}"))
        })?;
        let id = StatementId::from(b.id);
        let reason = TombstoneReason::from_u8(b.reason).unwrap_or(TombstoneReason::UserRequest);
        let wtxn = self.db.begin_write().map_err(transient)?;
        {
            statement_tombstone(&wtxn, id, reason, b.at_unix_nanos).map_err(|e| {
                MetadataSinkError::Corruption(format!("statement_tombstone: {e}"))
            })?;
            self.bump_next_lsn_in_txn(&wtxn, lsn)?;
        }
        wtxn.commit().map_err(transient)?;
        Ok(())
    }
}

/// OR the `IMPLICIT_PREDICATE` flag into a just-created statement row,
/// inside the caller's write txn. Mirrors the live apply path's
/// schemaless-create stamp.
fn stamp_implicit_predicate(
    wtxn: &WriteTransaction,
    id: StatementId,
) -> Result<(), MetadataSinkError> {
    let key = id.to_bytes();
    let existing: Option<StatementMetadata> = {
        let t = wtxn.open_table(STATEMENTS_TABLE).map_err(transient)?;
        let g = t.get(&key).map_err(transient)?;
        g.map(|guard| guard.value())
    };
    if let Some(mut row) = existing {
        if row.set_flag(statement_flags::IMPLICIT_PREDICATE) {
            let mut t = wtxn.open_table(STATEMENTS_TABLE).map_err(transient)?;
            t.insert(&key, &row).map_err(transient)?;
        }
    }
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use brain_core::{Entity, EntityId, EntityType};
    use tempfile::TempDir;

    use crate::db::MetadataDb;
    use crate::entity::ops::{entity_get, entity_put, normalize_name};
    use crate::recovery::phase_bodies::{
        encode_entity_create, encode_entity_tombstone, EntityTombstoneBody,
    };
    use crate::tables::entity::{flags, EntityMetadata};

    const NOW: u64 = 1_700_000_000_000_000_000;

    /// Open a fresh `MetadataDb`, which seeds the Person type at id=1 via
    /// the system-schema bootstrap — so `entity_put`'s
    /// `require_entity_type_exists` check passes without manual seeding.
    fn fresh_db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("metadata.redb")).expect("open")
    }

    fn person_entity(canonical: &str) -> Entity {
        let mut e = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            canonical.to_owned(),
            normalize_name(canonical),
            NOW,
        );
        e.aliases = vec!["priya".into(), "p. patel".into()];
        e
    }

    #[test]
    fn entity_create_replays_into_row_and_indexes() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;

        let body = encode_entity_create(&EntityMetadata::from(&e));
        db.apply_entity_create(10, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("entity present");
        assert_eq!(got.canonical_name, "Priya Patel");
        assert_eq!(got.aliases.len(), 2);
        assert_eq!(got.entity_type, EntityType::PERSON_ID);
    }

    #[test]
    fn entity_create_double_replay_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        let body = encode_entity_create(&EntityMetadata::from(&e));

        db.apply_entity_create(10, &body).unwrap();
        // Second apply (re-replay) must not error on the duplicate
        // canonical name and must leave state unchanged.
        db.apply_entity_create(10, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("entity present");
        assert_eq!(got.aliases.len(), 2);
    }

    #[test]
    fn entity_tombstone_replays_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, &e).unwrap();
            wtxn.commit().unwrap();
        }

        let body = encode_entity_tombstone(&EntityTombstoneBody {
            id: id.to_bytes(),
            at_unix_nanos: NOW + 999,
        });
        db.apply_entity_tombstone(11, &body).unwrap();
        // Re-replay: still succeeds (row persists, teardown is a no-op).
        db.apply_entity_tombstone(11, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("row persists");
        assert!(got.flags & flags::TOMBSTONED != 0);
    }

    #[test]
    fn entity_update_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{encode_entity_update, EntityUpdateBody};
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, &e).unwrap();
            wtxn.commit().unwrap();
        }

        let body = encode_entity_update(&EntityUpdateBody {
            id: id.to_bytes(),
            canonical_name: "Priya P. Patel".into(),
            aliases: vec!["priya".into()],
            attributes_blob: vec![1, 2, 3],
            at_unix_nanos: NOW + 60,
        });
        db.apply_entity_update(12, &body).unwrap();
        // Re-replay: the updated_at guard skips re-churn; no error.
        db.apply_entity_update(12, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("entity present");
        assert_eq!(got.canonical_name, "Priya P. Patel");
        // entity_update moves the old canonical into aliases; "priya" stays.
        assert!(got.aliases.iter().any(|a| a == "priya"));
    }

    #[test]
    fn entity_rename_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{encode_entity_rename, EntityRenameBody};
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let e = person_entity("Priya Patel");
        let id = e.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, &e).unwrap();
            wtxn.commit().unwrap();
        }

        let body = encode_entity_rename(&EntityRenameBody {
            id: id.to_bytes(),
            new_canonical_name: "Priya P. Patel".into(),
            at_unix_nanos: NOW + 30,
        });
        db.apply_entity_rename(13, &body).unwrap();
        // Re-replay: the canonical-name guard skips re-running; no error.
        db.apply_entity_rename(13, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, id).unwrap().expect("entity present");
        assert_eq!(got.canonical_name, "Priya P. Patel");
        // entity_rename moves the old canonical into aliases.
        assert!(got.aliases.iter().any(|a| a == "Priya Patel"));
    }

    #[test]
    fn entity_merge_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{encode_entity_merge, EntityMergeBody};
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let survivor = person_entity("Priya Patel");
        let merged = person_entity("Priya P");
        let survivor_id = survivor.id;
        let merged_id = merged.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, &survivor).unwrap();
            entity_put(&wtxn, &merged).unwrap();
            wtxn.commit().unwrap();
        }

        let body = encode_entity_merge(&EntityMergeBody {
            source: merged_id.to_bytes(),
            target: survivor_id.to_bytes(),
            retain_aliases: true,
            retain_attributes: true,
            at_unix_nanos: NOW + 100,
            confidence: 0.95,
            reason: "duplicate".into(),
            actor_kind: 0,
            actor_agent: [0u8; 16],
            grace_seconds: 7 * 24 * 60 * 60,
        });
        db.apply_entity_merge(14, &body).unwrap();
        // Re-replay: the merged_into guard skips the re-merge; no error.
        db.apply_entity_merge(14, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, merged_id)
            .unwrap()
            .expect("merged row persists");
        assert_eq!(got.merged_into, Some(survivor_id));
    }

    #[test]
    fn entity_unmerge_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{
            encode_entity_merge, encode_entity_unmerge, EntityMergeBody, EntityUnmergeBody,
        };
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let survivor = person_entity("Priya Patel");
        let merged = person_entity("Priya P");
        let survivor_id = survivor.id;
        let merged_id = merged.id;
        {
            let wtxn = db.write_txn().unwrap();
            entity_put(&wtxn, &survivor).unwrap();
            entity_put(&wtxn, &merged).unwrap();
            wtxn.commit().unwrap();
        }
        db.apply_entity_merge(
            14,
            &encode_entity_merge(&EntityMergeBody {
                source: merged_id.to_bytes(),
                target: survivor_id.to_bytes(),
                retain_aliases: true,
                retain_attributes: true,
                at_unix_nanos: NOW + 100,
                confidence: 0.95,
                reason: "dup".into(),
                actor_kind: 0,
                actor_agent: [0u8; 16],
                grace_seconds: 7 * 24 * 60 * 60,
            }),
        )
        .unwrap();

        let body = encode_entity_unmerge(&EntityUnmergeBody {
            merged: merged_id.to_bytes(),
            actor_kind: 0,
            actor_agent: [0u8; 16],
            at_unix_nanos: NOW + 200,
        });
        db.apply_entity_unmerge(15, &body).unwrap();
        // Re-replay: the merged_into guard skips re-unmerging; no error.
        db.apply_entity_unmerge(15, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = entity_get(&rtxn, merged_id)
            .unwrap()
            .expect("entity restored");
        assert_eq!(got.merged_into, None);
    }

    #[test]
    fn schema_update_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{encode_schema_update, SchemaUpdateBody};
        use crate::schema::store::schema_active;
        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let src = "
            namespace acme
            define entity_type Person { attributes {} }
            ";
        let body = encode_schema_update(&SchemaUpdateBody {
            namespace: "acme".into(),
            version: 1,
            blob: src.as_bytes().to_vec(),
            created_at_unix_nanos: NOW + 200,
        });
        db.apply_schema_update(15, &body).unwrap();
        // Re-replay: skip-if-(namespace,version)-exists — must NOT mint v2.
        db.apply_schema_update(15, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    }

    // ----- statement create / tombstone ---------------------------------

    fn schemaless_statement(subject: EntityId) -> brain_core::Statement {
        use brain_core::{
            EvidenceRef, ExtractorId, PredicateId, StatementId, StatementKind, StatementObject,
            StatementValue, SubjectRef,
        };
        use smallvec::SmallVec;
        // PredicateId(0) is the pre-intern placeholder; recovery re-resolves
        // it from the hint, mirroring the schemaless write path.
        brain_core::Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            PredicateId::from(0),
            StatementObject::Value(StatementValue::Text("blue".into())),
            0.9,
            EvidenceRef::Inline(Box::new(SmallVec::new())),
            ExtractorId::from(0),
            NOW,
            1,
        )
    }

    fn put_subject(db: &MetadataDb) -> EntityId {
        let e = person_entity("Subject Person");
        let id = e.id;
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    #[test]
    fn statement_create_schemaless_replays_interns_and_is_idempotent() {
        use crate::recovery::phase_bodies::{encode_statement_create, StatementCreateBody};
        use crate::statement::statement_get;
        use crate::tables::statement::{metadata_from_statement, statement_flags, STATEMENTS_TABLE};

        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let subject = put_subject(&db);
        let s = schemaless_statement(subject);
        let sid = s.id;

        let body = encode_statement_create(&StatementCreateBody {
            meta: metadata_from_statement(&s),
            predicate_intern_hint: Some(("app".into(), "knows".into())),
        });
        db.apply_statement_create(20, &body).unwrap();
        // Re-replay must be idempotent (skip-if-present), not error.
        db.apply_statement_create(20, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let got = statement_get(&rtxn, sid).unwrap().expect("statement present");
        // Predicate was re-interned away from the PredicateId(0) placeholder.
        assert_ne!(got.predicate, brain_core::PredicateId::from(0));
        // IMPLICIT_PREDICATE was stamped (schemaless write).
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let row = t.get(&sid.to_bytes()).unwrap().map(|g| g.value()).unwrap();
        assert!(row.flags & statement_flags::IMPLICIT_PREDICATE != 0);
    }

    #[test]
    fn statement_tombstone_replays_and_is_idempotent() {
        use crate::recovery::phase_bodies::{
            encode_statement_create, encode_statement_tombstone, StatementCreateBody,
            StatementTombstoneBody,
        };
        use crate::tables::statement::{metadata_from_statement, tombstone_reason, STATEMENTS_TABLE};

        let dir = TempDir::new().unwrap();
        let db = fresh_db(&dir);
        let subject = put_subject(&db);
        let s = schemaless_statement(subject);
        let sid = s.id;
        let create = encode_statement_create(&StatementCreateBody {
            meta: metadata_from_statement(&s),
            predicate_intern_hint: Some(("app".into(), "knows".into())),
        });
        db.apply_statement_create(20, &create).unwrap();

        let body = encode_statement_tombstone(&StatementTombstoneBody {
            id: sid.to_bytes(),
            reason: tombstone_reason::USER_REQUEST,
            at_unix_nanos: NOW + 5,
        });
        db.apply_statement_tombstone(21, &body).unwrap();
        // Re-replay: statement_tombstone short-circuits on an already-
        // tombstoned row, so this is a no-op.
        db.apply_statement_tombstone(21, &body).unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let row = t.get(&sid.to_bytes()).unwrap().map(|g| g.value()).unwrap();
        assert!(row.is_tombstoned());
        assert_eq!(row.is_current, 0);
    }
}
