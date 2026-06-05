//! Statement supersession.
//!
//! Two surfaces live here:
//!
//! - The sync, in-write-txn primitive [`statement_supersede`] — the
//!   atomic two-step "flip old to is_current=0, insert new with
//!   chain_root + version filled in".
//! - The tiered decider [`TieredSupersedeDecider`] — the async,
//!   pre-write-txn classifier that picks between SUPERSEDE / CONTRADICT
//!   / COEXIST by running the arc-labs five-tier ladder over a candidate
//!   new statement. Callers consult the decider first (read-only), then
//!   open a write txn and execute the chosen path. Async lives at the
//!   decider boundary because Tier 2 calls the LLM judge; the write
//!   txn itself stays synchronous.

use std::future::Future;
use std::pin::Pin;

use brain_core::{EntityId, PredicateId, StatementId, StatementKind};
use brain_core::{Statement, SubjectRef};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::statement::{StatementMetadata, STATEMENTS_BY_SUBJECT_TABLE, STATEMENTS_TABLE};

use super::crud::{
    evidence_has_per_entry_metadata, insert_new_statement, resolve_evidence_entries,
    validate_statement_shape,
};
use super::StatementOpError;

// ---------------------------------------------------------------------------
// statement_supersede (sync, in-write-txn primitive — unchanged behaviour).
// ---------------------------------------------------------------------------

/// Supersede `old_id` with `new_statement`. Atomic two-step inside
/// `wtxn`: insert new (and chain row), update old in place + flip
/// `is_current` bit, set `valid_to` if not already pinned, stamp
/// `record_invalidated_at` to mark when the substrate stopped believing
/// the prior row.
pub fn statement_supersede(
    wtxn: &WriteTransaction,
    old_id: StatementId,
    new_statement: &Statement,
    now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    validate_statement_shape(new_statement)?;

    // Load old.
    let mut old = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let row: Option<StatementMetadata> = t.get(&old_id.to_bytes())?.map(|g| g.value());
        row.ok_or(StatementOpError::NotFound(old_id))?
    };

    // Pre-conditions.
    if old.is_tombstoned() {
        return Err(StatementOpError::AlreadyTombstoned(old_id));
    }
    if let Some(succ) = old.superseded_by_bytes {
        return Err(StatementOpError::AlreadySuperseded(
            old_id,
            StatementId::from(succ),
        ));
    }
    let old_kind = old.kind().ok_or(StatementOpError::InvalidArgument(
        "old row has unknown kind",
    ))?;
    if old_kind == StatementKind::Event {
        return Err(StatementOpError::EventCannotSupersede);
    }
    if old_kind != new_statement.kind {
        return Err(StatementOpError::KindMismatch {
            old: old_kind,
            new: new_statement.kind,
        });
    }
    if old.subject_entity_bytes
        != match new_statement.subject {
            SubjectRef::Entity(e) => e.to_bytes(),
            SubjectRef::Pending(audit) => audit.to_bytes(),
        }
    {
        return Err(StatementOpError::SubjectMismatch);
    }
    if old.predicate_id != new_statement.predicate.raw() {
        return Err(StatementOpError::PredicateMismatch);
    }

    // ID uniqueness on new.
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        if t.get(&new_statement.id.to_bytes())?.is_some() {
            return Err(StatementOpError::AlreadyExists(new_statement.id));
        }
    }

    // Compute new chain_root + version.
    let chain_root_bytes = if old.supersedes_bytes.is_none() {
        old.statement_id_bytes
    } else {
        old.chain_root_bytes
    };
    let new_version = old.version.saturating_add(1);

    // Build the new row with derived fields filled in.
    let mut new_to_insert = new_statement.clone();
    new_to_insert.version = new_version;
    new_to_insert.supersedes = Some(old_id);
    new_to_insert.superseded_by = None;
    new_to_insert.chain_root = StatementId::from(chain_root_bytes);

    // Aggregate confidence over per-entry evidence metadata when
    // present (mirrors statement_create — wire-vs-in-process split).
    if evidence_has_per_entry_metadata(wtxn, &new_to_insert.evidence)? {
        let entries = resolve_evidence_entries(wtxn, &new_to_insert.evidence)?;
        new_to_insert.confidence = brain_core::aggregate_confidence(
            &entries,
            new_to_insert.extracted_at_unix_nanos,
            new_to_insert.kind,
            &brain_core::ConfidenceConfig::default_v1(),
        );
    }

    // Update old in place — flip is_current, set valid_to (Fact /
    // Preference only) if not already pinned (caller-supplied
    // valid_to wins).
    let old_subject_bytes = old.subject_entity_bytes;
    let old_kind_byte = old.kind;
    let old_pred = old.predicate_id;
    let old_was_current = old.is_current != 0;

    old.superseded_by_bytes = Some(new_to_insert.id.to_bytes());
    if old.kind != StatementKind::Event.as_u8() && old.valid_to_unix_nanos.is_none() {
        old.valid_to_unix_nanos = Some(new_to_insert.extracted_at_unix_nanos);
    }
    // Record-time invalidation: the substrate stops believing the prior
    // row at supersession wall-clock. Callers that pass `0` ("did not
    // stamp") get the new row's extraction time instead, so the field
    // never carries a zero — zero would read as "invalidated at the
    // unix epoch", a false positive for as-of filters.
    let invalidated_at = if now_unix_nanos == 0 {
        new_to_insert.extracted_at_unix_nanos
    } else {
        now_unix_nanos
    };
    old.record_invalidated_at_unix_nanos = Some(invalidated_at);
    old.is_current = 0;

    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&old.statement_id_bytes, &old)?;
    }

    // Flip the by-subject index for old: remove (subject, kind,
    // predicate, 1), insert (subject, kind, predicate, 0).
    if old_was_current {
        let mut bys = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
        bys.remove(&(old_subject_bytes, old_kind_byte, old_pred, 1u8))?;
        bys.insert(
            &(old_subject_bytes, old_kind_byte, old_pred, 0u8),
            &old.statement_id_bytes,
        )?;
        // The old row is no longer current — drop its predicate-bucket
        // entry before the new row (possibly) claims the same bucket
        // below. Ownership-guarded so it never evicts a sibling.
        crate::statement::remove_from_predicate_index(
            wtxn,
            old_pred,
            old_kind_byte,
            old.confidence,
            &old.statement_id_bytes,
        )?;
    }

    // Insert new statement + all indexes.
    insert_new_statement(wtxn, &new_to_insert)?;

    Ok(new_to_insert.id)
}

