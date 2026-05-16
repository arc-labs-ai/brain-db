//! Statement text indexer worker (phase 22.4).
//!
//! Hooks the statement create / supersede / tombstone / retract
//! post-commit pipelines into `statements.tantivy/`. See
//! `spec/27_knowledge_workers/02_text_indexer_workers.md` §3.

use std::sync::Arc;
use std::time::Instant;

use brain_core::knowledge::{Statement, StatementObject, StatementValue, SubjectRef};
use brain_core::{StatementId, StatementKind};
use brain_index::{schema_payload_json, IndexHandle, LexicalScope};
use flume::{bounded, Receiver, Sender};
use tantivy::schema::Field;
use tantivy::{IndexWriter, TantivyDocument, TantivyError, Term};
use thiserror::Error;
use tokio::time::{timeout_at, Instant as TokioInstant};
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

    /// Enqueue `op` for the indexer. Awaits on backpressure per
    /// §27/02 §1.
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

struct StatementFields {
    statement_id: Field,
    subject_name: Field,
    predicate_name: Field,
    predicate_id: Field,
    object_text: Field,
    kind: Field,
    confidence_bucket: Field,
    extracted_at: Field,
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
) -> Result<(), IndexerError> {
    let writer = build_writer(&handle)?;
    let fields = StatementFields::resolve(&handle)?;
    glommio::spawn_local(async move {
        run_loop(writer, fields, rx, policy).await;
    })
    .detach();
    Ok(())
}

/// Test / non-Linux entry point. Returns the loop future; the
/// caller picks the runtime.
pub async fn run_statement_text_indexer(
    handle: IndexHandle,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
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
    run_loop(writer, fields, rx, policy).await;
}

fn build_writer(handle: &IndexHandle) -> Result<IndexWriter, IndexerError> {
    debug_assert!(matches!(handle.scope, LexicalScope::StatementText));
    Ok(handle.index.writer_with_num_threads(1, 50_000_000)?)
}

async fn run_loop(
    mut writer: IndexWriter,
    fields: StatementFields,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
) {
    let mut batch: usize = 0;
    let mut last_commit = Instant::now();

    loop {
        let deadline = last_commit + policy.interval;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let tokio_deadline = TokioInstant::now() + remaining;

        match timeout_at(tokio_deadline, rx.recv_async()).await {
            Ok(Ok(op)) => {
                if let Err(err) = apply_op(&mut writer, &fields, &op) {
                    warn!(
                        target: "brain_ops::text_indexer",
                        error = %err,
                        "statement text indexer write failed; skipping op",
                    );
                } else {
                    batch += 1;
                }
                if batch >= policy.n_writes {
                    if commit_with_retry(&mut writer).is_err() {
                        return;
                    }
                    batch = 0;
                    last_commit = Instant::now();
                }
            }
            Ok(Err(_disconnected)) => {
                if batch > 0 {
                    let _ = commit_with_retry(&mut writer);
                }
                return;
            }
            Err(_elapsed) => {
                if batch > 0 {
                    if commit_with_retry(&mut writer).is_err() {
                        return;
                    }
                    batch = 0;
                }
                last_commit = Instant::now();
            }
        }
    }
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

/// Compute the confidence-bucket field per §26/01 §2:
/// `(confidence.clamp(0,1) * 10).floor()` ∈ `[0, 9]`.
#[must_use]
pub fn confidence_bucket(confidence: f32) -> u64 {
    let clamped = confidence.clamp(0.0, 1.0);
    let bucket = (clamped * 10.0).floor() as u64;
    bucket.min(9)
}

fn commit_with_retry(writer: &mut IndexWriter) -> Result<(), ()> {
    match attempt_commit(writer) {
        Ok(()) => Ok(()),
        Err(first) => {
            warn!(
                target: "brain_ops::text_indexer",
                error = %first,
                "statement text indexer commit failed; retrying",
            );
            match attempt_commit(writer) {
                Ok(()) => Ok(()),
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
        SubjectRef::Pending(_) => return None,
    };
    let subject = brain_metadata::entity_ops::entity_get(&rtxn, subject_id).ok()??;

    let predicate =
        brain_metadata::predicate_ops::predicate_get(&rtxn, statement.predicate).ok()??;

    let object_text = object_text_for_index(&statement.object, &rtxn);

    Some(StatementTextOp::Upsert {
        id: statement.id,
        subject_canonical_name: subject.canonical_name,
        predicate_id: u64::from(predicate.id.raw()),
        predicate_name: predicate.name,
        object_text,
        kind: statement.kind,
        confidence: statement.confidence,
        extracted_at_unix_ms: statement.extracted_at_unix_nanos / 1_000_000,
    })
}

/// Project a `StatementObject` to the text representation indexed
/// in `statements.tantivy/`. Per §27/02 §3:
///
/// - Entity → that entity's `canonical_name`.
/// - Value(Text) → the literal string.
/// - Value(Integer / Float / Bool / UnixNanos) → stringified value.
/// - Value(Blob) → empty (not text-indexable).
/// - Memory / Statement → empty (deferred to post-v1; would
///   require an additional read per indexer event).
fn object_text_for_index(object: &StatementObject, rtxn: &redb::ReadTransaction) -> String {
    match object {
        StatementObject::Entity(id) => brain_metadata::entity_ops::entity_get(rtxn, *id)
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
