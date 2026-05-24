//! StatementEmbedWorker — populates the per-shard Statement HNSW.
//!
//! ## Why this exists
//!
//! `StatementHnswIndex` ships with insert/search APIs and the
//! `SemanticRetriever` reads through it in `SemanticScope::Statement` /
//! `Both` modes, but until this worker landed there was no producer:
//! statements were committed to redb by the extractor pipeline and
//! nothing ever added their embeddings to the HNSW. As a result the
//! hybrid query path's statement-corpus semantic retriever returned
//! zero hits and hybrid recall over statements degenerated to BM25 +
//! graph only — the single biggest gap on the Recall@10 path.
//!
//! ## Flow
//!
//! 1. `statement_create` / `statement_supersede` insert a row into the
//!    `STATEMENT_EMBED_QUEUE_TABLE` redb table inside the same write
//!    txn that lands the statement (see
//!    [`brain_metadata::statement::crud::insert_new_statement`]). The
//!    queue is durable so a shard restart between the extractor commit
//!    and the worker drain doesn't lose embeddings.
//! 2. Every `interval` (default 1 s) the worker reads up to
//!    `max_per_tick` queue rows, loads each statement, builds a
//!    `subject + predicate + object` text, embeds the batch through
//!    the shared BGE dispatcher, inserts the vectors into
//!    [`StatementHnswIndex`], then opens a write txn and removes the
//!    drained queue rows.
//! 3. Tombstoned / superseded rows surfaced by the peek are skipped
//!    and removed from the queue; the worker never embeds a row that
//!    `SemanticRetriever` would post-filter away.
//!
//! ## Idempotency
//!
//! Re-running the worker on the same statement is safe by design:
//! [`StatementHnswIndex::insert`] errors on duplicate ids, so we
//! check `contains` before inserting; any row that's already in the
//! HNSW is dropped from the queue with no re-embed. A crash between
//! HNSW insert and queue delete just costs an idempotent re-embed on
//! restart.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use brain_core::{Statement, StatementObject, StatementValue, SubjectRef};
use brain_core::StatementId;
use brain_embed::Dispatcher;
use brain_index::statement_hnsw::StatementHnswIndex;
use brain_metadata::entity::ops::entity_get;
use brain_metadata::schema::predicate::predicate_get;
use brain_metadata::statement::{
    statement_embed_queue_peek, statement_embed_queue_remove_many, statement_get,
};
use brain_metadata::MetadataDb;
use brain_ops::StatementEmbedMetrics;
use parking_lot::{Mutex, RwLock};

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// Per-tick limits. Defaults match the master plan budget: 1 s tick
/// × 32-statement batch × 384-d BGE-small forward pass ≈ ~20 ms on
/// CPU, well under the worker's 5 s `max_runtime` ceiling.
#[derive(Clone, Copy, Debug)]
pub struct StatementEmbedKnobs {
    /// How many statements to forward through the embedder in one
    /// `embed_batch` call. Larger batches amortise BGE-small's matmul
    /// across more rows, smaller batches give the scheduler tighter
    /// preemption windows. 32 is the sweet spot on CPU.
    pub batch_size: usize,
    /// Hard cap on rows the worker pulls off the queue per cycle.
    /// Defends against unbounded queue growth flooding one tick: at
    /// 256 statements/cycle and a 1 s tick the worker drains
    /// ~256 statements/sec, which exceeds any LLM-bound extractor
    /// production rate.
    pub max_per_tick: usize,
}

pub const DEFAULT_BATCH_SIZE: usize = 32;
pub const DEFAULT_MAX_PER_TICK: usize = 256;

impl Default for StatementEmbedKnobs {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            max_per_tick: DEFAULT_MAX_PER_TICK,
        }
    }
}

pub struct StatementEmbedWorker {
    config: WorkerConfig,
    knobs: StatementEmbedKnobs,
    metadata: Arc<Mutex<MetadataDb>>,
    statement_hnsw: Arc<RwLock<StatementHnswIndex>>,
    embedder: Arc<dyn Dispatcher>,
    metrics: Option<Arc<StatementEmbedMetrics>>,
}

