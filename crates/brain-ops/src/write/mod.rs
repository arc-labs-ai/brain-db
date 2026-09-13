//! Unified write model.
//!
//! Every mutation in Brain — encode, forget, link, entity create,
//! statement supersede, extractor outcome, slot reclaim — is a
//! [`Write`] composed of one or more [`Phase`]s. The writer applies
//! all phases of a write against one redb `WriteTransaction`, in one
//! WAL append envelope, with one event burst.
//!
//! Outside an explicit transaction every wire request is a one-phase
//! (or small-N-phase) write. `TXN_COMMIT` buffers phases across many
//! wire calls and submits them as one [`Write`].
//!
//! Inside this module the layers ("substrate" vs "typed-graph") that
//! used to scatter writes across three code paths do not exist. There
//! is a write, with phases, going through one writer queue.

pub mod id;
pub mod phase;
pub mod transaction;

pub use id::{AllocatedId, IdKind, WriteId};
pub use phase::{
    EvidenceRefPhase, Phase, PhaseAck, SupersedeReplacement, SupersedeReplacementId,
    SupersedeTarget, TombstoneTarget,
};
pub use transaction::{PendingStage, Write, WriteAck};
