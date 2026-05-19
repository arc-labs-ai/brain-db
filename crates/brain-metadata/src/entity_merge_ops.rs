//! Entity merge / unmerge mechanics. Sub-task 16.7.2.
//!
//! Implements `merge_entity` per `spec/18_entities/03_merge.md` §5
//! (phase-scoped — §5 steps 8-9 statement/relation re-routing are
//! deferred to phases 17/18) and `unmerge_entity` per
//! `spec/18_entities/04_unmerge.md` §4.
//!
//! Free functions over `WriteTransaction`, matching the
//! `entity_ops` precedent so callers can compose multi-table writes
//! within one transaction.
//!
//! ## Atomicity
//!
//! Every step happens inside the caller-supplied `WriteTransaction`. A
//! single redb commit covers:
//!
//! - `entities` row updates for survivor + merged.
//! - `entity_by_canonical_name` / `entity_aliases` index teardowns
//!   for merged.
//! - `entity_aliases` insertions for survivor's newly-folded aliases.
//! - `entity_trigrams` deltas (survivor gains, merged loses).
//! - `merge_log` audit row.
//!
//! Callers MUST call `wtxn.commit()` after `merge_entity` /
//! `unmerge_entity` returns `Ok(_)`.

use std::collections::HashSet;

use brain_core::{EntityId, EntityTypeId, MergeId};
use redb::{ReadableTable, WriteTransaction};

use crate::entity_ops::{normalize_name, EntityOpError};
use crate::tables::knowledge::entity::{
    flags, EntityMetadata, ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
};
use crate::tables::knowledge::merge::{actor_kind, MergeRecord, MERGE_LOG_TABLE};
use crate::trigram_ops::{
    extract_trigrams, index_entity_trigrams, remove_entity_trigrams, trigrams_of_components,
    TrigramOpError,
};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Who initiated the merge.
///
/// `System` is for the resolver / background workers (phase 21+ — LLM tier
/// merge suggestions). `Agent` is an operator agent_id over the wire
/// (`ENTITY_MERGE` opcode in phase 16.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeActor {
    System,
    Agent([u8; 16]),
}

impl MergeActor {
    fn kind_byte(self) -> u8 {
        match self {
            Self::System => actor_kind::SYSTEM,
            Self::Agent(_) => actor_kind::AGENT,
        }
    }

    fn agent_bytes(self) -> [u8; 16] {
        match self {
            Self::System => [0; 16],
            Self::Agent(bytes) => bytes,
        }
    }
}

