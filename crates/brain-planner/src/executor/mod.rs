//! Executor side of the planner.
//!
//! Read-only and non-write paths: recall, plan, reason, path. Plus
//! the WriterHandle trait + WriterError + EdgeOutcome /
//! ForgetOutcome that wire handlers use for outcome classification.
//! Writes are owned by `brain_ops::RealWriterHandle::submit(Write)`.

pub mod analogical;
pub mod context;
pub mod error;
pub mod path;
pub mod reason;
pub mod recall;
pub mod result;
pub mod writer;

pub use context::{ExecutorContext, PendingMemorySnapshot, SharedMetadataDb, TxnSnapshot};
pub use error::ExecError;
pub use path::{execute_path, execute_path_stream};
pub use reason::{execute_reason, execute_reason_stream};
pub use recall::execute_recall;
pub use result::{
    EncodeResult, EvidenceItem, ForgetResult, InferenceKind, InferenceStep, InferenceStream,
    InferenceStreamTerminal, Path, PathFrame, PathResult, PathStream, PathStreamTerminal,
    PlanExecutionMetadata, PlanStatus, PlanTraceDirection, PlanTraceMeetingPoint, PlanTraceNode,
    ReasonResult, ReasonStatus, ReasonTrace, ReasonTraceBase, ReasonTraceCandidate,
    ReasonTraceCentroid, ReasonTraceEdgeCandidate, ReasonTraceScoreBreakdown, ReasonTraceTrim,
    ReasonTraceWalk, RecallHit, RecallResult,
};
pub use writer::{
    EdgeOutcome, EncodeOp, EncodeOpEdge, ForgetOp, ForgetOutcome, LinkOp, UnlinkOp, WriterError,
    WriterHandle,
};
