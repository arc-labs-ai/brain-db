//! SPACE_CREATE / SPACE_LIST / SPACE_DELETE handlers.
//!
//! Scoped to the caller's effective `(namespace, space)` — the dispatch layer
//! has already resolved `act_as` into `ctx.executor.caller_space` /
//! `caller_namespace`, so these handlers never trust client-supplied scope.
//! CREATE/DELETE ride the single `submit(Write)` path (WAL-before-ack, CRC,
//! idempotency-by-RequestId inherited); LIST is a read.

use brain_protocol::envelope::request::{SpaceCreateRequest, SpaceDeleteRequest, SpaceListRequest};
use brain_protocol::envelope::response::{
    SpaceCreateResponse, SpaceDeleteResponse, SpaceListResponse, SpaceView,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, Write, WriteId};

use super::registry_cascade::{tombstone_space_memories, writer_err};

pub async fn handle_space_create(
    req: SpaceCreateRequest,
    ctx: &OpsContext,
) -> Result<SpaceCreateResponse, OpError> {
    let ns = ctx.executor.caller_namespace;
    let space = ctx.executor.caller_space;
    let now = crate::clock::now_unix_nanos();

    let phase = Phase::SpaceCreate {
        created_at_unix_nanos: now,
        metadata: req.metadata.clone(),
    };
    let write = build_write(&req.request_id, ctx, phase, b"space_create");
    let writer = downcast_writer_pub(ctx)?;
    let ack = writer.submit(write).await.map_err(writer_err)?;
    let created = matches!(ack.single_phase(), PhaseAck::SpaceCreated { created: true });

    // Echo the persisted row.
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("space_create read: {e}")))?;
    let meta = brain_metadata::space_get(&rtxn, ns.raw(), space.into())
        .map_err(|e| OpError::Internal(format!("space_get: {e}")))?;
    let (created_at, last_active, memory_count, session_count) = meta
        .map(|m| {
            (
                m.created_at_unix_nanos,
                m.last_active_unix_nanos,
                m.memory_count,
                m.session_count,
            )
        })
        .unwrap_or((now, now, 0, 0));

    Ok(SpaceCreateResponse {
        space_id: space.into(),
        created,
        created_at_unix_nanos: created_at,
        last_active_unix_nanos: last_active,
        memory_count,
        session_count,
    })
}

pub async fn handle_space_list(
    req: SpaceListRequest,
    ctx: &OpsContext,
) -> Result<SpaceListResponse, OpError> {
    let ns = ctx.executor.caller_namespace;
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("space_list read: {e}")))?;
    let rows = brain_metadata::space_list(&rtxn, ns.raw(), req.limit as usize)
        .map_err(|e| OpError::Internal(format!("space_list: {e}")))?;
    let spaces = rows
        .into_iter()
        .map(|e| SpaceView {
            space_id: e.space_id,
            created_at_unix_nanos: e.meta.created_at_unix_nanos,
            last_active_unix_nanos: e.meta.last_active_unix_nanos,
            memory_count: e.meta.memory_count,
            session_count: e.meta.session_count,
        })
        .collect();
    Ok(SpaceListResponse {
        spaces,
        // TODO(tenancy): spaces are spread across shards by hash(space); v1
        // lists only this shard's spaces. A bounded cross-shard scatter-gather
        // (or a namespace-home-shard registry replica) makes this complete.
        cross_shard_complete: false,
    })
}

pub async fn handle_space_delete(
    req: SpaceDeleteRequest,
    ctx: &OpsContext,
) -> Result<SpaceDeleteResponse, OpError> {
    let space = ctx.executor.caller_space;
    let now = crate::clock::now_unix_nanos();

    // GDPR erasure: hard-tombstone every memory under (namespace, space),
    // reusing the FORGET cascade (which tears down the backing graph rows),
    // then drop the registry rows in the same trailing write.
    let memories_forgotten = tombstone_space_memories(ctx, /* hard */ true, None).await?;

    let phase = Phase::SpaceDelete { at_unix_nanos: now };
    let write = build_write(&req.request_id, ctx, phase, b"space_delete");
    let writer = downcast_writer_pub(ctx)?;
    let ack = writer.submit(write).await.map_err(writer_err)?;
    let existed = matches!(ack.single_phase(), PhaseAck::SpaceDeleted { existed: true });

    Ok(SpaceDeleteResponse {
        space_id: space.into(),
        existed,
        memories_forgotten,
    })
}

/// Build a single-phase registry write stamped with the caller's scope +
/// namespace + an idempotency hash over `(domain, request_id, space)`.
fn build_write(
    request_id: &[u8; 16],
    ctx: &OpsContext,
    phase: Phase,
    domain: &[u8],
) -> Write {
    let space = ctx.executor.caller_space;
    let write_id = WriteId::from_request(brain_core::RequestId::from(*request_id), space);
    let mut h = blake3::Hasher::new();
    h.update(domain);
    h.update(request_id);
    h.update(&<[u8; 16]>::from(space));
    let hash: [u8; 32] = *h.finalize().as_bytes();
    Write::single(write_id, space, phase)
        .with_namespace(ctx.executor.caller_namespace)
        .with_request_hash(hash)
}
