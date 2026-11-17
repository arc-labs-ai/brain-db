//! Where a shard runs the classifier tier's forward pass.
//!
//! A shard's workers are `glommio::spawn_local` tasks on the same reactor
//! thread that serves every request routed to that shard. The GLiNER forward
//! pass behind the classifier tier is 3.82 s of uninterruptible CPU work per
//! micro-batch — 97% of one extraction cycle — with no await point inside it,
//! so a pass run on that thread holds every request on the shard for its full
//! length: against a 1.80 ms median, two writes in a ten-second window took
//! 3.93 s and 3.86 s, one per one-second tick. It also overshoots the worker's
//! 5 s `max_runtime` ceiling, which can only be checked between batches.
//!
//! Glommio's blocking pool is where [`crate::statement_embed`] already sends
//! its embedder batch, for the same reason and with the same shape.
//!
//! ## Why the result returns over a channel
//!
//! The future `glommio::executor().spawn_blocking` hands back carries an `Rc`
//! into the reactor's blocking-thread queue, so it is not `Send`, while
//! `Extractor::run_batch` owes its caller a `Send` future. The handoff
//! therefore lives in its own local task and the `Send` future waits on a
//! one-slot channel for what that task produces.

use std::sync::Arc;

use brain_extractors::{ClassifiedBatchFuture, ClassifierModel, ClassifierOffload, ExtractorError};

/// Runs the classifier's batched forward pass on glommio's blocking pool,
/// leaving the shard's reactor free to answer requests while it runs.
///
/// Must be called from a task on a glommio executor — which is where the
/// extractor worker that drives the tier runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct GlommioClassifierOffload;

impl ClassifierOffload for GlommioClassifierOffload {
    fn predict_batch(
        &self,
        model: Arc<dyn ClassifierModel>,
        texts: Vec<String>,
        labels: Vec<String>,
    ) -> ClassifiedBatchFuture {
        let (tx, rx) = flume::bounded(1);
        glommio::spawn_local(async move {
            let classified = glommio::executor()
                .spawn_blocking(move || {
                    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let inputs: Vec<(&str, &[&str])> = texts
                        .iter()
                        .map(|text| (text.as_str(), label_refs.as_slice()))
                        .collect();
                    model.predict_batch(&inputs)
                })
                .await;
            let _ = tx.send_async(classified).await;
        })
        .detach();

        Box::pin(async move {
            rx.recv_async().await.unwrap_or_else(|_| {
                Err(ExtractorError::InferenceFailed {
                    reason: "classifier offload task ended without returning a result".into(),
                })
            })
        })
    }
}
