//! ConfidenceSweepWorker — periodic Statement confidence refresh.
//!
//! ## Why this exists
//!
//! A Statement's stored confidence is a snapshot, computed at write or
//! touch time (statement_create / statement_supersede / evidence
//! changes). It is *not* lazily recomputed at read. Without periodic
//! refresh the stored value drifts: Facts whose evidence has aged out
//! still report yesterday's confidence, Preferences whose 60-day
//! half-life has elapsed still rank as if they were fresh, and the
//! query ranker (which uses confidence as a weight) keeps surfacing
//! stale rows.
//!
//! This worker walks active Statement rows on a slow cadence,
//! recomputes their confidence via the noisy-OR aggregation from
//! `brain_core::aggregate_confidence`, and writes the new
//! value back when it moved beyond a small floor. Rows under a
//! minimum age (default 1 day) are left alone — they were just
//! touched, no point sweeping them.
//!
//! ## Idempotency
//!
//! Two ticks in a row over the same data converge: after the first
//! tick writes a row's recomputed confidence, the second tick
//! recomputes against the same evidence + `now` and observes
//! `|new - stored| < min_drift`, so no update happens. The property
//! holds even when `max_change_per_tick` clamps drift: subsequent
//! ticks chip away at the gap until convergence.
//!
//! ## Bounded drift
//!
//! `max_change_per_tick` caps how far a single tick can move stored
//! confidence per row. This smooths convergence and keeps a single
//! pathological row from yanking its bucket on `STATEMENTS_BY_PREDICATE`
//! mid-cycle. Set to `0.0` to disable clamping (each row jumps directly
//! to the target).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::{aggregate_confidence, ConfidenceConfig, EvidenceEntry, StatementKind};
use brain_core::{EvidenceOverflowId, ExtractorId, MemoryId, StatementId};
use brain_metadata::statement::{evidence_overflow_load, rekey_predicate_index};
use brain_metadata::tables::statement::{EvidenceEntryRow, StatementMetadata, STATEMENTS_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::ConfidenceSweepMetrics;
use redb::ReadableTable;
use tracing::{debug, trace, warn};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Knobs.
// ---------------------------------------------------------------------------

/// Default cadence: 1 h.
pub const DEFAULT_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_BATCH_SIZE: usize = 256;
pub const DEFAULT_MAX_PER_TICK: usize = 4_096;
pub const DEFAULT_MIN_AGE_SECONDS: u64 = 86_400;
pub const DEFAULT_MIN_DRIFT_FOR_WRITE: f32 = 0.001;
pub const DEFAULT_MAX_CHANGE_PER_TICK: f32 = 0.02;

/// Per-cycle knobs. `WorkerConfig` covers `interval / batch_size /
/// max_runtime`; this struct holds the sweep-specific tuning.
#[derive(Clone, Copy, Debug)]
pub struct ConfidenceSweepKnobs {
    /// Hard cap on rows the worker pulls off the table per cycle.
    /// Larger caps move the system to steady state faster on a fresh
    /// deployment; smaller caps keep the redb write txn short.
    pub max_per_tick: usize,
    /// Skip rows whose `extracted_at_unix_nanos > now - this`. Defaults
    /// to 1 day so freshly written rows don't get touched by the next
    /// sweep tick — they already carry their write-time confidence.
    pub min_age_seconds: u64,
    /// Don't write when `|new - stored| < min_drift_for_write`. Avoids
    /// flooding redb with no-op page updates when the aggregate barely
    /// moved.
    pub min_drift_for_write: f32,
    /// Cap on how much one tick can move a single row's stored
    /// confidence. `0.0` disables the cap (each row jumps to its
    /// target).
    pub max_change_per_tick: f32,
}

impl Default for ConfidenceSweepKnobs {
    fn default() -> Self {
        Self {
            max_per_tick: DEFAULT_MAX_PER_TICK,
            min_age_seconds: DEFAULT_MIN_AGE_SECONDS,
            min_drift_for_write: DEFAULT_MIN_DRIFT_FOR_WRITE,
            max_change_per_tick: DEFAULT_MAX_CHANGE_PER_TICK,
        }
    }
}

// ---------------------------------------------------------------------------
// Worker.
// ---------------------------------------------------------------------------

pub struct ConfidenceSweepWorker {
    config: WorkerConfig,
    knobs: ConfidenceSweepKnobs,
    confidence_config: ConfidenceConfig,
    metadata: Arc<MetadataDb>,
    metrics: Option<Arc<ConfidenceSweepMetrics>>,
}

impl ConfidenceSweepWorker {
    /// Construct with the default cadence + knobs. Override the cadence
    /// with [`Self::with_interval_secs`], which the shard wires from
    /// `[workers.confidence_sweep] interval_secs`.
    #[must_use]
    pub fn new(metadata: Arc<MetadataDb>) -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::ConfidenceSweep);
        config.interval = Duration::from_secs(DEFAULT_INTERVAL_SECS);
        // Cap the per-cycle scan at batch_size so the read txn doesn't
        // sit on the metadata lock arbitrarily long.
        config.batch_size = DEFAULT_BATCH_SIZE;
        Self {
            config,
            knobs: ConfidenceSweepKnobs::default(),
            confidence_config: ConfidenceConfig::default_v1(),
            metadata,
            metrics: None,
        }
    }

    /// Override the sweep cadence. The shard supplies
    /// `[workers.confidence_sweep] interval_secs`; a zero value is
    /// clamped to 1 second so the scheduler never busy-loops.
    #[must_use]
    pub fn with_interval_secs(mut self, interval_secs: u64) -> Self {
        self.config.interval = Duration::from_secs(interval_secs.max(1));
        self
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_knobs(mut self, knobs: ConfidenceSweepKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    #[must_use]
    pub fn with_confidence_config(mut self, cfg: ConfidenceConfig) -> Self {
        self.confidence_config = cfg;
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<ConfidenceSweepMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// One sweep tick. Public so integration tests can drive the
    /// worker without spinning up the scheduler.
    pub async fn tick(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let started = Instant::now();
        let now_ns = now_unix_nanos();
        if let Some(m) = &self.metrics {
            m.inc_cycles();
        }

        // ── Read phase: snapshot up to `max_per_tick` candidates. ───
        let candidates = match self.collect_candidates(ctx, now_ns, self.knobs.max_per_tick.max(1))
        {
            Ok(c) => c,
            Err(e) => {
                if let Some(m) = &self.metrics {
                    m.observe_duration(started.elapsed().as_secs_f64());
                }
                return Err(e);
            }
        };
        let scanned = candidates.len();
        if let Some(m) = &self.metrics {
            m.add_rows_swept(scanned as u64);
        }
        if candidates.is_empty() {
            if let Some(m) = &self.metrics {
                m.set_last_avg_drift(0.0);
                m.observe_duration(started.elapsed().as_secs_f64());
            }
            return Ok(0);
        }

        glommio::executor().yield_if_needed().await;

        // ── Compute targets (no lock held). ─────────────────────────
        let mut updates: Vec<PendingUpdate> = Vec::new();
        let mut drift_sum: f64 = 0.0;
        for cand in &candidates {
            if ctx.is_shutdown() {
                break;
            }
            let target =
                aggregate_confidence(&cand.evidence, now_ns, cand.kind, &self.confidence_config);
            let delta = target - cand.stored_confidence;
            if delta.abs() < self.knobs.min_drift_for_write {
                continue;
            }
            let bounded = if self.knobs.max_change_per_tick > 0.0 {
                let cap = self.knobs.max_change_per_tick;
                cand.stored_confidence + delta.clamp(-cap, cap)
            } else {
                target
            };
            let new_confidence = bounded.clamp(0.0, 1.0);
            // Re-check the floor against the *clamped* value — when the
            // cap is so tight the post-clamp drift is below the floor,
            // we leave the row for the next tick (convergence still
            // happens, just over more cycles).
            if (new_confidence - cand.stored_confidence).abs() < self.knobs.min_drift_for_write {
                continue;
            }
            drift_sum += f64::from((new_confidence - cand.stored_confidence).abs());
            updates.push(PendingUpdate {
                id: cand.id,
                old_confidence: cand.stored_confidence,
                new_confidence,
                predicate_id: cand.predicate_id,
                kind_byte: cand.kind_byte,
            });
        }

        glommio::executor().yield_if_needed().await;

        // ── Write phase: apply in a single wtxn. ───────────────────
        let n_updates = updates.len();
        if !updates.is_empty() {
            if let Err(e) = self.apply_updates(&updates) {
                if let Some(m) = &self.metrics {
                    m.observe_duration(started.elapsed().as_secs_f64());
                }
                return Err(e);
            }
        }

        let avg_drift = if n_updates == 0 {
            0.0
        } else {
            (drift_sum / n_updates as f64) as f32
        };
        if let Some(m) = &self.metrics {
            m.add_rows_updated(n_updates as u64);
            m.set_last_avg_drift(avg_drift);
            m.observe_duration(started.elapsed().as_secs_f64());
        }

        if n_updates > 0 {
            debug!(
                target: "brain_workers::confidence_sweep",
                scanned,
                updated = n_updates,
                avg_drift,
                duration_ms = started.elapsed().as_millis() as u64,
                "confidence sweep applied",
            );
        } else {
            trace!(
                target: "brain_workers::confidence_sweep",
                scanned,
                duration_ms = started.elapsed().as_millis() as u64,
                "confidence sweep tick (no updates)",
            );
        }

        Ok(n_updates)
    }

    /// Read-phase: scan `STATEMENTS_TABLE` from the start, pick rows
    /// that pass the eligibility checks, materialise their evidence.
    /// Returns up to `cap` rows.
    fn collect_candidates(
        &self,
        ctx: &WorkerContext,
        now_ns: u64,
        cap: usize,
    ) -> Result<Vec<Candidate>, WorkerError> {
        let min_age_ns = self.knobs.min_age_seconds.saturating_mul(1_000_000_000);
        let cutoff_ns = now_ns.saturating_sub(min_age_ns);

        let rtxn = self
            .metadata
            .read_txn()
            .map_err(|e| WorkerError::Internal(format!("confidence sweep rtxn: {e}")))?;
        let table = rtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| WorkerError::Internal(format!("open STATEMENTS: {e}")))?;

        let mut out: Vec<Candidate> = Vec::new();
        let iter = table
            .iter()
            .map_err(|e| WorkerError::Internal(format!("iter STATEMENTS: {e}")))?;
        for entry in iter {
            if ctx.is_shutdown() {
                break;
            }
            let (key, value) =
                entry.map_err(|e| WorkerError::Internal(format!("decode STATEMENTS row: {e}")))?;
            let id_bytes = key.value();
            let meta = value.value();
            if !is_eligible(&meta, cutoff_ns) {
                continue;
            }
            let kind = match meta.kind() {
                Some(k) => k,
                None => continue,
            };
            // Event rows can't decay — only skip them when decay is
            // disabled, which is the default. Saves the evidence
            // materialisation cost.
            if matches!(kind, StatementKind::Event) && self.confidence_config.event_decay_disabled {
                continue;
            }
            let evidence = match materialise_evidence(&rtxn, &meta) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        target: "brain_workers::confidence_sweep",
                        statement_id = ?StatementId::from(id_bytes),
                        error = %e,
                        "could not materialise evidence; skipping row",
                    );
                    continue;
                }
            };
            if evidence.is_empty() {
                continue;
            }
            out.push(Candidate {
                id: StatementId::from(id_bytes),
                kind,
                kind_byte: meta.kind,
                stored_confidence: meta.confidence,
                predicate_id: meta.predicate_id,
                evidence,
            });
            if out.len() >= cap {
                break;
            }
        }
        Ok(out)
    }

    /// Write-phase: open one wtxn, write each row, fix up the
    /// `STATEMENTS_BY_PREDICATE` bucket when it moved.
    fn apply_updates(&self, updates: &[PendingUpdate]) -> Result<(), WorkerError> {
        let wtxn = self
            .metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("confidence sweep wtxn: {e}")))?;
        // Bucket re-keys deferred until the STATEMENTS handle drops — the
        // shared `rekey_predicate_index` opens STATEMENTS_BY_PREDICATE
        // itself, and redb forbids holding two handles to one table.
        // Tuple: (predicate_id, kind, old_confidence, new_confidence, id).
        let mut rekey_moves: Vec<(u32, u8, f32, f32, [u8; 16])> = Vec::new();
        {
            let mut s_table = wtxn
                .open_table(STATEMENTS_TABLE)
                .map_err(|e| WorkerError::Internal(format!("open STATEMENTS (w): {e}")))?;
            for u in updates {
                let key = u.id.to_bytes();
                let prior = s_table
                    .get(key)
                    .map_err(|e| WorkerError::Internal(format!("get STATEMENTS row: {e}")))?
                    .map(|g| g.value());
                let Some(mut meta) = prior else { continue };
                // Defensive: if the row changed under us (concurrent
                // supersede / tombstone) skip it — let the next tick
                // pick it up.
                if (meta.confidence - u.old_confidence).abs() > self.knobs.min_drift_for_write {
                    continue;
                }
                if meta.is_tombstoned() || meta.is_current == 0 {
                    continue;
                }
                let old_conf = meta.confidence;
                meta.confidence = u.new_confidence;
                s_table
                    .insert(key, meta)
                    .map_err(|e| WorkerError::Internal(format!("insert STATEMENTS: {e}")))?;
                rekey_moves.push((u.predicate_id, u.kind_byte, old_conf, u.new_confidence, key));
            }
        }
        // Re-key the predicate-bucket index for every row whose confidence
        // moved. The helper is a no-op when the coarse bucket is
        // unchanged and is ownership-guarded against evicting a
        // bucket-sharing sibling.
        for (pred, kind, old_conf, new_conf, id) in rekey_moves {
            rekey_predicate_index(&wtxn, pred, kind, old_conf, new_conf, &id)
                .map_err(|e| WorkerError::Internal(format!("rekey by_predicate: {e}")))?;
        }
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("confidence sweep commit: {e}")))?;
        Ok(())
    }
}

