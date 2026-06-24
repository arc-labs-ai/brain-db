//! [`Write`] and [`WriteAck`] — the universal carriers.
//!
//! A `Write` is the unit of intent. A `WriteAck` is the result of
//! applying it. Single-op wire requests build a `Write` with one phase.
//! `TXN_COMMIT` builds a `Write` with N phases accumulated across the
//! transaction. Workers (edge derivation, reclamation, lifecycle)
//! build their own `Write`s and submit them.
//!
//! The writer doesn't distinguish among these origins. One queue,
//! one apply path, one WAL envelope, one event burst.

use brain_core::{AgentId, MemoryId};
use brain_storage::wal::record::Lsn;

use super::id::WriteId;
use super::phase::{Phase, PhaseAck};

/// Per-memory background stage triggered by this write. The writer's
/// post-commit fan-out builds one of these for every successful
/// enqueue onto a worker channel. Clients use the list as the
/// checklist for `--wait`: they expect one `StageCompleted` event
/// per entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingStage {
    pub memory_id: MemoryId,
    pub stage_kind: brain_protocol::StageKind,
}

/// One submission to the writer. Owns its phases (no borrowing — the
/// writer's queue is a `flume::Sender<Write>` and the work crosses
/// task boundaries).
#[derive(Clone, Debug)]
pub struct Write {
    pub write_id: WriteId,
    /// Authenticated caller. Stamped onto audit rows and event
    /// envelopes; `AgentId::default()` for anonymous / test paths.
    pub agent_id: AgentId,
    /// When the handler (or worker) began building this write. Used
    /// by the writer for tracing + by audit rows that need a
    /// "submitted_at" timestamp distinct from "committed_at".
    pub started_at_unix_nanos: u64,
    /// One or more phases. Order matters — the apply loop honors it
    /// (e.g. an edge phase referencing an entity created by a prior
    /// phase in the same write).
    pub phases: Vec<Phase>,
    /// Caller-supplied hash over the original request payload. Lets
    /// the writer's idempotency cache distinguish a true replay
    /// (same WriteId, same hash → cached ack) from a conflict (same
    /// WriteId, different hash → `WriterError::Conflict`). `None`
    /// disables conflict detection — workers / internal writes that
    /// can't conflict by construction skip the hash. Wire ops set it.
    pub request_hash: Option<[u8; 32]>,
}

impl Write {
    /// Build a single-phase write. The most common shape — every
    /// non-TXN wire request after the migration produces one of these.
    #[must_use]
    pub fn single(write_id: WriteId, agent_id: AgentId, phase: Phase) -> Self {
        Self {
            write_id,
            agent_id,
            started_at_unix_nanos: 0,
            phases: vec![phase],
            request_hash: None,
        }
    }

    /// Build from a vec of phases. Used by the TXN_COMMIT path and by
    /// workers that derive multiple phases per drained trigger.
    #[must_use]
    pub fn from_phases(write_id: WriteId, agent_id: AgentId, phases: Vec<Phase>) -> Self {
        Self {
            write_id,
            agent_id,
            started_at_unix_nanos: 0,
            phases,
            request_hash: None,
        }
    }

    /// Stamp the started-at timestamp; chainable from the builder.
    #[must_use]
    pub fn started_at(mut self, ts_nanos: u64) -> Self {
        self.started_at_unix_nanos = ts_nanos;
        self
    }

    /// Stamp the request-hash; chainable from the builder. Wire ops
    /// set this so the writer can detect idempotency conflicts.
    #[must_use]
    pub fn with_request_hash(mut self, hash: [u8; 32]) -> Self {
        self.request_hash = Some(hash);
        self
    }

    /// Number of phases in this write.
    #[inline]
    #[must_use]
    pub fn phase_count(&self) -> usize {
        self.phases.len()
    }

    /// `true` if this is a one-phase write — the common case. The
    /// writer's WAL framing can elide begin/end markers for single-
    /// phase writes (one WAL record carries everything).
    #[inline]
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.phases.len() == 1
    }
}

/// Per-write ack, returned by the writer after `submit`. Carries the
/// LSN range the write occupied (one LSN per phase) and one
/// [`PhaseAck`] per phase, in submit order.
#[derive(Clone, Debug)]
pub struct WriteAck {
    pub write_id: WriteId,
    pub committed_at_unix_nanos: u64,
    pub lsn_first: Lsn,
    pub lsn_last: Lsn,
    pub phase_acks: Vec<PhaseAck>,
    /// Background stages this write triggered. One entry per
    /// successful enqueue onto a worker channel — clients waiting
    /// on the write's full completion count these down as
    /// `StageCompleted` events arrive. Empty when no background
    /// work was queued (no UpsertMemory phases, or workers not
    /// wired in this build).
    pub pending_stages: Vec<PendingStage>,
}

impl WriteAck {
    /// Convenience for the common single-phase case. Panics if the
    /// write had zero or more than one phase — callers using this
    /// helper are asserting via type that they built a single-phase
    /// write.
    ///
    /// # Panics
    /// If `phase_acks.len() != 1`.
    #[must_use]
    pub fn single_phase(&self) -> &PhaseAck {
        assert_eq!(
            self.phase_acks.len(),
            1,
            "invariant: caller expected exactly one phase ack, got {}",
            self.phase_acks.len()
        );
        &self.phase_acks[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::phase::Phase;
    use brain_core::{ContextId, MemoryId, MemoryKind, Salience};
    use brain_embed::VECTOR_DIM;

    fn sample_phase() -> Phase {
        Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: "test".into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 1_700_000_000_000,
            occurred_at_unix_nanos: None,
            arena_slot: 1,
            embedding_model_fp: [0; 16],
            content_hash: None,
            deduplicate: false,
        }
    }

    #[test]
    fn write_from_phases_preserves_order() {
        let phases = vec![sample_phase(), sample_phase(), sample_phase()];
        let w = Write::from_phases(WriteId::new(), AgentId::default(), phases.clone());
        assert_eq!(w.phase_count(), 3);
        assert!(!w.is_single());
        for (i, p) in w.phases.iter().enumerate() {
            assert_eq!(p, &phases[i]);
        }
    }

    #[test]
    fn write_started_at_chainable() {
        let w = Write::single(WriteId::new(), AgentId::default(), sample_phase()).started_at(42);
        assert_eq!(w.started_at_unix_nanos, 42);
    }
}
