//! FORGET cascade worker.
//!
//! Triggered by the writer's `submit(Write)` post-commit fan-out for
//! every `Phase::Tombstone(Memory)` (soft or hard). The worker drains
//! `ForgetCascadeJob`s from its `flume::Receiver` and walks dependent
//! statements + edges via
//! [`brain_metadata::cascade::cascade_forget_to_statements`] and
//! [`brain_metadata::cascade::cascade_forget_to_edges`].
//!
//! Per Rule 3 the cascade must, for every statement
//! whose `evidence_inline` cites the forgotten memory:
//!
//! 1. Drop the forgotten memory from the evidence list.
//! 2. Re-derive `confidence` via noisy-OR over the remaining evidence
//!    (`brain_core::aggregate_confidence`).
//! 3. If the inline list becomes empty AND no overflow row exists AND
//!    the recomputed confidence is below the cascade threshold,
//!    tombstone the statement with reason `SourceMemoryForgotten`
//!    (supersede-with-null sentinel).
//! 4. Otherwise persist the shrunken row — the statement stays
//!    queryable for audit at its reduced confidence.
//!
//! The cascade also unlinks substrate / mention edges anchored at the
//! forgotten memory and tombstones typed relations whose sole evidence
//! was that memory; multi-evidence relations just drop the evidence
//! entry.
//!
//! ## Soft vs hard
//!
//! Both soft and hard FORGET enqueue the cascade. Rule 3.1 explicitly
//! flags soft-tombstoned memories for re-derivation as soon as the
//! tombstone lands — readers should not see a statement at full
//! confidence backed by a memory the user already forgot, even during
//! the 7-day grace window. The slot-reclamation worker is a separate
//! sweep that operates against the eventual byte-zero; it does NOT
//! enqueue the cascade today (a known gap; see plan W1.5).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use brain_metadata::cascade::{
    cascade_forget_to_edges, cascade_forget_to_statements, DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
};
use brain_ops::{ForgetCascadeJob, ForgetCascadeKind, ForgetCascadeMetrics};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Worker id for the shared `worker_checkpoints` table.
pub const WORKER_ID: &str = "forget_cascade";

/// Per-cascade-job wall-time cap. Heavily-referenced memories
/// produce continuation jobs (post-v1).
pub const PER_JOB_BATCH_CAP: usize = 256;

pub struct ForgetCascadeWorker {
    config: WorkerConfig,
    queue: flume::Receiver<ForgetCascadeJob>,
    confidence_threshold: f32,
    metrics: Arc<ForgetCascadeMetrics>,
}