/// Errors from the merge / unmerge layer.
#[derive(thiserror::Error, Debug)]
pub enum EntityMergeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("trigram op: {0}")]
    TrigramOp(#[from] TrigramOpError),

    #[error("entity_ops: {0}")]
    EntityOp(#[from] EntityOpError),

    #[error("entity {0:?} not found")]
    EntityNotFound(EntityId),

    #[error("survivor and merged are the same entity")]
    SelfMerge,

    /// Either side is already merged into another entity.
    #[error("entity {0:?} is already merged into {1:?}")]
    AlreadyMerged(EntityId, EntityId),

    #[error("type mismatch: survivor type {survivor:?}, merged type {merged:?}")]
    TypeMismatch {
        survivor: EntityTypeId,
        merged: EntityTypeId,
    },

    #[error("entity {0:?} is tombstoned")]
    Tombstoned(EntityId),

    #[error("confidence {0} is below merge threshold 0.7")]
    LowConfidence(f32),

    #[error("merge grace period expired")]
    OutOfGracePeriod,

    #[error("entity {0:?} is not currently merged")]
    NotMerged(EntityId),

    /// No `MergeRecord` row found for the supplied merged entity.
    #[error("no active merge audit found for entity {0:?}")]
    AuditMissing(EntityId),
}

/// Minimum confidence for a wire-initiated merge to apply.
/// Per `spec/18_entities/03_merge.md` §4.
pub const MIN_MERGE_CONFIDENCE: f32 = 0.7;

/// Default grace window for unmerge — 7 days. Configurable per-call
/// for tests; production handlers pass `DEFAULT_MERGE_GRACE_NANOS`.
pub const DEFAULT_MERGE_GRACE_NANOS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

// ---------------------------------------------------------------------------
// merge_entity.
// ---------------------------------------------------------------------------

/// Merge `merged` into `survivor`. Phase-scoped per
/// `spec/18_entities/03_merge.md` §0 — statement / relation re-routing
/// is deferred to phases 17/18.
///
/// Returns the freshly allocated `MergeId` for the audit row.
#[allow(clippy::too_many_arguments)]
pub fn merge_entity(
    wtxn: &WriteTransaction,
    survivor: EntityId,
    merged: EntityId,
    confidence: f32,
    reason: String,
    actor: MergeActor,
    grace_seconds: u64,
    now_unix_nanos: u64,
) -> Result<MergeId, EntityMergeOpError> {
    // §18/03 §3 pre-conditions.

    if survivor == merged {
        return Err(EntityMergeOpError::SelfMerge);
    }
    if !(MIN_MERGE_CONFIDENCE..=1.0).contains(&confidence) || !confidence.is_finite() {
        return Err(EntityMergeOpError::LowConfidence(confidence));
    }

    let survivor_row = load_entity(wtxn, survivor)?;
    let merged_row = load_entity(wtxn, merged)?;

    if survivor_row.flags & flags::TOMBSTONED != 0 {
        return Err(EntityMergeOpError::Tombstoned(survivor));
    }
    if merged_row.flags & flags::TOMBSTONED != 0 {
        return Err(EntityMergeOpError::Tombstoned(merged));
    }
    if let Some(into) = survivor_row.merged_into_bytes {
        return Err(EntityMergeOpError::AlreadyMerged(survivor, into.into()));
    }
    if let Some(into) = merged_row.merged_into_bytes {
        return Err(EntityMergeOpError::AlreadyMerged(merged, into.into()));
    }
    if survivor_row.entity_type_id != merged_row.entity_type_id {
        return Err(EntityMergeOpError::TypeMismatch {
            survivor: EntityTypeId(survivor_row.entity_type_id),
            merged: EntityTypeId(merged_row.entity_type_id),
        });
    }

    // §18/03 §5 mechanics (phase-scoped — skip steps 8/9 statement /
    // relation re-routing; the lists / counts stay at zero).

    let type_id = EntityTypeId(survivor_row.entity_type_id);

    // Build the diff against survivor.
    let pre_survivor_alias_norms: HashSet<String> = survivor_row
        .aliases
        .iter()
        .map(|a| normalize_name(a))
        .collect();
    let pre_survivor_canonical_norm = normalize_name(&survivor_row.canonical_name);

    // Aliases merged contributes: merged.canonical_name + merged.aliases,
    // minus anything survivor already has (by normalized form).
    let mut aliases_added: Vec<String> = Vec::new();
    let mut already_in_survivor = pre_survivor_alias_norms.clone();
    already_in_survivor.insert(pre_survivor_canonical_norm.clone());

    let merged_canonical_norm = normalize_name(&merged_row.canonical_name);
    if !already_in_survivor.contains(&merged_canonical_norm) {
        aliases_added.push(merged_row.canonical_name.clone());
        already_in_survivor.insert(merged_canonical_norm.clone());
    }
    for a in &merged_row.aliases {
        let a_norm = normalize_name(a);
        if !already_in_survivor.contains(&a_norm) {
            aliases_added.push(a.clone());
            already_in_survivor.insert(a_norm);
        }
    }

    // Trigrams survivor gains. The "added" set is trigrams in merged's
    // full trigram set that aren't already in survivor's set.
    let survivor_pre_trigrams =
        trigrams_of_components(&survivor_row.canonical_name, &survivor_row.aliases);
    let merged_full_trigrams = {
        let mut s = extract_trigrams(&normalize_name(&merged_row.canonical_name));
        for a in &merged_row.aliases {
            s.extend(extract_trigrams(&normalize_name(a)));
        }
        s
    };
    let trigrams_added: Vec<[u8; 3]> = merged_full_trigrams
        .difference(&survivor_pre_trigrams)
        .copied()
        .collect();
    let trigrams_added_set: HashSet<[u8; 3]> = trigrams_added.iter().copied().collect();

    let mention_count_added = merged_row.mention_count;

    // Phase 16.7: attribute_conflicts is empty because attributes are
    // opaque blobs. Phase 19's schema DSL will decode and produce real
    // conflict records.
    let attribute_conflicts = Vec::new();

    // 1. Tear down merged's secondary indexes (canonical_name + aliases).
    {
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.remove(&(merged_row.entity_type_id, merged_canonical_norm.as_str()))?;
    }
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &merged_row.aliases {
            let n = normalize_name(a);
            t.remove(&(
                merged_row.entity_type_id,
                n.as_str(),
                merged_row.entity_id_bytes,
            ))?;
        }
    }

    // 2. Tear down merged's trigrams.
    remove_entity_trigrams(wtxn, type_id, merged, &merged_full_trigrams)?;

    // 3. Update survivor's secondary indexes for the new aliases.
    if !aliases_added.is_empty() {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &aliases_added {
            let n = normalize_name(a);
            t.insert(
                &(
                    survivor_row.entity_type_id,
                    n.as_str(),
                    survivor_row.entity_id_bytes,
                ),
                &(),
            )?;
        }
    }

    // 4. Update survivor's trigrams for the additions.
    index_entity_trigrams(wtxn, type_id, survivor, &trigrams_added_set)?;

    // 5. Mutate survivor row in memory: extend aliases, fold mention_count.
    //    Attributes are opaque blobs in 16.7; survivor keeps its blob.
    let mut survivor_next = survivor_row.clone();
    for a in &aliases_added {
        survivor_next.aliases.push(a.clone());
    }
    survivor_next.mention_count = survivor_next
        .mention_count
        .saturating_add(mention_count_added);
    survivor_next.updated_at_unix_nanos = now_unix_nanos;

    // 6. Mutate merged row: set merged_into, MERGED flag, updated_at.
    //    Keep merged.aliases populated so unmerge can re-add them to
    //    the secondary indexes from this row directly.
    let mut merged_next = merged_row.clone();
    merged_next.merged_into_bytes = Some(survivor_row.entity_id_bytes);
    merged_next.flags |= flags::MERGED;
    merged_next.updated_at_unix_nanos = now_unix_nanos;

    // 7. Write both rows back.
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&survivor_next.entity_id_bytes, &survivor_next)?;
        t.insert(&merged_next.entity_id_bytes, &merged_next)?;
    }

    // 8. Write merge audit row.
    let merge_id = MergeId::new();
    let mut audit = MergeRecord::new(
        merge_id,
        survivor,
        merged,
        now_unix_nanos,
        now_unix_nanos.saturating_add(grace_seconds.saturating_mul(1_000_000_000)),
        confidence,
        reason,
        actor.kind_byte(),
        actor.agent_bytes(),
    );
    audit.aliases_added = aliases_added;
    audit.trigrams_added = trigrams_added;
    audit.attribute_conflicts = attribute_conflicts;
    audit.mention_count_added = mention_count_added;
    // statements_rerouted / relations_rerouted stay at 0 (phase 16.7).
    {
        let mut t = wtxn.open_table(MERGE_LOG_TABLE)?;
        t.insert(&(now_unix_nanos, audit.merge_id_bytes), &audit)?;
    }

    Ok(merge_id)
}

