//! Memory text indexer worker (phase 22.3).
//!
//! Hooks the ENCODE / FORGET post-commit pipelines into
//! `memory_text.tantivy/`. See
//! `spec/27_knowledge_workers/02_text_indexer_workers.md` §2.

use std::sync::Arc;
use std::time::Instant;

use brain_core::{AgentId, MemoryId, MemoryKind};
use brain_index::{schema_payload_json, IndexHandle, LexicalScope};
use flume::{bounded, Receiver, Sender};
use tantivy::schema::Field;
use tantivy::{IndexWriter, TantivyDocument, TantivyError, Term};
use thiserror::Error;
use tokio::time::{timeout_at, Instant as TokioInstant};
use tracing::{error, warn};

use super::{CommitPolicy, DEFAULT_QUEUE_CAPACITY};

/// Per-shard event consumed by the memory text indexer.
#[derive(Debug, Clone)]
pub enum MemoryTextOp {
    Upsert {
        id: MemoryId,
        text: String,
        agent: AgentId,
        kind: MemoryKind,
        created_at_unix_ms: u64,
    },
    Forget {
        id: MemoryId,
    },
}

/// Foreground-side handle for `OpsContext` to enqueue indexer
/// work post-commit. Cloneable; shares the inner `flume::Sender`.
#[derive(Clone)]
pub struct MemoryTextDispatcher {
    tx: Sender<MemoryTextOp>,
}

impl MemoryTextDispatcher {
    /// Construct a dispatcher + receiver pair. The caller owns
    /// the receiver and feeds it to [`spawn_memory_text_indexer_local`].
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, Receiver<MemoryTextOp>) {
        let (tx, rx) = bounded(capacity);
        (Self { tx }, rx)
    }

    /// Convenience for the default queue capacity.
    #[must_use]
    pub fn default_channel() -> (Self, Receiver<MemoryTextOp>) {
        Self::channel(DEFAULT_QUEUE_CAPACITY)
    }

    /// Enqueue `op` for the indexer. **Awaits** if the queue is
    /// full — the explicit §27/02 §1 backpressure-on-overflow
    /// discipline. Returns `Err` only if the indexer has shut
    /// down (drop of the receiver), which signals shard
    /// teardown; the caller logs + continues to drain whatever
    /// else is in flight.
    pub async fn dispatch(&self, op: MemoryTextOp) {
        if self.tx.send_async(op).await.is_err() {
            warn!(
                target: "brain_ops::text_indexer",
                "memory text indexer receiver dropped; event discarded (shard shutting down)",
            );
        }
    }
}

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("required field `{0}` missing from memory_text schema")]
    MissingField(&'static str),
    #[error("tantivy IndexWriter creation: {0}")]
    Writer(#[from] TantivyError),
}

/// Resolved schema fields, looked up once at worker construction
/// time so the hot path is allocation-free.
struct MemoryFields {
    memory_id: Field,
    text: Field,
    agent_id: Field,
    kind: Field,
    created_at: Field,
}

impl MemoryFields {
    fn resolve(handle: &IndexHandle) -> Result<Self, IndexerError> {
        let schema = handle.index.schema();
        let get = |name: &'static str| -> Result<Field, IndexerError> {
            schema
                .get_field(name)
                .map_err(|_| IndexerError::MissingField(name))
        };
        Ok(Self {
            memory_id: get("memory_id")?,
            text: get("text")?,
            agent_id: get("agent_id")?,
            kind: get("kind")?,
            created_at: get("created_at")?,
        })
    }
}

/// Spawn the drain loop using `glommio::spawn_local` and return
/// immediately. Server-side path.
///
/// In test contexts where Glommio isn't running, call
/// [`run_memory_text_indexer`] directly inside a `tokio::spawn`.
#[cfg(target_os = "linux")]
pub fn spawn_memory_text_indexer_local(
    handle: IndexHandle,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
) -> Result<(), IndexerError> {
    let writer = build_writer(&handle)?;
    let fields = MemoryFields::resolve(&handle)?;
    glommio::spawn_local(async move {
        run_loop(writer, fields, rx, policy).await;
    })
    .detach();
    Ok(())
}

/// Non-Linux + test path. Returns the future; the caller spawns
/// it on whatever runtime they have. The future never returns
/// `Result` — it logs commit failures and self-terminates on
/// receiver close.
pub async fn run_memory_text_indexer(
    handle: IndexHandle,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
) {
    let writer = match build_writer(&handle) {
        Ok(w) => w,
        Err(e) => {
            error!(target: "brain_ops::text_indexer", error = %e, "writer init failed");
            return;
        }
    };
    let fields = match MemoryFields::resolve(&handle) {
        Ok(f) => f,
        Err(e) => {
            error!(target: "brain_ops::text_indexer", error = %e, "schema fields missing");
            return;
        }
    };
    run_loop(writer, fields, rx, policy).await;
}

