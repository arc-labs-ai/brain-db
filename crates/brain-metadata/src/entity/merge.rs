//! Entity merge / unmerge mechanics.
//!
//! Implements `merge_entity` and `unmerge_entity`.
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

use std::collections::{BTreeMap, HashSet};

use brain_core::{
    canonical_pair, EdgeKindRef, EntityId, EntityTypeId, MergeId, NodeRef, RelationId,
    RelationTypeId, StatementKind, StatementObject,
};
use redb::{ReadableTable, WriteTransaction};

use super::ops::{normalize_name, EntityOpError};
use super::trigram::{
    extract_trigrams, index_entity_trigrams, remove_entity_trigrams, trigrams_of_components,
    TrigramOpError,
};
use crate::tables::edge::{
    self, derived_by, origin, EdgeData, EdgeOpError, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use crate::tables::entity::{
    flags, EntityMetadata, ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE,
};
use crate::tables::merge::{
    actor_kind, conflict_outcome, conflict_policy, AttributeConflictRecord, MergeRecord,
    RelationReroute, StatementReroute, MERGE_LOG_TABLE,
};
use crate::tables::relation::{RelationMetadata, RELATION_METADATA_TABLE};
use crate::tables::scope::RowScope;
use crate::tables::statement::{
    encode_object, StatementMetadata, STATEMENTS_BY_EVENT_TIME_TABLE,
    STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_SUBJECT_ID_TABLE, STATEMENTS_BY_SUBJECT_TABLE,
    STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Who initiated the merge.
///
/// `System` is for the resolver / background workers (e.g. LLM-tier
/// merge suggestions). `Space` is an operator space_id over the wire
/// (the `ENTITY_MERGE` opcode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeActor {
    System,
    Space([u8; 16]),
}

impl MergeActor {
    fn kind_byte(self) -> u8 {
        match self {
            Self::System => actor_kind::SYSTEM,
            Self::Space(_) => actor_kind::SPACE,
        }
    }

    fn space_bytes(self) -> [u8; 16] {
        match self {
            Self::System => [0; 16],
            Self::Space(bytes) => bytes,
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

    #[error("edge op: {0}")]
    EdgeOp(#[from] EdgeOpError),

    #[error("edge key decode: {0}")]
    EdgeKey(#[from] crate::tables::edge::EdgeKeyError),

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
pub const MIN_MERGE_CONFIDENCE: f32 = 0.7;

/// Default grace window for unmerge — 7 days. Configurable per-call
/// for tests; production handlers pass `DEFAULT_MERGE_GRACE_NANOS`.
pub const DEFAULT_MERGE_GRACE_NANOS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

/// The result of a successful [`merge_entity`]: the audit id plus the
/// re-routed graph-row counts (surfaced to the wire event / ack).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub merge_id: MergeId,
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
}

// ---------------------------------------------------------------------------
// merge_entity.
// ---------------------------------------------------------------------------

/// Merge `merged` into `survivor`.
///
/// Returns the freshly allocated audit id plus the number of statements
/// and relations re-routed onto the survivor.
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
) -> Result<MergeOutcome, EntityMergeOpError> {
    // Pre-conditions.

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

    // A merge is intra-tenant: both rows share the owning scope, which
    // bounds every secondary-index key touched here.
    if survivor_row.scope() != merged_row.scope() {
        return Err(EntityMergeOpError::TypeMismatch {
            survivor: EntityTypeId(survivor_row.entity_type_id),
            merged: EntityTypeId(merged_row.entity_type_id),
        });
    }
    let scope = survivor_row.scope();

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

    // Step 6 — attribute fold (survivor_wins).
    //
    // Entity attributes are an opaque `attributes_blob` (a future
    // rkyv `BTreeMap<String, Value>` — no keyed codec has landed), so a
    // per-key union is not yet possible. The fold therefore operates at
    // whole-blob granularity, which is the faithful degenerate case of
    // survivor_wins: the survivor keeps its own blob unconditionally; a
    // survivor that carried no attributes adopts merged's blob (the only
    // "add what only merged holds" move available without keys). When
    // both blobs are non-empty and differ, one conflict record is stamped
    // so the decision is auditable and reversible.
    let survivor_attributes_before = survivor_row.attributes_blob.clone();
    let mut attribute_conflicts: Vec<AttributeConflictRecord> = Vec::new();
    let adopt_merged_attributes =
        survivor_row.attributes_blob.is_empty() && !merged_row.attributes_blob.is_empty();
    if !survivor_row.attributes_blob.is_empty()
        && !merged_row.attributes_blob.is_empty()
        && survivor_row.attributes_blob != merged_row.attributes_blob
    {
        attribute_conflicts.push(AttributeConflictRecord {
            // Whole-blob sentinel until the keyed attribute codec lands.
            attribute_key: "*".to_string(),
            survivor_value_blob: survivor_row.attributes_blob.clone(),
            merged_value_blob: merged_row.attributes_blob.clone(),
            policy: conflict_policy::SURVIVOR_WINS,
            outcome: conflict_outcome::KEPT_SURVIVOR,
        });
    }

    // Steps 8 + 9 — re-route statements and relations onto the survivor.
    // Both run in this same write transaction, so the whole merge is
    // atomic. Scope-bounded: the enumerations range only within the
    // merge's `(namespace, space)`.
    let rerouted_statements =
        reroute_statements(wtxn, scope, merged, survivor, survivor_row.entity_id_bytes)?;
    let rerouted_relations = reroute_relations(
        wtxn,
        scope,
        merged,
        survivor_row.entity_id_bytes,
        now_unix_nanos,
    )?;
    let statements_rerouted = u32::try_from(rerouted_statements.len()).unwrap_or(u32::MAX);
    let relations_rerouted = u32::try_from(rerouted_relations.len()).unwrap_or(u32::MAX);

    // 1. Tear down merged's secondary indexes (canonical_name + aliases).
    {
        let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE)?;
        t.remove(&(
            scope.namespace_id,
            scope.space_id_bytes,
            merged_row.entity_type_id,
            merged_canonical_norm.as_str(),
        ))?;
    }
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &merged_row.aliases {
            let n = normalize_name(a);
            t.remove(&(
                scope.namespace_id,
                scope.space_id_bytes,
                merged_row.entity_type_id,
                n.as_str(),
                merged_row.entity_id_bytes,
            ))?;
        }
    }

    // 2. Tear down merged's trigrams.
    remove_entity_trigrams(wtxn, scope, type_id, merged, &merged_full_trigrams)?;

    // 3. Update survivor's secondary indexes for the new aliases.
    if !aliases_added.is_empty() {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &aliases_added {
            let n = normalize_name(a);
            t.insert(
                &(
                    scope.namespace_id,
                    scope.space_id_bytes,
                    survivor_row.entity_type_id,
                    n.as_str(),
                    survivor_row.entity_id_bytes,
                ),
                &(),
            )?;
        }
    }

    // 4. Update survivor's trigrams for the additions.
    index_entity_trigrams(wtxn, scope, type_id, survivor, &trigrams_added_set)?;

    // 5. Mutate survivor row in memory: extend aliases, fold mention_count,
    //    fold attributes (survivor_wins; adopt merged's blob only when the
    //    survivor carried none).
    let mut survivor_next = survivor_row.clone();
    for a in &aliases_added {
        survivor_next.aliases.push(a.clone());
    }
    survivor_next.mention_count = survivor_next
        .mention_count
        .saturating_add(mention_count_added);
    if adopt_merged_attributes {
        survivor_next.attributes_blob = merged_row.attributes_blob.clone();
    }
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
        actor.space_bytes(),
    );
    audit.aliases_added = aliases_added;
    audit.trigrams_added = trigrams_added;
    audit.attribute_conflicts = attribute_conflicts;
    audit.mention_count_added = mention_count_added;
    audit.statements_rerouted = statements_rerouted;
    audit.relations_rerouted = relations_rerouted;
    audit.rerouted_statements = rerouted_statements;
    audit.rerouted_relations = rerouted_relations;
    audit.survivor_attributes_before = survivor_attributes_before;
    {
        let mut t = wtxn.open_table(MERGE_LOG_TABLE)?;
        t.insert(&(now_unix_nanos, audit.merge_id_bytes), &audit)?;
    }

    Ok(MergeOutcome {
        merge_id,
        statements_rerouted,
        relations_rerouted,
    })
}

// ---------------------------------------------------------------------------
// unmerge_entity.
// ---------------------------------------------------------------------------

/// Reverse a recent merge identified by the `merged` entity.
///
/// Restores the merged entity, strips the survivor of everything the
/// merge contributed (aliases, mention_count, attributes), and re-points
/// every re-routed statement / relation back to the merged entity using
/// the audit's recorded diff.
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
    // Both rows share the owning scope (a merge never crosses tenants).
    let scope = merged_row.scope();

    // Unmerge mechanics.

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
    // Restore the survivor's pre-merge attribute blob (survivor_wins fold
    // either left it untouched or, if the survivor had none, adopted
    // merged's — either way the recorded snapshot is the exact reversal).
    survivor_next.attributes_blob = audit.survivor_attributes_before.clone();
    survivor_next.updated_at_unix_nanos = now_unix_nanos;

    // 2. Strip survivor's secondary indexes of merged's contribution.
    {
        let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE)?;
        for a in &audit.aliases_added {
            let n = normalize_name(a);
            t.remove(&(
                scope.namespace_id,
                scope.space_id_bytes,
                survivor_row.entity_type_id,
                n.as_str(),
                survivor_row.entity_id_bytes,
            ))?;
        }
    }
    let trigrams_added_set: HashSet<[u8; 3]> = audit.trigrams_added.iter().copied().collect();
    remove_entity_trigrams(wtxn, scope, type_id, survivor, &trigrams_added_set)?;

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
                scope.namespace_id,
                scope.space_id_bytes,
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
                    scope.namespace_id,
                    scope.space_id_bytes,
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
    index_entity_trigrams(wtxn, scope, type_id, merged, &merged_full_trigrams)?;

    // 5b. Re-point every re-routed statement / relation back to the merged
    //     entity (exact reversal driven by the audit's recorded diff).
    reverse_statement_reroutes(wtxn, scope, &audit.rerouted_statements, merged, survivor)?;
    reverse_relation_reroutes(wtxn, &audit.rerouted_relations, now_unix_nanos)?;

    // 6. Write both rows back.
    {
        let mut t = wtxn.open_table(ENTITIES_TABLE)?;
        t.insert(&survivor_next.entity_id_bytes, &survivor_next)?;
        t.insert(&merged_next.entity_id_bytes, &merged_next)?;
    }

    // 7. Mark the audit row as unmerged + finalized.
    audit.unmerged_at_unix_nanos = now_unix_nanos;
    audit.unmerged_by_actor_kind = actor.kind_byte();
    audit.unmerged_by_space_bytes = actor.space_bytes();
    audit.finalized = 1;
    {
        let mut t = wtxn.open_table(MERGE_LOG_TABLE)?;
        t.insert(&audit_key, &audit)?;
    }

    Ok(survivor)
}

