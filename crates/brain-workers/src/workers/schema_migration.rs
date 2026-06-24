//! Schema migration worker.
//!
//! Triggered by the writer's `submit(Write)` post-commit fan-out for
//! every `Phase::UpsertSchema`. The worker drains
//! [`SchemaFlagSweepJob`]s from its `flume::Receiver` and re-aligns the
//! `OUTSIDE_ACTIVE_SCHEMA` flag bit across the namespace's statements
//! against the just-committed schema vocabulary.
//!
//! ## Why post-commit
//!
//! The flag-sweep is a full `STATEMENTS_TABLE` scan with a per-row
//! predicate-membership check. Running it inside the upload's redb
//! wtxn pinned upload-commit ack latency to corpus size — on large
//! datasets the upload caller would wait seconds before seeing
//! success. Moving the sweep here decouples upload-commit latency
//! from sweep cost: the upload acks as soon as the version + intern
//! writes land, and the sweep catches up within the next worker tick
//! (1 s default).
//!
//! ## Dropped jobs
//!
//! The writer's `try_enqueue_schema_flag_sweep` is best-effort — on a
//! full channel it logs and drops. That's acceptable because:
//!
//! - Drops are observable via `SchemaMigrationMetrics::drops_total`.
//! - A later sweep (admin-triggered or another upload to the same
//!   namespace) re-aligns the flag bit; pre-existing rows merely keep
//!   the stale bit until then.
//! - The flag is advisory — admin tools surface it for cleanup
//!   decisions but it doesn't gate query correctness. A missed sweep
//!   degrades observability, not data.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use brain_metadata::schema::apply::flag_statements_outside_schema;
use brain_metadata::schema::predicate::predicates_active_for_schema;
use brain_ops::{SchemaFlagSweepJob, SchemaMigrationMetrics};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub const WORKER_ID: &str = "schema_migration";

/// Outcome of one flag-sweep against the namespace's `STATEMENTS_TABLE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// Rows that gained the `OUTSIDE_ACTIVE_SCHEMA` bit on this pass.
    pub rows_flagged: usize,
    /// Rows that lost the bit on this pass (predicate re-introduced).
    pub rows_cleared: usize,
}

pub struct SchemaMigrationWorker {
    config: WorkerConfig,
    queue: flume::Receiver<SchemaFlagSweepJob>,
    metrics: Arc<SchemaMigrationMetrics>,
}