impl StatementEmbedWorker {
    /// Construct a worker. `metadata` + `statement_hnsw` + `embedder`
    /// are the per-shard handles already plumbed elsewhere for the
    /// `SemanticRetriever` and the extractor worker.
    #[must_use]
    pub fn new(
        metadata: Arc<Mutex<MetadataDb>>,
        statement_hnsw: Arc<RwLock<StatementHnswIndex>>,
        embedder: Arc<dyn Dispatcher>,
    ) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::StatementEmbed),
            knobs: StatementEmbedKnobs::default(),
            metadata,
            statement_hnsw,
            embedder,
            metrics: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_knobs(mut self, knobs: StatementEmbedKnobs) -> Self {
        self.knobs = knobs;
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<StatementEmbedMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Run a single tick. Public so integration tests can drive the
    /// worker without spinning up the full scheduler.
    pub async fn tick(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let started = Instant::now();
        if let Some(m) = &self.metrics {
            m.inc_cycles();
        }

        // 1. Snapshot the queue head.
        let pending: Vec<StatementId> = {
            let db = self.metadata.lock();
            let rtxn = db
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("read_txn: {e}")))?;
            statement_embed_queue_peek(&rtxn, self.knobs.max_per_tick)
                .map_err(|e| WorkerError::Internal(format!("peek queue: {e}")))?
                .into_iter()
                .map(|(id, _ts)| id)
                .collect()
        };
        if pending.is_empty() {
            if let Some(m) = &self.metrics {
                m.observe_batch_duration(started.elapsed().as_secs_f64());
            }
            return Ok(0);
        }

        // 2. Resolve each id to a `(StatementId, embed_text)` pair.
        //    Rows that are no longer current / tombstoned / pending-
        //    subject / un-decodable are collected separately for queue
        //    cleanup (so a doomed row doesn't linger in the queue
        //    forever).
        let mut to_embed: Vec<(StatementId, String)> = Vec::with_capacity(pending.len());
        let mut already_in_hnsw: Vec<StatementId> = Vec::new();
        let mut to_drop: Vec<StatementId> = Vec::new();
        {
            let db = self.metadata.lock();
            let rtxn = db
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("read_txn: {e}")))?;
            let hnsw = self.statement_hnsw.read();
            for id in &pending {
                if ctx.is_shutdown() {
                    break;
                }
                if hnsw.contains(*id) {
                    already_in_hnsw.push(*id);
                    continue;
                }
                let statement = match statement_get(&rtxn, *id) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        // Row vanished — either the test fixture is
                        // unusual, or a parallel write reaped it. Drop
                        // the queue entry rather than ticking forever.
                        to_drop.push(*id);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "brain_workers::statement_embed",
                            statement_id = ?id,
                            error = %e,
                            "statement_get failed; leaving queue row for next tick",
                        );
                        continue;
                    }
                };
                if !is_eligible_for_embedding(&statement) {
                    to_drop.push(*id);
                    continue;
                }
                match render_embed_text(&rtxn, &statement) {
                    Ok(text) => to_embed.push((*id, text)),
                    Err(reason) => {
                        tracing::debug!(
                            target: "brain_workers::statement_embed",
                            statement_id = ?id,
                            reason,
                            "statement not renderable for embedding; dropping from queue",
                        );
                        to_drop.push(*id);
                    }
                }
            }
        }

        // 3. Embed in chunks of `batch_size` and insert into HNSW.
        let mut embedded: Vec<StatementId> = Vec::with_capacity(to_embed.len());
        let chunk = self.knobs.batch_size.max(1);
        for slice in to_embed.chunks(chunk) {
            if ctx.is_shutdown() {
                break;
            }
            let texts: Vec<&str> = slice.iter().map(|(_, t)| t.as_str()).collect();
            let vectors = match self.embedder.embed_batch(&texts) {
                Ok(v) => v,
                Err(e) => {
                    if let Some(m) = &self.metrics {
                        m.inc_embed_errors();
                    }
                    tracing::warn!(
                        target: "brain_workers::statement_embed",
                        batch = slice.len(),
                        error = %e,
                        "embedder batch failed; leaving rows in queue for next tick",
                    );
                    continue;
                }
            };
            if vectors.len() != slice.len() {
                if let Some(m) = &self.metrics {
                    m.inc_embed_errors();
                }
                tracing::warn!(
                    target: "brain_workers::statement_embed",
                    expected = slice.len(),
                    actual = vectors.len(),
                    "embedder returned wrong vector count; skipping batch",
                );
                continue;
            }
            let mut hnsw = self.statement_hnsw.write();
            for ((id, _), vec) in slice.iter().zip(vectors) {
                if hnsw.contains(*id) {
                    embedded.push(*id);
                    continue;
                }
                match hnsw.insert(*id, &vec) {
                    Ok(()) => embedded.push(*id),
                    Err(e) => {
                        tracing::warn!(
                            target: "brain_workers::statement_embed",
                            statement_id = ?id,
                            error = %e,
                            "StatementHnswIndex::insert failed; leaving queue row for next tick",
                        );
                    }
                }
            }
        }

        // 4. Drop the queue entries: rows we just embedded + rows we
        //    found already in the HNSW + ineligible rows.
        let removable_total = embedded.len() + already_in_hnsw.len() + to_drop.len();
        if removable_total > 0 {
            let mut removable: Vec<StatementId> = Vec::with_capacity(removable_total);
            removable.extend_from_slice(&embedded);
            removable.extend_from_slice(&already_in_hnsw);
            removable.extend_from_slice(&to_drop);
            let mut db = self.metadata.lock();
            let wtxn = db
                .write_txn()
                .map_err(|e| WorkerError::Internal(format!("write_txn: {e}")))?;
            statement_embed_queue_remove_many(&wtxn, &removable)
                .map_err(|e| WorkerError::Internal(format!("queue remove: {e}")))?;
            wtxn.commit()
                .map_err(|e| WorkerError::Internal(format!("queue commit: {e}")))?;
        }

        if let Some(m) = &self.metrics {
            m.add_rows_embedded(embedded.len() as u64);
            m.add_rows_skipped((already_in_hnsw.len() + to_drop.len()) as u64);
            m.observe_batch_duration(started.elapsed().as_secs_f64());
        }

        Ok(embedded.len() + already_in_hnsw.len() + to_drop.len())
    }
}