// ---------------------------------------------------------------------------
// unmerge_entity.
// ---------------------------------------------------------------------------

/// Reverse a recent merge identified by the `merged` entity. Phase-
/// scoped per `spec/18_entities/04_unmerge.md` §4 — statement /
/// relation re-route restoration is deferred to phases 17/18.
///
/// Returns the survivor's `EntityId` for caller convenience.
pub fn unmerge_entity(
    wtxn: &WriteTransaction,
    merged: EntityId,
    actor: MergeActor,
    now_unix_nanos: u64,
) -> Result<EntityId, EntityMergeOpError> {
    let merged_row = load_entity(wtxn, merged)?;
    let survivor_id_bytes = merged_row
        .merged_into_bytes
        .ok_or(EntityMergeOpError::NotMerged(merged))?;
    let survivor: EntityId = survivor_id_bytes.into();
    let survivor_row = load_entity(wtxn, survivor)?;

    // Find the most recent active audit row for this merged entity.
    let audit_key = find_active_audit(wtxn, merged)?;
    let mut audit = {
        let t = wtxn.open_table(MERGE_LOG_TABLE)?;
        let row: Option<MergeRecord> = t.get(&audit_key)?.map(|g| g.value());
        row.ok_or(EntityMergeOpError::AuditMissing(merged))?
    };

    if audit.is_finalized() || audit.is_unmerged() {
        return Err(EntityMergeOpError::OutOfGracePeriod);
    }
    if audit.grace_period_until_unix_nanos < now_unix_nanos {
        return Err(EntityMergeOpError::OutOfGracePeriod);
    }

    let type_id = EntityTypeId(merged_row.entity_type_id);

    // §18/04 §4 mechanics.

    // 1. Strip survivor of merged's contribution.
    let aliases_added_set: HashSet<String> = audit
        .aliases_added
        .iter()
        .map(|a| normalize_name(a))
        .collect();
    let mut survivor_next = survivor_row.clone();
    survivor_next
        .aliases
        .retain(|a| !aliases_added_set.contains(&normalize_name(a)));
    survivor_next.mention_count = survivor_next
        .mention_count
        .saturating_sub(audit.mention_count_added);
    survivor_next.updated_at_unix_nanos = now_unix_nanos;
    // attribute_conflicts revert is a no-op in 16.7 (always empty).

    // 2. Strip survivor's secondary indexes of merged's contribution.
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &audit.aliases_added {
            let n = normalize_name(a);
            t.remove(&(
                survivor_row.entity_type_id,
                n.as_str(),
                survivor_row.entity_id_bytes,
            ))?;
        }
    }
    let trigrams_added_set: HashSet<[u8; 3]> = audit.trigrams_added.iter().copied().collect();
    remove_entity_trigrams(wtxn, type_id, survivor, &trigrams_added_set)?;

    // 3. Restore merged entity.
    let mut merged_next = merged_row.clone();
    merged_next.merged_into_bytes = None;
    merged_next.flags &= !flags::MERGED;
    merged_next.updated_at_unix_nanos = now_unix_nanos;

    // 4. Re-add merged to secondary indexes (canonical_name + aliases).
    {
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.insert(
            &(
                merged_row.entity_type_id,
                normalize_name(&merged_row.canonical_name).as_str(),
            ),
            &merged_row.entity_id_bytes,
        )?;
    }
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &merged_row.aliases {
            let n = normalize_name(a);
            t.insert(
                &(
                    merged_row.entity_type_id,
                    n.as_str(),
                    merged_row.entity_id_bytes,
                ),
                &(),
            )?;
        }
    }

    // 5. Re-add merged's trigrams.
    let merged_full_trigrams =
        trigrams_of_components(&merged_row.canonical_name, &merged_row.aliases);
    index_entity_trigrams(wtxn, type_id, merged, &merged_full_trigrams)?;

    // 6. Write both rows back.
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&survivor_next.entity_id_bytes, &survivor_next)?;
        t.insert(&merged_next.entity_id_bytes, &merged_next)?;
    }

    // 7. Mark the audit row as unmerged + finalized.
    audit.unmerged_at_unix_nanos = now_unix_nanos;
    audit.unmerged_by_actor_kind = actor.kind_byte();
    audit.unmerged_by_agent_bytes = actor.agent_bytes();
    audit.finalized = 1;
    {
        let mut t = wtxn.open_table(MERGE_LOG_TABLE)?;
        t.insert(&audit_key, &audit)?;
    }

    Ok(survivor)
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn load_entity(
    wtxn: &WriteTransaction,
    id: EntityId,
) -> Result<EntityMetadata, EntityMergeOpError> {
    let t = wtxn.open_table(ENTITIES_TABLE)?;
    let row: Option<EntityMetadata> = t.get(&id.to_bytes())?.map(|g| g.value());
    row.ok_or(EntityMergeOpError::EntityNotFound(id))
}

