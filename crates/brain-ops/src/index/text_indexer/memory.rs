//! Memory text indexer worker.
//!
//! Hooks the ENCODE / FORGET post-commit pipelines into
//! `memory_text.tantivy/`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::{MemoryId, MemoryKind, SpaceId};
use brain_index::{schema_payload_json, IndexHandle, LexicalScope};
use flume::{bounded, Receiver, Sender};
use tantivy::index::SegmentId;
use tantivy::schema::Field;
use tantivy::{IndexWriter, TantivyDocument, TantivyError, Term};
use thiserror::Error;
use tracing::{error, warn};

use super::{CommitPolicy, DEFAULT_QUEUE_CAPACITY};

/// Per-shard event consumed by the memory text indexer.
#[derive(Debug, Clone)]
pub enum MemoryTextOp {
    Upsert {
        id: MemoryId,
        text: String,
        space: SpaceId,
        kind: MemoryKind,
        created_at_unix_ms: u64,
        /// Session/conversation scope tag indexed as a tantivy fast
        /// field so the read funnel can pre-filter on it.
        session: u64,
    },
    Forget {
        id: MemoryId,
        /// Hard forget: the caller demands the memory's plaintext be
        /// physically evicted from the on-disk segments, not merely
        /// tombstoned. Triggers an inline commit + force-merge of any
        /// segment carrying deletes so the term/stored text is gone,
        /// satisfying the hard-forget purge-immediacy invariant. Soft
        /// forget (`false`) only tombstones the doc, leaving it
        /// recoverable within the grace window.
        hard: bool,
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
    /// full — the explicit backpressure-on-overflow
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
    space_id: Field,
    kind: Field,
    created_at: Field,
    session: Field,
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
            space_id: get("space_id")?,
            kind: get("kind")?,
            created_at: get("created_at")?,
            session: get("session")?,
        })
    }
}

/// Spawn the drain loop using `glommio::spawn_local` and hand the
/// caller its [`glommio::Task`]. Server-side path.
///
/// **The task must be awaited at shard teardown.** It was previously
/// `.detach()`ed, which loses the final commit: a Glommio executor
/// polls its main future to completion and then drops whatever tasks
/// are still pending, so an op applied (or still queued) when the
/// shard's main loop returned never reached `commit()`. That is
/// observable — a hard FORGET purges the redb `TEXTS` row in the
/// tombstone's own write txn, but its lexical delete rode this queue,
/// so the memory's text stayed on disk and keyword-searchable after a
/// graceful shutdown. Returning the handle is what lets
/// `shard_main_loop` wait for the commit instead of racing it.
#[cfg(target_os = "linux")]
pub fn spawn_memory_text_indexer_local(
    handle: IndexHandle,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
    shutdown: Receiver<()>,
    control: Receiver<super::IndexerControl>,
) -> Result<glommio::Task<()>, IndexerError> {
    let writer = build_writer(&handle)?;
    let fields = MemoryFields::resolve(&handle)?;
    Ok(glommio::spawn_local(async move {
        run_loop(writer, fields, rx, policy, shutdown, control).await;
    }))
}

/// Build the writer + resolved fields and run the drain loop. The
/// caller spawns this on the current Glommio executor — production
/// goes through [`spawn_memory_text_indexer_local`] which detaches;
/// tests `glommio::spawn_local` it and `.await` the returned task.
#[cfg(target_os = "linux")]
pub async fn run_memory_text_indexer(
    handle: IndexHandle,
    rx: Receiver<MemoryTextOp>,
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
    let fields = match MemoryFields::resolve(&handle) {
        Ok(f) => f,
        Err(e) => {
            error!(target: "brain_ops::text_indexer", error = %e, "schema fields missing");
            return;
        }
    };
    run_loop(writer, fields, rx, policy, shutdown, control).await;
}

fn build_writer(handle: &IndexHandle) -> Result<IndexWriter, IndexerError> {
    debug_assert!(matches!(handle.scope, LexicalScope::MemoryText));
    // 50 MB heap, 1 writer thread. Tantivy enforces a minimum of
    // ~15 MB; 50 MB is comfortable for the 256-doc batch shape.
    Ok(handle.index.writer_with_num_threads(1, 50_000_000)?)
}

