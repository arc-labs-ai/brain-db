//! Executor for the `ENCODE` cognitive operation.
//!
//! Stages (spec §08/04 §3):
//!
//! 1. Embed the cue text (the cache lookup happens inside the
//!    `CachingDispatcher` if one wraps the embedder).
//! 2. Resolve the context — wire shape only carries `WireContextId`
//!    in v1, so this is always `Explicit` and a no-op here.
//! 3. Build an `EncodeOp` and hand it to the writer. Spec §08/04 §4's
//!    idempotency check happens inside the writer (it owns both
//!    directions of the idempotency table).
//! 4. Translate the writer's ack to `EncodeResult`.
//!
//! The executor does NOT touch the WAL / arena / metadata writer
//! tables directly — that's the writer's job. CLAUDE.md §5 invariant
//! 1 ("WAL-before-acknowledge") is honoured because we only return
//! `Ok` after `submit_encode.await` resolves.

use crate::plan::{ContextResolutionStep, EncodePlan};

use super::context::ExecutorContext;
use super::error::ExecError;
use super::result::EncodeResult;
use super::writer::{EncodeOp, EncodeOpEdge};

pub async fn execute_encode(
    plan: EncodePlan,
    ctx: &ExecutorContext,
) -> Result<EncodeResult, ExecError> {
    // 1. Embed. Cache lookup is internal to the dispatcher (if it's
    //    a CachingDispatcher); otherwise it's a cold inference call.
    let vector = ctx.embedder.embed(&plan.embedding.text)?;

    // 2. Context resolution. v1 wire shape only carries WireContextId
    //    → always Explicit. The GetOrCreate branch is reserved for
    //    when the wire adds a named-context field.
    let context_id = match plan.context_resolution {
        ContextResolutionStep::Explicit(id) => id,
        ContextResolutionStep::GetOrCreate { .. } => {
            return Err(ExecError::Unsupported(
                "named-context resolution not yet implemented",
            ));
        }
    };

    // 3. Build the EncodeOp.
    let edges = plan
        .edges
        .iter()
        .map(|step| EncodeOpEdge {
            target: step.edge.target,
            kind: step.edge.kind,
            weight: step.edge.weight,
        })
        .collect();
    // Compute the content hash once. The writer only consults it
    // when `deduplicate` is set, but computing here keeps the writer
    // side branch-free and pure-redb (no hashing under the metadata
    // lock).
    let content_hash = *blake3::hash(plan.embedding.text.as_bytes()).as_bytes();

    let op = EncodeOp {
        request_id: plan.idempotency_check.request_id,
        context_id,
        kind: plan.wal_append.kind,
        text: plan.embedding.text.clone(),
        vector,
        salience_initial: plan.wal_append.salience_initial,
        fingerprint: ctx.embedder.fingerprint(),
        edges,
        deduplicate: plan.deduplicate,
        content_hash,
        agent_id: ctx.caller_agent,
    };

    // 4. Submit. WriterError → ExecError::WriterFailed via #[from].
    let ack = ctx.writer.submit_encode(op).await?;

    Ok(EncodeResult {
        memory_id: ack.memory_id,
        edge_results: ack.edge_results,
        replayed: ack.replayed,
        was_deduplicated: ack.was_deduplicated,
        lsn: ack.lsn,
        created_at_unix_nanos: ack.created_at_unix_nanos,
        edges_out_count: ack.edges_out_count,
    })
}
