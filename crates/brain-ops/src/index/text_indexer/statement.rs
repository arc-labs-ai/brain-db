//! Statement text indexer worker.
//!
//! Hooks the statement create / supersede / tombstone / retract
//! post-commit pipelines into `statements.tantivy/`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::{Statement, StatementObject, StatementValue, SubjectRef};
use brain_core::{StatementId, StatementKind};
use brain_index::{schema_payload_json, IndexHandle, LexicalScope};
use flume::{bounded, Receiver, Sender};
use tantivy::schema::Field;
use tantivy::{IndexWriter, TantivyDocument, TantivyError, Term};
use thiserror::Error;
use tracing::{error, warn};

use super::{CommitPolicy, DEFAULT_QUEUE_CAPACITY};

/// Per-shard event consumed by the statement text indexer.
#[derive(Debug, Clone)]
pub enum StatementTextOp {
    Upsert {
        id: StatementId,
        subject_canonical_name: String,
        predicate_id: u64,
        predicate_name: String,
        object_text: String,
        kind: StatementKind,
        confidence: f32,
        extracted_at_unix_ms: u64,
        /// The statement row's space — the lexical tenancy wall.
        space_id: [u8; 16],
    },
    Delete {
        id: StatementId,
    },
}

/// Foreground-side handle for `OpsContext` to enqueue indexer
/// work post-commit.
#[derive(Clone)]
pub struct StatementTextDispatcher {
    tx: Sender<StatementTextOp>,
}

impl StatementTextDispatcher {
    /// Construct a dispatcher + receiver pair. The caller owns
    /// the receiver and feeds it to [`spawn_statement_text_indexer_local`].
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, Receiver<StatementTextOp>) {
        let (tx, rx) = bounded(capacity);
        (Self { tx }, rx)
    }

    #[must_use]
    pub fn default_channel() -> (Self, Receiver<StatementTextOp>) {
        Self::channel(DEFAULT_QUEUE_CAPACITY)
    }

    /// Enqueue `op` for the indexer. Awaits on backpressure.
    pub async fn dispatch(&self, op: StatementTextOp) {
        if self.tx.send_async(op).await.is_err() {
            warn!(
                target: "brain_ops::text_indexer",
                "statement text indexer receiver dropped; event discarded (shard shutting down)",
            );
        }
    }
}

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("required field `{0}` missing from statements schema")]
    MissingField(&'static str),
    #[error("tantivy IndexWriter creation: {0}")]
    Writer(#[from] TantivyError),
}

#[derive(Clone, Copy)]
struct StatementFields {
    statement_id: Field,
    subject_name: Field,
    predicate_name: Field,
    predicate_id: Field,
    object_text: Field,
    kind: Field,
    confidence_bucket: Field,
    extracted_at: Field,
    space_id: Field,
}

impl StatementFields {
    fn resolve(handle: &IndexHandle) -> Result<Self, IndexerError> {
        let schema = handle.index.schema();
        let get = |name: &'static str| -> Result<Field, IndexerError> {
            schema
                .get_field(name)
                .map_err(|_| IndexerError::MissingField(name))
        };
        Ok(Self {
            statement_id: get("statement_id")?,
            subject_name: get("subject_name")?,
            predicate_name: get("predicate_name")?,
            predicate_id: get("predicate_id")?,
            object_text: get("object_text")?,
            kind: get("kind")?,
            confidence_bucket: get("confidence_bucket")?,
            extracted_at: get("extracted_at")?,
            space_id: get("space_id")?,
        })
    }
}

/// Glommio-local spawn entry point used by the server's shard
/// spawn path (Linux only).
#[cfg(target_os = "linux")]
pub fn spawn_statement_text_indexer_local(
    handle: IndexHandle,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
    shutdown: Receiver<()>,
    control: Receiver<super::IndexerControl>,
) -> Result<glommio::Task<()>, IndexerError> {
    let writer = build_writer(&handle)?;
    let fields = StatementFields::resolve(&handle)?;
    let commit_gen = handle.commit_generation_counter();
    Ok(glommio::spawn_local(async move {
        run_loop(writer, fields, commit_gen, rx, policy, shutdown, control).await;
    }))
}