fn build_writer(handle: &IndexHandle) -> Result<IndexWriter, IndexerError> {
    debug_assert!(matches!(handle.scope, LexicalScope::MemoryText));
    // 50 MB heap, 1 writer thread. Tantivy enforces a minimum of
    // ~15 MB; 50 MB is comfortable for the 256-doc batch shape
    // §26/01 §3 specifies.
    Ok(handle.index.writer_with_num_threads(1, 50_000_000)?)
}

async fn run_loop(
    mut writer: IndexWriter,
    fields: MemoryFields,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
) {
    let mut batch: usize = 0;
    let mut last_commit = Instant::now();

    loop {
        // Wait up to `policy.interval - elapsed` for the next op.
        let deadline = last_commit + policy.interval;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let tokio_deadline = TokioInstant::now() + remaining;

        let next = timeout_at(tokio_deadline, rx.recv_async()).await;

        match next {
            Ok(Ok(op)) => {
                if let Err(err) = apply_op(&mut writer, &fields, &op) {
                    warn!(
                        target: "brain_ops::text_indexer",
                        error = %err,
                        "memory text indexer write failed; skipping op",
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
                // Sender side dropped — drain + final commit + exit.
                if batch > 0 {
                    let _ = commit_with_retry(&mut writer);
                }
                return;
            }
            Err(_elapsed) => {
                // T-deadline hit. Commit if we have buffered work.
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
    fields: &MemoryFields,
    op: &MemoryTextOp,
) -> Result<(), TantivyError> {
    let id = match op {
        MemoryTextOp::Upsert { id, .. } | MemoryTextOp::Forget { id } => *id,
    };
    let id_bytes = memory_id_bytes(id);
    let term = Term::from_field_bytes(fields.memory_id, &id_bytes);
    writer.delete_term(term);

    if let MemoryTextOp::Upsert {
        text,
        agent,
        kind,
        created_at_unix_ms,
        ..
    } = op
    {
        let mut doc = TantivyDocument::default();
        doc.add_bytes(fields.memory_id, &id_bytes);
        doc.add_text(fields.text, text);
        doc.add_bytes(fields.agent_id, &agent_bytes(*agent));
        doc.add_u64(fields.kind, kind_to_u64(*kind));
        doc.add_u64(fields.created_at, *created_at_unix_ms);
        writer.add_document(doc)?;
    }
    Ok(())
}

fn memory_id_bytes(id: MemoryId) -> [u8; 16] {
    id.raw().to_be_bytes()
}

fn agent_bytes(agent: AgentId) -> [u8; 16] {
    agent.into()
}

fn kind_to_u64(kind: MemoryKind) -> u64 {
    // Match the substrate's WAL / metadata encoding
    // (`brain-storage::wal::payload`, `brain-metadata::tables::memory`).
    match kind {
        MemoryKind::Episodic => 0,
        MemoryKind::Semantic => 1,
        MemoryKind::Consolidated => 2,
    }
}

/// Commit, retry once on failure, then escalate.
///
/// `PreparedCommit` is single-use; on retry we re-prepare. Adds /
/// deletes since the failed `commit()` remain in the
/// `IndexWriter` buffer per tantivy semantics.
///
/// Returns `Err(())` on the **second** failure, signalling that
/// the caller should terminate the drain loop. The shard
/// supervisor sees the drop of the dispatcher's receiver and
/// alerts (§27/02 §4 — text indexing failure is shard-fatal).
fn commit_with_retry(writer: &mut IndexWriter) -> Result<(), ()> {
    match attempt_commit(writer) {
        Ok(()) => Ok(()),
        Err(first) => {
            warn!(
                target: "brain_ops::text_indexer",
                error = %first,
                "memory text indexer commit failed; retrying",
            );
            match attempt_commit(writer) {
                Ok(()) => Ok(()),
                Err(second) => {
                    error!(
                        target: "brain_ops::text_indexer",
                        error = %second,
                        "memory text indexer commit failed twice; shard fatal",
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

/// Convenience: hold both the dispatcher and the receiver until
/// the caller spawns the drain task. Used by `brain-server`'s
/// shard spawn path.
pub struct MemoryTextIndexerHandles {
    pub dispatcher: Arc<MemoryTextDispatcher>,
    pub receiver: Receiver<MemoryTextOp>,
}

impl MemoryTextIndexerHandles {
    #[must_use]
    pub fn with_default_capacity() -> Self {
        let (dispatcher, receiver) = MemoryTextDispatcher::default_channel();
        Self {
            dispatcher: Arc::new(dispatcher),
            receiver,
        }
    }
}

#[cfg(test)]
mod tests;