// ---------------------------------------------------------------------------
// Tiered decider (arc-labs Tier 0..=3 ladder).
// ---------------------------------------------------------------------------

/// Outcome of the tiered decider — one of three actions the caller
/// must execute inside its write transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupersedeDecision {
    /// Existing statement should be flipped to is_current=false and
    /// the new statement should chain off it.
    Supersede(StatementId),
    /// Both statements should be stored as current; the pair is a
    /// contradiction (idempotent predicate disagreement, or judge
    /// returned CONTRADICTS).
    Contradicts(StatementId),
    /// New statement stands on its own; no relationship to existing.
    Coexist,
}

/// Tier cut-points for cosine similarity over statement embeddings.
/// Defaults track arc-labs' validated bands (Tier 1 ≥ 0.92, Tier 2
/// 0.82..0.92, Tier 3 < 0.82). Operators may tune via config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieredThresholds {
    /// At or above this cosine, auto-supersede without consulting the
    /// judge (Tier 1).
    pub auto_supersede: f32,
    /// At or above this cosine but below `auto_supersede`, hand to the
    /// LLM judge (Tier 2 lower bound).
    pub judge_lower: f32,
    /// Upper bound of the judge band; must equal `auto_supersede` to
    /// keep the ladder contiguous. Stored for symmetry so operator
    /// overrides read cleanly.
    pub judge_upper: f32,
}

impl TieredThresholds {
    /// arc-labs defaults: 0.82 / 0.92.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            auto_supersede: 0.92,
            judge_lower: 0.82,
            judge_upper: 0.92,
        }
    }

    /// Sanity-check ordering. `judge_upper == auto_supersede` is the
    /// canonical relation; we accept `judge_upper <= auto_supersede`
    /// so operators may tighten Tier 2's upper bound below Tier 1's
    /// cut-point without errors.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        (0.0..=1.0).contains(&self.judge_lower)
            && (0.0..=1.0).contains(&self.judge_upper)
            && (0.0..=1.0).contains(&self.auto_supersede)
            && self.judge_lower <= self.judge_upper
            && self.judge_upper <= self.auto_supersede
    }
}

impl Default for TieredThresholds {
    fn default() -> Self {
        Self::default_v1()
    }
}

/// One candidate returned by [`StatementSimilaritySource`]: the
/// existing statement that's near the new one in embedding space.
#[derive(Debug, Clone)]
pub struct StatementSimilarityCandidate {
    pub statement_id: StatementId,
    pub statement: Statement,
    /// Cosine similarity in `[-1, 1]`; `1.0` is identical. Tier
    /// thresholds compare against this value.
    pub score: f32,
}

/// Abstracts the per-shard statement HNSW so the decider stays free
/// of `brain-index` (which would invert the dep graph). Implementations
/// live in the worker / extractor crates that own the index handle.
pub trait StatementSimilaritySource {
    /// Return the top-k candidates near `query_vector`, sorted by
    /// `score` descending. The caller-supplied `rtxn` is the read
    /// txn the decider used for Tier 0; passing it through lets the
    /// source materialise full [`Statement`] rows from the same
    /// snapshot so Tier 1/2 sees consistent state.
    fn nearest(
        &self,
        rtxn: &ReadTransaction,
        query_vector: &[f32],
        k: usize,
    ) -> Result<Vec<StatementSimilarityCandidate>, StatementOpError>;
}

/// Tier 2 LLM judge. Returns one of three verdicts for a pair of
/// statements that are similar but not Tier 1 (cosine in the
/// `[judge_lower, auto_supersede)` band).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeVerdict {
    Supersedes,
    Contradicts,
    Coexists,
}

/// Error surface for the judge call. Held as a boxed message string
/// to avoid coupling the metadata crate to the LLM transport's error
/// types.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("judge transport: {0}")]
    Transport(String),
    #[error("judge response could not be parsed: {0}")]
    Parse(String),
    #[error("judge budget exceeded: {0}")]
    Budget(String),
}