/// Build the writer + resolved fields and run the drain loop on
/// the current Glommio executor. See the matching docs on
/// [`super::memory::run_memory_text_indexer`].
#[cfg(target_os = "linux")]
pub async fn run_statement_text_indexer(
    handle: IndexHandle,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
    shutdown: Receiver<()>,
    control: Receiver<super::IndexerControl>,
) {
    let writer = match build_writer(&handle) {
        Ok(w) => w,
        Err(e) => {
            error!(target: "brain_ops::text_indexer", error = %e, "writer init failed");
            return;
        }
    };
    let fields = match StatementFields::resolve(&handle) {
        Ok(f) => f,
        Err(e) => {
            error!(target: "brain_ops::text_indexer", error = %e, "schema fields missing");
            return;
        }
    };
    let commit_gen = handle.commit_generation_counter();
    run_loop(writer, fields, commit_gen, rx, policy, shutdown, control).await;
}

fn build_writer(handle: &IndexHandle) -> Result<IndexWriter, IndexerError> {
    debug_assert!(matches!(handle.scope, LexicalScope::StatementText));
    Ok(handle.index.writer_with_num_threads(1, 50_000_000)?)
}

/// Outcome of the per-iteration wait inside `run_loop`. See the
/// matching docs in [`super::memory`].
enum NextOp<T> {
    Op(T),
    Disconnected,
    DeadlineHit,
    /// Shard teardown asked this loop to flush and exit. See the
    /// matching variant in [`super::memory`] for why a signal is used
    /// rather than waiting for the op channel to close.
    Shutdown,
    /// The shard's live-rebuild dance sent a control message. See the
    /// matching variant in [`super::memory`].
    Control(super::IndexerControl),
}

#[cfg(target_os = "linux")]
async fn wait_next<T: 'static>(
    rx: &Receiver<T>,
    shutdown: &Receiver<()>,
    control: &Receiver<super::IndexerControl>,
    remaining: Duration,
) -> NextOp<T> {
    use futures_lite::FutureExt;
    let recv = async {
        match rx.recv_async().await {
            Ok(op) => NextOp::Op(op),
            Err(_) => NextOp::Disconnected,
        }
    };
    let stop = async {
        let _ = shutdown.recv_async().await;
        NextOp::Shutdown
    };
    let ctrl = async {
        match control.recv_async().await {
            Ok(msg) => NextOp::Control(msg),
            // Control channel closed: keep serving ops; fall through to a
            // benign deadline so the select never resolves here.
            Err(_) => {
                glommio::timer::sleep(remaining).await;
                NextOp::DeadlineHit
            }
        }
    };
    let timer = async {
        glommio::timer::sleep(remaining).await;
        NextOp::DeadlineHit
    };
    recv.or(stop).or(ctrl).or(timer).await
}

