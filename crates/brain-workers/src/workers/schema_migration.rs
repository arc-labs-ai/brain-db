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

use brain_core::MemoryId;
use brain_metadata::schema::predicate::predicates_active_for_schema;
use brain_ops::{SchemaFlagSweepJob, SchemaMigrationMetrics};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

pub const WORKER_ID: &str = "schema_migration";

/// Outcome of one flag-sweep against the namespace's `STATEMENTS_TABLE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// Rows that gained the `OUTSIDE_ACTIVE_SCHEMA` bit on this pass.
    pub rows_flagged: usize,
    /// Rows that lost the bit on this pass (predicate re-introduced).
    pub rows_cleared: usize,
    /// Distinct source memories behind the newly-flagged rows, from each
    /// statement's evidence. These are the memories whose extraction
    /// predates the new vocabulary — the exact set a re-extraction would
    /// need to touch, and the number an operator needs before deciding to
    /// pay for one. Collected during the sweep's existing scan, so it
    /// costs no extra pass.
    pub stale_memories: Vec<[u8; 16]>,
}

pub struct SchemaMigrationWorker {
    config: WorkerConfig,
    queue: flume::Receiver<SchemaFlagSweepJob>,
    metrics: Arc<SchemaMigrationMetrics>,
    /// Mirrors `[workers.extractor] reextract_on_schema_change`. See
    /// [`Self::with_reextract_on_schema_change`].
    reextract_on_schema_change: bool,
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
            reextract_on_schema_change: false,
        }
    }

    /// Enqueue the memories behind newly-flagged statements for
    /// re-extraction after each sweep.
    ///
    /// Off by default because re-extraction is one LLM call per memory and
    /// a single schema upload can cover a whole corpus. With it off the
    /// sweep still reports the affected count so an operator can run the
    /// same work through `POST /v1/extract/backfill` on purpose.
    #[must_use]
    pub fn with_reextract_on_schema_change(mut self, on: bool) -> Self {
        self.reextract_on_schema_change = on;
        self
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
                        stale_memories = stats.stale_memories.len(),
                        duration_seconds = elapsed,
                        "schema flag-sweep complete",
                    );
                    self.settle_stale_memories(ctx, &job.namespace, job.new_version, &stats);
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

    /// Decide what happens to the memories the sweep found outside the new
    /// schema: re-extract them, or just say how many there are.
    ///
    /// This is the step whose absence made the schema-migration worker's own
    /// module doc wrong. The stale-extraction detector said "the
    /// schema-migration worker is the side that re-extracts"; it only ever
    /// flipped an advisory flag bit, so uploading a schema never re-aligned
    /// a single existing memory and nothing said so.
    fn settle_stale_memories(
        &self,
        ctx: &WorkerContext,
        namespace: &str,
        new_version: u32,
        stats: &SweepStats,
    ) {
        if stats.stale_memories.is_empty() {
            return;
        }
        if !self.reextract_on_schema_change {
            // The operator's cue. Not a warning: staying flagged is the
            // configured behaviour, not a fault.
            tracing::info!(
                target: "brain_workers::schema_migration",
                namespace = %namespace,
                new_version,
                stale_memories = stats.stale_memories.len(),
                "memories carry statements outside the new schema; \
                 re-extraction is off (workers.extractor.reextract_on_schema_change). \
                 Run POST /v1/extract/backfill to re-align them",
            );
            return;
        }

        let metadata = ctx.ops.executor.metadata.as_ref();
        let writer = ctx.ops.executor.writer.as_ref();
        let mut enqueued = 0usize;
        let mut skipped = 0usize;
        for id_bytes in &stats.stale_memories {
            if ctx.is_shutdown() {
                break;
            }
            match memory_text_for_reextraction(metadata, *id_bytes) {
                Some(text) => {
                    if writer.enqueue_for_extraction(MemoryId::from_be_bytes(*id_bytes), &text) {
                        enqueued += 1;
                    } else {
                        // Channel full or unwired. The statement keeps its
                        // flag, so the next upload (or the admin backfill)
                        // picks it up — never re-queued here, which would
                        // spin against a full channel.
                        skipped += 1;
                    }
                }
                // Tombstoned, hard-forgotten, or text row gone: there is
                // nothing left to re-extract from.
                None => skipped += 1,
            }
        }
        tracing::info!(
            target: "brain_workers::schema_migration",
            namespace = %namespace,
            new_version,
            enqueued,
            skipped,
            "re-extraction enqueued for memories outside the new schema",
        );
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

        // Write phase: a single scan of the namespace's statements that
        // both flips flag bits to match `active` and tallies the exact
        // per-direction transition counts. This replaces the previous
        // approach that derived counts from a before/after net diff
        // (which mis-attributes a mixed pass — a "+5 gained / -3 lost"
        // sweep collapses to "+2 flagged / 0 cleared") and ran two extra
        // full-table scans on top of the sweep's own scan.
        let wtxn = metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep wtxn: {e}")))?;
        let stats = sweep_and_count(&wtxn, &job.namespace, &active)?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep commit: {e}")))?;

        Ok(stats)
    }
}

