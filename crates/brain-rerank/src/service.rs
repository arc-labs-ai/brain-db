//! Off-core rerank service.
//!
//! The cross-encoder is a 278M-param transformer; a single
//! `score_pairs` forward over the top-N fused candidates is heavy
//! CPU work. Running it inline on the Glommio shard core would
//! freeze the whole shard for the duration of the forward — every
//! other request on that core would wait. Instead, the loaded
//! encoder is moved onto a dedicated OS thread at shard spawn; the
//! shard sends a scoring job over a channel and awaits the reply
//! with `recv_async`, which parks the *task* rather than the thread
//! and lets the core keep serving other work while the model runs.
//!
//! One service (one thread, one model) per shard — matching the
//! per-shard encoder ownership the spawn path already uses.

use std::thread;

use flume::{Receiver, Sender};

use crate::model::{CrossEncoder, RerankError};

/// A scoring request handed to the rerank thread. Carries a one-shot
/// reply channel the worker writes the result back on.
struct RerankJob {
    query: String,
    candidates: Vec<String>,
    reply: Sender<Result<Vec<f32>, RerankError>>,
}

/// Handle to the per-shard rerank worker thread. Cloneable and
/// `Send + Sync` (it holds only the job `Sender`), so it slots into
/// the `Arc<RerankService>` that rides on the ops context.
#[derive(Clone)]
pub struct RerankService {
    jobs: Sender<RerankJob>,
}

impl RerankService {
    /// Move `encoder` onto a dedicated OS thread and return a handle
    /// to it. The thread lives until every `RerankService` clone is
    /// dropped — at which point the job channel closes and the worker
    /// loop exits.
    #[must_use]
    pub fn spawn(encoder: CrossEncoder) -> Self {
        // Unbounded: recalls are naturally rate-limited by the
        // connection layer, and a scoring job is small. The model's
        // serial processing is the backpressure that matters.
        let (tx, rx): (Sender<RerankJob>, Receiver<RerankJob>) = flume::unbounded();
        thread::Builder::new()
            .name("brain-rerank".to_owned())
            .spawn(move || worker_loop(encoder, &rx))
            .expect("invariant: OS refused to spawn the rerank worker thread");
        Self { jobs: tx }
    }

    /// Score `(query, candidate)` pairs on the worker thread. Awaits
    /// the reply without blocking the calling executor — safe to call
    /// from a Glommio task. Returns [`RerankError::ServiceUnavailable`]
    /// when the worker thread is gone (panicked or shut down).
    pub async fn score_pairs(
        &self,
        query: &str,
        candidates: &[&str],
    ) -> Result<Vec<f32>, RerankError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let job = RerankJob {
            query: query.to_owned(),
            candidates: candidates.iter().map(|c| (*c).to_owned()).collect(),
            reply: reply_tx,
        };
        self.jobs
            .send_async(job)
            .await
            .map_err(|_| RerankError::ServiceUnavailable)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| RerankError::ServiceUnavailable)?
    }
}

/// Worker loop: own the encoder, drain jobs, score, reply. The
/// `rx.recv()` is a real OS-thread block — which is the point: this
/// thread exists precisely to absorb the heavy forward pass away from
/// the latency-critical shard core. Exits when the last handle drops.
fn worker_loop(encoder: CrossEncoder, rx: &Receiver<RerankJob>) {
    while let Ok(job) = rx.recv() {
        let refs: Vec<&str> = job.candidates.iter().map(String::as_str).collect();
        let result = encoder.score_pairs(&job.query, &refs);
        // The reply receiver may already be dropped if the recall task
        // was cancelled mid-flight; that's fine — discard the result.
        let _ = job.reply.send(result);
    }
}