#[cfg(target_os = "linux")]
async fn run_loop(
    mut writer: IndexWriter,
    fields: StatementFields,
    mut commit_gen: Arc<AtomicU64>,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
    shutdown: Receiver<()>,
    control: Receiver<super::IndexerControl>,
) {
    let mut pending: Vec<StatementTextOp> = Vec::new();
    let mut last_commit = Instant::now();

    loop {
        let deadline = last_commit + policy.interval;
        let remaining = deadline.saturating_duration_since(Instant::now());

        match wait_next(&rx, &shutdown, &control, remaining).await {
            NextOp::Op(op) => {
                pending.push(op);
                if pending.len() >= policy.n_writes {
                    let (w, ok) = flush_off_reactor(
                        writer,
                        fields,
                        std::mem::take(&mut pending),
                        &commit_gen,
                    )
                    .await;
                    writer = w;
                    if !ok {
                        return;
                    }
                    last_commit = Instant::now();
                }
            }
            NextOp::Disconnected | NextOp::Shutdown => {
                // Drain what is still queued before the final commit —
                // see the matching arm in [`super::memory`].
                while let Ok(op) = rx.try_recv() {
                    pending.push(op);
                }
                if !pending.is_empty() {
                    let _ = flush_off_reactor(writer, fields, pending, &commit_gen).await;
                }
                return;
            }
            NextOp::DeadlineHit => {
                if !pending.is_empty() {
                    let (w, ok) = flush_off_reactor(
                        writer,
                        fields,
                        std::mem::take(&mut pending),
                        &commit_gen,
                    )
                    .await;
                    writer = w;
                    if !ok {
                        return;
                    }
                }
                last_commit = Instant::now();
            }
            NextOp::Control(super::IndexerControl::Quiesce { ack }) => {
                // Release the live-dir writer lock so the shard's rebuild
                // dance can replace the on-disk index. See the matching arm
                // in [`super::memory`] for why the uncommitted batch is
                // discarded rather than flushed.
                drop(writer);
                pending.clear();
                let _ = ack.send_async(()).await;
                match super::wait_while_paused(&control, &shutdown).await {
                    Some((w, gen)) => {
                        writer = w;
                        // Adopt the reopened index's counter so post-resume
                        // commits bump the generation the swapped-in retriever
                        // now watches.
                        commit_gen = gen;
                        last_commit = Instant::now();
                    }
                    None => return,
                }
            }
            NextOp::Control(super::IndexerControl::Resume { ack, .. }) => {
                // Resume with no preceding Quiesce: writer already live. Ack
                // so the orchestrator does not block.
                let _ = ack.send_async(()).await;
            }
        }
    }
}

/// Apply `ops` and group-commit, entirely off the shard's reactor thread.
///
/// Every `IndexWriter` call is blocking: `add_document` hands the document to
/// tantivy's own indexing thread and `commit` waits on it, both parking on a
/// futex. Running them from inside the glommio executor blocks the shard's
/// single reactor thread, and the wait degrades catastrophically — a 256-doc
/// batch plus commit measured ~27 ms on an ordinary thread but ~72 s on the
/// reactor. While the shard was stuck the indexer stopped draining, its
/// bounded op channel filled, and the foreground STATEMENT_CREATE blocked on
/// the backpressure send until clients hit their request timeout.
///
/// So the loop buffers ops (cheap, async) and hands the whole batch to
/// glommio's blocking pool once per commit cycle — at most one hop per
/// `n_writes` ops or per commit interval, which the ~27 ms real cost makes
/// negligible. Returns the writer so the caller keeps ownership across the
/// hop, and `false` when the commit failed twice (shard-fatal).
#[cfg(target_os = "linux")]
async fn flush_off_reactor(
    writer: IndexWriter,
    fields: StatementFields,
    ops: Vec<StatementTextOp>,
    commit_gen: &Arc<AtomicU64>,
) -> (IndexWriter, bool) {
    let gen = Arc::clone(commit_gen);
    glommio::executor()
        .spawn_blocking(move || {
            let mut writer = writer;
            for op in &ops {
                if let Err(err) = apply_op(&mut writer, &fields, op) {
                    warn!(
                        target: "brain_ops::text_indexer",
                        error = %err,
                        "statement text indexer write failed; skipping op",
                    );
                }
            }
            let ok = commit_with_retry(&mut writer, &gen).is_ok();
            (writer, ok)
        })
        .await
}

fn apply_op(
    writer: &mut IndexWriter,
    fields: &StatementFields,
    op: &StatementTextOp,
) -> Result<(), TantivyError> {
    let id = match op {
        StatementTextOp::Upsert { id, .. } | StatementTextOp::Delete { id } => *id,
    };
    let id_bytes = statement_id_bytes(id);
    let term = Term::from_field_bytes(fields.statement_id, &id_bytes);
    writer.delete_term(term);

    if let StatementTextOp::Upsert {
        subject_canonical_name,
        predicate_id,
        predicate_name,
        object_text,
        kind,
        confidence,
        extracted_at_unix_ms,
        space_id,
        ..
    } = op
    {
        let mut doc = TantivyDocument::default();
        doc.add_bytes(fields.statement_id, &id_bytes);
        doc.add_text(fields.subject_name, subject_canonical_name);
        doc.add_text(fields.predicate_name, predicate_name);
        doc.add_u64(fields.predicate_id, *predicate_id);
        doc.add_text(fields.object_text, object_text);
        doc.add_u64(fields.kind, kind_to_u64(*kind));
        doc.add_u64(fields.confidence_bucket, confidence_bucket(*confidence));
        doc.add_u64(fields.extracted_at, *extracted_at_unix_ms);
        doc.add_bytes(fields.space_id, space_id);
        writer.add_document(doc)?;
    }
    Ok(())
}