impl ForgetCascadeWorker {
    /// Wire up the worker. The matching `flume::Sender` must be
    /// installed on the writer via
    /// `RealWriterHandle::set_forget_cascade_sender` before any FORGET
    /// runs; otherwise the cascade queue stays empty and statements
    /// citing forgotten memories remain at their pre-FORGET confidence.
    #[must_use]
    pub fn new(queue: flume::Receiver<ForgetCascadeJob>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::ForgetCascade),
            queue,
            confidence_threshold: DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
            metrics: Arc::new(ForgetCascadeMetrics::new()),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_confidence_threshold(mut self, threshold: f32) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Install the shared metric handle. Production uses the same
    /// `Arc<ForgetCascadeMetrics>` it handed to the writer via
    /// `RealWriterHandle::set_forget_cascade_metrics`; tests pass a
    /// fresh instance.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<ForgetCascadeMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<ForgetCascadeMetrics> {
        self.metrics.clone()
    }

    /// Current queue depth — surfaces in metrics + tests.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    async fn drive_one_batch(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let mut processed = 0usize;
        let started = Instant::now();
        while processed < self.config.batch_size {
            if ctx.is_shutdown() || started.elapsed() >= self.config.max_runtime {
                break;
            }
            let job = match self.queue.try_recv() {
                Ok(j) => j,
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => {
                    tracing::debug!(
                        target: "brain_workers::forget_cascade",
                        "cascade sender disconnected; worker idling"
                    );
                    break;
                }
            };
            if matches!(job.kind, ForgetCascadeKind::Revert) {
                tracing::warn!(
                    target: "brain_workers::forget_cascade",
                    memory_id = ?job.memory_id,
                    "cascade revert requested; v1 implementation pending — job dropped",
                );
                processed += 1;
                continue;
            }
            // Use the FORGET wall-clock as `now` for the noisy-OR
            // recompute. A cascade running minutes after the FORGET
            // must re-derive against the FORGET timestamp, not the
            // drain time — otherwise drain-latency variance would
            // produce different confidences for the same input, and a
            // FORGET stuck in a backlog would silently age every
            // surviving evidence entry by the queue dwell time.
            let now_ns = job.forgot_at_unix_nanos.max(1);
            let metadata = ctx.ops.executor.metadata.as_ref();
            let wtxn = metadata
                .write_txn()
                .map_err(|e| WorkerError::Internal(format!("cascade write_txn: {e}")))?;
            let stmt_summary = cascade_forget_to_statements(
                &wtxn,
                job.memory_id,
                self.confidence_threshold,
                PER_JOB_BATCH_CAP,
                now_ns,
            )
            .map_err(|e| WorkerError::Internal(format!("cascade: {e}")))?;
            // Same wtxn: drop substrate / mention edges anchored at the
            // forgotten memory and tombstone typed relations whose sole
            // evidence was that memory. Splitting these would leave a
            // dangling edge / orphan relation past the FORGET visibility
            // boundary if the second txn failed.
            let edge_summary = cascade_forget_to_edges(&wtxn, job.memory_id, now_ns)
                .map_err(|e| WorkerError::Internal(format!("cascade edges: {e}")))?;
            wtxn.commit()
                .map_err(|e| WorkerError::Internal(format!("cascade commit: {e}")))?;

            self.metrics.add_job_processed();
            self.metrics
                .add_statements_evidence_dropped(stmt_summary.evidence_dropped);
            self.metrics
                .add_statements_tombstoned(stmt_summary.tombstoned);
            self.metrics
                .add_statements_kept_stale(stmt_summary.kept_stale);
            self.metrics
                .add_relations_tombstoned(edge_summary.relations_tombstoned);
            self.metrics
                .add_relations_evidence_dropped(edge_summary.relations_evidence_dropped);
            self.metrics
                .add_edges_unlinked(edge_summary.substrate_unlinked);

            tracing::debug!(
                target: "brain_workers::forget_cascade",
                memory_id = ?job.memory_id,
                mode = ?job.mode,
                scanned = stmt_summary.scanned,
                evidence_dropped = stmt_summary.evidence_dropped,
                kept_stale = stmt_summary.kept_stale,
                tombstoned = stmt_summary.tombstoned,
                substrate_unlinked = edge_summary.substrate_unlinked,
                relations_tombstoned = edge_summary.relations_tombstoned,
                relations_evidence_dropped = edge_summary.relations_evidence_dropped,
                "cascade applied",
            );
            processed += 1;
            let _ = job.forgot_at_unix_nanos;
        }
        Ok(processed)
    }
}