impl Worker for StatementEmbedWorker {
    fn name(&self) -> &'static str {
        WorkerKind::StatementEmbed.name()
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::StatementEmbed
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
// Statement → embed text.
// ---------------------------------------------------------------------------

/// A statement is eligible for the Statement HNSW iff it is current
/// (`is_current` is derived from `tombstoned + superseded_by` —
/// matches the `SemanticRetriever` post-filter chain) and the subject
/// is a resolved entity (the embed text needs a real name). Anything
/// else is dropped from the queue.
fn is_eligible_for_embedding(s: &Statement) -> bool {
    if s.tombstoned {
        return false;
    }
    if s.superseded_by.is_some() {
        return false;
    }
    if !matches!(s.subject, SubjectRef::Entity(_)) {
        return false;
    }
    true
}

/// Render `(subject, predicate, object)` to the canonical embed text
/// per `crates/brain-index/src/statement_hnsw.rs` module docs.
///
/// `Err(reason)` indicates the statement is structurally fine but
/// cannot produce a useful embedding (subject entity missing,
/// predicate gone, etc.). Caller drops the queue row in that case.
fn render_embed_text(rtxn: &redb::ReadTransaction, s: &Statement) -> Result<String, &'static str> {
    let SubjectRef::Entity(subject_id) = s.subject else {
        return Err("subject is pending");
    };
    let subject_entity = entity_get(rtxn, subject_id)
        .map_err(|_| "subject lookup failed")?
        .ok_or("subject entity missing")?;

    let predicate = predicate_get(rtxn, s.predicate)
        .map_err(|_| "predicate lookup failed")?
        .ok_or("predicate missing")?;
    // The qname's namespace prefix carries no semantic signal for an
    // embedding model — "brain:" or "user_ns:" are bookkeeping. Use
    // only the predicate name (and the original qname if the row
    // landed on a wildcard sink).
    let predicate_text = s
        .original_predicate_qname
        .as_deref()
        .map(|qn| qn.rsplit(':').next().unwrap_or(qn))
        .unwrap_or(&predicate.name);