fn statement_id_bytes(id: StatementId) -> [u8; 16] {
    id.to_bytes()
}

fn kind_to_u64(kind: StatementKind) -> u64 {
    // Match the on-the-wire u8 encoding used by `statement_kind_from_wire`.
    kind.as_u8() as u64
}

/// Confidence-bucket field for the tantivy StatementText index.
///
/// Delegates to the canonical
/// [`brain_metadata::tables::statement::confidence_bucket`] (0..=10) so
/// the tantivy index and the redb `statements_by_predicate` index bucket
/// identically. Previously this used `.min(9)`, so a `confidence = 1.0`
/// row landed in bucket 10 in redb but bucket 9 here — a silent
/// cross-index disagreement at the boundary.
#[must_use]
pub fn confidence_bucket(confidence: f32) -> u64 {
    u64::from(brain_metadata::tables::statement::confidence_bucket(
        confidence,
    ))
}

fn commit_with_retry(writer: &mut IndexWriter, commit_gen: &AtomicU64) -> Result<(), ()> {
    match attempt_commit(writer) {
        Ok(()) => {
            commit_gen.fetch_add(1, Ordering::Release);
            Ok(())
        }
        Err(first) => {
            warn!(
                target: "brain_ops::text_indexer",
                error = %first,
                "statement text indexer commit failed; retrying",
            );
            match attempt_commit(writer) {
                Ok(()) => {
                    commit_gen.fetch_add(1, Ordering::Release);
                    Ok(())
                }
                Err(second) => {
                    error!(
                        target: "brain_ops::text_indexer",
                        error = %second,
                        "statement text indexer commit failed twice; shard fatal",
                    );
                    Err(())
                }
            }
        }
    }
}

fn attempt_commit(writer: &mut IndexWriter) -> Result<(), TantivyError> {
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&schema_payload_json());
    prepared.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// High-level dispatch helpers used by the statement handlers.
// ---------------------------------------------------------------------------

/// Compose a `StatementTextOp::Upsert` from a fresh `Statement` value
/// by joining against the metadata DB. Returns `None` when the
/// metadata required for indexing is missing (corrupt entity row,
/// pending subject, deleted predicate, etc.); the caller logs and
/// skips the dispatch.
pub fn upsert_op_from_statement(
    statement: &Statement,
    metadata: &brain_metadata::MetadataDb,
) -> Option<StatementTextOp> {
    let rtxn = metadata.read_txn().ok()?;

    // Subject must be a resolved entity (Pending subjects aren't
    // indexable — they have no canonical name yet).
    let subject_id = match statement.subject {
        SubjectRef::Entity(id) => id,
        // Memory + Pending subjects have no entity canonical name to
        // index — skip text indexing for them.
        SubjectRef::Memory(_) | SubjectRef::Pending(_) => return None,
    };
    let subject = brain_metadata::entity::ops::entity_get(&rtxn, subject_id).ok()??;

    let predicate =
        brain_metadata::schema::predicate::predicate_get(&rtxn, statement.predicate).ok()??;

    let object_text = object_text_for_index(&statement.object, &rtxn);

    // The core `Statement` carries no scope; its stored row does. A row that
    // is not there yet is not indexable — never index into a guessed space.
    let space_id = {
        use brain_metadata::tables::statement::STATEMENTS_TABLE;
        let table = rtxn.open_table(STATEMENTS_TABLE).ok()?;
        let row = table.get(&statement.id.to_bytes()).ok()??;
        row.value().space_id_bytes
    };

    Some(StatementTextOp::Upsert {
        id: statement.id,
        subject_canonical_name: subject.canonical_name,
        predicate_id: u64::from(predicate.id.raw()),
        predicate_name: predicate.name,
        object_text,
        kind: statement.kind,
        confidence: statement.confidence,
        extracted_at_unix_ms: statement.extracted_at_unix_nanos / 1_000_000,
        space_id,
    })
}