/// Resolve a memory's text for re-extraction, or `None` when there is
/// nothing to re-extract from.
///
/// Applies the same skip rules as the admin `EXTRACT_BACKFILL` path — an
/// inactive or hard-forgotten memory, a missing text row, or non-UTF-8
/// bytes are all "skip", never an error. Re-extraction is best-effort
/// maintenance; one unreadable row must not abort the batch.
fn memory_text_for_reextraction(
    metadata: &brain_metadata::MetadataDb,
    memory_id_bytes: [u8; 16],
) -> Option<String> {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    use brain_metadata::tables::text::TEXTS_TABLE;

    let rtxn = metadata.read_txn().ok()?;
    let memories = rtxn.open_table(MEMORIES_TABLE).ok()?;
    let row = memories.get(&memory_id_bytes).ok()??.value();
    if !row.is_active() || row.is_hard_forgotten() {
        return None;
    }
    let texts = rtxn.open_table(TEXTS_TABLE).ok()?;
    let guard = texts.get(&memory_id_bytes).ok()??;
    std::str::from_utf8(guard.value()).ok().map(str::to_owned)
}

/// One-pass flag-sweep over `namespace`'s statements. Flips the
/// `OUTSIDE_ACTIVE_SCHEMA` bit on every in-namespace statement to match
/// `active_predicate_ids` and returns the exact `(rows_flagged,
/// rows_cleared)` split observed during the scan.
///
/// Counting the two directions directly from the single sweep scan is
/// what makes a mixed pass report faithfully: a net before/after diff
/// would cancel gains against losses (5 gained and 3 lost would look
/// like 2 flagged / 0 cleared), and it would also require two additional
/// full-table scans. Here the scan that computes the updates is the same
/// one that produces the counts.
fn sweep_and_count(
    wtxn: &redb::WriteTransaction,
    namespace: &str,
    active_predicate_ids: &std::collections::HashSet<brain_core::PredicateId>,
) -> Result<SweepStats, WorkerError> {
    use brain_core::PredicateId;
    use brain_metadata::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};
    use brain_metadata::tables::statement::{statement_flags, StatementMetadata, STATEMENTS_TABLE};
    use redb::ReadableTable;
    use std::collections::HashSet;

    // Which predicate ids belong to this namespace — so we skip rows
    // owned by other namespaces entirely.
    let in_namespace: HashSet<PredicateId> = {
        let t = wtxn
            .open_table(PREDICATES_TABLE)
            .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates open: {e}")))?;
        let mut set = HashSet::new();
        for entry in t
            .iter()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates iter: {e}")))?
        {
            let (k, v) = entry
                .map_err(|e| WorkerError::Internal(format!("flag_sweep predicates entry: {e}")))?;
            let row: PredicateDefinition = v.value();
            if row.namespace == namespace {
                set.insert(PredicateId::from(k.value()));
            }
        }
        set
    };

    // Single scan: compute the rows that need a transition, tallying the
    // gained / lost split as we go.
    let mut rows_flagged = 0usize;
    let mut rows_cleared = 0usize;
    // Distinct, insertion-ordered: one memory usually backs several
    // newly-flagged statements, and re-extracting it once re-derives all of
    // them. A set would lose the order the scan found them in, which is the
    // closest thing we have to "oldest first" for a bounded backfill.
    let mut stale_memories: Vec<[u8; 16]> = Vec::new();
    let mut seen_memories: HashSet<[u8; 16]> = HashSet::new();
    let updates: Vec<([u8; 16], StatementMetadata)> = {
        let t = wtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts open: {e}")))?;
        let mut out = Vec::new();
        for entry in t
            .iter()
            .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts iter: {e}")))?
        {
            let (k, v) =
                entry.map_err(|e| WorkerError::Internal(format!("flag_sweep stmts entry: {e}")))?;
            let row: StatementMetadata = v.value();
            let pid = PredicateId::from(row.predicate_id);
            if !in_namespace.contains(&pid) {
                continue;
            }
            let should_flag = !active_predicate_ids.contains(&pid);
            let has_flag = row.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
            if should_flag == has_flag {
                continue;
            }
            let mut new_row = row;
            if should_flag {
                new_row.set_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
                rows_flagged += 1;
                // Evidence is how a statement points back at the memory it
                // was extracted from; there is no direct source-memory
                // column. A statement with no evidence (hand-written via
                // STATEMENT_CREATE rather than extracted) contributes
                // nothing, which is right: re-extraction cannot reproduce a
                // row no memory ever produced.
                for ev in &new_row.evidence_inline {
                    if seen_memories.insert(ev.memory_id_bytes) {
                        stale_memories.push(ev.memory_id_bytes);
                    }
                }
            } else {
                new_row.clear_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
                rows_cleared += 1;
            }
            out.push((k.value(), new_row));
        }
        out
    };

    {
        let mut t = wtxn
            .open_table(STATEMENTS_TABLE)
            .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts open-w: {e}")))?;
        for (k, row) in updates {
            t.insert(&k, &row)
                .map_err(|e| WorkerError::Internal(format!("flag_sweep stmts insert: {e}")))?;
        }
    }

    Ok(SweepStats {
        rows_flagged,
        rows_cleared,
        stale_memories,
    })
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
    fn __ts() -> brain_metadata::RowScope {
        brain_metadata::RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xA1; 16])
    }

    use super::*;
    use brain_core::{
        Entity, EntityType, EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue,
        SubjectRef,
    };
    use brain_core::{
        EntityId, ExtractorId, MemoryId, PredicateId, SessionId, SpaceId, StatementId,
        StatementKind,
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

    fn put_subject(metadata: &SharedMetadataDb, space: SpaceId) -> EntityId {
        let _ = space;
        let id = EntityId::new();
        let wtxn = metadata.write_txn().unwrap();
        entity_put(
            &wtxn,
            __ts(),
            brain_core::SessionId::DEFAULT,
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
            MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
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
        let sid =
            statement_create(&wtxn, __ts(), brain_core::SessionId::DEFAULT, &stmt, NOW).unwrap();
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
        let subject = put_subject(&fx.metadata, SpaceId::default());
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
        let subject = put_subject(&fx.metadata, SpaceId::default());
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
    fn mixed_pass_reports_both_flagged_and_cleared_counts() {
        // A single sweep that both flags some rows and clears others must
        // report each direction faithfully — not the net. Set up 5 rows
        // that gain the flag and 3 that lose it in one pass; expect
        // (flagged=5, cleared=3), not the net (2, 0).
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());

        // "Clear" group: 3 predicates. "Flag" group: 5 predicates.
        let clear_preds = ["cx1", "cx2", "cx3"];
        let flag_preds = ["fy1", "fy2", "fy3", "fy4", "fy5"];
        let mut clear_sids = Vec::new();
        let mut flag_sids = Vec::new();
        for p in clear_preds {
            let (sid, _) = write_statement(&fx.metadata, subject, "acme", p);
            clear_sids.push(sid);
        }
        for p in flag_preds {
            let (sid, _) = write_statement(&fx.metadata, subject, "acme", p);
            flag_sids.push(sid);
        }

        // v1 declares only the flag-group predicates. Sweep flags the 3
        // clear-group rows; the flag-group rows stay clear.
        let v1 = upload_schema(&fx.metadata, &schema_with_predicates("acme", &flag_preds));
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v1,
                enqueued_at_unix_nanos: NOW,
            })
            .unwrap();
        drive_once(&fx.worker, &fx.ctx);
        for sid in &clear_sids {
            assert!(statement_has_outside_flag(&fx.metadata, *sid));
        }
        for sid in &flag_sids {
            assert!(!statement_has_outside_flag(&fx.metadata, *sid));
        }
        let snap_v1 = fx.worker.metrics().snapshot();

        // v2 declares only the clear-group predicates. The single sweep
        // now clears the 3 clear-group rows AND flags the 5 flag-group
        // rows — a mixed pass.
        let v2 = upload_schema(&fx.metadata, &schema_with_predicates("acme", &clear_preds));
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v2,
                enqueued_at_unix_nanos: NOW + 1,
            })
            .unwrap();
        drive_once(&fx.worker, &fx.ctx);
        for sid in &clear_sids {
            assert!(!statement_has_outside_flag(&fx.metadata, *sid));
        }
        for sid in &flag_sids {
            assert!(statement_has_outside_flag(&fx.metadata, *sid));
        }

        let snap_v2 = fx.worker.metrics().snapshot();
        let flagged_delta = snap_v2.rows_flagged_total - snap_v1.rows_flagged_total;
        let cleared_delta = snap_v2.rows_cleared_total - snap_v1.rows_cleared_total;
        assert_eq!(
            flagged_delta, 5,
            "mixed pass must report 5 rows flagged, not the net",
        );
        assert_eq!(
            cleared_delta, 3,
            "mixed pass must report 3 rows cleared, not 0",
        );
    }

    #[test]
    fn sweep_idempotent_on_replay() {
        // Two ticks on the same enqueued job: the second is a no-op
        // (every row is already at its correct flag state).
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());
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

    // ── re-extraction after a schema change ─────────────────────────────

    /// Run the sweep directly so the test can read `SweepStats`, which the
    /// worker loop consumes and does not expose.
    fn sweep(fx: &Fixture, namespace: &str, version: u32) -> SweepStats {
        fx.worker
            .run_flag_sweep(
                &fx.ctx,
                &SchemaFlagSweepJob {
                    namespace: namespace.into(),
                    new_version: version,
                    enqueued_at_unix_nanos: NOW,
                },
            )
            .expect("sweep")
    }

    #[test]
    fn sweep_reports_the_memory_behind_a_newly_flagged_row() {
        // The whole point of the re-extraction path: knowing WHICH memories
        // to replay. A flag bit alone never told anyone that.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());
        write_statement(&fx.metadata, subject, "acme", "ghost");
        let v = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));

        let stats = sweep(&fx, "acme", v);
        assert_eq!(stats.rows_flagged, 1);
        assert_eq!(
            stats.stale_memories,
            vec![MemoryId::pack(1, SessionId::DEFAULT.into(), 0).to_be_bytes()],
            "the evidence memory must be reported so it can be re-extracted",
        );
    }

    #[test]
    fn an_already_flagged_row_is_not_reported_a_second_time() {
        // `stale_memories` tracks the flag TRANSITION, not the flagged set.
        // Reporting on every sweep would re-enqueue the same memories on
        // each upload and re-spend the LLM budget for no new information.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());
        write_statement(&fx.metadata, subject, "acme", "ghost");
        let v = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));

        assert_eq!(sweep(&fx, "acme", v).stale_memories.len(), 1);
        let second = sweep(&fx, "acme", v);
        assert_eq!(second.rows_flagged, 0);
        assert!(second.stale_memories.is_empty());
    }

    #[test]
    fn one_memory_behind_several_flagged_rows_is_reported_once() {
        // Re-extracting a memory re-derives every statement it produced, so
        // the same id three times would be three identical LLM calls.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());
        for name in ["ghost_a", "ghost_b", "ghost_c"] {
            write_statement(&fx.metadata, subject, "acme", name);
        }
        let v = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));

        let stats = sweep(&fx, "acme", v);
        assert_eq!(stats.rows_flagged, 3);
        assert_eq!(stats.stale_memories.len(), 1);
    }

    #[test]
    fn clearing_a_flag_reports_no_stale_memory() {
        // A predicate coming BACK into the schema needs no re-extraction —
        // the rows were already correct and were only mis-flagged.
        let fx = build_fixture();
        let subject = put_subject(&fx.metadata, SpaceId::default());
        write_statement(&fx.metadata, subject, "acme", "ghost");
        let v1 = upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]));
        assert_eq!(sweep(&fx, "acme", v1).rows_flagged, 1);

        let v2 = upload_schema(
            &fx.metadata,
            &schema_with_predicates("acme", &["prefers", "ghost"]),
        );
        let stats = sweep(&fx, "acme", v2);
        assert_eq!(stats.rows_cleared, 1);
        assert!(stats.stale_memories.is_empty());
    }

    #[test]
    fn re_extraction_is_off_unless_configured() {
        // The default must stay off: enabling it spends real money per
        // memory, and a default nobody chose is the wrong way to find that
        // out. If this flips, it should be a deliberate edit that fails
        // here first.
        let fx = build_fixture();
        assert!(!fx.worker.reextract_on_schema_change);
    }

    /// Put the memory row + text that `memory_text_for_reextraction`
    /// requires, for the id `write_statement` stamps into evidence.
    fn put_memory_with_text(metadata: &SharedMetadataDb, id: MemoryId, text: &str) {
        use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
        use brain_metadata::tables::text::TEXTS_TABLE;
        let wtxn = metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let row = MemoryMetadata::new_active(
                id,
                brain_core::NamespaceId::SYSTEM,
                SpaceId::default(),
                SessionId::DEFAULT,
                0,
                0,
                brain_core::MemoryKind::Episodic,
                [0x55; 16],
                0.5,
                text.len() as u32,
                NOW,
            );
            t.insert(&id.to_be_bytes(), &row).unwrap();
        }
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&id.to_be_bytes(), text.as_bytes()).unwrap();
        }
        wtxn.commit().unwrap();
    }

    /// A fixture whose writer has a real extractor channel, so the test can
    /// observe what re-extraction actually enqueues.
    fn fixture_with_extractor_channel(
        reextract: bool,
    ) -> (Fixture, flume::Receiver<brain_ops::ExtractorEnqueue>) {
        let tempdir = tempfile::tempdir().unwrap();
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).unwrap());
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();

        let (ex_tx, ex_rx) = flume::unbounded::<brain_ops::ExtractorEnqueue>();
        let mut writer = RealWriterHandle::new(metadata.clone(), hnsw_writer);
        writer.set_extractor_sender(ex_tx);

        let (tx, rx) = flume::unbounded::<SchemaFlagSweepJob>();
        let worker = SchemaMigrationWorker::new(rx)
            .with_metrics(Arc::new(SchemaMigrationMetrics::new()))
            .with_reextract_on_schema_change(reextract);

        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            Arc::new(writer) as Arc<dyn WriterHandle>,
        );
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        let ctx = WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        (
            Fixture {
                worker,
                ctx,
                metadata,
                tx,
                _tempdir: tempdir,
            },
            ex_rx,
        )
    }

    fn stage_one_stale_memory(fx: &Fixture) -> u32 {
        let subject = put_subject(&fx.metadata, SpaceId::default());
        write_statement(&fx.metadata, subject, "acme", "ghost");
        put_memory_with_text(
            &fx.metadata,
            MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
            "Priya prefers Playwright.",
        );
        upload_schema(&fx.metadata, &schema_with_predicates("acme", &["prefers"]))
    }

    #[test]
    fn enabling_reextract_enqueues_the_stale_memory() {
        let (fx, ex_rx) = fixture_with_extractor_channel(true);
        let v = stage_one_stale_memory(&fx);
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v,
                enqueued_at_unix_nanos: NOW,
            })
            .unwrap();
        assert_eq!(drive_once(&fx.worker, &fx.ctx), 1);

        let queued: Vec<_> = ex_rx.drain().collect();
        assert_eq!(queued.len(), 1, "the stale memory must be re-extracted");
        assert_eq!(queued[0].0, MemoryId::pack(1, SessionId::DEFAULT.into(), 0));
        assert_eq!(&*queued[0].1, "Priya prefers Playwright.");
    }

    #[test]
    fn the_default_sweeps_but_enqueues_nothing() {
        // Identical setup, flag off: the flag bit still moves (observability
        // is never withheld) and not one LLM call is scheduled.
        let (fx, ex_rx) = fixture_with_extractor_channel(false);
        let v = stage_one_stale_memory(&fx);
        fx.tx
            .send(SchemaFlagSweepJob {
                namespace: "acme".into(),
                new_version: v,
                enqueued_at_unix_nanos: NOW,
            })
            .unwrap();
        assert_eq!(drive_once(&fx.worker, &fx.ctx), 1);

        assert_eq!(fx.worker.metrics().snapshot().rows_flagged_total, 1);
        assert!(
            ex_rx.drain().next().is_none(),
            "re-extraction must not run unless it was asked for",
        );
    }
}