// ---------------------------------------------------------------------------
// Statement re-routing (step 8) + reversal.
// ---------------------------------------------------------------------------

/// Re-route every statement that references `merged` onto `survivor`.
///
/// Subject-side rows (subject == merged) have their subject re-pointed,
/// their `version` bumped, and the by-subject / by-event-time / chain
/// indexes rewritten. Object-side rows (object == `Entity(merged)`) have
/// their object re-pointed and the by-object-entity index rewritten
/// (no version bump). A self-referential row gets both.
///
/// Version bump discipline: every member of a supersession chain shares
/// the merged subject, so the whole chain is re-routed. Bumping each
/// member by `+1` would collide in the version-keyed chain table, so the
/// chain is shifted by a uniform delta (its current max version) — this
/// preserves ordering AND leaves every version unique.
fn reroute_statements(
    wtxn: &WriteTransaction,
    scope: RowScope,
    merged: EntityId,
    survivor: EntityId,
    survivor_bytes: [u8; 16],
) -> Result<Vec<StatementReroute>, EntityMergeOpError> {
    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;
    let merged_b = merged.to_bytes();

    // Phase A — enumerate subject-side and object-side statement ids.
    let mut sides: BTreeMap<[u8; 16], (bool, bool)> = BTreeMap::new();
    {
        let t = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        let lo = (ns, ag, merged_b, 0u8, 0u32, 0u8, [0u8; 16]);
        let hi = (ns, ag, merged_b, u8::MAX, u32::MAX, 1u8, [0xffu8; 16]);
        for entry in t.range(lo..=hi)? {
            let (k, v) = entry?;
            let (k_ns, k_ag, k_subj, ..) = k.value();
            if k_ns != ns || k_ag != ag || k_subj != merged_b {
                continue;
            }
            sides.entry(v.value()).or_insert((false, false)).0 = true;
        }
    }
    {
        let t = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
        let lo = (ns, ag, merged_b, 0u8, [0u8; 16]);
        let hi = (ns, ag, merged_b, u8::MAX, [0xffu8; 16]);
        for entry in t.range(lo..=hi)? {
            let (k, v) = entry?;
            let (k_ns, k_ag, k_obj, ..) = k.value();
            if k_ns != ns || k_ag != ag || k_obj != merged_b {
                continue;
            }
            sides.entry(v.value()).or_insert((false, false)).1 = true;
        }
    }

    // Phase B — load the affected rows.
    let mut rows: Vec<(bool, bool, StatementMetadata)> = Vec::with_capacity(sides.len());
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        for (sid, (subj, obj)) in &sides {
            let Some(m) = t.get(sid)?.map(|g| g.value()) else {
                continue;
            };
            // Scope wall: the primary table is a flat keyspace; the index
            // is scoped, but re-confirm on the row.
            if m.namespace_id != ns || m.space_id_bytes != ag {
                continue;
            }
            rows.push((*subj, *obj, m));
        }
    }

    // Phase C — per-chain version-shift delta (subject-changed rows only).
    let mut chain_delta: BTreeMap<[u8; 16], u32> = BTreeMap::new();
    {
        let t = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
        for (subj, _obj, m) in &rows {
            if !*subj {
                continue;
            }
            if chain_delta.contains_key(&m.chain_root_bytes) {
                continue;
            }
            let lo = (ns, ag, m.chain_root_bytes, 0u32);
            let hi = (ns, ag, m.chain_root_bytes, u32::MAX);
            let mut max = 0u32;
            for entry in t.range(lo..=hi)? {
                let (k, _) = entry?;
                let (.., ver) = k.value();
                max = max.max(ver);
            }
            chain_delta.insert(m.chain_root_bytes, max);
        }
    }

    // Phase D — apply mutations with one handle per table.
    let mut records: Vec<StatementReroute> = Vec::with_capacity(rows.len());
    let mut st = wtxn.open_table(STATEMENTS_TABLE)?;
    let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let mut bysi = wtxn.open_table(STATEMENTS_BY_SUBJECT_ID_TABLE)?;
    let mut byo = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
    let mut byt = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
    let mut cht = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    for (subj, obj, mut m) in rows {
        let old_version = m.version;
        let mut new_version = old_version;

        if obj {
            byo.remove(&(ns, ag, merged_b, m.kind, m.statement_id_bytes))?;
            byo.insert(
                &(ns, ag, survivor_bytes, m.kind, m.statement_id_bytes),
                &m.statement_id_bytes,
            )?;
            m.object_blob = encode_object(&StatementObject::Entity(survivor));
            // object_discriminant stays Entity (== 1).
        }

        if subj {
            let delta = chain_delta.get(&m.chain_root_bytes).copied().unwrap_or(0);
            new_version = old_version.saturating_add(delta);

            bys.remove(&(
                ns,
                ag,
                merged_b,
                m.kind,
                m.predicate_id,
                m.is_current,
                m.statement_id_bytes,
            ))?;
            bys.insert(
                &(
                    ns,
                    ag,
                    survivor_bytes,
                    m.kind,
                    m.predicate_id,
                    m.is_current,
                    m.statement_id_bytes,
                ),
                &m.statement_id_bytes,
            )?;
            // Move the immutable id-ordered pagination twin to the survivor.
            bysi.remove(&(ns, ag, merged_b, m.statement_id_bytes))?;
            bysi.insert(&(ns, ag, survivor_bytes, m.statement_id_bytes), &())?;

            if m.kind == StatementKind::Event.as_u8() {
                if let Some(event_at) = m.event_at_unix_nanos {
                    byt.remove(&(ns, ag, event_at, merged_b, m.statement_id_bytes))?;
                    byt.insert(
                        &(ns, ag, event_at, survivor_bytes, m.statement_id_bytes),
                        &m.statement_id_bytes,
                    )?;
                }
            }

            if new_version != old_version {
                cht.remove(&(ns, ag, m.chain_root_bytes, old_version))?;
                cht.insert(
                    &(ns, ag, m.chain_root_bytes, new_version),
                    &m.statement_id_bytes,
                )?;
            }

            m.subject_entity_bytes = survivor_bytes;
            m.version = new_version;
        }

        st.insert(&m.statement_id_bytes, &m)?;
        records.push(StatementReroute {
            statement_id_bytes: m.statement_id_bytes,
            subject_changed: u8::from(subj),
            object_changed: u8::from(obj),
            old_version,
            new_version,
            chain_root_bytes: m.chain_root_bytes,
        });
    }
    Ok(records)
}

