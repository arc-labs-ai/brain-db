//! Transactions (sub-task 7.9).
//!
//! True buffer-and-apply transaction semantics per spec §09/08.
//! Operations carrying a `txn_id` push into a per-txn `TxnBuffer`
//! instead of mutating redb/HNSW; TXN_COMMIT applies the entire
//! buffer in a single redb write txn via `WriterHandle::submit_batch`;
//! TXN_ABORT drops the buffer.
//!
//! Out of scope for v1: WAL records (Phase 9 wires the shard's Wal),
//! cross-shard txns, substrate-level nested-txn detection. See the
//! plan in `.claude/plans/phase-07-task-09.md` for the full design.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_planner::{
    TxnBatch, TxnEncode, TxnForget as TxnBatchForget, TxnLink as TxnBatchLink,
    TxnUnlink as TxnBatchUnlink,
};
use brain_protocol::request::{ForgetMode, TxnAbortRequest, TxnBeginRequest, TxnCommitRequest};
use brain_protocol::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};
use parking_lot::Mutex;

use crate::context::OpsContext;
use crate::error::OpError;

pub type TxnId = [u8; 16];

const MIN_TIMEOUT_SECONDS: u32 = 1;
const MAX_TIMEOUT_SECONDS: u32 = 300;
const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

// ---------------------------------------------------------------------------
// State + entries.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Committed,
    Aborted,
    Expired,
}