impl SchemaMigrationWorker {
    /// Wire up the worker. The matching `flume::Sender` must be
    /// installed on the writer via
    /// `RealWriterHandle::set_schema_flag_sweep_sender` before any
    /// SCHEMA_UPLOAD runs; otherwise the queue stays empty and
    /// pre-existing statements keep their stale flag bit indefinitely.
    #[must_use]
    pub fn new(queue: flume::Receiver<SchemaFlagSweepJob>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::SchemaMigration),
            queue,
            metrics: Arc::new(SchemaMigrationMetrics::new()),
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Install the shared metric handle. Production uses the same
    /// `Arc<SchemaMigrationMetrics>` it handed to the writer via
    /// `RealWriterHandle::set_schema_flag_sweep_metrics`; tests pass a
    /// fresh instance.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<SchemaMigrationMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<SchemaMigrationMetrics> {
        self.metrics.clone()
    }

    /// Current queue depth — surfaces in metrics + tests.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    async fn drive_one_batch(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let mut processed = 0usize;
        let cycle_started = Instant::now();
        while processed < self.config.batch_size {
            if ctx.is_shutdown() || cycle_started.elapsed() >= self.config.max_runtime {
                break;
            }
            let job = match self.queue.try_recv() {
                Ok(j) => j,
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => {
                    tracing::debug!(
                        target: "brain_workers::schema_migration",
                        "schema_flag_sweep sender disconnected; worker idling"
                    );
                    break;
                }
            };

            let sweep_started = Instant::now();
            match self.run_flag_sweep(ctx, &job) {
                Ok(stats) => {
                    let elapsed = sweep_started.elapsed().as_secs_f64();
                    self.metrics.add_sweep_completed();
                    self.metrics.add_rows_flagged(stats.rows_flagged as u64);
                    self.metrics.add_rows_cleared(stats.rows_cleared as u64);
                    self.metrics.observe_sweep_duration_seconds(elapsed);
                    tracing::info!(
                        target: "brain_workers::schema_migration",
                        namespace = %job.namespace,
                        new_version = job.new_version,
                        rows_flagged = stats.rows_flagged,
                        rows_cleared = stats.rows_cleared,
                        duration_seconds = elapsed,
                        "schema flag-sweep complete",
                    );
                }
                Err(e) => {
                    self.metrics.inc_error();
                    tracing::warn!(
                        target: "brain_workers::schema_migration",
                        namespace = %job.namespace,
                        new_version = job.new_version,
                        error = %e,
                        "schema flag-sweep failed; will retry on next enqueue",
                    );
                    // Don't re-enqueue. A subsequent upload (or admin
                    // re-trigger) to the same namespace will re-align
                    // the flag bit; chasing a single failed sweep
                    // forever masks the real error.
                }
            }
            processed += 1;
        }
        Ok(processed)
    }

    fn run_flag_sweep(
        &self,
        ctx: &WorkerContext,
        job: &SchemaFlagSweepJob,
    ) -> Result<SweepStats, WorkerError> {
        let metadata = ctx.ops.executor.metadata.as_ref();

        // Read phase: snapshot the active vocabulary for the namespace
        // + version the upload just committed. The wtxn we'll open
        // shortly sees the same state because the upload's wtxn
        // committed before this enqueue.
        let active = {
            let rtxn = metadata
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("flag_sweep rtxn: {e}")))?;
            predicates_active_for_schema(&rtxn, &job.namespace, job.new_version)
                .map_err(|e| WorkerError::Internal(format!("flag_sweep active vocab: {e}")))?
        };

        // Write phase: walk STATEMENTS_TABLE for this namespace's
        // predicates and flip flag bits to match the active set. The
        // helper internally separates "should flag now but isn't" from
        // "is flagged but shouldn't be" — we have to re-derive the
        // counts because the helper only returns the total count of
        // changed rows. Pre-snapshot the prior-flag state for an
        // exact `(flagged, cleared)` split.
        let pre_flagged_in_namespace = {
            let rtxn = metadata
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("flag_sweep rtxn-2: {e}")))?;
            count_flagged_in_namespace(&rtxn, &job.namespace)?
        };

        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep wtxn: {e}")))?;
        let _changed = flag_statements_outside_schema(&wtxn, &job.namespace, &active)
            .map_err(|e| WorkerError::Internal(format!("flag_sweep: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep commit: {e}")))?;

        let post_flagged_in_namespace = {
            let rtxn = metadata
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("flag_sweep rtxn-3: {e}")))?;
            count_flagged_in_namespace(&rtxn, &job.namespace)?
        };

        // Diff before/after the sweep: rows that gained the flag
        // versus rows that lost it. `_changed` is the helper's
        // total-change count and matches `|gained| + |lost|`.
        let (rows_flagged, rows_cleared) = if post_flagged_in_namespace >= pre_flagged_in_namespace
        {
            (post_flagged_in_namespace - pre_flagged_in_namespace, 0)
        } else {
            (0, pre_flagged_in_namespace - post_flagged_in_namespace)
        };

        Ok(SweepStats {
            rows_flagged,
            rows_cleared,
        })
    }
}

/// Count statements whose predicate lives in `namespace` AND that
/// currently carry the `OUTSIDE_ACTIVE_SCHEMA` bit. Used by the worker
/// to derive an exact `(flagged, cleared)` split across one sweep.
fn count_flagged_in_namespace(
    rtxn: &redb::ReadTransaction,
    namespace: &str,
) -> Result<usize, WorkerError> {
    use brain_core::PredicateId;
    use brain_metadata::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};
    use brain_metadata::tables::statement::{statement_flags, StatementMetadata, STATEMENTS_TABLE};
    use redb::ReadableTable;
    use std::collections::HashSet;

    let mut in_ns: HashSet<PredicateId> = HashSet::new();
    let t = rtxn
        .open_table(PREDICATES_TABLE)
        .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates open: {e}")))?;
    for entry in t
        .iter()
        .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates iter: {e}")))?
    {
        let (k, v) = entry
            .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates entry: {e}")))?;
        let row: PredicateDefinition = v.value();
        if row.namespace == namespace {
            in_ns.insert(PredicateId::from(k.value()));
        }
    }

    let mut count = 0usize;
    let stmts = rtxn
        .open_table(STATEMENTS_TABLE)
        .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts open: {e}")))?;
    for entry in stmts
        .iter()
        .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts iter: {e}")))?
    {
        let (_, v) =
            entry.map_err(|e| WorkerError::Internal(format!("flag_sweep stmts entry: {e}")))?;
        let row: StatementMetadata = v.value();
        let pid = PredicateId::from(row.predicate_id);
        if !in_ns.contains(&pid) {
            continue;
        }
        if row.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA) {
            count += 1;
        }
    }
    Ok(count)
}