/// Reverse [`reroute_statements`] using the recorded diff: re-point each
/// statement's subject / object back to `merged` and restore the version
/// + chain-table key.
fn reverse_statement_reroutes(
    wtxn: &WriteTransaction,
    scope: RowScope,
    records: &[StatementReroute],
    merged: EntityId,
    survivor: EntityId,
) -> Result<(), EntityMergeOpError> {
    if records.is_empty() {
        return Ok(());
    }
    let ns = scope.namespace_id;
    let ag = scope.space_id_bytes;
    let merged_b = merged.to_bytes();
    let survivor_b = survivor.to_bytes();

    let mut st = wtxn.open_table(STATEMENTS_TABLE)?;
    let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let mut bysi = wtxn.open_table(STATEMENTS_BY_SUBJECT_ID_TABLE)?;
    let mut byo = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE)?;
    let mut byt = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE)?;
    let mut cht = wtxn.open_table(STATEMENT_CHAIN_TABLE)?;
    for r in records {
        let Some(mut m) = st.get(&r.statement_id_bytes)?.map(|g| g.value()) else {
            continue;
        };

        if r.object_changed != 0 {
            byo.remove(&(ns, ag, survivor_b, m.kind, m.statement_id_bytes))?;
            byo.insert(
                &(ns, ag, merged_b, m.kind, m.statement_id_bytes),
                &m.statement_id_bytes,
            )?;
            m.object_blob = encode_object(&StatementObject::Entity(merged));
        }

        if r.subject_changed != 0 {
            bys.remove(&(
                ns,
                ag,
                survivor_b,
                m.kind,
                m.predicate_id,
                m.is_current,
                m.statement_id_bytes,
            ))?;
            bys.insert(
                &(
                    ns,
                    ag,
                    merged_b,
                    m.kind,
                    m.predicate_id,
                    m.is_current,
                    m.statement_id_bytes,
                ),
                &m.statement_id_bytes,
            )?;
            // Reverse the immutable id-ordered pagination twin's move.
            bysi.remove(&(ns, ag, survivor_b, m.statement_id_bytes))?;
            bysi.insert(&(ns, ag, merged_b, m.statement_id_bytes), &())?;

            if m.kind == StatementKind::Event.as_u8() {
                if let Some(event_at) = m.event_at_unix_nanos {
                    byt.remove(&(ns, ag, event_at, survivor_b, m.statement_id_bytes))?;
                    byt.insert(
                        &(ns, ag, event_at, merged_b, m.statement_id_bytes),
                        &m.statement_id_bytes,
                    )?;
                }
            }

            if r.new_version != r.old_version {
                cht.remove(&(ns, ag, r.chain_root_bytes, r.new_version))?;
                cht.insert(
                    &(ns, ag, r.chain_root_bytes, r.old_version),
                    &m.statement_id_bytes,
                )?;
            }

            m.subject_entity_bytes = merged_b;
            m.version = r.old_version;
        }

        st.insert(&m.statement_id_bytes, &m)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation re-routing (step 9) + reversal.
// ---------------------------------------------------------------------------

/// Re-route every relation with an endpoint on `merged` onto `survivor`.
///
/// Rewrites the unified edge rows (forward + reverse, plus the explicit
/// symmetric mirror) and the sidecar `from` / `to`. Symmetric relations
/// are re-canonicalised so the stored pair stays ordered by id.
fn reroute_relations(
    wtxn: &WriteTransaction,
    scope: RowScope,
    merged: EntityId,
    survivor_bytes: [u8; 16],
    now_unix_nanos: u64,
) -> Result<Vec<RelationReroute>, EntityMergeOpError> {
    let merged_b = merged.to_bytes();
    let entity_tag = NodeRef::Entity(merged).tag();

    // Phase A — enumerate affected relations (single scoped scan).
    let mut affected: Vec<(RelationId, RelationMetadata)> = Vec::new();
    {
        let t = wtxn.open_table(RELATION_METADATA_TABLE)?;
        for entry in t.iter()? {
            let (k, v) = entry?;
            let m = v.value();
            if m.namespace_id != scope.namespace_id || m.space_id_bytes != scope.space_id_bytes {
                continue;
            }
            let from_hit = m.from_tag == entity_tag && m.from_bytes == merged_b;
            let to_hit = m.to_tag == entity_tag && m.to_bytes == merged_b;
            if from_hit || to_hit {
                affected.push((RelationId::from(k.value()), m));
            }
        }
    }

    // Phase B — apply.
    let mut records: Vec<RelationReroute> = Vec::with_capacity(affected.len());
    let mut edges = wtxn.open_table(EDGES_TABLE)?;
    let mut reverse = wtxn.open_table(EDGES_REVERSE_TABLE)?;
    let mut sidecar = wtxn.open_table(RELATION_METADATA_TABLE)?;
    for (rel_id, m) in affected {
        let from_changed = m.from_tag == entity_tag && m.from_bytes == merged_b;
        let to_changed = m.to_tag == entity_tag && m.to_bytes == merged_b;
        let old_from = m.from_bytes;
        let old_to = m.to_bytes;

        let mut new_from = if from_changed {
            survivor_bytes
        } else {
            old_from
        };
        let mut new_to = if to_changed { survivor_bytes } else { old_to };
        if m.is_symmetric() {
            let (a, b) = canonical_pair(EntityId::from(new_from), EntityId::from(new_to));
            new_from = a.to_bytes();
            new_to = b.to_bytes();
        }

        rewrite_relation_edges(
            &mut edges,
            &mut reverse,
            &m,
            rel_id,
            old_from,
            old_to,
            new_from,
            new_to,
            now_unix_nanos,
        )?;

        let mut nm = m.clone();
        nm.from_bytes = new_from;
        nm.to_bytes = new_to;
        sidecar.insert(&rel_id.to_bytes(), &nm)?;

        // A reroute can collide with an existing current relation of the
        // same (from, type, to). Edge rows never clash (the disambiguator
        // is the unique RelationId), so no data is lost, but two current
        // relations may now violate a One* cardinality. v1 keeps both
        // pointing at the survivor and logs the collision rather than
        // tombstoning (which would complicate reversal); a cardinality
        // sweep is the place to reconcile.
        if let Some(dupe) = find_current_duplicate(&sidecar, scope, &nm, rel_id)? {
            tracing::warn!(
                target: "brain_metadata::merge",
                relation = ?rel_id,
                duplicate = ?dupe,
                "entity merge re-routed a relation onto an endpoint that already has a \
                 current relation of the same type; both retained"
            );
        }

        records.push(RelationReroute {
            relation_id_bytes: rel_id.to_bytes(),
            old_from_bytes: old_from,
            old_to_bytes: old_to,
            new_from_bytes: new_from,
            new_to_bytes: new_to,
            from_changed: u8::from(from_changed),
            to_changed: u8::from(to_changed),
        });
    }
    Ok(records)
}

/// Reverse [`reroute_relations`]: unlink the survivor-side edge rows and
/// relink the original merged-side ones, then restore the sidecar
/// endpoints.
fn reverse_relation_reroutes(
    wtxn: &WriteTransaction,
    records: &[RelationReroute],
    now_unix_nanos: u64,
) -> Result<(), EntityMergeOpError> {
    if records.is_empty() {
        return Ok(());
    }
    let mut edges = wtxn.open_table(EDGES_TABLE)?;
    let mut reverse = wtxn.open_table(EDGES_REVERSE_TABLE)?;
    let mut sidecar = wtxn.open_table(RELATION_METADATA_TABLE)?;
    for r in records {
        let Some(m) = sidecar.get(&r.relation_id_bytes)?.map(|g| g.value()) else {
            continue;
        };
        let rel_id = RelationId::from(r.relation_id_bytes);
        // Current endpoints are the post-merge ones; relink to the
        // recorded originals.
        rewrite_relation_edges(
            &mut edges,
            &mut reverse,
            &m,
            rel_id,
            r.new_from_bytes,
            r.new_to_bytes,
            r.old_from_bytes,
            r.old_to_bytes,
            now_unix_nanos,
        )?;
        let mut nm = m.clone();
        nm.from_bytes = r.old_from_bytes;
        nm.to_bytes = r.old_to_bytes;
        sidecar.insert(&r.relation_id_bytes, &nm)?;
    }
    Ok(())
}

/// Unlink the `(old_from, type, old_to)` edge rows for `rel_id` and link
/// `(new_from, type, new_to)`, preserving the original [`EdgeData`] and
/// mirroring symmetric relations explicitly (typed edges are not
/// auto-mirrored by [`edge::link`]).
#[allow(clippy::too_many_arguments)]
fn rewrite_relation_edges(
    edges: &mut redb::Table<'_, &[u8], EdgeData>,
    reverse: &mut redb::Table<'_, &[u8], EdgeData>,
    m: &RelationMetadata,
    rel_id: RelationId,
    old_from: [u8; 16],
    old_to: [u8; 16],
    new_from: [u8; 16],
    new_to: [u8; 16],
    now_unix_nanos: u64,
) -> Result<(), EntityMergeOpError> {
    let kind = EdgeKindRef::Typed(RelationTypeId::from(m.relation_type_id));
    let disamb = rel_id.to_bytes();
    let old_from_n = NodeRef::Entity(EntityId::from(old_from));
    let old_to_n = NodeRef::Entity(EntityId::from(old_to));

    // Preserve the existing edge weight/provenance if present. `Table`
    // (write handle) supports point reads, so no separate read txn.
    let existing_key = edge::EdgeKey {
        from: old_from_n,
        kind,
        to: old_to_n,
        disambiguator: disamb,
    }
    .encode();
    let data = edges
        .get(existing_key.as_slice())?
        .map(|g| g.value())
        .unwrap_or_else(|| {
            EdgeData::new(
                1.0,
                origin::AUTO_DERIVED,
                derived_by::CLIENT,
                now_unix_nanos,
            )
        });

    let symmetric = m.is_symmetric();
    edge::unlink(edges, reverse, old_from_n, kind, old_to_n, disamb)?;
    if symmetric && old_from != old_to {
        edge::unlink(edges, reverse, old_to_n, kind, old_from_n, disamb)?;
    }

    let new_from_n = NodeRef::Entity(EntityId::from(new_from));
    let new_to_n = NodeRef::Entity(EntityId::from(new_to));
    edge::link(edges, reverse, new_from_n, kind, new_to_n, disamb, &data)?;
    if symmetric && new_from != new_to {
        edge::link(edges, reverse, new_to_n, kind, new_from_n, disamb, &data)?;
    }
    Ok(())
}

/// Return a current relation that shares `(from, type, to)` with `nm`
/// but is a different relation id, if one exists — used only to flag a
/// cardinality collision after a merge reroute.
fn find_current_duplicate(
    sidecar: &redb::Table<'_, [u8; 16], RelationMetadata>,
    scope: RowScope,
    nm: &RelationMetadata,
    rel_id: RelationId,
) -> Result<Option<RelationId>, EntityMergeOpError> {
    if nm.is_current == 0 {
        return Ok(None);
    }
    for entry in sidecar.iter()? {
        let (k, v) = entry?;
        let other_id = RelationId::from(k.value());
        if other_id == rel_id {
            continue;
        }
        let o = v.value();
        if o.namespace_id != scope.namespace_id || o.space_id_bytes != scope.space_id_bytes {
            continue;
        }
        if o.is_current != 0
            && o.relation_type_id == nm.relation_type_id
            && o.from_bytes == nm.from_bytes
            && o.to_bytes == nm.to_bytes
        {
            return Ok(Some(other_id));
        }
    }
    Ok(None)
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
    use crate::entity::ops::{
        entity_get, entity_get_resolved, entity_lookup_by_alias, entity_lookup_by_canonical_name,
        entity_put,
    };
    use crate::relation::ops::{
        relation_create, relation_get, relation_list_from, relation_list_to, RelationListFilter,
    };
    use crate::relation::types::relation_type_intern;
    use crate::schema::predicate::predicate_intern;
    use crate::statement::crud::statement_create;
    use crate::statement::list::{statement_list, StatementListFilter};
    use crate::tables::statement::STATEMENTS_BY_OBJECT_ENTITY_TABLE;
    use crate::MetadataDb;
    use brain_core::{
        Cardinality, Entity, EntityType, EvidenceRef, ExtractorId, PredicateId, Relation,
        RelationId, RelationTypeId, Statement, StatementId, StatementKind, StatementObject,
        SubjectRef,
    };
    use redb::ReadableTable;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;
    const LATER: u64 = NOW + 60_000_000_000; // +1 minute
    const GRACE_SECS: u64 = 7 * 24 * 60 * 60;

    fn db_path(dir: &TempDir) -> PathBuf {
        dir.path().join("metadata.redb")
    }
    use crate::tables::scope::RowScope;
    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
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
        entity_put(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, e).unwrap();
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
                entity_lookup_by_canonical_name(
                    &rtxn,
                    test_scope(),
                    EntityType::PERSON_ID,
                    "Alice"
                )
                .unwrap(),
                Some(alice.id)
            );
            assert_eq!(
                entity_lookup_by_canonical_name(
                    &rtxn,
                    test_scope(),
                    EntityType::PERSON_ID,
                    "Alyss"
                )
                .unwrap(),
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
                MergeActor::Space([1u8; 16]),
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
            entity_lookup_by_canonical_name(&rtxn, test_scope(), EntityType::PERSON_ID, "Alyss")
                .unwrap(),
            None
        );
        let by_alias =
            entity_lookup_by_alias(&rtxn, test_scope(), EntityType::PERSON_ID, "Alyss").unwrap();
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
            MergeActor::Space([1; 16]),
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
            MergeActor::Space([1; 16]),
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
                MergeActor::Space([1; 16]),
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
            MergeActor::Space([1; 16]),
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
                MergeActor::Space([1; 16]),
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
                MergeActor::Space([2; 16]),
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
            entity_lookup_by_canonical_name(&rtxn, test_scope(), EntityType::PERSON_ID, "Alyss")
                .unwrap(),
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
                MergeActor::Space([1; 16]),
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
            MergeActor::Space([2; 16]),
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
        let err = unmerge_entity(&wtxn, alice.id, MergeActor::Space([1; 16]), LATER).unwrap_err();
        assert!(matches!(err, EntityMergeOpError::NotMerged(_)));
    }

    #[test]
    fn merge_tombstoned_rejected() {
        use crate::entity::ops::entity_tombstone;
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
            MergeActor::Space([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap_err();
        assert!(matches!(err, EntityMergeOpError::Tombstoned(_)));
    }

    // ----- Statement / relation re-routing -----------------------------

    fn intern_fact_entity_pred(db: &MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            /* object: Entity */ 1,
            1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn make_fact(
        db: &MetadataDb,
        subject: EntityId,
        predicate: PredicateId,
        object: EntityId,
    ) -> StatementId {
        let s = Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Entity(object),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            NOW,
            1,
        );
        let wtxn = db.write_txn().unwrap();
        let id =
            statement_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &s, NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_rel_type(db: &MetadataDb, name: &str, symmetric: bool) -> RelationTypeId {
        let wtxn = db.write_txn().unwrap();
        let id = relation_type_intern(
            &wtxn,
            "test",
            name,
            None,
            None,
            Cardinality::ManyToMany,
            symmetric,
            1,
            "",
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn make_relation(
        db: &MetadataDb,
        rel_type: RelationTypeId,
        from: EntityId,
        to: EntityId,
        symmetric: bool,
    ) -> RelationId {
        let r = Relation::new_root(
            RelationId::new(),
            rel_type,
            from,
            to,
            0.9,
            vec![],
            ExtractorId::from(0),
            NOW,
            symmetric,
        );
        let wtxn = db.write_txn().unwrap();
        let id =
            relation_create(&wtxn, test_scope(), brain_core::SessionId::DEFAULT, &r, NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn do_merge(db: &MetadataDb, survivor: EntityId, merged: EntityId) -> MergeOutcome {
        let wtxn = db.write_txn().unwrap();
        let out = merge_entity(
            &wtxn,
            survivor,
            merged,
            0.99,
            "dup".into(),
            MergeActor::Space([1; 16]),
            GRACE_SECS,
            LATER,
        )
        .unwrap();
        wtxn.commit().unwrap();
        out
    }

    #[test]
    fn merge_reroutes_subject_and_object_statements_reachable_via_survivor() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let survivor = person("Survivor");
        let merged = person("Merged");
        let other = person("Other");
        put(&mut db, &survivor);
        put(&mut db, &merged);
        put(&mut db, &other);
        let pred = intern_fact_entity_pred(&db, "knows");

        // A fact whose SUBJECT is the merged entity.
        let subj_fact = make_fact(&db, merged.id, pred, other.id);
        // A fact whose OBJECT is the merged entity.
        let obj_fact = make_fact(&db, other.id, pred, merged.id);

        let out = do_merge(&db, survivor.id, merged.id);
        assert_eq!(out.statements_rerouted, 2, "both subject + object rerouted");

        let rtxn = db.read_txn().unwrap();
        // The subject-side fact is now reachable via the survivor.
        let by_subject = statement_list(
            &rtxn,
            test_scope(),
            &StatementListFilter {
                subject: Some(survivor.id),
                predicate: None,
                kind: None,
                current_only: true,
                min_confidence: None,
                limit: 0,
            },
        )
        .unwrap();
        assert!(
            by_subject.iter().any(|s| s.id == subj_fact),
            "subject-merged fact must be reachable via the survivor"
        );
        // No statement remains anchored on the merged subject.
        let stale = statement_list(
            &rtxn,
            test_scope(),
            &StatementListFilter {
                subject: Some(merged.id),
                predicate: None,
                kind: None,
                current_only: false,
                min_confidence: None,
                limit: 0,
            },
        )
        .unwrap();
        assert!(stale.is_empty(), "merged subject index must be emptied");

        // The object-side fact now points at the survivor via the object
        // index, and the stored object decodes to the survivor.
        let byo = rtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
        let s = test_scope();
        let lo = (
            s.namespace_id,
            s.space_id_bytes,
            survivor.id.to_bytes(),
            0u8,
            [0u8; 16],
        );
        let hi = (
            s.namespace_id,
            s.space_id_bytes,
            survivor.id.to_bytes(),
            u8::MAX,
            [0xffu8; 16],
        );
        let mut found_obj = false;
        for e in byo.range(lo..=hi).unwrap() {
            let (_, v) = e.unwrap();
            if v.value() == obj_fact.to_bytes() {
                found_obj = true;
            }
        }
        assert!(
            found_obj,
            "object-merged fact must index under the survivor"
        );
    }

    #[test]
    fn merge_reroutes_relations_from_and_to() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let survivor = person("RelSurvivor");
        let merged = person("RelMerged");
        let other = person("RelOther");
        put(&mut db, &survivor);
        put(&mut db, &merged);
        put(&mut db, &other);
        let rt = intern_rel_type(&db, "reports_to", false);

        // merged -> other (from side), other -> merged (to side).
        let from_rel = make_relation(&db, rt, merged.id, other.id, false);
        let to_rel = make_relation(&db, rt, other.id, merged.id, false);

        let out = do_merge(&db, survivor.id, merged.id);
        assert_eq!(out.relations_rerouted, 2);

        let rtxn = db.read_txn().unwrap();
        let f = RelationListFilter {
            relation_type: None,
            current_only: false,
            limit: 0,
        };
        let from_survivor = relation_list_from(&rtxn, test_scope(), survivor.id, &f).unwrap();
        assert!(
            from_survivor.iter().any(|r| r.id == from_rel),
            "from-side relation reachable via survivor"
        );
        let to_survivor = relation_list_to(&rtxn, test_scope(), survivor.id, &f).unwrap();
        assert!(
            to_survivor.iter().any(|r| r.id == to_rel),
            "to-side relation reachable via survivor"
        );
        // Nothing left anchored on merged.
        assert!(relation_list_from(&rtxn, test_scope(), merged.id, &f)
            .unwrap()
            .is_empty());
        assert!(relation_list_to(&rtxn, test_scope(), merged.id, &f)
            .unwrap()
            .is_empty());

        // The sidecar endpoints reflect the survivor.
        let fr = relation_get(&rtxn, from_rel).unwrap().unwrap();
        assert_eq!(fr.from_entity, survivor.id);
        let tr = relation_get(&rtxn, to_rel).unwrap().unwrap();
        assert_eq!(tr.to_entity, survivor.id);
    }

    #[test]
    fn entity_get_resolved_follows_multi_hop_chain() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let a = person("HopA");
        let b = person("HopB");
        let c = person("HopC");
        put(&mut db, &a);
        put(&mut db, &b);
        put(&mut db, &c);

        // A merged into B, then B merged into C.
        do_merge(&db, b.id, a.id);
        do_merge(&db, c.id, b.id);

        let rtxn = db.read_txn().unwrap();
        // Raw get on A returns the redirect row.
        assert_eq!(
            entity_get(&rtxn, a.id).unwrap().unwrap().merged_into,
            Some(b.id)
        );
        // Resolved get on A collapses the chain to C.
        assert_eq!(entity_get_resolved(&rtxn, a.id).unwrap().unwrap().id, c.id);
        assert_eq!(entity_get_resolved(&rtxn, b.id).unwrap().unwrap().id, c.id);
        assert_eq!(entity_get_resolved(&rtxn, c.id).unwrap().unwrap().id, c.id);
    }

    #[test]
    fn unmerge_restores_rerouted_statements_and_relations() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let survivor = person("URSurvivor");
        let merged = person("URMerged");
        let other = person("UROther");
        put(&mut db, &survivor);
        put(&mut db, &merged);
        put(&mut db, &other);
        let pred = intern_fact_entity_pred(&db, "knows");
        let rt = intern_rel_type(&db, "reports_to", false);

        let subj_fact = make_fact(&db, merged.id, pred, other.id);
        let obj_fact = make_fact(&db, other.id, pred, merged.id);
        let from_rel = make_relation(&db, rt, merged.id, other.id, false);
        let to_rel = make_relation(&db, rt, other.id, merged.id, false);

        do_merge(&db, survivor.id, merged.id);

        // Unmerge within grace.
        {
            let wtxn = db.write_txn().unwrap();
            let restored = unmerge_entity(
                &wtxn,
                merged.id,
                MergeActor::Space([2; 16]),
                LATER + 1_000_000_000,
            )
            .unwrap();
            assert_eq!(restored, survivor.id);
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        // Merged resolves again (redirect cleared).
        assert!(!entity_get(&rtxn, merged.id).unwrap().unwrap().is_merged());
        assert_eq!(
            entity_get_resolved(&rtxn, merged.id).unwrap().unwrap().id,
            merged.id
        );

        // Statements are back on merged.
        let on_merged = statement_list(
            &rtxn,
            test_scope(),
            &StatementListFilter {
                subject: Some(merged.id),
                predicate: None,
                kind: None,
                current_only: true,
                min_confidence: None,
                limit: 0,
            },
        )
        .unwrap();
        assert!(on_merged.iter().any(|s| s.id == subj_fact));
        // And no longer on the survivor.
        let on_survivor = statement_list(
            &rtxn,
            test_scope(),
            &StatementListFilter {
                subject: Some(survivor.id),
                predicate: None,
                kind: None,
                current_only: false,
                min_confidence: None,
                limit: 0,
            },
        )
        .unwrap();
        assert!(!on_survivor.iter().any(|s| s.id == subj_fact));

        // Object fact decodes back to merged.
        let obj = crate::statement::crud::statement_get(&rtxn, obj_fact)
            .unwrap()
            .unwrap();
        assert_eq!(obj.object, StatementObject::Entity(merged.id));

        // Relations are back on merged.
        let f = RelationListFilter {
            relation_type: None,
            current_only: false,
            limit: 0,
        };
        assert!(relation_list_from(&rtxn, test_scope(), merged.id, &f)
            .unwrap()
            .iter()
            .any(|r| r.id == from_rel));
        assert!(relation_list_to(&rtxn, test_scope(), merged.id, &f)
            .unwrap()
            .iter()
            .any(|r| r.id == to_rel));
        assert!(relation_list_from(&rtxn, test_scope(), survivor.id, &f)
            .unwrap()
            .is_empty());
        assert!(relation_list_to(&rtxn, test_scope(), survivor.id, &f)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn attribute_fold_survivor_wins_records_conflict() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let mut survivor = person("AttrSurvivor");
        survivor.attributes = brain_core::EntityAttributes::from(vec![1, 2, 3]);
        let mut merged = person("AttrMerged");
        merged.attributes = brain_core::EntityAttributes::from(vec![9, 9, 9]);
        put(&mut db, &survivor);
        put(&mut db, &merged);

        let out = do_merge(&db, survivor.id, merged.id);
        let merge_id = out.merge_id;

        let rtxn = db.read_txn().unwrap();
        // Survivor keeps its own blob (survivor_wins).
        let sv = entity_get(&rtxn, survivor.id).unwrap().unwrap();
        assert_eq!(sv.attributes.as_bytes(), &[1, 2, 3]);

        // The audit records exactly one conflict, KeptSurvivor.
        let t = rtxn.open_table(MERGE_LOG_TABLE).unwrap();
        let mut rec = None;
        for e in t.iter().unwrap() {
            let (_, v) = e.unwrap();
            let r = v.value();
            if r.merge_id() == merge_id {
                rec = Some(r);
            }
        }
        let rec = rec.unwrap();
        assert_eq!(rec.attribute_conflicts.len(), 1);
        assert_eq!(
            rec.attribute_conflicts[0].outcome,
            crate::tables::merge::conflict_outcome::KEPT_SURVIVOR
        );
    }

    #[test]
    fn attribute_fold_empty_survivor_adopts_merged_and_unmerge_restores_empty() {
        let dir = TempDir::new().unwrap();
        let mut db = fresh_db(&dir);
        let survivor = person("EmptyAttrSurvivor"); // no attributes
        let mut merged = person("HasAttrMerged");
        merged.attributes = brain_core::EntityAttributes::from(vec![7, 7]);
        put(&mut db, &survivor);
        put(&mut db, &merged);

        do_merge(&db, survivor.id, merged.id);
        {
            let rtxn = db.read_txn().unwrap();
            let sv = entity_get(&rtxn, survivor.id).unwrap().unwrap();
            assert_eq!(
                sv.attributes.as_bytes(),
                &[7, 7],
                "empty survivor adopts merged blob"
            );
        }

        // Unmerge restores the survivor to its empty attributes.
        {
            let wtxn = db.write_txn().unwrap();
            unmerge_entity(
                &wtxn,
                merged.id,
                MergeActor::Space([2; 16]),
                LATER + 1_000_000_000,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.read_txn().unwrap();
        let sv = entity_get(&rtxn, survivor.id).unwrap().unwrap();
        assert!(
            sv.attributes.as_bytes().is_empty(),
            "attributes restored to empty"
        );
    }
}