#[derive(Debug, Clone, Copy)]
pub struct TxnFinalCommit {
    pub committed_at_unix_nanos: u64,
    pub operations_applied: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct TxnFinalAbort {
    pub operations_discarded: u32,
}

#[derive(Debug, Clone, Copy)]
pub enum TxnFinalResponse {
    Commit(TxnFinalCommit),
    Abort(TxnFinalAbort),
}

pub struct TxnEntry {
    pub state: TxnState,
    pub started_at_unix_nanos: u64,
    pub expires_at_unix_nanos: u64,
    pub timeout_seconds: u32,
    pub final_response: Option<TxnFinalResponse>,
    pub buffer: Option<TxnBuffer>,
}

// ---------------------------------------------------------------------------
// Buffer.
// ---------------------------------------------------------------------------

/// One buffered ENCODE within an active txn. Carries everything the
/// commit-time batch + the read-your-writes lens need.
#[derive(Debug, Clone)]
pub struct BufferedEncode {
    pub memory_id: MemoryId,
    pub metadata: MemoryMetadata,
    pub text: String,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub edges: Vec<BufferedEdgeSpec>,
    pub kind: brain_core::MemoryKind,
    pub context_id: brain_core::ContextId,
    pub salience_initial: f32,
    pub fingerprint: [u8; 16],
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedEdgeSpec {
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedLink {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedUnlink {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedForget {
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
}

/// Cached per-`request_id` response within a txn so we can replay
/// intra-txn duplicate submits.
#[derive(Debug, Clone)]
pub enum BufferedReplay {
    Encode {
        memory_id: MemoryId,
        edge_outcomes: Vec<brain_planner::EdgeOutcome>,
    },
    Forget {
        memory_id: MemoryId,
        outcome: brain_planner::ForgetOutcome,
    },
    Link {
        source: MemoryId,
        target: MemoryId,
        kind: EdgeKind,
        weight: f32,
        created_at_unix_nanos: u64,
        already_existed: bool,
    },
    Unlink {
        source: MemoryId,
        target: MemoryId,
        kind: EdgeKind,
        removed: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TxnBuffer {
    pub encodes: Vec<BufferedEncode>,
    pub forgets: Vec<BufferedForget>,
    pub links: Vec<BufferedLink>,
    pub unlinks: Vec<BufferedUnlink>,
    /// For RECALL/PLAN/REASON lensing.
    pub tombstoned: HashSet<MemoryId>,
    /// Edges the txn has unlinked (canonical triple).
    pub unlinked_edges: HashSet<(MemoryId, EdgeKind, MemoryId)>,
    /// For intra-txn replay.
    pub request_id_cache: HashMap<[u8; 16], BufferedReplay>,
    /// For request-hash conflict detection on intra-txn replay.
    pub request_hashes: HashMap<[u8; 16], [u8; 32]>,
}

impl TxnBuffer {
    #[must_use]
    pub fn ops_count(&self) -> u32 {
        let n = self.encodes.len() + self.forgets.len() + self.links.len() + self.unlinks.len();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

// ---------------------------------------------------------------------------
// TxnStore.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct TxnStore {
    entries: Mutex<HashMap<TxnId, TxnEntry>>,
}

impl TxnStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sweep expired entries (state Active but past their expiry).
    /// Called inline on every TXN_BEGIN; cheap (one pass over the map).
    fn sweep_expired_locked(entries: &mut HashMap<TxnId, TxnEntry>, now_unix_nanos: u64) {
        for entry in entries.values_mut() {
            if matches!(entry.state, TxnState::Active)
                && entry.expires_at_unix_nanos <= now_unix_nanos
            {
                entry.state = TxnState::Expired;
                entry.buffer = None;
            }
        }
    }

    /// Validate that `txn_id` exists and is `Active`. Touches the
    /// sweeper inline so a stale "Active" entry past its expiry is
    /// observed as `Expired`. Returns Ok with the entry's expiry on
    /// success.
    pub fn validate_active(&self, txn_id: TxnId) -> Result<u64, OpError> {
        let mut entries = self.entries.lock();
        let now = now_unix_nanos();
        Self::sweep_expired_locked(&mut entries, now);
        match entries.get(&txn_id) {
            None => Err(OpError::TxnExpired),
            Some(e) => match e.state {
                TxnState::Active => Ok(e.expires_at_unix_nanos),
                _ => Err(OpError::TxnExpired),
            },
        }
    }

    /// Apply `f` to the mutable buffer of an Active txn. Errors with
    /// `TxnExpired` if the txn isn't Active.
    pub fn with_buffer<R>(
        &self,
        txn_id: TxnId,
        f: impl FnOnce(&mut TxnBuffer) -> Result<R, OpError>,
    ) -> Result<R, OpError> {
        let mut entries = self.entries.lock();
        let now = now_unix_nanos();
        Self::sweep_expired_locked(&mut entries, now);
        match entries.get_mut(&txn_id) {
            None => Err(OpError::TxnExpired),
            Some(entry) => match (&entry.state, &mut entry.buffer) {
                (TxnState::Active, Some(buf)) => f(buf),
                _ => Err(OpError::TxnExpired),
            },
        }
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

pub async fn handle_txn_begin(
    req: TxnBeginRequest,
    ctx: &OpsContext,
) -> Result<TxnBeginResponse, OpError> {
    let timeout_seconds = clamp_timeout(req.timeout_seconds);
    let store = &*ctx.txn_store;
    let mut entries = store.entries.lock();
    let now = now_unix_nanos();
    TxnStore::sweep_expired_locked(&mut entries, now);

    // Replay support: same txn_id → return the cached begin response.
    if let Some(entry) = entries.get(&req.txn_id) {
        return Ok(TxnBeginResponse {
            txn_id: req.txn_id,
            timeout_seconds: entry.timeout_seconds,
            started_at_unix_nanos: entry.started_at_unix_nanos,
        });
    }

    let expires_at = now.saturating_add(u64::from(timeout_seconds) * 1_000_000_000);
    let entry = TxnEntry {
        state: TxnState::Active,
        started_at_unix_nanos: now,
        expires_at_unix_nanos: expires_at,
        timeout_seconds,
        final_response: None,
        buffer: Some(TxnBuffer::default()),
    };
    entries.insert(req.txn_id, entry);

    Ok(TxnBeginResponse {
        txn_id: req.txn_id,
        timeout_seconds,
        started_at_unix_nanos: now,
    })
}

pub async fn handle_txn_commit(
    req: TxnCommitRequest,
    ctx: &OpsContext,
) -> Result<TxnCommitResponse, OpError> {
    let store = &*ctx.txn_store;

    // Take the buffer + mark in-progress while we apply (under lock).
    let (buffer, started_at) = {
        let mut entries = store.entries.lock();
        let now = now_unix_nanos();
        TxnStore::sweep_expired_locked(&mut entries, now);
        let entry = entries.get_mut(&req.txn_id).ok_or(OpError::TxnExpired)?;
        // Replay support.
        if let Some(TxnFinalResponse::Commit(c)) = entry.final_response {
            return Ok(TxnCommitResponse {
                txn_id: req.txn_id,
                committed_at_unix_nanos: c.committed_at_unix_nanos,
                operations_applied: c.operations_applied,
            });
        }
        if !matches!(entry.state, TxnState::Active) {
            return Err(OpError::TxnExpired);
        }
        let buf = entry.buffer.take().ok_or(OpError::TxnExpired)?;
        let started_at = entry.started_at_unix_nanos;
        (buf, started_at)
    };

    let ops_applied = buffer.ops_count();
    let batch = build_batch(&buffer);

    // Apply atomically via the writer's submit_batch. On failure,
    // re-mark the txn aborted (the redb wtxn auto-rolled back).
    match ctx.executor.writer.submit_batch(batch).await {
        Ok(_acks) => {}
        Err(err) => {
            let mut entries = store.entries.lock();
            if let Some(entry) = entries.get_mut(&req.txn_id) {
                entry.state = TxnState::Aborted;
                entry.final_response = Some(TxnFinalResponse::Abort(TxnFinalAbort {
                    operations_discarded: ops_applied,
                }));
            }
            return Err(OpError::ExecError(brain_planner::ExecError::WriterFailed(
                err,
            )));
        }
    }

    let committed_at = now_unix_nanos();
    {
        let mut entries = store.entries.lock();
        if let Some(entry) = entries.get_mut(&req.txn_id) {
            entry.state = TxnState::Committed;
            entry.final_response = Some(TxnFinalResponse::Commit(TxnFinalCommit {
                committed_at_unix_nanos: committed_at,
                operations_applied: ops_applied,
            }));
        }
    }
    let _ = started_at; // reserved for future logging

    Ok(TxnCommitResponse {
        txn_id: req.txn_id,
        committed_at_unix_nanos: committed_at,
        operations_applied: ops_applied,
    })
}

pub async fn handle_txn_abort(
    req: TxnAbortRequest,
    ctx: &OpsContext,
) -> Result<TxnAbortResponse, OpError> {
    let store = &*ctx.txn_store;
    let mut entries = store.entries.lock();
    let now = now_unix_nanos();
    TxnStore::sweep_expired_locked(&mut entries, now);

    let entry = entries.get_mut(&req.txn_id).ok_or(OpError::TxnExpired)?;
    if let Some(TxnFinalResponse::Abort(a)) = entry.final_response {
        return Ok(TxnAbortResponse {
            txn_id: req.txn_id,
            operations_discarded: a.operations_discarded,
        });
    }
    if !matches!(entry.state, TxnState::Active) {
        return Err(OpError::TxnExpired);
    }

    let ops = entry.buffer.as_ref().map(TxnBuffer::ops_count).unwrap_or(0);
    entry.state = TxnState::Aborted;
    entry.buffer = None;
    entry.final_response = Some(TxnFinalResponse::Abort(TxnFinalAbort {
        operations_discarded: ops,
    }));

    Ok(TxnAbortResponse {
        txn_id: req.txn_id,
        operations_discarded: ops,
    })
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn clamp_timeout(req: u32) -> u32 {
    let v = if req == 0 {
        DEFAULT_TIMEOUT_SECONDS
    } else {
        req
    };
    v.clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS)
}

/// Convert a `TxnBuffer` into the `TxnBatch` the writer consumes.
fn build_batch(buffer: &TxnBuffer) -> TxnBatch {
    let memories: Vec<TxnEncode> = buffer
        .encodes
        .iter()
        .map(|e| TxnEncode {
            memory_id: e.memory_id,
            request_id: brain_core::RequestId::from(e.request_id),
            request_hash: e.request_hash,
            context_id: e.context_id,
            kind: e.kind,
            text: e.text.clone(),
            vector: e.vector,
            salience_initial: e.salience_initial,
            fingerprint: e.fingerprint,
            edges: e
                .edges
                .iter()
                .map(|edge| brain_planner::EncodeOpEdge {
                    target: edge.target,
                    kind: edge.kind,
                    weight: edge.weight,
                })
                .collect(),
            created_at_unix_nanos: e.created_at_unix_nanos,
        })
        .collect();
    let links: Vec<TxnBatchLink> = buffer
        .links
        .iter()
        .map(|l| TxnBatchLink {
            request_id: brain_core::RequestId::from(l.request_id),
            request_hash: l.request_hash,
            source: l.source,
            target: l.target,
            kind: l.kind,
            weight: l.weight,
            created_at_unix_nanos: l.created_at_unix_nanos,
        })
        .collect();
    let unlinks: Vec<TxnBatchUnlink> = buffer
        .unlinks
        .iter()
        .map(|u| TxnBatchUnlink {
            request_id: brain_core::RequestId::from(u.request_id),
            request_hash: u.request_hash,
            source: u.source,
            target: u.target,
            kind: u.kind,
            created_at_unix_nanos: u.created_at_unix_nanos,
        })
        .collect();
    let forgets: Vec<TxnBatchForget> = buffer
        .forgets
        .iter()
        .map(|f| TxnBatchForget {
            request_id: brain_core::RequestId::from(f.request_id),
            request_hash: f.request_hash,
            memory_id: f.memory_id,
            mode: f.mode,
            created_at_unix_nanos: f.created_at_unix_nanos,
        })
        .collect();
    TxnBatch {
        memories,
        links,
        unlinks,
        forgets,
    }
}

// Re-export for the lens.
pub use self::{
    BufferedEncode as PendingMemory, BufferedForget as PendingForget, BufferedLink as PendingLink,
    BufferedUnlink as PendingUnlink,
};

#[must_use]
pub fn now_unix_nanos_pub() -> u64 {
    now_unix_nanos()
}