    let object_text = match &s.object {
        StatementObject::Entity(eid) => {
            let entity = entity_get(rtxn, *eid)
                .map_err(|_| "object entity lookup failed")?
                .ok_or("object entity missing")?;
            entity.canonical_name
        }
        StatementObject::Value(v) => render_value(v),
        StatementObject::Memory(m) => format!("memory:{}", m.raw()),
        StatementObject::Statement(s) => format!("statement:{}", uuid_hex(&s.to_bytes())),
    };

    Ok(format!(
        "{} {} {}",
        subject_entity.canonical_name, predicate_text, object_text
    ))
}

fn render_value(v: &StatementValue) -> String {
    match v {
        StatementValue::Text(s) => s.clone(),
        StatementValue::Integer(n) => n.to_string(),
        StatementValue::Float(f) => f.to_string(),
        StatementValue::Bool(b) => b.to_string(),
        StatementValue::UnixNanos(ns) => ns.to_string(),
        // Opaque bytes have no human-readable form; embed as a short
        // hash-shaped tag so the corpus carries *something*. A
        // Blob-valued statement is unusual; recall over them goes
        // through structured filters, not text similarity.
        StatementValue::Blob(b) => format!("blob:{}", b.len()),
    }
}

fn uuid_hex(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
#[allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send post-9.7 (audit §4)
mod tests {
    use super::*;
    use brain_core::{Entity, EntityType, EvidenceEntry, EvidenceRef};
    use brain_core::{ContextId, EntityId, ExtractorId, MemoryId, PredicateId, StatementKind};
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::statement_hnsw::StatementHnswParams;
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::entity::ops::entity_put;
    use brain_metadata::schema::predicate::predicate_intern_or_get;
    use brain_metadata::statement::{statement_create, statement_tombstone};
    use brain_metadata::tables::statement::STATEMENT_EMBED_QUEUE_TABLE;
    use brain_metadata::MetadataDb;
    use brain_ops::{OpsContext, RealWriterHandle};
    use brain_planner::{ExecutorContext, WriterHandle};
    use redb::ReadableTableMetadata;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Deterministic dispatcher that embeds based on a hash of the
    /// input text. Sparse one-hot-ish vectors so HNSW maps distinct
    /// texts to distinct neighbourhoods.
    struct HashDispatcher {
        embed_count: AtomicUsize,
    }

    impl HashDispatcher {
        fn new() -> Self {
            Self {
                embed_count: AtomicUsize::new(0),
            }
        }
    }

    impl Dispatcher for HashDispatcher {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            self.embed_count.fetch_add(1, Ordering::Relaxed);
            let mut v = [0.0_f32; VECTOR_DIM];
            let h = blake3::hash(text.as_bytes());
            let bytes = h.as_bytes();
            let idx = (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
                % VECTOR_DIM;
            v[idx] = 1.0;
            Ok(v)
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0xAB; 16]
        }
    }

    /// Dispatcher whose `embed_batch` always errors. Lets tests assert
    /// that a failed embed leaves queue rows for the next tick.
    struct FailingDispatcher;

    impl Dispatcher for FailingDispatcher {
        fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Err(EmbedError::WarmupFailed("synthetic failure".into()))
        }

        fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Err(EmbedError::WarmupFailed("synthetic failure".into()))
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0xCD; 16]
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        metadata: Arc<Mutex<MetadataDb>>,
        hnsw: Arc<RwLock<StatementHnswIndex>>,
        dispatcher: Arc<HashDispatcher>,
        worker_ctx: WorkerContext,
    }

    fn build_worker_ctx(
        metadata: Arc<Mutex<MetadataDb>>,
        dispatcher: Arc<dyn Dispatcher>,
    ) -> WorkerContext {
        // The StatementEmbedWorker only reads `ctx.is_shutdown()`; the
        // rest of its dependencies (metadata, HNSW, embedder) come
        // through the worker constructor. The OpsContext is built only
        // to satisfy WorkerContext's shape — mirrors the decay /
        // consolidation integration-test fixtures.
        let (shared, hnsw_writer) =
            SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw::new");
        let writer: Arc<dyn WriterHandle> =
            Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(dispatcher, shared, metadata, writer);
        let ops = Arc::new(OpsContext::new(executor));
        WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let metadata = MetadataDb::open(dir.path().join("test.redb")).expect("open metadata");
        let metadata = Arc::new(Mutex::new(metadata));
        let hnsw = Arc::new(RwLock::new(
            StatementHnswIndex::new(StatementHnswParams::default_v1()).unwrap(),
        ));
        let dispatcher = Arc::new(HashDispatcher::new());
        let worker_ctx = build_worker_ctx(metadata.clone(), dispatcher.clone());
        Fixture {
            _dir: dir,
            metadata,
            hnsw,
            dispatcher,
            worker_ctx,
        }
    }

    fn now() -> u64 {
        1_700_000_000_000_000_000
    }

    /// Seed a single Fact statement with a Person subject + object and
    /// an `is_stateful = false` predicate. Returns the StatementId.
    fn seed_statement(metadata: &Arc<Mutex<MetadataDb>>, n: u8) -> StatementId {
        let mut db = metadata.lock();
        let wtxn = db.write_txn().unwrap();

        let subj_id = EntityId::new();
        let obj_id = EntityId::new();
        entity_put(
            &wtxn,
            &Entity::new_active(
                subj_id,
                EntityType::PERSON_ID,
                format!("Subject{n}"),
                format!("subject{n}"),
                now(),
            ),
        )
        .unwrap();
        entity_put(
            &wtxn,
            &Entity::new_active(
                obj_id,
                EntityType::PERSON_ID,
                format!("Object{n}"),
                format!("object{n}"),
                now(),
            ),
        )
        .unwrap();

        // Open-vocabulary intern matches the path the LLM extractor
        // takes when a brain:fact wildcard predicate lands; the
        // resulting row is `is_stateful=false` so each seeded
        // statement is its own current row (no auto-supersede).
        let pred_id = predicate_intern_or_get(&wtxn, "test", &format!("p_{n}"), 0, now()).unwrap();

        let stmt_id = StatementId::new();
        let evidence = EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
            MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            0.9,
            now(),
            ExtractorId::from(0),
        )]);
        let s = Statement::new_root(
            stmt_id,
            StatementKind::Fact,
            SubjectRef::Entity(subj_id),
            pred_id,
            StatementObject::Entity(obj_id),
            0.9,
            evidence,
            ExtractorId::from(0),
            now(),
            1,
        );

        let id = statement_create(&wtxn, &s, now()).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn queue_len(metadata: &Arc<Mutex<MetadataDb>>) -> u64 {
        let db = metadata.lock();
        let rtxn = db.read_txn().unwrap();
        let t = match rtxn.open_table(STATEMENT_EMBED_QUEUE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return 0,
            Err(e) => panic!("open queue: {e}"),
        };
        t.len().unwrap()
    }

    // ---------- Pure-function tests (no MetadataDb wiring) ----------

    #[test]
    fn ineligible_filters_match_post_filter_chain() {
        // Build the simplest possible Statement value and flip each
        // disqualifying flag in turn to assert
        // `is_eligible_for_embedding`'s contract.
        let id = StatementId::new();
        let subj = EntityId::new();
        let mut s = Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subj),
            PredicateId::from(1),
            StatementObject::Entity(EntityId::new()),
            0.9,
            EvidenceRef::default(),
            ExtractorId::from(0),
            now(),
            1,
        );
        assert!(is_eligible_for_embedding(&s));
        s.tombstoned = true;
        assert!(!is_eligible_for_embedding(&s));
        s.tombstoned = false;
        s.superseded_by = Some(StatementId::new());
        assert!(!is_eligible_for_embedding(&s));
        s.superseded_by = None;
        s.subject = SubjectRef::Pending(brain_core::AuditId::new());
        assert!(!is_eligible_for_embedding(&s));
    }

    #[test]
    fn render_value_covers_every_variant() {
        assert_eq!(render_value(&StatementValue::Text("hi".into())), "hi");
        assert_eq!(render_value(&StatementValue::Integer(-42)), "-42");
        assert_eq!(render_value(&StatementValue::Float(3.5)), "3.5");
        assert_eq!(render_value(&StatementValue::Bool(true)), "true");
        assert_eq!(
            render_value(&StatementValue::UnixNanos(1_700_000_000_000_000_000)),
            "1700000000000000000"
        );
        assert_eq!(render_value(&StatementValue::Blob(vec![0u8; 5])), "blob:5");
    }

    // ---------- Integration tests against MetadataDb + HNSW ----------

    #[test]
    fn tick_embeds_pending_statements_and_marks_them() {
        let fx = fixture();
        let ids: Vec<StatementId> = (0..5).map(|i| seed_statement(&fx.metadata, i)).collect();
        assert_eq!(queue_len(&fx.metadata), 5);

        let worker =
            StatementEmbedWorker::new(fx.metadata.clone(), fx.hnsw.clone(), fx.dispatcher.clone());

        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 5);
        for id in &ids {
            assert!(
                fx.hnsw.read().contains(*id),
                "statement {id:?} missing from HNSW"
            );
        }
        assert_eq!(queue_len(&fx.metadata), 0, "queue drained");
    }

    #[test]
    fn tick_skips_tombstoned_statements() {
        let fx = fixture();
        let active = seed_statement(&fx.metadata, 0);
        let dead = seed_statement(&fx.metadata, 1);
        {
            let mut db = fx.metadata.lock();
            let wtxn = db.write_txn().unwrap();
            statement_tombstone(
                &wtxn,
                dead,
                brain_core::TombstoneReason::UserRequest,
                now(),
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        // statement_tombstone removed the queue row already; only the
        // active statement remains pending.
        assert_eq!(queue_len(&fx.metadata), 1);

        let worker =
            StatementEmbedWorker::new(fx.metadata.clone(), fx.hnsw.clone(), fx.dispatcher.clone());
        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 1);
        assert!(fx.hnsw.read().contains(active));
        assert!(!fx.hnsw.read().contains(dead));
    }

    #[test]
    fn tick_respects_max_per_tick() {
        let fx = fixture();
        for i in 0..20 {
            seed_statement(&fx.metadata, i);
        }
        assert_eq!(queue_len(&fx.metadata), 20);

        let worker =
            StatementEmbedWorker::new(fx.metadata.clone(), fx.hnsw.clone(), fx.dispatcher.clone())
                .with_knobs(StatementEmbedKnobs {
                    batch_size: 5,
                    max_per_tick: 7,
                });

        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 7, "max_per_tick honoured");
        assert_eq!(queue_len(&fx.metadata), 13);
        assert_eq!(fx.hnsw.read().len(), 7);
    }

    #[test]
    fn tick_idempotent_on_re_run() {
        let fx = fixture();
        seed_statement(&fx.metadata, 0);
        let worker =
            StatementEmbedWorker::new(fx.metadata.clone(), fx.hnsw.clone(), fx.dispatcher.clone());

        let first = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(first, 1);
        assert_eq!(fx.hnsw.read().len(), 1);

        // Re-run: queue drained, nothing to embed. Idempotent.
        let second = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(second, 0);
        assert_eq!(fx.hnsw.read().len(), 1, "no duplicate inserts");
    }

    #[test]
    fn embedder_failure_leaves_queue_for_next_tick() {
        let fx = fixture();
        seed_statement(&fx.metadata, 0);
        seed_statement(&fx.metadata, 1);
        assert_eq!(queue_len(&fx.metadata), 2);

        let metrics = Arc::new(StatementEmbedMetrics::new());
        let worker = StatementEmbedWorker::new(
            fx.metadata.clone(),
            fx.hnsw.clone(),
            Arc::new(FailingDispatcher),
        )
        .with_metrics(metrics.clone());
        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 0, "no rows embedded");
        assert_eq!(queue_len(&fx.metadata), 2, "queue preserved");
        let s = metrics.snapshot();
        assert!(
            s.embed_errors_total >= 1,
            "embed_errors_total = {}",
            s.embed_errors_total
        );
    }

    #[test]
    fn worker_kind_name() {
        let fx = fixture();
        let worker =
            StatementEmbedWorker::new(fx.metadata.clone(), fx.hnsw.clone(), fx.dispatcher.clone());
        assert_eq!(worker.name(), "statement_embed");
        assert_eq!(worker.kind(), WorkerKind::StatementEmbed);
    }
}