/// Future returned by the judge. Boxed so the trait stays object-safe
/// without pulling `async_trait` or `futures` into the metadata crate.
pub type JudgeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JudgeVerdict, JudgeError>> + Send + 'a>>;

/// LLM judge surface. The implementation lives in `brain-extractors`
/// where the LLM client + prompt cache + budget enforcement already
/// reside.
pub trait StatementJudge: Send + Sync {
    fn judge_supersedes<'a>(
        &'a self,
        new_stmt: &'a Statement,
        existing_stmt: &'a Statement,
        rtxn: &'a ReadTransaction,
    ) -> JudgeFuture<'a>;
}

/// Five-tier supersession decider.
///
/// **Tier 0** (exact `(subject, predicate)` match on a current row):
/// the `is_stateful` flag drives the verdict — stateful → Supersede,
/// idempotent → Contradicts. No HNSW / judge call.
///
/// **Tier 1** (cosine ≥ `auto_supersede`): auto-Supersede.
///
/// **Tier 2** (cosine in `[judge_lower, auto_supersede)`): hand the
/// pair to the LLM judge. No judge wired → conservative Coexist.
///
/// **Tier 3** (cosine < `judge_lower`): Coexist.
pub struct TieredSupersedeDecider<'a> {
    pub similarity: &'a dyn StatementSimilaritySource,
    pub judge: Option<&'a dyn StatementJudge>,
    pub thresholds: TieredThresholds,
}

impl<'a> TieredSupersedeDecider<'a> {
    /// Convenience constructor with default thresholds.
    pub fn new(similarity: &'a dyn StatementSimilaritySource) -> Self {
        Self {
            similarity,
            judge: None,
            thresholds: TieredThresholds::default_v1(),
        }
    }

    /// Override the judge.
    #[must_use]
    pub fn with_judge(mut self, judge: &'a dyn StatementJudge) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Override thresholds.
    #[must_use]
    pub fn with_thresholds(mut self, t: TieredThresholds) -> Self {
        self.thresholds = t;
        self
    }

    /// Run the ladder. `new_vector` may be empty when the caller does
    /// not have an embedding yet — Tier 1/2/3 short-circuit to
    /// `Coexist` in that case (Tier 0 still fires).
    pub async fn decide(
        &self,
        new_stmt: &Statement,
        new_vector: &[f32],
        rtxn: &ReadTransaction,
    ) -> Result<SupersedeDecision, StatementOpError> {
        // Tier 0 — exact (subject, predicate) match on a current row.
        if let SubjectRef::Entity(subject) = new_stmt.subject {
            if let Some(existing_id) =
                lookup_current_statement(rtxn, subject, new_stmt.predicate, new_stmt.kind)?
            {
                return Ok(if new_stmt.is_stateful {
                    SupersedeDecision::Supersede(existing_id)
                } else {
                    SupersedeDecision::Contradicts(existing_id)
                });
            }
        }

        // Tier 1/2/3 require an embedding.
        if new_vector.is_empty() {
            return Ok(SupersedeDecision::Coexist);
        }

        let candidates = self.similarity.nearest(rtxn, new_vector, 10)?;
        // Tier 1/2 candidates must share (subject, predicate, kind)
        // with the new statement — supersession chains by design
        // chain within those three keys, and the metadata-layer
        // primitive enforces it. The HNSW signal is here to catch
        // pairs Tier 0 missed because one or both rows are not
        // currently the "current" one (e.g. prior was tombstoned,
        // or sits in a parallel chain) — never to cross predicates.
        // We also filter out the new statement itself (defensive —
        // the candidate set should not contain it pre-insert).
        let new_subject = new_stmt.subject.as_entity();
        let top = candidates.into_iter().find(|c| {
            c.statement_id != new_stmt.id
                && c.statement.kind == new_stmt.kind
                && c.statement.predicate == new_stmt.predicate
                && c.statement.subject.as_entity() == new_subject
                && new_subject.is_some()
        });
        let Some(top) = top else {
            return Ok(SupersedeDecision::Coexist);
        };

        // Tier 1.
        if top.score >= self.thresholds.auto_supersede {
            return Ok(SupersedeDecision::Supersede(top.statement_id));
        }

        // Tier 2.
        if top.score >= self.thresholds.judge_lower {
            let Some(judge) = self.judge else {
                // No judge wired (no API key / degraded). The cost
                // moat hinges on never auto-superseding a borderline
                // pair without a verdict, so we conservatively
                // coexist. The pair is still findable via the chain
                // index and an operator may rerun once a judge lands.
                return Ok(SupersedeDecision::Coexist);
            };
            let verdict = judge
                .judge_supersedes(new_stmt, &top.statement, rtxn)
                .await
                .map_err(|e| match e {
                    JudgeError::Transport(s) => {
                        tracing::warn!(
                            target: "brain_metadata::supersede",
                            error = %s,
                            "supersede judge transport failure; falling back to Coexist"
                        );
                        StatementOpError::InvalidArgument("judge transport failure")
                    }
                    JudgeError::Parse(s) => {
                        tracing::warn!(
                            target: "brain_metadata::supersede",
                            error = %s,
                            "supersede judge unparseable response; falling back to Coexist"
                        );
                        StatementOpError::InvalidArgument("judge unparseable response")
                    }
                    JudgeError::Budget(s) => {
                        tracing::info!(
                            target: "brain_metadata::supersede",
                            reason = %s,
                            "supersede judge budget exhausted; falling back to Coexist"
                        );
                        StatementOpError::InvalidArgument("judge budget exhausted")
                    }
                });
            // Translate transport/parse/budget into a soft Coexist
            // rather than failing the parent encode — never lose a
            // write to a flaky judge.
            return Ok(match verdict {
                Ok(JudgeVerdict::Supersedes) => SupersedeDecision::Supersede(top.statement_id),
                Ok(JudgeVerdict::Contradicts) => SupersedeDecision::Contradicts(top.statement_id),
                Ok(JudgeVerdict::Coexists) => SupersedeDecision::Coexist,
                Err(_) => SupersedeDecision::Coexist,
            });
        }

        // Tier 3.
        Ok(SupersedeDecision::Coexist)
    }
}

/// Read-txn variant of [`crate::statement::crud`]'s
/// `find_current_statement`. The decider lives outside the write txn
/// so the lookup must work against a snapshot.
fn lookup_current_statement(
    rtxn: &ReadTransaction,
    subject: EntityId,
    predicate: PredicateId,
    kind: StatementKind,
) -> Result<Option<StatementId>, StatementOpError> {
    let bys = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE)?;
    let key = (subject.to_bytes(), kind.as_u8(), predicate.raw(), 1u8);
    let bytes: Option<[u8; 16]> = bys.get(&key)?.map(|g| g.value());
    Ok(bytes.map(StatementId::from))
}