impl Worker for SchemaMigrationWorker {
    fn name(&self) -> &'static str {
        WorkerKind::SchemaMigration.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::SchemaMigration
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
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use brain_core::{
        AgentId, ContextId, EntityId, ExtractorId, MemoryId, PredicateId, StatementId,
        StatementKind,
    };
    use brain_core::{
        Entity, EntityType, EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue,
        SubjectRef,
    };
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::schema::predicate::predicate_intern_or_get;
    use brain_metadata::schema::store::schema_upload;
    use brain_metadata::statement::statement_create;
    use brain_metadata::tables::statement::{statement_flags, STATEMENTS_TABLE};
    use brain_metadata::MetadataDb;
    use brain_ops::RealWriterHandle;
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use brain_protocol::schema::{parse_schema, validate, ValidatedSchema};

    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    struct MockDispatcher;
    impl Dispatcher for MockDispatcher {
        fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            texts.iter().map(|_| Ok([0.0; VECTOR_DIM])).collect()
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0x55; 16]
        }
    }

    struct Fixture {
        worker: SchemaMigrationWorker,
        ctx: WorkerContext,
        metadata: SharedMetadataDb,
        tx: flume::Sender<SchemaFlagSweepJob>,
        _tempdir: TempDir,
    }

    fn build_fixture() -> Fixture {
        let tempdir = tempfile::tempdir().unwrap();
        let db_path = tempdir.path().join("metadata.redb");
        let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

        let (tx, rx) = flume::unbounded::<SchemaFlagSweepJob>();
        let metrics = Arc::new(SchemaMigrationMetrics::new());
        let worker = SchemaMigrationWorker::new(rx).with_metrics(metrics);

        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        Fixture {
            worker,
            ctx,
            metadata,
            tx,
            _tempdir: tempdir,
        }
    }

    fn validated_schema(src: &str) -> ValidatedSchema {
        let s = parse_schema(src).expect("parse");
        validate(&s).expect("validate")
    }

    fn schema_with_predicates(namespace: &str, names: &[&str]) -> ValidatedSchema {
        let preds: String = names
            .iter()
            .map(|n| format!("define predicate {n} {{ kind: Fact object: Value<text> }}\n"))
            .collect();
        validated_schema(&format!(
            "
            namespace {namespace}
            define entity_type Person {{ attributes {{}} }}
            {preds}
            ",
        ))
    }

    fn put_subject(metadata: &SharedMetadataDb, agent: AgentId) -> EntityId {
        let _ = agent;
        let id = EntityId::new();
        let wtxn = metadata.write_txn().unwrap();
        entity_put(
            &wtxn,
            &Entity::new_active(
                id,
                EntityType::PERSON_ID,
                "anchor".into(),
                normalize_name("anchor"),
                NOW,
            ),
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn write_statement(
        metadata: &SharedMetadataDb,
        subject: EntityId,
        namespace: &str,
        predicate_name: &str,
    ) -> (StatementId, PredicateId) {
        let wtxn = metadata.write_txn().unwrap();
        let pid = predicate_intern_or_get(&wtxn, namespace, predicate_name, 0, NOW).unwrap();
        let evidence_entry = EvidenceEntry::from_parts(
            MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            1.0,
            0,
            ExtractorId::default(),
        );
        let stmt = Statement::new_root(
            StatementId::new(),
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            pid,
            StatementObject::Value(StatementValue::Text("v".into())),
            0.9,
            EvidenceRef::inline_from_slice(&[evidence_entry]),
            ExtractorId::default(),
            NOW,
            1,
        );
        let sid = statement_create(&wtxn, &stmt, NOW).unwrap();
        wtxn.commit().unwrap();
        (sid, pid)
    }

    fn upload_schema(metadata: &SharedMetadataDb, schema: &ValidatedSchema) -> u32 {
        let wtxn = metadata.write_txn().unwrap();
        let v = schema_upload(&wtxn, schema, NOW).unwrap();
        wtxn.commit().unwrap();
        v
    }

    fn statement_has_outside_flag(metadata: &SharedMetadataDb, sid: StatementId) -> bool {
        let rtxn = metadata.read_txn().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let row = t.get(&sid.to_bytes()).unwrap().unwrap().value();
        row.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA)
    }

    fn drive_once(worker: &SchemaMigrationWorker, ctx: &WorkerContext) -> usize {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(worker.drive_one_batch(ctx)).unwrap()
    }

    #[test]
    fn tick_drains_pending_flag_sweep_jobs() {
        let fx = build_fixture();
        // Enqueue three jobs; even with zero matching rows the worker
        // must process and ack each one.
        for i in 0..3 {
            fx.tx
                .send(SchemaFlagSweepJob {
                    namespace: format!("ns_{i}"),
                    new_version: 1,
                    enqueued_at_unix_nanos: NOW,
                })
                .unwrap();
        }
        assert_eq!(fx.worker.queue_depth(), 3);
        let processed = drive_once(&fx.worker, &fx.ctx);
        assert_eq!(processed, 3);
        assert_eq!(fx.worker.queue_depth(), 0);
        let s = fx.worker.metrics().snapshot();
        assert_eq!(s.sweeps_completed_total, 3);
    }

    #[test]
    fn flag_sweep_marks_statements_outside_active_schema() {
        // Pre-schema: write a statement against the open-vocabulary
        // predicate `acme:ghost`. Then upload a schema that declares
        // only `prefers`. The worker's sweep must flag the ghost row.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, AgentId::default());
        let (sid_ghost, _) = write_statement(&fx.metadata, subject, "acme", "ghost");

        let v = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));
        assert_eq!(v, 1);
        // Pre-sweep: storage layer doesn't set the flag.
        assert!(!statement_has_outside_flag(&fx.metadata, sid_ghost));

        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v,
                enqueued_at_unix_nanos: NOW,
            })
            .unwrap();
        let processed = drive_once(&fx.worker, &fx.ctx);
        assert_eq!(processed, 1);
        assert!(
            statement_has_outside_flag(&fx.metadata, sid_ghost),
            "ghost-predicate row must carry OUTSIDE_ACTIVE_SCHEMA after sweep",
        );
        let s = fx.worker.metrics().snapshot();
        assert_eq!(s.sweeps_completed_total, 1);
        assert_eq!(s.rows_flagged_total, 1);
        assert_eq!(s.rows_cleared_total, 0);
    }

    #[test]
    fn flag_sweep_clears_flag_when_predicate_reappears() {
        // v1 schema declares only `prefers`. Write a `ghost` statement
        // — sweep flags it. v2 schema adds `ghost`. Sweep against v2
        // must CLEAR the flag.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, AgentId::default());
        let (sid_ghost, _) = write_statement(&fx.metadata, subject, "acme", "ghost");

        // v1: ghost is OUT.
        let v1 = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));
        assert_eq!(v1, 1);
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v1,
                enqueued_at_unix_nanos: NOW,
            })
            .unwrap();
        drive_once(&fx.worker, &fx.ctx);
        assert!(statement_has_outside_flag(&fx.metadata, sid_ghost));

        // v2 brings ghost into vocab. Sweep clears the bit.
        let v2 = upload_schema(
            &fx.metadata,
            &schema_with_predicates("acme", &["prefers", "ghost"]),
        );
        assert_eq!(v2, 2);
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v2,
                enqueued_at_unix_nanos: NOW + 1,
            })
            .unwrap();
        drive_once(&fx.worker, &fx.ctx);
        assert!(
            !statement_has_outside_flag(&fx.metadata, sid_ghost),
            "ghost-predicate row must lose the flag after the v2 sweep",
        );
        let s = fx.worker.metrics().snapshot();
        assert!(s.rows_cleared_total >= 1, "snapshot: {s:?}");
    }

    #[test]
    fn sweep_idempotent_on_replay() {
        // Two ticks on the same enqueued job: the second is a no-op
        // (every row is already at its correct flag state).
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, AgentId::default());
        let (sid, _) = write_statement(&fx.metadata, subject, "acme", "ghost");
        let v = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));

        for _ in 0..2 {
            fx.tx
                .send(SchemaFlagSweepJob {
                    namespace: "acme".into(),
                    new_version: v,
                    enqueued_at_unix_nanos: NOW,
                })
                .unwrap();
        }
        drive_once(&fx.worker, &fx.ctx);
        assert!(statement_has_outside_flag(&fx.metadata, sid));
        let snap1 = fx.worker.metrics().snapshot();
        // First sweep flagged exactly one row.
        assert_eq!(snap1.rows_flagged_total, 1);
        // Drain the second job — must be a no-op.
        drive_once(&fx.worker, &fx.ctx);
        let snap2 = fx.worker.metrics().snapshot();
        assert_eq!(
            snap2.rows_flagged_total, snap1.rows_flagged_total,
            "replay sweep must not double-count flagged rows",
        );
        assert_eq!(snap2.sweeps_completed_total, 2);
    }
}