/// Read a freshly-committed statement back from `metadata` and enqueue
/// its text-index upsert. Shared by the wire `STATEMENT_CREATE` handler
/// and the extractor apply path so both keep `statements.tantivy/` in
/// sync — neither builds the op inline.
///
/// The statement is re-read (not passed in) because callers only hold
/// the `StatementId` post-commit, and dispatching must happen *after*
/// the write txn commits (a rolled-back txn must never index a phantom
/// row). A vanished statement or missing metadata is logged and skipped
/// — text indexing is best-effort and never blocks the durable write.
pub async fn dispatch_statement_text_upsert(
    metadata: &brain_metadata::MetadataDb,
    dispatcher: &StatementTextDispatcher,
    id: StatementId,
) {
    let upsert_op = {
        let rtxn = match metadata.read_txn() {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    target: "brain_ops::text_indexer",
                    error = %err,
                    "statement text indexer dispatch: read_txn failed",
                );
                return;
            }
        };
        let statement = match brain_metadata::statement::statement_get(&rtxn, id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    target: "brain_ops::text_indexer",
                    ?id,
                    "statement vanished between commit and indexer dispatch",
                );
                return;
            }
            Err(err) => {
                warn!(
                    target: "brain_ops::text_indexer",
                    error = %err,
                    "statement_get during text-indexer dispatch failed",
                );
                return;
            }
        };
        drop(rtxn);
        upsert_op_from_statement(&statement, metadata)
    };

    if let Some(op) = upsert_op {
        dispatcher.dispatch(op).await;
    } else {
        tracing::debug!(
            target: "brain_ops::text_indexer",
            ?id,
            "statement text indexer skip — Pending subject or missing metadata",
        );
    }
}

/// Project a `StatementObject` to the text representation indexed
/// in `statements.tantivy/`:
///
/// - Entity → that entity's `canonical_name`.
/// - Value(Text) → the literal string.
/// - Value(Integer / Float / Bool / UnixNanos) → stringified value.
/// - Value(Blob) → empty (not text-indexable).
/// - Memory / Statement → empty (deferred to post-v1; would
///   require an additional read per indexer event).
fn object_text_for_index(object: &StatementObject, rtxn: &redb::ReadTransaction) -> String {
    match object {
        StatementObject::Entity(id) => brain_metadata::entity::ops::entity_get(rtxn, *id)
            .ok()
            .flatten()
            .map(|e| e.canonical_name)
            .unwrap_or_default(),
        StatementObject::Value(StatementValue::Text(s)) => s.clone(),
        StatementObject::Value(StatementValue::Integer(n)) => n.to_string(),
        StatementObject::Value(StatementValue::Float(f)) => f.to_string(),
        StatementObject::Value(StatementValue::Bool(b)) => b.to_string(),
        StatementObject::Value(StatementValue::UnixNanos(n)) => n.to_string(),
        StatementObject::Value(StatementValue::Blob(_)) => String::new(),
        StatementObject::Memory(_) | StatementObject::Statement(_) => String::new(),
    }
}

/// Convenience bundle for the server-spawn site.
pub struct StatementTextIndexerHandles {
    pub dispatcher: Arc<StatementTextDispatcher>,
    pub receiver: Receiver<StatementTextOp>,
}

impl StatementTextIndexerHandles {
    #[must_use]
    pub fn with_default_capacity() -> Self {
        let (dispatcher, receiver) = StatementTextDispatcher::default_channel();
        Self {
            dispatcher: Arc::new(dispatcher),
            receiver,
        }
    }
}

#[cfg(test)]
mod tests;