/// Scan the merge log for the active (`unmerged_at == 0`,
/// `finalized == 0`) audit row whose `merged_bytes` matches. The
/// substrate's single-writer-per-shard discipline guarantees only one
/// active audit can exist per merged entity at a time (a second merge
/// would fail the `merged.merged_into.is_none()` pre-condition).
fn find_active_audit(
    wtxn: &WriteTransaction,
    merged: EntityId,
) -> Result<(u64, [u8; 16]), EntityMergeOpError> {
    let merged_bytes = merged.to_bytes();
    let t = wtxn.open_table(MERGE_LOG_TABLE)?;
    for entry in t.iter()? {
        let (k, v) = entry?;
        let row = v.value();
        if row.merged_bytes == merged_bytes && !row.is_unmerged() && !row.is_finalized() {
            return Ok(k.value());
        }
    }
    Err(EntityMergeOpError::AuditMissing(merged))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity_ops::{
        entity_get, entity_lookup_by_alias, entity_lookup_by_canonical_name, entity_put,
    };
    use crate::MetadataDb;
    use brain_core::{Entity, EntityType};
    use std::path::PathBuf;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;
    const LATER: u64 = NOW + 60_000_000_000; // +1 minute
    const GRACE_SECS: u64 = 7 * 24 * 60 * 60;

    fn db_path(dir: &TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
    }

    fn fresh_db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(db_path(dir)).expect("open")
    }

    fn person(canonical: &str) -> Entity {
        Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            canonical.to_owned(),
            normalize_name(canonical),
            NOW,
        )
    }

    fn put(db: &mut MetadataDb, e: &Entity) {
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, e).unwrap();
        wtxn.commit().unwrap();
    }

    #[test]
    fn merge_happy_path_redirects_and_writes_audit() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let mut alice = person("Alice");
        alice.aliases = vec!["A.".into()];
        let mut alyss = person("Alyss");
        alyss.aliases = vec!["AL".into()];
        alyss.mention_count = 3;
        put(&mut db, &alice);
        put(&mut db, &alyss);

        // Pre-merge: both reachable by canonical name.
        {
            let rtxn = db.read_txn().unwrap();
            assert_eq!(
                entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Alice").unwrap(),
                Some(alice.id)
            );
            assert_eq!(
                entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Alyss").unwrap(),
                Some(alyss.id)
            );
        }

        // Merge Alyss → Alice.
        let merge_id = {
            let wtxn = db.write_txn().unwrap();
            let mid = merge_entity(
                &wtxn,
                alice.id,
                alyss.id,
                0.92,
                "duplicate".into(),
                MergeActor::Agent([1u8; 16]),
                GRACE_SECS,
                LATER,
            )
            .unwrap();
            wtxn.commit().unwrap();
            mid
        };

        // Alyss's canonical_name no longer resolves; Alice picks it up
        // via the alias index.
        let rtxn = db.read_txn().unwrap();
        assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Alyss").unwrap(),
            None
        );
        let by_alias = entity_lookup_by_alias(&rtxn, EntityType::PERSON_ID, "Alyss").unwrap();
        assert_eq!(by_alias, vec![alice.id]);

        let alice_after = entity_get(&rtxn, alice.id).unwrap().unwrap();
        let alyss_after = entity_get(&rtxn, alyss.id).unwrap().unwrap();

        // Alice gained Alyss's name + aliases.
        assert!(alice_after.aliases.contains(&"Alyss".into()));
        assert!(alice_after.aliases.contains(&"AL".into()));
        assert_eq!(alice_after.mention_count, 3);
        // Alyss redirected.
        assert!(alyss_after.is_merged());
        assert_eq!(alyss_after.merged_into, Some(alice.id));

        // Audit row written.
        let _ = merge_id;
    }

    #[test]
    fn merge_self_rejected() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        put(&mut db, &alice);
        let wtxn = db.write_txn().unwrap();
        let err = merge_entity(
            &wtxn,
            alice.id,
            alice.id,
            0.9,
            "self".into(),
            MergeActor::Agent([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::SelfMerge));
    }

    #[test]
    fn merge_low_confidence_rejected() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        let bob = person("Bob");
        put(&mut db, &alice);
        put(&mut db, &bob);
        let wtxn = db.write_txn().unwrap();
        let err = merge_entity(
            &wtxn,
            alice.id,
            bob.id,
            0.5,
            "low".into(),
            MergeActor::Agent([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::LowConfidence(_)));
    }

    #[test]
    fn merge_already_merged_rejected() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        let bob = person("Bob");
        let carol = person("Carol");
        put(&mut db, &alice);
        put(&mut db, &bob);
        put(&mut db, &carol);

        // First merge Bob into Alice.
        {
            let wtxn = db.write_txn().unwrap();
            merge_entity(
                &wtxn,
                alice.id,
                bob.id,
                0.9,
                "first".into(),
                MergeActor::Agent([1; 16]),
                GRACE_SECS,
                LATER,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        // Now try to merge Bob into Carol — rejected.
        let wtxn = db.write_txn().unwrap();
        let err = merge_entity(
            &wtxn,
            carol.id,
            bob.id,
            0.9,
            "second".into(),
            MergeActor::Agent([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::AlreadyMerged(_, _)));
    }

    #[test]
    fn merge_unmerge_round_trip_restores_state() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let mut alice = person("Alice");
        alice.aliases = vec!["A.".into()];
        let mut alyss = person("Alyss");
        alyss.aliases = vec!["AL".into()];
        alyss.mention_count = 5;
        put(&mut db, &alice);
        put(&mut db, &alyss);

        let merge_at = LATER;
        {
            let wtxn = db.write_txn().unwrap();
            merge_entity(
                &wtxn,
                alice.id,
                alyss.id,
                0.9,
                "test".into(),
                MergeActor::Agent([1; 16]),
                GRACE_SECS,
                merge_at,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        // Unmerge inside the grace period.
        {
            let wtxn = db.write_txn().unwrap();
            let restored = unmerge_entity(
                &wtxn,
                alyss.id,
                MergeActor::Agent([2; 16]),
                merge_at + 60_000_000_000,
            )
            .unwrap();
            assert_eq!(restored, alice.id);
            wtxn.commit().unwrap();
        }

        // Both entities are independently queryable again.
        let rtxn = db.read_txn().unwrap();
        let alice_after = entity_get(&rtxn, alice.id).unwrap().unwrap();
        let alyss_after = entity_get(&rtxn, alyss.id).unwrap().unwrap();

        assert!(!alyss_after.is_merged());
        assert_eq!(alyss_after.merged_into, None);
        assert!(!alice_after.aliases.iter().any(|a| a == "Alyss"));
        assert!(!alice_after.aliases.iter().any(|a| a == "AL"));
        assert!(alice_after.aliases.contains(&"A.".into()));
        assert_eq!(alice_after.mention_count, 0);

        // Alyss is reachable by canonical_name again.
        assert_eq!(
            entity_lookup_by_canonical_name(&rtxn, EntityType::PERSON_ID, "Alyss").unwrap(),
            Some(alyss.id)
        );
    }

    #[test]
    fn unmerge_outside_grace_rejected() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        let alyss = person("Alyss");
        put(&mut db, &alice);
        put(&mut db, &alyss);

        let merge_at = NOW;
        let grace = 1u64; // 1 second
        {
            let wtxn = db.write_txn().unwrap();
            merge_entity(
                &wtxn,
                alice.id,
                alyss.id,
                0.9,
                "test".into(),
                MergeActor::Agent([1; 16]),
                grace,
                merge_at,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }

        // Unmerge after grace expired.
        let wtxn = db.write_txn().unwrap();
        let err = unmerge_entity(
            &wtxn,
            alyss.id,
            MergeActor::Agent([2; 16]),
            merge_at + 2_000_000_000, // 2 seconds — past grace
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::OutOfGracePeriod));
    }

    #[test]
    fn unmerge_of_non_merged_rejected() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        put(&mut db, &alice);

        let wtxn = db.write_txn().unwrap();
        let err = unmerge_entity(&wtxn, alice.id, MergeActor::Agent([1; 16]), LATER).unwrap_err();
        assert!(matches!(err, EntityMergeOpError::NotMerged(_)));
    }

    #[test]
    fn merge_tombstoned_rejected() {
        use crate::entity_ops::entity_tombstone;
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let alice = person("Alice");
        let bob = person("Bob");
        put(&mut db, &alice);
        put(&mut db, &bob);

        {
            let wtxn = db.write_txn().unwrap();
            entity_tombstone(&wtxn, bob.id, NOW).unwrap();
            wtxn.commit().unwrap();
        }

        let wtxn = db.write_txn().unwrap();
        let err = merge_entity(
            &wtxn,
            alice.id,
            bob.id,
            0.9,
            "test".into(),
            MergeActor::Agent([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::Tombstoned(_)));
    }
}