impl Worker for ConfidenceSweepWorker {
    fn name(&self) -> &'static str {
        WorkerKind::ConfidenceSweep.name()
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::ConfidenceSweep
    }

    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }

    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.tick(ctx))
    }
}

// ---------------------------------------------------------------------------
// Helpers — pulled out so unit tests can target them directly.
// ---------------------------------------------------------------------------

/// `decay(age_seconds, kind)` — the per-kind decay function
/// used inside `aggregate_confidence`. Exposed here so the worker's
/// tests can sanity-check the numbers without going through the full
/// noisy-OR path.
///
/// - Fact: 365-day half-life characteristic time (`exp(-age / half_life)`).
/// - Preference: 60-day half-life characteristic time.
/// - Event: no decay (returns 1.0).
#[must_use]
pub fn decay(age_seconds: u64, kind: StatementKind, config: &ConfidenceConfig) -> f32 {
    brain_core::resolution::confidence::decay_for(kind, age_seconds as f32, config)
}

/// A Statement row qualifies for the sweep iff it is current
/// (not tombstoned, not superseded) and was extracted at least
/// `cutoff_ns` ago.
fn is_eligible(meta: &StatementMetadata, cutoff_ns: u64) -> bool {
    if meta.is_tombstoned() || meta.is_current == 0 {
        return false;
    }
    if meta.extracted_at_unix_nanos > cutoff_ns {
        return false;
    }
    true
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Materialise a `StatementMetadata` row's evidence into `EvidenceEntry`
/// values. Inline rows decode directly; overflow rows load via
/// `evidence_overflow_load`.
fn materialise_evidence(
    rtxn: &redb::ReadTransaction,
    meta: &StatementMetadata,
) -> Result<Vec<EvidenceEntry>, WorkerError> {
    if let Some(overflow_bytes) = meta.evidence_overflow_id_bytes {
        let overflow_id = EvidenceOverflowId::from(overflow_bytes);
        let resolved = evidence_overflow_load(rtxn, overflow_id)
            .map_err(|e| WorkerError::Internal(format!("overflow load: {e}")))?
            .unwrap_or_default();
        return Ok(resolved);
    }
    Ok(meta
        .evidence_inline
        .iter()
        .map(EvidenceEntryRow::to_entry)
        .collect())
}

#[allow(dead_code)] // tests use these helpers
fn evidence_entry(memory_byte: u8, confidence: f32, timestamp_unix_nanos: u64) -> EvidenceEntry {
    EvidenceEntry::from_parts(
        MemoryId::pack(memory_byte as u16, brain_core::ContextId::DEFAULT.into(), 0),
        confidence,
        timestamp_unix_nanos,
        ExtractorId::from(0),
    )
}

#[derive(Debug)]
struct Candidate {
    id: StatementId,
    kind: StatementKind,
    kind_byte: u8,
    stored_confidence: f32,
    predicate_id: u32,
    evidence: Vec<EvidenceEntry>,
}

#[derive(Debug)]
struct PendingUpdate {
    id: StatementId,
    old_confidence: f32,
    new_confidence: f32,
    predicate_id: u32,
    kind_byte: u8,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use brain_core::{
        ContextId, EntityId, EvidenceOverflowId, ExtractorId, MemoryId, PredicateId, StatementId,
    };
    use brain_core::{Entity, EntityType, EvidenceRef, Statement, StatementObject, SubjectRef};
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::statement_hnsw::{StatementHnswIndex, StatementHnswParams};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::entity::ops::entity_put;
    use brain_metadata::schema::predicate::predicate_intern_or_get;
    use brain_metadata::statement::{allocate_evidence_overflow, statement_create, statement_get};
    use brain_metadata::MetadataDb;
    use brain_ops::RealWriterHandle;
    use brain_planner::{ExecutorContext, WriterHandle};
    use parking_lot::RwLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    const NS_PER_DAY: u64 = 86_400 * 1_000_000_000;

    fn ctx_now() -> u64 {
        // Frozen "now". Tests reference offsets relative to this.
        1_800_000_000_000_000_000
    }

    // ---------- Pure-function tests ----------

    #[test]
    fn decay_function_fact_half_life_at_365_days() {
        // uses `exp(-age / half_life)` — at age =
        // one half-life the decay factor is `1/e ≈ 0.3679`, not 0.5.
        // Confirm both the published number and the formula shape.
        let cfg = ConfidenceConfig::default_v1();
        let one_year_secs: u64 = 365 * 86_400;
        let d = decay(one_year_secs, StatementKind::Fact, &cfg);
        let expected = (-1.0_f32).exp();
        assert!(
            (d - expected).abs() < 1e-4,
            "decay(365d, Fact) = {d}, expected ~{expected}",
        );
        // At age 0 the factor is 1.0.
        assert!((decay(0, StatementKind::Fact, &cfg) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decay_function_preference_half_life_at_60_days() {
        let cfg = ConfidenceConfig::default_v1();
        let sixty_days_secs: u64 = 60 * 86_400;
        let d = decay(sixty_days_secs, StatementKind::Preference, &cfg);
        let expected = (-1.0_f32).exp();
        assert!(
            (d - expected).abs() < 1e-4,
            "decay(60d, Preference) = {d}, expected ~{expected}",
        );
    }

    #[test]
    fn decay_function_event_no_decay() {
        let cfg = ConfidenceConfig::default_v1();
        assert_eq!(decay(1_000_000, StatementKind::Event, &cfg), 1.0);
        assert_eq!(decay(5 * 365 * 86_400, StatementKind::Event, &cfg), 1.0,);
    }

    #[test]
    fn noisy_or_aggregation_two_evidence_combines() {
        // Two Event-kind evidence rows at c=0.7 and c=0.8, no decay →
        // 1 - (1-0.7)*(1-0.8) = 1 - 0.3 * 0.2 = 0.94.
        let cfg = ConfidenceConfig::default_v1();
        let now = ctx_now();
        let e = [evidence_entry(1, 0.7, now), evidence_entry(2, 0.8, now)];
        let r = aggregate_confidence(&e, now, StatementKind::Event, &cfg);
        assert!((r - 0.94).abs() < 1e-4, "got {r}");
    }

    #[test]
    fn is_eligible_filters_match_post_filter_chain() {
        let now = ctx_now();
        let cutoff = now - NS_PER_DAY;
        let mut meta = StatementMetadata {
            statement_id_bytes: [0u8; 16],
            chain_root_bytes: [0u8; 16],
            version: 1,
            kind: StatementKind::Fact.as_u8(),
            subject_entity_bytes: [0u8; 16],
            subject_kind: 0,
            predicate_id: 1,
            object_blob: Vec::new(),
            object_discriminant: 1,
            confidence: 0.9,
            extractor_id: 0,
            schema_version: 0,
            extracted_at_unix_nanos: cutoff - 1,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            event_at_unix_nanos: None,
            superseded_by_bytes: None,
            supersedes_bytes: None,
            evidence_inline: Vec::new(),
            evidence_overflow_id_bytes: None,
            tombstoned: 0,
            tombstoned_at_unix_nanos: None,
            tombstone_reason: 0,
            record_invalidated_at_unix_nanos: None,
            is_current: 1,
            flags: 0,
            is_stateful: 0,
        };
        assert!(is_eligible(&meta, cutoff));
        meta.tombstoned = 1;
        assert!(!is_eligible(&meta, cutoff));
        meta.tombstoned = 0;
        meta.is_current = 0;
        assert!(!is_eligible(&meta, cutoff));
        meta.is_current = 1;
        meta.extracted_at_unix_nanos = cutoff + 1;
        assert!(!is_eligible(&meta, cutoff));
    }

    // ---------- Integration tests over MetadataDb ----------

    struct NoopDispatcher;
    impl Dispatcher for NoopDispatcher {
        fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0_f32; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(vec![[0.0_f32; VECTOR_DIM]; texts.len()])
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0u8; 16]
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        metadata: Arc<MetadataDb>,
        ctx: WorkerContext,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let metadata = MetadataDb::open(dir.path().join("test.redb")).expect("open metadata");
        let metadata = Arc::new(metadata);
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(NoopDispatcher);
        let (shared, hnsw_writer) =
            SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw::new");
        let writer: Arc<dyn WriterHandle> =
            Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let _statement_hnsw = Arc::new(RwLock::new(
            StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap(),
        ));
        let executor = ExecutorContext::new(dispatcher, shared, metadata.clone(), writer);
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        Fixture {
            _dir: dir,
            metadata,
            ctx,
        }
    }

    /// Seed one Fact statement with the given extraction time and a
    /// single evidence row at the same timestamp + confidence. Returns
    /// the StatementId.
    fn seed_statement_with_age(
        metadata: &Arc<MetadataDb>,
        n: u8,
        extracted_at: u64,
        evidence_confidence: f32,
        evidence_age_offset_ns: u64,
        kind: StatementKind,
    ) -> StatementId {
        let wtxn = metadata.write_txn().unwrap();

        let subj = EntityId::new();
        let obj = EntityId::new();
        entity_put(
            &wtxn,
            &Entity::new_active(
                subj,
                EntityType::PERSON_ID,
                format!("Subject{n}"),
                format!("subject{n}"),
                extracted_at,
            ),
        )
        .unwrap();
        entity_put(
            &wtxn,
            &Entity::new_active(
                obj,
                EntityType::PERSON_ID,
                format!("Object{n}"),
                format!("object{n}"),
                extracted_at,
            ),
        )
        .unwrap();
        let pred =
            predicate_intern_or_get(&wtxn, "test", &format!("p_{n}"), 0, extracted_at).unwrap();

        let stmt_id = StatementId::new();
        // Evidence timestamp = extracted_at - offset (so the evidence
        // is older than the row by `offset` nanos).
        let evidence_ts = extracted_at.saturating_sub(evidence_age_offset_ns);
        let evidence = EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
            MemoryId::pack(n as u16, ContextId::DEFAULT.into(), 0),
            evidence_confidence,
            evidence_ts,
            ExtractorId::from(0),
        )]);
        let mut s = Statement::new_root(
            stmt_id,
            kind,
            SubjectRef::Entity(subj),
            pred,
            StatementObject::Entity(obj),
            // Stored confidence equals the per-evidence c — what the
            // wire write path would have computed at age 0.
            evidence_confidence,
            evidence,
            ExtractorId::from(0),
            extracted_at,
            1,
        );
        if matches!(kind, StatementKind::Event) {
            s.event_at_unix_nanos = Some(extracted_at);
        }
        let id = statement_create(&wtxn, &s, extracted_at).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn read_confidence(metadata: &Arc<MetadataDb>, id: StatementId) -> f32 {
        let rtxn = metadata.read_txn().unwrap();
        let s = statement_get(&rtxn, id).unwrap().unwrap();
        s.confidence
    }

    #[test]
    fn tick_decays_aged_facts() {
        // Seed a Fact whose evidence is one Fact-half-life old.
        // Expected target via noisy-OR: 1 - (1 - 0.9 / e).
        // = 1 - (1 - 0.331) = 0.331.
        // The default max_change_per_tick (0.02) clamps the drop, so a
        // single tick moves from 0.9 → 0.88. Disable the cap and assert
        // we converge to the target.
        let fx = fixture();
        let now = now_unix_nanos();
        let age_secs: u64 = 365 * 86_400;
        let age_ns = age_secs * 1_000_000_000;
        let extracted_at = now.saturating_sub(age_ns);
        let id =
            seed_statement_with_age(&fx.metadata, 0, extracted_at, 0.9, 0, StatementKind::Fact);

        let worker =
            ConfidenceSweepWorker::new(fx.metadata.clone()).with_knobs(ConfidenceSweepKnobs {
                max_per_tick: 64,
                min_age_seconds: 0, // we want this row eligible
                min_drift_for_write: 0.001,
                max_change_per_tick: 0.0, // no clamping — jump to target
            });
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 1, "Fact row should have been updated");
        let new = read_confidence(&fx.metadata, id);
        let expected = 1.0 - (1.0 - 0.9 * (-1.0_f32).exp());
        assert!(
            (new - expected).abs() < 5e-3,
            "Fact decayed to {new}, expected ~{expected}",
        );
    }

    #[test]
    fn tick_idempotent_on_converged_set() {
        let fx = fixture();
        let now = now_unix_nanos();
        let age_ns: u64 = 365 * 86_400 * 1_000_000_000;
        let extracted_at = now.saturating_sub(age_ns);
        let id =
            seed_statement_with_age(&fx.metadata, 0, extracted_at, 0.9, 0, StatementKind::Fact);

        let knobs = ConfidenceSweepKnobs {
            max_per_tick: 64,
            min_age_seconds: 0,
            min_drift_for_write: 0.001,
            max_change_per_tick: 0.0,
        };
        let worker = ConfidenceSweepWorker::new(fx.metadata.clone()).with_knobs(knobs);

        let first = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(first, 1);
        let after_first = read_confidence(&fx.metadata, id);

        // Re-run: target hasn't moved (same `now_unix_nanos` to within
        // microseconds — drift < min_drift_for_write).
        let second = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(second, 0, "converged set produces no updates");
        let after_second = read_confidence(&fx.metadata, id);
        assert!(
            (after_first - after_second).abs() < 1e-4,
            "stored confidence drifted between two converged ticks",
        );
    }

    #[test]
    fn tick_respects_min_age() {
        let fx = fixture();
        let now = now_unix_nanos();
        // Row extracted 1 hour ago — younger than the default
        // min_age_seconds (1 day).
        let extracted_at = now.saturating_sub(3600 * 1_000_000_000);
        let id =
            seed_statement_with_age(&fx.metadata, 0, extracted_at, 0.9, 0, StatementKind::Fact);
        let before = read_confidence(&fx.metadata, id);

        let worker = ConfidenceSweepWorker::new(fx.metadata.clone());
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 0, "young row must not be swept");
        let after = read_confidence(&fx.metadata, id);
        assert!((before - after).abs() < 1e-6);
    }

    #[test]
    fn tick_caps_drift_at_max_change() {
        // Seed a Fact whose target drift in one tick is ~0.57 (0.9 →
        // 0.331). Default max_change_per_tick=0.02 — assert the row
        // moves exactly that far.
        let fx = fixture();
        let now = now_unix_nanos();
        let age_ns: u64 = 365 * 86_400 * 1_000_000_000;
        let extracted_at = now.saturating_sub(age_ns);
        let id =
            seed_statement_with_age(&fx.metadata, 0, extracted_at, 0.9, 0, StatementKind::Fact);
        let before = read_confidence(&fx.metadata, id);

        let worker =
            ConfidenceSweepWorker::new(fx.metadata.clone()).with_knobs(ConfidenceSweepKnobs {
                max_per_tick: 64,
                min_age_seconds: 0,
                min_drift_for_write: 0.001,
                max_change_per_tick: 0.02,
            });
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 1);
        let after = read_confidence(&fx.metadata, id);
        let diff = (before - after).abs();
        assert!(
            (diff - 0.02).abs() < 1e-3,
            "drift {diff} exceeded max_change_per_tick=0.02",
        );
    }

    #[test]
    fn tick_skips_event_when_decay_disabled() {
        let fx = fixture();
        let now = now_unix_nanos();
        let extracted_at = now.saturating_sub(5 * 365 * 86_400 * 1_000_000_000);
        let id =
            seed_statement_with_age(&fx.metadata, 0, extracted_at, 0.9, 0, StatementKind::Event);
        let before = read_confidence(&fx.metadata, id);

        let worker =
            ConfidenceSweepWorker::new(fx.metadata.clone()).with_knobs(ConfidenceSweepKnobs {
                max_per_tick: 64,
                min_age_seconds: 0,
                min_drift_for_write: 0.001,
                max_change_per_tick: 0.0,
            });
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 0);
        let after = read_confidence(&fx.metadata, id);
        assert!((before - after).abs() < 1e-6);
    }

    #[test]
    fn tick_clears_metrics_drift_when_no_updates() {
        let fx = fixture();
        // No rows at all — empty sweep.
        let metrics = Arc::new(ConfidenceSweepMetrics::new());
        let worker = ConfidenceSweepWorker::new(fx.metadata.clone()).with_metrics(metrics.clone());
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 0);
        let snap = metrics.snapshot();
        assert_eq!(snap.cycles_total, 1);
        assert_eq!(snap.rows_updated_total, 0);
        assert_eq!(snap.last_avg_drift, 0.0);
    }

    // env-override parsing is tested once in crate::env.

    // Use the suppressed helper to satisfy clippy's dead-code check
    // when integration tests are compiled but unused.
    #[test]
    fn unused_helpers_compile() {
        let e = evidence_entry(1, 0.5, 0);
        assert_eq!(e.confidence(), 0.5);
        let _ = EvidenceOverflowId::new();
        let _ = PredicateId::from(0u32);
    }

    /// Build an evidence-overflow Fact row directly (12 evidence rows,
    /// overflowing the inline cap), age it, and assert the sweep
    /// materialises overflow evidence correctly and recomputes
    /// confidence over the full set.
    #[test]
    fn tick_handles_evidence_overflow() {
        let fx = fixture();
        let now = now_unix_nanos();
        let age_ns: u64 = 365 * 86_400 * 1_000_000_000;
        let extracted_at = now.saturating_sub(age_ns);

        let stmt_id = {
            let wtxn = fx.metadata.write_txn().unwrap();
            let subj = EntityId::new();
            let obj = EntityId::new();
            entity_put(
                &wtxn,
                &Entity::new_active(
                    subj,
                    EntityType::PERSON_ID,
                    "Subject".into(),
                    "subject".into(),
                    extracted_at,
                ),
            )
            .unwrap();
            entity_put(
                &wtxn,
                &Entity::new_active(
                    obj,
                    EntityType::PERSON_ID,
                    "Object".into(),
                    "object".into(),
                    extracted_at,
                ),
            )
            .unwrap();
            let pred =
                predicate_intern_or_get(&wtxn, "test", "p_overflow", 0, extracted_at).unwrap();

            // 12 evidence rows — overflow.
            let entries: Vec<EvidenceEntry> = (0..12)
                .map(|i| {
                    EvidenceEntry::from_parts(
                        MemoryId::pack(i as u16 + 1, ContextId::DEFAULT.into(), 0),
                        0.5,
                        extracted_at,
                        ExtractorId::from(0),
                    )
                })
                .collect();
            let overflow_id = allocate_evidence_overflow(&wtxn, &entries, extracted_at).unwrap();
            let evidence = EvidenceRef::Overflow(overflow_id);

            let stmt_id = StatementId::new();
            let s = Statement::new_root(
                stmt_id,
                StatementKind::Fact,
                SubjectRef::Entity(subj),
                pred,
                StatementObject::Entity(obj),
                0.9, // intentionally stale: sweep should recompute lower
                evidence,
                ExtractorId::from(0),
                extracted_at,
                1,
            );
            let id = statement_create(&wtxn, &s, extracted_at).unwrap();
            wtxn.commit().unwrap();
            id
        };

        let worker =
            ConfidenceSweepWorker::new(fx.metadata.clone()).with_knobs(ConfidenceSweepKnobs {
                max_per_tick: 64,
                min_age_seconds: 0,
                min_drift_for_write: 0.001,
                max_change_per_tick: 0.0,
            });
        let updated = futures_lite::future::block_on(worker.tick(&fx.ctx)).unwrap();
        assert_eq!(updated, 1, "overflow-evidence row should be updated");
        let new = read_confidence(&fx.metadata, stmt_id);
        // Sanity: 12 evidence rows at c=0.5 with a Fact 1/e decay
        // factor give a noisy-OR that is well above the original 0.9
        // (more independent evidence → higher aggregate even with
        // decay). The exact value depends on the decay form; here we
        // assert the worker wrote a recomputed value distinct from
        // the seeded 0.9.
        assert!(
            (new - 0.9).abs() > 0.001,
            "overflow evidence was not recomputed (still {new})",
        );
    }
}