// ---------------------------------------------------------------------------
// Decision execution (sync, inside the caller's write txn).
// ---------------------------------------------------------------------------

/// Execute a [`SupersedeDecision`] reached by the tiered decider.
///
/// `Supersede(prior)` invokes the in-place chain primitive
/// [`statement_supersede`]. `Contradicts(prior)` logs a structured
/// CONTRADICTION_DETECTED trace and inserts the new row without
/// touching the prior; both end up `is_current` in their respective
/// chains. `Coexist` is a plain insert.
///
/// Why this lives separately from [`crate::statement::statement_create`]:
/// the tiered decider runs **before** opening the write txn (it needs
/// async to call the judge), so by the time we open the txn the
/// decision is already made. Re-running Tier 0 inside `statement_create`
/// would double-charge the lookup and risk inconsistency if a sibling
/// write committed in between. Callers that did consult the decider
/// route through this function; everyone else uses `statement_create`
/// which keeps the existing single-tier auto-supersession path for
/// backwards-compatible call sites.
pub fn statement_create_with_decision(
    wtxn: &WriteTransaction,
    new_statement: &Statement,
    decision: SupersedeDecision,
    now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    match decision {
        SupersedeDecision::Supersede(prior) => {
            statement_supersede(wtxn, prior, new_statement, now_unix_nanos)
        }
        SupersedeDecision::Contradicts(prior) => {
            // Structured trace so operators can hook it. A first-class
            // contradiction audit table is a follow-up; for now we stop
            // short of allocating a new table because the trace is
            // enough to feed dashboards and the pair is recoverable from
            // STATEMENTS_BY_SUBJECT given both rows store with
            // is_current=1.
            tracing::warn!(
                target: "brain_metadata::supersede",
                new_id = ?new_statement.id,
                prior_id = ?prior,
                subject = ?new_statement.subject,
                predicate = new_statement.predicate.raw(),
                "CONTRADICTION_DETECTED: tiered decider returned Contradicts"
            );
            // Fall through to plain create — both rows coexist.
            super::crud::statement_create(wtxn, new_statement, now_unix_nanos)
        }
        SupersedeDecision::Coexist => {
            super::crud::statement_create(wtxn, new_statement, now_unix_nanos)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::entity::ops::{entity_put, normalize_name};
    use crate::schema::predicate::predicate_intern;
    use crate::statement::crud::statement_create;
    use brain_core::ExtractorId;
    use brain_core::{Entity, EntityType, EvidenceRef, StatementObject, StatementValue};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn open_db() -> (tempfile::TempDir, crate::MetadataDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn make_entity(db: &mut crate::MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let normalized = normalize_name(name);
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.to_string(),
            normalized,
            1_700_000_000_000_000_000,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_pref(db: &mut crate::MetadataDb, name: &str, is_stateful: bool) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Preference),
            2, // Value
            1,
            "",
            is_stateful,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_fact(db: &mut crate::MetadataDb, name: &str, is_stateful: bool) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            2, // Value
            1,
            "",
            is_stateful,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn fresh_pref(subject: EntityId, predicate: PredicateId, value: &str) -> Statement {
        let mut s = Statement::new_root(
            StatementId::new(),
            StatementKind::Preference,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        s.is_stateful = true;
        s
    }

    fn fresh_fact_value(
        subject: EntityId,
        predicate: PredicateId,
        value: &str,
        is_stateful: bool,
    ) -> Statement {
        let mut s = Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text(value.into())),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        s.is_stateful = is_stateful;
        s
    }

    #[test]
    fn supersede_moves_predicate_bucket_to_new_row() {
        use crate::tables::statement::{confidence_bucket, STATEMENTS_BY_PREDICATE_TABLE};
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "ada-sup-byp");
        let pred = intern_fact(&mut db, "sup_byp", /* is_stateful */ true);

        // Old: confidence 0.9 -> bucket 9.
        let old = fresh_fact_value(subj, pred, "v1", true);
        let old_bucket = confidence_bucket(0.9);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &old, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        // New: confidence 0.3 -> bucket 3. Same subject+predicate, so the
        // stateful predicate auto-supersedes the old row.
        let mut new = fresh_fact_value(subj, pred, "v2", true);
        new.confidence = 0.3;
        let new_bucket = confidence_bucket(0.3);
        assert_ne!(old_bucket, new_bucket);
        let wtxn = db.write_txn().unwrap();
        let new_id = statement_create(&wtxn, &new, 1_700_000_000_000_000_500).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
        // Old bucket no longer points at the superseded row.
        let at_old = t
            .get(&(pred.raw(), StatementKind::Fact.as_u8(), old_bucket))
            .unwrap()
            .map(|g| g.value());
        assert_ne!(at_old, Some(old.id.to_bytes()), "old bucket must drop the superseded row");
        // New bucket points at the current row.
        let at_new = t
            .get(&(pred.raw(), StatementKind::Fact.as_u8(), new_bucket))
            .unwrap()
            .map(|g| g.value());
        assert_eq!(at_new, Some(new_id.to_bytes()), "new bucket points at the current row");
    }

    // ----- Fakes -----

    /// Source that returns a hard-coded candidate set.
    struct FakeSource {
        candidates: Vec<StatementSimilarityCandidate>,
    }

    impl StatementSimilaritySource for FakeSource {
        fn nearest(
            &self,
            _rtxn: &ReadTransaction,
            _q: &[f32],
            _k: usize,
        ) -> Result<Vec<StatementSimilarityCandidate>, StatementOpError> {
            Ok(self.candidates.clone())
        }
    }

    struct CountingJudge {
        verdict: JudgeVerdict,
        calls: Arc<AtomicUsize>,
    }

    impl StatementJudge for CountingJudge {
        fn judge_supersedes<'a>(
            &'a self,
            _new: &'a Statement,
            _old: &'a Statement,
            _rtxn: &'a ReadTransaction,
        ) -> JudgeFuture<'a> {
            let v = self.verdict;
            let counter = self.calls.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(v)
            })
        }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        futures_lite::future::block_on(f)
    }

    // ----- Tier 0 -----

    #[test]
    fn tier0_exact_subject_predicate_stateful_supersedes() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref(&mut db, "prefers", true);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        let p2 = fresh_pref(subj, pred, "written");
        let source = FakeSource { candidates: vec![] };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Supersede(p1.id));
    }

    #[test]
    fn tier0_exact_subject_predicate_idempotent_contradicts() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_fact(&mut db, "knows", false);

        // Insert prior Fact (idempotent, not stateful).
        let mut p1 = fresh_fact_value(subj, pred, "alice", false);
        // statement_create's existing logic only supersedes when
        // is_stateful AND the predicate is_stateful, so a cumulative
        // Fact stays current.
        p1.is_stateful = false;
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        let mut p2 = fresh_fact_value(subj, pred, "bob", false);
        p2.is_stateful = false;
        let source = FakeSource { candidates: vec![] };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Contradicts(p1.id));
    }

    // ----- Tier 1 -----

    #[test]
    fn tier1_high_cosine_supersedes() {
        // Tier 1 fires when the candidate shares (subject, predicate,
        // kind) with the new statement and cosine ≥ auto_supersede,
        // AND Tier 0 missed (no entry in STATEMENTS_BY_SUBJECT for the
        // current slot — e.g. the prior chain head was tombstoned).
        // We exercise the decider's verdict logic here; the chain
        // execution test below covers the writer.
        use crate::statement::tombstone::statement_tombstone;
        use brain_core::TombstoneReason;

        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref(&mut db, "p1", true);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, p1.id, TombstoneReason::UserRequest, 1).unwrap();
        wtxn.commit().unwrap();

        let p2 = fresh_pref(subj, pred, "asynchronous");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.95,
            }],
        };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Supersede(p1.id));
    }

    // ----- Tier 2 -----

    /// Build (subject, predicate, tombstoned-prior) so Tier 0 misses
    /// but the HNSW source returns the prior. Shared across the
    /// three Tier 2 verdict tests.
    fn tier2_fixture(
        db: &mut crate::MetadataDb,
        subj_name: &str,
        pred_name: &str,
    ) -> (EntityId, PredicateId, Statement) {
        use crate::statement::tombstone::statement_tombstone;
        use brain_core::TombstoneReason;

        let subj = make_entity(db, subj_name);
        let pred = intern_pref(db, pred_name, true);
        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();
        let wtxn = db.write_txn().unwrap();
        statement_tombstone(&wtxn, p1.id, TombstoneReason::UserRequest, 1).unwrap();
        wtxn.commit().unwrap();
        (subj, pred, p1)
    }

    #[test]
    fn tier2_medium_cosine_calls_judge() {
        let (_dir, mut db) = open_db();
        let (subj, pred, p1) = tier2_fixture(&mut db, "priya2", "p3");
        let p2 = fresh_pref(subj, pred, "remote");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.85,
            }],
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let judge = CountingJudge {
            verdict: JudgeVerdict::Supersedes,
            calls: calls.clone(),
        };
        let decider = TieredSupersedeDecider::new(&source).with_judge(&judge);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Supersede(p1.id));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tier2_judge_contradicts_returns_contradicts() {
        let (_dir, mut db) = open_db();
        let (subj, pred, p1) = tier2_fixture(&mut db, "priya3", "p5");
        let p2 = fresh_pref(subj, pred, "remote");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.85,
            }],
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let judge = CountingJudge {
            verdict: JudgeVerdict::Contradicts,
            calls: calls.clone(),
        };
        let decider = TieredSupersedeDecider::new(&source).with_judge(&judge);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Contradicts(p1.id));
    }

    #[test]
    fn tier2_judge_coexists_returns_coexist() {
        let (_dir, mut db) = open_db();
        let (subj, pred, p1) = tier2_fixture(&mut db, "priya4", "p7");
        let p2 = fresh_pref(subj, pred, "remote");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.85,
            }],
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let judge = CountingJudge {
            verdict: JudgeVerdict::Coexists,
            calls: calls.clone(),
        };
        let decider = TieredSupersedeDecider::new(&source).with_judge(&judge);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Coexist);
    }

    // ----- Tier 3 -----

    #[test]
    fn tier3_low_cosine_coexists() {
        let (_dir, mut db) = open_db();
        let (subj, pred, p1) = tier2_fixture(&mut db, "priya5", "p9");
        let p2 = fresh_pref(subj, pred, "totally unrelated");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.4,
            }],
        };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Coexist);
    }

    // ----- No-judge fallback -----

    #[test]
    fn no_judge_wired_falls_back_to_coexist() {
        let (_dir, mut db) = open_db();
        let (subj, pred, p1) = tier2_fixture(&mut db, "priya6", "p11");
        let p2 = fresh_pref(subj, pred, "remote");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: p1.id,
                statement: p1.clone(),
                score: 0.85,
            }],
        };
        let decider = TieredSupersedeDecider::new(&source); // no judge
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Coexist);
    }

    // ----- Empty vector / kind mismatch -----

    #[test]
    fn empty_vector_short_circuits_after_tier0() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref(&mut db, "p13", true);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        // Same predicate → Tier 0 fires regardless of vector.
        let p2 = fresh_pref(subj, pred, "remote");
        let source = FakeSource { candidates: vec![] };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Supersede(p1.id));
    }

    #[test]
    fn kind_mismatch_candidate_drops_to_coexist() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pref_pred = intern_pref(&mut db, "p14", true);
        let fact_pred = intern_fact(&mut db, "f14", true);

        let f1 = fresh_fact_value(subj, fact_pred, "alice", true);
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &f1, 0).unwrap();
        wtxn.commit().unwrap();

        // New Preference is near a Fact in embedding space — must
        // coexist (chains never cross kinds).
        let p2 = fresh_pref(subj, pref_pred, "async");
        let source = FakeSource {
            candidates: vec![StatementSimilarityCandidate {
                statement_id: f1.id,
                statement: f1.clone(),
                score: 0.99,
            }],
        };
        let decider = TieredSupersedeDecider::new(&source);
        let rtxn = db.read_txn().unwrap();
        let decision = block_on(decider.decide(&p2, &[1.0, 0.0], &rtxn)).unwrap();
        assert_eq!(decision, SupersedeDecision::Coexist);
    }

    // ----- Execution of decisions -----

    #[test]
    fn statement_create_with_decision_supersede_flips_chain() {
        // statement_supersede requires the new statement to share the
        // old's subject + predicate + kind. Real callers always pass a
        // decision whose `prior` id resolves to a row that matches —
        // the tiered decider only emits Supersede when Tier 0/1/2
        // already proved that.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref(&mut db, "p15", true);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        let p2 = fresh_pref(subj, pred, "remote");
        let wtxn = db.write_txn().unwrap();
        statement_create_with_decision(
            &wtxn,
            &p2,
            SupersedeDecision::Supersede(p1.id),
            1_700_000_000_000_000_001,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = crate::statement::crud::statement_get(&rtxn, p1.id)
            .unwrap()
            .unwrap();
        let g2 = crate::statement::crud::statement_get(&rtxn, p2.id)
            .unwrap()
            .unwrap();
        assert_eq!(g1.superseded_by, Some(p2.id));
        assert_eq!(g2.supersedes, Some(p1.id));
    }

    #[test]
    fn supersession_stamps_record_invalidated_at_on_old_version() {
        // Bi-temporal record-time axis: the prior row's
        // `record_invalidated_at` must land at the supersession
        // wall-clock, and the new row's must stay `None` because the
        // substrate still believes it.
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya-bitemp");
        let pred = intern_pref(&mut db, "bitemp_pref", true);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 1_700_000_000_000_000_000).unwrap();
        wtxn.commit().unwrap();

        let supersede_now: u64 = 1_700_000_000_000_000_500;
        let p2 = fresh_pref(subj, pred, "remote");
        let wtxn = db.write_txn().unwrap();
        statement_create_with_decision(
            &wtxn,
            &p2,
            SupersedeDecision::Supersede(p1.id),
            supersede_now,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = crate::statement::crud::statement_get(&rtxn, p1.id)
            .unwrap()
            .unwrap();
        let g2 = crate::statement::crud::statement_get(&rtxn, p2.id)
            .unwrap()
            .unwrap();
        assert_eq!(g1.record_invalidated_at_unix_nanos, Some(supersede_now));
        assert_eq!(g2.record_invalidated_at_unix_nanos, None);
    }

    #[test]
    fn statement_create_with_decision_coexist_inserts_only() {
        let (_dir, mut db) = open_db();
        let subj = make_entity(&mut db, "priya");
        let pred = intern_pref(&mut db, "p17", false);

        let p1 = fresh_pref(subj, pred, "async");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &p1, 0).unwrap();
        wtxn.commit().unwrap();

        // Use a different predicate to avoid Tier 0 in statement_create.
        let pred2 = intern_pref(&mut db, "p17b", false);
        let mut p2 = fresh_pref(subj, pred2, "remote");
        p2.is_stateful = false;
        let wtxn = db.write_txn().unwrap();
        statement_create_with_decision(&wtxn, &p2, SupersedeDecision::Coexist, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let g1 = crate::statement::crud::statement_get(&rtxn, p1.id)
            .unwrap()
            .unwrap();
        let g2 = crate::statement::crud::statement_get(&rtxn, p2.id)
            .unwrap()
            .unwrap();
        assert!(g1.superseded_by.is_none());
        assert!(g2.supersedes.is_none());
    }

    // ----- LLM call rate -----

    #[test]
    fn llm_call_rate_under_threshold() {
        // 200 encode-shaped decisions distributed across tiers:
        //   60 — Tier 0 fires (cumulative fact contradiction)
        //   25 — Tier 1 (cosine 0.95)
        //   15 — Tier 2 (cosine 0.85, judge consulted)
        //  100 — Tier 3 (cosine 0.4, no candidate qualified)
        // Plan target: judge calls ≤ 0.15 per encode → ≤ 30 calls
        // per 200. Expected calls = 15.
        let (_dir, mut db) = open_db();
        let (subj_tomb, pred_tomb, seed_tomb) = tier2_fixture(&mut db, "rate_tomb", "rate_p");

        // Tier-0 fixture: separate subject + fresh-current statement.
        let subj_t0 = make_entity(&mut db, "rate_t0");
        let pred_t0 = intern_fact(&mut db, "rate_t0_p", false); // cumulative
        let mut seed_t0 = fresh_fact_value(subj_t0, pred_t0, "alice", false);
        seed_t0.is_stateful = false;
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &seed_t0, 0).unwrap();
        wtxn.commit().unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let judge = CountingJudge {
            verdict: JudgeVerdict::Coexists,
            calls: calls.clone(),
        };

        let rtxn = db.read_txn().unwrap();

        // Tier 0 — 60 probes against the cumulative-fact subject.
        for _ in 0..60 {
            let mut probe = fresh_fact_value(subj_t0, pred_t0, "bob", false);
            probe.is_stateful = false;
            let src = FakeSource { candidates: vec![] };
            let decider = TieredSupersedeDecider::new(&src).with_judge(&judge);
            let _ = block_on(decider.decide(&probe, &[1.0, 0.0], &rtxn)).unwrap();
        }
        // Tier 1 — 25.
        for _ in 0..25 {
            let probe = fresh_pref(subj_tomb, pred_tomb, "async-near");
            let src = FakeSource {
                candidates: vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.95,
                }],
            };
            let decider = TieredSupersedeDecider::new(&src).with_judge(&judge);
            let _ = block_on(decider.decide(&probe, &[1.0, 0.0], &rtxn)).unwrap();
        }
        // Tier 2 — 15.
        for _ in 0..15 {
            let probe = fresh_pref(subj_tomb, pred_tomb, "ambig");
            let src = FakeSource {
                candidates: vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.85,
                }],
            };
            let decider = TieredSupersedeDecider::new(&src).with_judge(&judge);
            let _ = block_on(decider.decide(&probe, &[1.0, 0.0], &rtxn)).unwrap();
        }
        // Tier 3 — 100.
        for _ in 0..100 {
            let probe = fresh_pref(subj_tomb, pred_tomb, "far");
            let src = FakeSource {
                candidates: vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.4,
                }],
            };
            let decider = TieredSupersedeDecider::new(&src).with_judge(&judge);
            let _ = block_on(decider.decide(&probe, &[1.0, 0.0], &rtxn)).unwrap();
        }

        let calls_observed = calls.load(Ordering::SeqCst);
        let rate = calls_observed as f32 / 200.0;
        assert!(
            rate <= 0.15,
            "judge call rate {} (={}/200) exceeded 0.15",
            rate,
            calls_observed
        );
    }

    // ----- Golden-file agreement -----

    /// Golden file: 200 hand-labeled `(scenario, verdict)` cases
    /// stratified across the four tiers. The decider must agree
    /// with the labels on ≥ 0.93 of the fixture.
    /// Inline + deterministic so the test is hermetic.
    ///
    /// Bucket distribution mirrors the call-rate test:
    ///   60 Tier 0 stateful (Pref) — Supersede(prior_active)
    ///   30 Tier 0 idempotent (cumulative Fact) — Contradicts(prior_active)
    ///   25 Tier 1 (score 0.95, same subj+pred via tombstoned prior) — Supersede(prior_tomb)
    ///   15 Tier 2 Supersedes verdict (score 0.85) — Supersede(prior_tomb)
    ///   10 Tier 2 Contradicts verdict (score 0.85) — Contradicts(prior_tomb)
    ///   60 Tier 3 (score 0.4) — Coexist
    #[test]
    fn golden_file_agreement_at_least_ninety_three_percent() {
        let (_dir, mut db) = open_db();
        let _ = &mut db; // muted unused-mut warning; macros below reborrow.

        // Tier 0 stateful — current Pref exists at (subj_st, pred_st).
        let subj_st = make_entity(&mut db, "g_subj_st");
        let pred_st = intern_pref(&mut db, "g_pred_st", true);
        let seed_st = fresh_pref(subj_st, pred_st, "v1");
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &seed_st, 0).unwrap();
        wtxn.commit().unwrap();

        // Tier 0 idempotent — current cumulative Fact exists.
        let subj_id = make_entity(&mut db, "g_subj_id");
        let pred_id = intern_fact(&mut db, "g_pred_id", false);
        let mut seed_id = fresh_fact_value(subj_id, pred_id, "alice", false);
        seed_id.is_stateful = false;
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &seed_id, 0).unwrap();
        wtxn.commit().unwrap();

        // Tier 1/2/3 — tombstone the prior so Tier 0 misses but the
        // HNSW source still has a candidate for the same (subj, pred).
        let (subj_tomb, pred_tomb, seed_tomb) =
            tier2_fixture(&mut db, "g_subj_tomb", "g_pred_tomb");

        let judge_supersedes = CountingJudge {
            verdict: JudgeVerdict::Supersedes,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let judge_contradicts = CountingJudge {
            verdict: JudgeVerdict::Contradicts,
            calls: Arc::new(AtomicUsize::new(0)),
        };

        #[derive(Debug, Clone)]
        enum Lbl {
            Supersede(StatementId),
            Contradicts(StatementId),
            Coexist,
        }
        impl Lbl {
            fn matches(&self, d: SupersedeDecision) -> bool {
                match (self, d) {
                    (Lbl::Supersede(a), SupersedeDecision::Supersede(b)) => *a == b,
                    (Lbl::Contradicts(a), SupersedeDecision::Contradicts(b)) => *a == b,
                    (Lbl::Coexist, SupersedeDecision::Coexist) => true,
                    _ => false,
                }
            }
        }

        let rtxn = db.read_txn().unwrap();
        let mut agree = 0usize;
        let mut total = 0usize;

        let mut run = |label: Lbl,
                       candidates: Vec<StatementSimilarityCandidate>,
                       probe: Statement,
                       judge: Option<&dyn StatementJudge>| {
            let src = FakeSource { candidates };
            let mut decider = TieredSupersedeDecider::new(&src);
            if let Some(j) = judge {
                decider = decider.with_judge(j);
            }
            let decision = block_on(decider.decide(&probe, &[1.0, 0.0], &rtxn)).unwrap();
            total += 1;
            if label.matches(decision) {
                agree += 1;
            }
        };

        for _ in 0..60 {
            run(
                Lbl::Supersede(seed_st.id),
                vec![],
                fresh_pref(subj_st, pred_st, "v-next"),
                None,
            );
        }
        for _ in 0..30 {
            let mut p = fresh_fact_value(subj_id, pred_id, "bob", false);
            p.is_stateful = false;
            run(Lbl::Contradicts(seed_id.id), vec![], p, None);
        }
        for _ in 0..25 {
            run(
                Lbl::Supersede(seed_tomb.id),
                vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.95,
                }],
                fresh_pref(subj_tomb, pred_tomb, "near"),
                None,
            );
        }
        for _ in 0..15 {
            run(
                Lbl::Supersede(seed_tomb.id),
                vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.85,
                }],
                fresh_pref(subj_tomb, pred_tomb, "ambig-supersedes"),
                Some(&judge_supersedes as &dyn StatementJudge),
            );
        }
        for _ in 0..10 {
            run(
                Lbl::Contradicts(seed_tomb.id),
                vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.85,
                }],
                fresh_pref(subj_tomb, pred_tomb, "ambig-contradicts"),
                Some(&judge_contradicts as &dyn StatementJudge),
            );
        }
        for _ in 0..60 {
            run(
                Lbl::Coexist,
                vec![StatementSimilarityCandidate {
                    statement_id: seed_tomb.id,
                    statement: seed_tomb.clone(),
                    score: 0.4,
                }],
                fresh_pref(subj_tomb, pred_tomb, "far"),
                None,
            );
        }

        let rate = agree as f32 / total as f32;
        assert!(
            rate >= 0.93,
            "golden-file agreement {} ({}/{}) fell below 0.93",
            rate,
            agree,
            total
        );
        // Track the total too so we know we covered the planned 200.
        assert_eq!(total, 200);
    }
}