impl Worker for ForgetCascadeWorker {
    fn name(&self) -> &'static str {
        WorkerKind::ForgetCascade.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::ForgetCascade
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.drive_one_batch(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{
        AgentId, ContextId, EntityId, ExtractorId as CoreExtractorId, MemoryId, MemoryKind,
    };
    use brain_core::{
        Entity, EntityType, EvidenceEntry, EvidenceRef, PredicateId, Statement, StatementId,
        StatementKind, StatementObject, StatementValue, SubjectRef,
    };
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::schema::predicate::predicate_intern;
    use brain_metadata::statement::{statement_create, statements_citing_memory};
    use brain_metadata::tables::memory::MemoryMetadata;
    use brain_metadata::tables::statement::STATEMENTS_TABLE;
    use brain_metadata::MetadataDb;
    use brain_ops::ForgetCascadeMode;
    use smallvec::SmallVec;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("md.redb")).unwrap();
        (dir, db)
    }

    fn seed_memory(db: &mut MetadataDb, memory_id: MemoryId) {
        let row = MemoryMetadata::new_active(
            memory_id,
            AgentId::default(),
            ContextId::DEFAULT,
            /* arena_slot */ memory_id.slot(),
            memory_id.version(),
            MemoryKind::Episodic,
            [0u8; 16],
            /* salience */ 0.5,
            /* text_len */ 16,
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn
                .open_table(brain_metadata::tables::memory::MEMORIES_TABLE)
                .unwrap();
            t.insert(&memory_id.to_be_bytes(), row).unwrap();
        }
        wtxn.commit().unwrap();
    }

    fn make_entity(db: &mut MetadataDb, name: &str) -> EntityId {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        );
        let wtxn = db.write_txn().unwrap();
        entity_put(&wtxn, &e).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn intern_pred(db: &mut MetadataDb, name: &str) -> PredicateId {
        let wtxn = db.write_txn().unwrap();
        let id = predicate_intern(
            &wtxn,
            "test",
            name,
            Some(StatementKind::Fact),
            /* object: Value */ 2,
            /* schema_version */ 1,
            "",
            false,
            NOW,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn seed_statement(
        db: &mut MetadataDb,
        predicate: PredicateId,
        subject: EntityId,
        evidence: Vec<(MemoryId, f32)>,
    ) -> StatementId {
        let entries: Vec<EvidenceEntry> = evidence
            .iter()
            .map(|(mid, c)| EvidenceEntry::from_parts(*mid, *c, NOW, CoreExtractorId::from(0)))
            .collect();
        // Statement-level confidence reflects the seeded inline list so
        // re-derivation after FORGET is observably different (the
        // assertion would be hollow if the row already stored the
        // post-cascade value).
        let stmt_conf = if entries.is_empty() {
            0.0
        } else {
            entries.iter().map(|e| e.confidence()).sum::<f32>() / entries.len() as f32
        };
        let id = StatementId::new();
        let mut s = Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            predicate,
            StatementObject::Value(StatementValue::Text("placeholder".into())),
            stmt_conf,
            EvidenceRef::Inline(Box::new(SmallVec::from_vec(entries))),
            CoreExtractorId::from(0),
            NOW,
            1,
        );
        s.confidence = stmt_conf;
        let wtxn = db.write_txn().unwrap();
        statement_create(&wtxn, &s, NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn statement_confidence(db: &MetadataDb, stmt_id: StatementId) -> Option<f32> {
        let rtxn = db.read_txn().ok()?;
        let t = rtxn.open_table(STATEMENTS_TABLE).ok()?;
        let row = t.get(&stmt_id.to_bytes()).ok().flatten()?.value();
        Some(row.confidence)
    }

    fn statement_evidence_len(db: &MetadataDb, stmt_id: StatementId) -> Option<usize> {
        let rtxn = db.read_txn().ok()?;
        let t = rtxn.open_table(STATEMENTS_TABLE).ok()?;
        let row = t.get(&stmt_id.to_bytes()).ok().flatten()?.value();
        Some(row.evidence_inline.len())
    }

    fn statement_is_tombstoned(db: &MetadataDb, stmt_id: StatementId) -> bool {
        let Ok(rtxn) = db.read_txn() else {
            return false;
        };
        let Ok(t) = rtxn.open_table(STATEMENTS_TABLE) else {
            return false;
        };
        match t.get(&stmt_id.to_bytes()) {
            Ok(Some(g)) => g.value().is_tombstoned(),
            _ => false,
        }
    }

    /// Drive the cascade pipeline in-process: enqueue + drain via the
    /// metadata helpers directly (the worker's `Worker::run_cycle`
    /// requires a full `WorkerContext` we don't spin up for unit
    /// tests). Mirrors `drive_one_batch`'s wtxn shape.
    fn drive_cascade(
        db: &mut MetadataDb,
        job: ForgetCascadeJob,
        threshold: f32,
    ) -> brain_metadata::cascade::CascadeSummary {
        let wtxn = db.write_txn().unwrap();
        let stmt_summary = cascade_forget_to_statements(
            &wtxn,
            job.memory_id,
            threshold,
            PER_JOB_BATCH_CAP,
            job.forgot_at_unix_nanos,
        )
        .unwrap();
        let _edge_summary =
            cascade_forget_to_edges(&wtxn, job.memory_id, job.forgot_at_unix_nanos).unwrap();
        wtxn.commit().unwrap();
        stmt_summary
    }

    #[test]
    fn enqueue_grows_queue_depth() {
        let (tx, rx) = flume::unbounded::<ForgetCascadeJob>();
        let w = ForgetCascadeWorker::new(rx);
        tx.send(ForgetCascadeJob {
            memory_id: MemoryId::pack(0, 1, 1),
            mode: ForgetCascadeMode::Soft,
            kind: ForgetCascadeKind::Apply,
            forgot_at_unix_nanos: NOW,
        })
        .unwrap();
        assert_eq!(w.queue_depth(), 1);
    }

    #[test]
    fn cascade_finds_statements_citing_forgotten_memory() {
        let (_dir, mut db) = open_db();
        let m = MemoryId::pack(0, 1, 1);
        seed_memory(&mut db, m);
        let pred = intern_pred(&mut db, "prefers_color");
        let subj = make_entity(&mut db, "alice-finds");
        let s = seed_statement(&mut db, pred, subj, vec![(m, 0.8)]);

        // Pre-cascade: the lookup helper returns the dependent.
        {
            let rtxn = db.read_txn().unwrap();
            let dependents = statements_citing_memory(&rtxn, m).unwrap();
            assert_eq!(dependents, vec![s]);
        }

        let summary = drive_cascade(
            &mut db,
            ForgetCascadeJob {
                memory_id: m,
                mode: ForgetCascadeMode::Hard,
                kind: ForgetCascadeKind::Apply,
                forgot_at_unix_nanos: NOW + 1,
            },
            DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        );
        // The statement had a single piece of evidence; cascade
        // tombstones it (confidence collapses to 0 < threshold).
        assert_eq!(summary.tombstoned, 1);
        assert!(statement_is_tombstoned(&db, s));
        assert_eq!(statement_evidence_len(&db, s), Some(0));
    }

    #[test]
    fn rederive_with_remaining_evidence_keeps_statement_active() {
        let (_dir, mut db) = open_db();
        let m1 = MemoryId::pack(0, 1, 1);
        let m2 = MemoryId::pack(0, 2, 1);
        seed_memory(&mut db, m1);
        seed_memory(&mut db, m2);
        let pred = intern_pred(&mut db, "prefers_color");
        let subj = make_entity(&mut db, "alice-multi");
        let s = seed_statement(&mut db, pred, subj, vec![(m1, 0.8), (m2, 0.8)]);

        let pre = statement_confidence(&db, s).unwrap();

        let summary = drive_cascade(
            &mut db,
            ForgetCascadeJob {
                memory_id: m1,
                mode: ForgetCascadeMode::Hard,
                kind: ForgetCascadeKind::Apply,
                forgot_at_unix_nanos: NOW + 1,
            },
            DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        );
        assert_eq!(summary.evidence_dropped, 1);
        assert_eq!(summary.tombstoned, 0);

        // m1 dropped, m2 remains. Noisy-OR over a single c=0.8 entry
        // with zero age = 0.8 exactly (Fact decay at age 0 = 1.0).
        let post = statement_confidence(&db, s).unwrap();
        assert!(
            (post - 0.8).abs() < 1e-3,
            "expected re-derived confidence ~0.8, got {post} (pre={pre})"
        );
        assert!(!statement_is_tombstoned(&db, s));
        assert_eq!(statement_evidence_len(&db, s), Some(1));
    }

    #[test]
    fn supersede_with_null_when_evidence_empties() {
        // Single evidence; FORGET it; statement loses every evidence
        // pointer. The cascade tombstones with reason
        // `SourceMemoryForgotten` — the supersede-with-null sentinel.
        let (_dir, mut db) = open_db();
        let m = MemoryId::pack(0, 3, 1);
        seed_memory(&mut db, m);
        let pred = intern_pred(&mut db, "prefers_color");
        let subj = make_entity(&mut db, "alice-null");
        let s = seed_statement(&mut db, pred, subj, vec![(m, 0.9)]);

        let summary = drive_cascade(
            &mut db,
            ForgetCascadeJob {
                memory_id: m,
                mode: ForgetCascadeMode::Hard,
                kind: ForgetCascadeKind::Apply,
                forgot_at_unix_nanos: NOW + 1,
            },
            DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        );
        assert_eq!(summary.tombstoned, 1);
        assert_eq!(summary.evidence_dropped, 0);

        // The row stays present so audit can walk it; tombstoned + 0
        // evidence + reason set.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let row = t.get(&s.to_bytes()).unwrap().unwrap().value();
        assert!(row.is_tombstoned());
        assert!(row.evidence_inline.is_empty());
        assert_eq!(row.is_current, 0);
        assert_eq!(
            row.tombstone_reason,
            brain_core::TombstoneReason::SourceMemoryForgotten.as_u8()
        );
    }

    #[test]
    fn cascade_idempotent_on_replay() {
        // Running the cascade twice on the same forgotten memory must
        // not change the state after the first call. The second pass
        // sees no rows that still cite the memory and is a structural
        // no-op.
        let (_dir, mut db) = open_db();
        let m = MemoryId::pack(0, 4, 1);
        seed_memory(&mut db, m);
        let pred = intern_pred(&mut db, "prefers_color");
        let subj = make_entity(&mut db, "alice-idem");
        let _s = seed_statement(&mut db, pred, subj, vec![(m, 0.7)]);

        let first = drive_cascade(
            &mut db,
            ForgetCascadeJob {
                memory_id: m,
                mode: ForgetCascadeMode::Hard,
                kind: ForgetCascadeKind::Apply,
                forgot_at_unix_nanos: NOW + 1,
            },
            DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        );
        assert!(first.tombstoned + first.evidence_dropped + first.kept_stale > 0);

        let second = drive_cascade(
            &mut db,
            ForgetCascadeJob {
                memory_id: m,
                mode: ForgetCascadeMode::Hard,
                kind: ForgetCascadeKind::Apply,
                forgot_at_unix_nanos: NOW + 2,
            },
            DEFAULT_CASCADE_CONFIDENCE_THRESHOLD,
        );
        assert_eq!(second.evidence_dropped, 0);
        assert_eq!(second.tombstoned, 0);
        assert_eq!(second.kept_stale, 0);
    }
}