/// Outcome of the per-iteration wait inside `run_loop`. Decouples
/// the Glommio-aware wait helper (`wait_next`) from the body.
enum NextOp<T> {
    /// Received an op from the dispatcher.
    Op(T),
    /// Sender side dropped — shard tearing down.
    Disconnected,
    /// Commit-interval deadline elapsed without any op arriving.
    DeadlineHit,
    /// Shard teardown asked this loop to flush and exit.
    ///
    /// Distinct from [`Disconnected`](Self::Disconnected): the op
    /// channel's senders live inside `OpsContext` and the writer, both
    /// reachable through `Arc`s the shard cannot reliably drop before it
    /// needs the final commit. Waiting for the channel to close would
    /// mean waiting on refcount discipline; an explicit signal does not.
    Shutdown,
    /// The shard's live-rebuild dance sent a control message (quiesce /
    /// resume). Handled between ops so the writer lock is released and
    /// reacquired at a batch boundary.
    Control(super::IndexerControl),
}

/// Wait for the next op or the commit deadline. Glommio-only — both
/// production (`spawn_*_local`) and tests (`run_in_glommio`) run
/// under Glommio, so there is no Tokio fallback. Mixing Tokio
/// primitives onto a Glommio thread panics looking for a Tokio
/// reactor (see `crates/brain-ops/src/test_support.rs`).
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
    // Either a signal or a dropped sender means "shard is going away";
    // both must flush rather than exit silently.
    let stop = async {
        let _ = shutdown.recv_async().await;
        NextOp::Shutdown
    };
    let ctrl = async {
        match control.recv_async().await {
            Ok(msg) => NextOp::Control(msg),
            // Control channel closed: the rebuild plane is gone. Not a
            // teardown signal on its own — keep serving ops; fall through
            // to a benign deadline so the select never resolves here.
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
    fields: MemoryFields,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
    shutdown: Receiver<()>,
    control: Receiver<super::IndexerControl>,
) {
    let mut batch: usize = 0;
    let mut last_commit = Instant::now();

    loop {
        let deadline = last_commit + policy.interval;
        let remaining = deadline.saturating_duration_since(Instant::now());

        match wait_next(&rx, &shutdown, &control, remaining).await {
            NextOp::Op(op) => {
                let is_hard_forget = matches!(op, MemoryTextOp::Forget { hard: true, .. });
                if let Err(err) = apply_op(&mut writer, &fields, &op) {
                    warn!(
                        target: "brain_ops::text_indexer",
                        error = %err,
                        "memory text indexer write failed; skipping op",
                    );
                } else {
                    batch += 1;
                }
                if is_hard_forget {
                    // Hard forget: don't wait for the batch/interval — commit
                    // the delete and force-merge so the memory's plaintext is
                    // physically evicted from the on-disk segments now, not on
                    // some incidental future merge (invariant #6).
                    if purge_hard_forget(&mut writer).await.is_err() {
                        return;
                    }
                    batch = 0;
                    last_commit = Instant::now();
                } else if batch >= policy.n_writes {
                    if commit_with_retry(&mut writer).is_err() {
                        return;
                    }
                    batch = 0;
                    last_commit = Instant::now();
                }
            }
            NextOp::Disconnected | NextOp::Shutdown => {
                // Teardown. Anything still sitting in the queue was
                // accepted from a caller that already got its ack, so
                // drain it before the final commit rather than dropping
                // it — a FORGET's lexical delete is typically the last
                // op enqueued and would otherwise be the one lost.
                while let Ok(op) = rx.try_recv() {
                    if let Err(err) = apply_op(&mut writer, &fields, &op) {
                        warn!(
                            target: "brain_ops::text_indexer",
                            error = %err,
                            "memory text indexer write failed during drain; skipping op",
                        );
                    } else {
                        batch += 1;
                    }
                }
                if batch > 0 {
                    let _ = commit_with_retry(&mut writer);
                }
                return;
            }
            NextOp::DeadlineHit => {
                if batch > 0 {
                    if commit_with_retry(&mut writer).is_err() {
                        return;
                    }
                    batch = 0;
                }
                last_commit = Instant::now();
            }
            NextOp::Control(super::IndexerControl::Quiesce { ack }) => {
                // Release the live-dir writer lock so the shard's rebuild
                // dance can replace the on-disk index. The uncommitted batch
                // is discarded, not flushed: every op it held was applied
                // after its redb commit, so the authoritative rows are in
                // redb and the rebuild reconstructs them (and any op still
                // buffered in `rx` re-drains after Resume). Committing here
                // would only write into the directory about to be renamed
                // away.
                drop(writer);
                batch = 0;
                let _ = ack.send_async(()).await;
                // Park until Resume hands us a writer on the reopened index
                // (or teardown). `rx` keeps buffering ops meanwhile.
                match super::wait_while_paused(&control, &shutdown).await {
                    Some(w) => {
                        writer = w;
                        last_commit = Instant::now();
                    }
                    None => return,
                }
            }
            NextOp::Control(super::IndexerControl::Resume { ack, .. }) => {
                // Resume without a preceding Quiesce: nothing to do (the
                // writer is already live). Ack so the orchestrator does not
                // block on a protocol misstep.
                let _ = ack.send_async(()).await;
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
        MemoryTextOp::Upsert { id, .. } | MemoryTextOp::Forget { id, .. } => *id,
    };
    let id_bytes = memory_id_bytes(id);
    let term = Term::from_field_bytes(fields.memory_id, &id_bytes);
    writer.delete_term(term);

    if let MemoryTextOp::Upsert {
        text,
        space,
        kind,
        created_at_unix_ms,
        session,
        ..
    } = op
    {
        let mut doc = TantivyDocument::default();
        doc.add_bytes(fields.memory_id, &id_bytes);
        doc.add_text(fields.text, text);
        doc.add_bytes(fields.space_id, &space_bytes(*space));
        doc.add_u64(fields.kind, kind_to_u64(*kind));
        doc.add_u64(fields.created_at, *created_at_unix_ms);
        doc.add_u64(fields.session, *session);
        writer.add_document(doc)?;
    }
    Ok(())
}

fn memory_id_bytes(id: MemoryId) -> [u8; 16] {
    id.raw().to_be_bytes()
}

fn space_bytes(space: SpaceId) -> [u8; 16] {
    space.into()
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
/// Returns `Err(())` on the **second** failure, signalling that the caller
/// should terminate the drain loop. Text indexing is correctness, not
/// best-effort, so a dead indexer is shard-fatal. NOTE: a runtime supervisor
/// that observes this task's completion outside teardown and fail-stops the
/// shard is not yet wired (follow-up); today the dead loop is only noticed at
/// the next shutdown join or by a rebuild's control ack failing.
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

/// Physically evict deleted docs after a hard forget.
///
/// `delete_term` only tombstones the doc — its term postings and stored
/// text stay in the on-disk segment until an incidental merge, which may
/// never come (there is no scheduled force-merge elsewhere). Hard forget
/// promises immediate purge, so this:
///
/// 1. commits the pending delete (so it's reflected in segment metadata),
/// 2. force-merges every segment that now carries deletes — rewriting them
///    without the deleted docs' bytes,
/// 3. garbage-collects the superseded segment files off disk.
///
/// Best-effort past the commit: a merge/GC failure is logged, not fatal —
/// the delete itself is durable (the doc is gone from queries) and a later
/// merge still reclaims the bytes. Only a failed commit terminates the
/// drain loop (`Err(())`), matching [`commit_with_retry`].
#[cfg(target_os = "linux")]
async fn purge_hard_forget(writer: &mut IndexWriter) -> Result<(), ()> {
    commit_with_retry(writer)?;

    let with_deletes: Vec<SegmentId> = match writer.index().searchable_segment_metas() {
        Ok(metas) => metas
            .iter()
            .filter(|m| m.num_deleted_docs() > 0)
            .map(tantivy::index::SegmentMeta::id)
            .collect(),
        Err(err) => {
            warn!(
                target: "brain_ops::text_indexer",
                error = %err,
                "hard forget: could not read segment metas for purge; bytes evicted on next merge",
            );
            return Ok(());
        }
    };

    if with_deletes.is_empty() {
        // No segment retained the deleted doc (e.g. it was added and
        // deleted before ever being committed to a segment) — nothing to
        // compact.
        return Ok(());
    }

    if let Err(err) = writer.merge(&with_deletes).await {
        warn!(
            target: "brain_ops::text_indexer",
            error = %err,
            "hard forget: force-merge failed; bytes evicted on next merge",
        );
        return Ok(());
    }

    if let Err(err) = writer.garbage_collect_files().await {
        warn!(
            target: "brain_ops::text_indexer",
            error = %err,
            "hard forget: garbage collect failed; superseded segment files linger until next GC",
        );
    }

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
