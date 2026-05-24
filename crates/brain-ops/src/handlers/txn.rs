//! Transactions (sub-task 7.9).
//!
//! True buffer-and-apply transaction semantics.
//! Operations carrying a `txn_id` push into a per-txn `TxnBuffer`
//! instead of mutating redb/HNSW. TXN_COMMIT translates the buffer
//! into a multi-phase `Write` and submits it through the unified
//! write path — every phase commits in one redb wtxn. TXN_ABORT
//! drops the buffer.
//!
//! Out of scope for v1: WAL records (Phase 9 wires the shard's Wal),
//! cross-shard txns, substrate-level nested-txn detection. See the
//! plan in `.claude/plans/phase-07-task-09.md` for the full design.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::memory::MemoryMetadata;
use brain_protocol::envelope::request::{ForgetMode, TxnAbortRequest, TxnBeginRequest, TxnCommitRequest};
use brain_protocol::envelope::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};
use parking_lot::Mutex;

use crate::context::OpsContext;
use crate::error::OpError;

pub type TxnId = [u8; 16];

const MIN_TIMEOUT_SECONDS: u32 = 1;
const MAX_TIMEOUT_SECONDS: u32 = 300;
const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

/// Maximum number of buffered operations (ENCODE + FORGET + LINK +
/// UNLINK) a single transaction may hold. Spec §05/04 §10 fixes the
/// limit at 1000; beyond it, TXN_COMMIT returns `TransactionTooLarge`
/// and the client splits into multiple transactions.
///
/// Fixed at a compile-time constant in v1.0: the value is a protocol
/// commitment, not an operational knob. If a future deployment needs a
/// different cap, it becomes a runtime setting at that point — keeping
/// it static now avoids accumulating a config knob no one tunes.
pub const MAX_TXN_OPS: u32 = 1000;

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
    /// Wire-level session that opened this txn. The connection layer
    /// fans out [`TxnStore::abort_orphaned_for_session`] when this
    /// session's TCP/TLS connection drops, so buffered work doesn't
    /// linger occupying RAM until the per-txn expiry sweep. All-zero
    /// means "no session" (in-process tests) — the sweep treats it as
    /// "never owned by any disconnect" and leaves the entry alone.
    pub session_id: [u8; 16],
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
    pub agent_id: brain_core::AgentId,
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
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedUnlink {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
    pub agent_id: brain_core::AgentId,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferedForget {
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
    pub agent_id: brain_core::AgentId,
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

    /// Reject a buffer mutation that would push past the per-transaction
    /// op cap. Called at the top of every buffer-mutating in-txn handler
    /// so the 1001st op fails fast — the agent learns about the cap
    /// immediately instead of buffering thousands of doomed ops only to
    /// be rejected at TXN_COMMIT. `handle_txn_commit` runs the same
    /// check on the taken buffer as defense-in-depth.
    pub fn check_capacity_for_push(&self) -> Result<(), OpError> {
        let current = self.ops_count();
        if current >= MAX_TXN_OPS {
            return Err(OpError::TransactionTooLarge {
                ops: current,
                cap: MAX_TXN_OPS,
            });
        }
        Ok(())
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
    /// observed as `Expired`. Bumps the txn's `expires_at` to
    /// `now + timeout_seconds` — every in-flight op resets the
    /// deadline, so an interactive REPL session doesn't expire
    /// while the user is typing. Returns Ok with the (new) expiry.
    pub fn validate_active(&self, txn_id: TxnId) -> Result<u64, OpError> {
        let mut entries = self.entries.lock();
        let now = now_unix_nanos();
        Self::sweep_expired_locked(&mut entries, now);
        match entries.get_mut(&txn_id) {
            None => Err(OpError::TxnNotFound),
            Some(e) => match e.state {
                TxnState::Active => {
                    e.expires_at_unix_nanos =
                        now.saturating_add(u64::from(e.timeout_seconds) * 1_000_000_000);
                    Ok(e.expires_at_unix_nanos)
                }
                _ => Err(OpError::TxnExpired),
            },
        }
    }

    /// Auto-abort every Active txn opened by the given wire session.
    /// Called by the connection layer when a TCP/TLS connection drops
    /// before the client COMMIT/ABORT — the spec (§05/04, §01/03 §8)
    /// is explicit that buffered work from a dropped connection must
    /// not take effect, and the per-txn timeout sweep is too lazy:
    /// the entries would otherwise occupy RAM for up to
    /// `timeout_seconds` after the socket closed.
    ///
    /// `session_id == [0u8; 16]` is treated as a no-op so in-process
    /// callers (tests, embedded harnesses) can't accidentally wipe
    /// their own txns by passing the default.
    ///
    /// Returns the list of txn ids that were aborted (for logging).
    pub fn abort_orphaned_for_session(&self, session_id: [u8; 16]) -> Vec<TxnId> {
        if session_id == [0u8; 16] {
            return Vec::new();
        }
        let mut entries = self.entries.lock();
        let mut aborted = Vec::new();
        for (txn_id, entry) in entries.iter_mut() {
            if entry.session_id != session_id {
                continue;
            }
            if !matches!(entry.state, TxnState::Active) {
                continue;
            }
            let ops = entry.buffer.as_ref().map(TxnBuffer::ops_count).unwrap_or(0);
            entry.state = TxnState::Aborted;
            entry.buffer = None;
            entry.final_response = Some(TxnFinalResponse::Abort(TxnFinalAbort {
                operations_discarded: ops,
            }));
            aborted.push(*txn_id);
        }
        aborted
    }

    /// Apply `f` to the mutable buffer of an Active txn. Errors with
    /// `TxnNotFound` if no such id was ever created, `TxnExpired`
    /// otherwise. Bumps `expires_at` on success — every buffer
    /// mutation counts as activity.
    pub fn with_buffer<R>(
        &self,
        txn_id: TxnId,
        f: impl FnOnce(&mut TxnBuffer) -> Result<R, OpError>,
    ) -> Result<R, OpError> {
        let mut entries = self.entries.lock();
        let now = now_unix_nanos();
        Self::sweep_expired_locked(&mut entries, now);
        match entries.get_mut(&txn_id) {
            None => Err(OpError::TxnNotFound),
            Some(entry) => match (&entry.state, &mut entry.buffer) {
                (TxnState::Active, Some(buf)) => {
                    let r = f(buf)?;
                    entry.expires_at_unix_nanos =
                        now.saturating_add(u64::from(entry.timeout_seconds) * 1_000_000_000);
                    Ok(r)
                }
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
    session_id: [u8; 16],
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
        session_id,
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
        let entry = entries.get_mut(&req.txn_id).ok_or(OpError::TxnNotFound)?;
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

    // Defense-in-depth: the append-time guard rejects any push that
    // would breach `MAX_TXN_OPS`, but a future code path that extends
    // the buffer through a different route would slip past it. Reject
    // an oversized buffer here too, before we touch the writer.
    if ops_applied > MAX_TXN_OPS {
        let mut entries = store.entries.lock();
        if let Some(entry) = entries.get_mut(&req.txn_id) {
            entry.state = TxnState::Aborted;
            entry.final_response = Some(TxnFinalResponse::Abort(TxnFinalAbort {
                operations_discarded: ops_applied,
            }));
        }
        return Err(OpError::TransactionTooLarge {
            ops: ops_applied,
            cap: MAX_TXN_OPS,
        });
    }

    // Build a single multi-phase Write from the buffer. The WAL
    // envelope (TxnBegin/Phase×N/TxnCommit) makes the whole commit
    // atomic; recovery's existing TXN state machine replays the
    // batch or discards it.
    let phases = build_phases(&buffer);
    // Deterministic WriteId from txn_id — a retried TXN_COMMIT under the
    // same txn_id resolves to the same idempotency cache key. The
    // request hash captures the phase plan so a buffer mutation
    // between the original commit and the retry surfaces as Conflict
    // instead of silently returning the cached ack.
    let write_id = write_id_from_txn(req.txn_id);
    let request_hash = hash_txn_commit_request(req.txn_id, &phases);
    let write = crate::write::Write::from_phases(write_id, ctx.executor.caller_agent, phases)
        .with_request_hash(request_hash);
    let real_writer = crate::handlers::link::downcast_writer_pub(ctx)?;
    match real_writer.submit(write).await {
        Ok(_ack) => {}
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

    let entry = entries.get_mut(&req.txn_id).ok_or(OpError::TxnNotFound)?;
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

/// Convert a `TxnBuffer` into a `Vec<Phase>` for the unified write
/// path. Phase ordering:
///
///   per encode: UpsertMemory + N × Link (encode-inline edges) for
///                Inserted outcomes only — TargetMissing edges drop
///                at buffer-time so they don't reach this point.
///   then       : standalone Link phases (TXN LINK ops)
///   then       : standalone Unlink phases (TXN UNLINK ops)
///   then       : Tombstone(Memory) phases (TXN FORGET ops)
///
/// The encode-first ordering matters: a TXN containing
/// ENCODE-A + LINK(A → B) needs A's row to land before the
/// LINK touches the edge table (the LINK doesn't read MEMORIES, but
/// the EDGES table key uses A's MemoryId — only valid if A was
/// minted).
pub(crate) fn build_phases(buffer: &TxnBuffer) -> Vec<crate::write::Phase> {
    use crate::write::phase::TombstoneMode as PhaseTombstoneMode;
    use crate::write::{Phase, TombstoneTarget};
    use brain_core::{EdgeKindRef, NodeRef, Salience};
    use brain_metadata::tables::edge::{derived_by, origin, zero_disambiguator};

    let mut phases: Vec<Phase> = Vec::with_capacity(
        buffer.encodes.len() * 2 + buffer.links.len() + buffer.unlinks.len() + buffer.forgets.len(),
    );

    for e in &buffer.encodes {
        phases.push(Phase::UpsertMemory {
            id: e.memory_id,
            text: e.text.clone(),
            vector: Box::new(e.vector),
            kind: e.kind,
            salience: Salience::new(e.salience_initial),
            context: e.context_id,
            created_at_unix_nanos: e.created_at_unix_nanos,
            arena_slot: e.memory_id.slot(),
            embedding_model_fp: e.fingerprint,
            content_hash: None,
            deduplicate: false,
        });
        for edge in &e.edges {
            phases.push(Phase::Link {
                from: NodeRef::Memory(e.memory_id),
                to: NodeRef::Memory(edge.target),
                kind: EdgeKindRef::Builtin(edge.kind),
                weight: edge.weight,
                origin: origin::EXPLICIT,
                derived_by: derived_by::CLIENT,
                disambiguator: zero_disambiguator(),
                created_at_unix_nanos: e.created_at_unix_nanos,
            });
        }
    }

    for l in &buffer.links {
        phases.push(Phase::Link {
            from: NodeRef::Memory(l.source),
            to: NodeRef::Memory(l.target),
            kind: EdgeKindRef::Builtin(l.kind),
            weight: l.weight,
            origin: origin::EXPLICIT,
            derived_by: derived_by::CLIENT,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: l.created_at_unix_nanos,
        });
    }

    for u in &buffer.unlinks {
        phases.push(Phase::Unlink {
            from: NodeRef::Memory(u.source),
            to: NodeRef::Memory(u.target),
            kind: EdgeKindRef::Builtin(u.kind),
            disambiguator: zero_disambiguator(),
        });
    }

    for f in &buffer.forgets {
        phases.push(Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id: f.memory_id,
                mode: match f.mode {
                    brain_protocol::envelope::request::ForgetMode::Soft => PhaseTombstoneMode::Soft,
                    brain_protocol::envelope::request::ForgetMode::Hard => PhaseTombstoneMode::Hard,
                },
            },
            reason: 1, // ClientRequest
            at_unix_nanos: f.created_at_unix_nanos,
        });
    }

    phases
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

/// Deterministic WriteId for a TXN_COMMIT. TxnCommitRequest has no
/// `request_id`, so we derive the WriteId from the `txn_id` itself —
/// a retried commit under the same txn_id maps to the same cache slot.
fn write_id_from_txn(txn_id: TxnId) -> crate::write::WriteId {
    crate::write::WriteId::from_bytes(txn_id)
}

/// BLAKE3 over the canonical TXN_COMMIT plan: txn_id plus the Debug
/// form of every phase. Debug is stable per project convention; the
/// fold lets the idempotency cache detect a retry against a mutated
/// buffer (different phases) and surface Conflict instead of silently
/// returning the cached ack.
fn hash_txn_commit_request(txn_id: TxnId, phases: &[crate::write::Phase]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"txn_commit:");
    h.update(&txn_id);
    h.update(b"\0");
    for phase in phases {
        h.update(phase.tag().as_bytes());
        h.update(b"\0");
        let dbg = format!("{phase:?}");
        h.update(dbg.as_bytes());
        h.update(b"\0");
    }
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stuff `count` no-op forgets into the buffer to drive
    /// `ops_count` to a known value. The contents don't matter — the
    /// cap check looks only at the running total.
    fn fill_buffer(buf: &mut TxnBuffer, count: usize) {
        for i in 0..count {
            buf.forgets.push(BufferedForget {
                memory_id: MemoryId::from(u128::from(i as u64) + 1),
                mode: ForgetMode::Soft,
                request_id: [0u8; 16],
                request_hash: [0u8; 32],
                created_at_unix_nanos: 0,
                agent_id: brain_core::AgentId(uuid::Uuid::nil()),
            });
        }
    }

    #[test]
    fn check_capacity_for_push_passes_below_cap() {
        let mut buf = TxnBuffer::default();
        fill_buffer(&mut buf, (MAX_TXN_OPS - 1) as usize);
        // Pushing the 1000th op is fine — the cap is exclusive of the
        // pending push, so a count of 999 still has room.
        buf.check_capacity_for_push()
            .expect("999/1000 buffer must accept one more push");
    }

    #[test]
    fn check_capacity_for_push_rejects_at_cap() {
        let mut buf = TxnBuffer::default();
        fill_buffer(&mut buf, MAX_TXN_OPS as usize);
        // Pushing the 1001st op fails — the buffer is already at
        // MAX_TXN_OPS and one more would breach the cap.
        let err = buf
            .check_capacity_for_push()
            .expect_err("at-cap buffer must reject the next push");
        match err {
            OpError::TransactionTooLarge { ops, cap } => {
                assert_eq!(ops, MAX_TXN_OPS);
                assert_eq!(cap, MAX_TXN_OPS);
            }
            other => panic!("expected TransactionTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn check_capacity_for_push_rejects_when_already_over_cap() {
        // Defense-in-depth case: if a future code path somehow pushed
        // past the cap, the check must still reject — never silently
        // accept an over-sized buffer.
        let mut buf = TxnBuffer::default();
        fill_buffer(&mut buf, (MAX_TXN_OPS as usize) + 5);
        let err = buf
            .check_capacity_for_push()
            .expect_err("over-cap buffer must reject");
        match err {
            OpError::TransactionTooLarge { ops, cap } => {
                assert_eq!(ops, MAX_TXN_OPS + 5);
                assert_eq!(cap, MAX_TXN_OPS);
            }
            other => panic!("expected TransactionTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn ops_count_sums_across_kinds() {
        // Sanity: the cap is enforced against the total — encodes +
        // forgets + links + unlinks all count equally. Future cap
        // refactors must preserve this property.
        let mut buf = TxnBuffer::default();
        fill_buffer(&mut buf, 3);
        // Forge three different op kinds via direct pushes (the
        // BufferedX shapes don't need plausible payloads for the
        // count check).
        buf.links.push(BufferedLink {
            source: MemoryId::from(1u128),
            target: MemoryId::from(2u128),
            kind: EdgeKind::Caused,
            weight: 1.0,
            request_id: [0u8; 16],
            request_hash: [0u8; 32],
            created_at_unix_nanos: 0,
            agent_id: brain_core::AgentId(uuid::Uuid::nil()),
        });
        buf.unlinks.push(BufferedUnlink {
            source: MemoryId::from(3u128),
            target: MemoryId::from(4u128),
            kind: EdgeKind::Caused,
            request_id: [0u8; 16],
            request_hash: [0u8; 32],
            created_at_unix_nanos: 0,
            agent_id: brain_core::AgentId(uuid::Uuid::nil()),
        });
        assert_eq!(buf.ops_count(), 5);
    }
}
