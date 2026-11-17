//! Handing the classifier's forward pass to a thread that is not the
//! caller's.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::model::{ClassifiedSpan, ClassifierModel};
use crate::framework::extractor::ExtractorError;

/// One batched classification: one span vector per input text, in input
/// order.
pub type ClassifiedBatch = Result<Vec<Vec<ClassifiedSpan>>, ExtractorError>;

/// Boxed future carrying a batched classification.
///
/// `Send`, because [`crate::framework::extractor::Extractor::run_batch`]
/// returns a `Send` future and the classifier tier is driven through it.
pub type ClassifiedBatchFuture = Pin<Box<dyn Future<Output = ClassifiedBatch> + Send + 'static>>;

/// Runs [`ClassifierModel::predict_batch`] somewhere other than the calling
/// thread.
///
/// GLiNER inference is an uninterruptible CPU forward pass with no await
/// point in it: 3.82 s per micro-batch, 97% of one extraction cycle. The
/// tier is driven from a worker that shares its shard's reactor thread with
/// every request served by that shard, so running the pass on the calling
/// thread stalls all of them for its full duration — against a 1.80 ms
/// median, two writes in a ten-second window took 3.93 s and 3.86 s, one per
/// one-second tick. Moving the pass does not make it cheaper; what it buys
/// back is the shard's ability to answer while the pass runs.
///
/// Injected as a trait because this crate must not know which executor the
/// pass is handed to: a shard's implementation lives next to the equivalent
/// handoff for statement embedding, and a caller with no reactor to protect
/// (a test, a benchmark, an offline tool) supplies none and the pass runs
/// inline.
pub trait ClassifierOffload: Send + Sync {
    /// Classify each of `texts` against the shared `labels`, off the calling
    /// thread.
    ///
    /// Everything is owned: a thread pool outlives the call that fed it, so
    /// it cannot borrow the caller's buffers. A few kilobytes of copies
    /// against a multi-second matmul.
    fn predict_batch(
        &self,
        model: Arc<dyn ClassifierModel>,
        texts: Vec<String>,
        labels: Vec<String>,
    ) -> ClassifiedBatchFuture;
}
