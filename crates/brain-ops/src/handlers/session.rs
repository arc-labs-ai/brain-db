//! SESSION_CREATE / SESSION_LIST / SESSION_DELETE handlers.
//!
//! Scoped to the caller's effective `(namespace, space)`. CREATE/DELETE ride
//! the single `submit(Write)` path; LIST is a read that returns one space's
//! sessions newest-first. The default session (`session_id = 0`) is
//! non-deletable.

use brain_core::SessionId;
use brain_protocol::envelope::request::{
    SessionCreateRequest, SessionDeleteRequest, SessionListRequest,
};
use brain_protocol::envelope::response::{
    SessionCreateResponse, SessionDeleteResponse, SessionListResponse, SessionView,
};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, Write, WriteId};

use super::registry_cascade::{tombstone_space_memories, writer_err};

pub async fn handle_session_create(
    req: SessionCreateRequest,
    ctx: &OpsContext,
) -> Result<SessionCreateResponse, OpError> {
    let ns = ctx.executor.caller_namespace;
    let space = ctx.executor.caller_space;
    let now = crate::clock::now_unix_nanos();

    let phase = Phase::SessionCreate {
        session_id: SessionId(req.session_id),
        title: req.title.clone(),
        created_at_unix_nanos: now,
    };
    let write = build_write(&req.request_id, ctx, phase, b"session_create");
    let writer = downcast_writer_pub(ctx)?;
    let ack = writer.submit(write).await.map_err(writer_err)?;
    let created = matches!(
        ack.single_phase(),
        PhaseAck::SessionCreated { created: true }
    );

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("session_create read: {e}")))?;
    let meta = brain_metadata::session_get(&rtxn, ns.raw(), space.into(), req.session_id)
        .map_err(|e| OpError::Internal(format!("session_get: {e}")))?;
    let (created_at, last_active, memory_count) = meta
        .map(|m| {
            (
                m.created_at_unix_nanos,
                m.last_active_unix_nanos,
                m.memory_count,
            )
        })
        .unwrap_or((now, now, 0));

    Ok(SessionCreateResponse {
        space_id: space.into(),
        session_id: req.session_id,
        created,
        created_at_unix_nanos: created_at,
        last_active_unix_nanos: last_active,
        memory_count,
    })
}

pub async fn handle_session_list(
    req: SessionListRequest,
    ctx: &OpsContext,
) -> Result<SessionListResponse, OpError> {
    let ns = ctx.executor.caller_namespace;
    let space = ctx.executor.caller_space;
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("session_list read: {e}")))?;
    let rows = brain_metadata::session_list(&rtxn, ns.raw(), space.into(), req.limit as usize)
        .map_err(|e| OpError::Internal(format!("session_list: {e}")))?;
    let sessions = rows
        .into_iter()
        .map(|e| SessionView {
            session_id: e.session_id,
            created_at_unix_nanos: e.meta.created_at_unix_nanos,
            last_active_unix_nanos: e.meta.last_active_unix_nanos,
            title: e.meta.title,
            memory_count: e.meta.memory_count,
        })
        .collect();
    Ok(SessionListResponse {
        space_id: space.into(),
        sessions,
    })
}

pub async fn handle_session_delete(
    req: SessionDeleteRequest,
    ctx: &OpsContext,
) -> Result<SessionDeleteResponse, OpError> {
    let space = ctx.executor.caller_space;
    let now = crate::clock::now_unix_nanos();

    // The default session is a structural invariant — memories encoded without
    // an explicit session land there — so it can never be deleted.
    if req.session_id == SessionId::DEFAULT.raw() {
        return Err(OpError::InvalidRequest(
            "the default session (session_id = 0) is non-deletable".into(),
        ));
    }

    // Cascade the session's memories (default soft/tombstone like FORGET),
    // then drop the registry row.
    let memories_forgotten = tombstone_space_memories(ctx, req.hard, Some(req.session_id)).await?;

    let phase = Phase::SessionDelete {
        session_id: SessionId(req.session_id),
        hard: req.hard,
        at_unix_nanos: now,
    };
    let write = build_write(&req.request_id, ctx, phase, b"session_delete");
    let writer = downcast_writer_pub(ctx)?;
    let ack = writer.submit(write).await.map_err(writer_err)?;
    let existed = matches!(
        ack.single_phase(),
        PhaseAck::SessionDeleted { existed: true }
    );

    Ok(SessionDeleteResponse {
        space_id: space.into(),
        session_id: req.session_id,
        existed,
        memories_forgotten,
    })
}

fn build_write(request_id: &[u8; 16], ctx: &OpsContext, phase: Phase, domain: &[u8]) -> Write {
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
