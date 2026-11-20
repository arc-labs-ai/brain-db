//! Off-reactor embedding helpers shared by async operation and worker paths,
//! and the per-shard window that collects waiting writes into one forward pass.
//!
//! The window is assembled here rather than behind the dispatcher: the model
//! surface honours a batch its caller already holds and never waits for one to
//! form, and judging which requests are worth waiting for needs the shard's own
//! view of what is in flight.
//!
//! Every helper hands the embed to Glommio's blocking pool, so every one needs a
//! `LocalExecutor` on the calling thread: `glommio::executor()` resolves
//! through a scoped thread-local and panics where none is set. Every
//! production caller arrives through a shard's request channel and therefore
//! runs inside that shard's executor. A test that drives them brings its own
//! executor up first — `test_support::run_in_glommio` for a `Send` fixture,
//! or `glommio::LocalExecutor::default().run(..)` to keep a `!Send` fixture
//! on the calling thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use tokio::sync::oneshot;

thread_local! {
    /// Candle chooses the matmul split count itself and offers no setter, while
    /// gemm hands the pieces to whichever Rayon pool is current. Both the split
    /// and this pool's width are read from `RAYON_NUM_THREADS`, which the
    /// server settles once at startup from the machine's cores and the number
    /// of shards dividing them. Naming no width here is what lets that one
    /// value reach both: a width set on the builder would narrow the pieces
    /// gemm runs while candle went on cutting the matmul into as many as the
    /// host has cores, and a split wider than the pool running it was the
    /// slowest of the four combinations measured — a p50 of 46.0 ms, against
    /// 20.7 ms when both came from one number, for one shard on ten cores.
    ///
    /// Spreading a single embed across cores is not where a loaded shard's
    /// throughput comes from — collecting the texts already waiting on it into
    /// one forward pass is.
    ///
    /// The pool is thread-local because `install()` runs the closure on the pool's
    /// workers and blocks the caller. A process-global one-worker pool would funnel
    /// every shard's embeds through one worker and serialize embedding across the
    /// process, while one pool per blocking thread keeps the shards independent.
    /// It is built once per thread and reused: `thread_local!` with a non-const
    /// initializer is lazy and runs once on each thread — which is also why the
    /// variable has to be set before a shard first embeds, since a pool fixes
    /// its width when it is built and never consults the variable again.
    static INTRA_OP_POOL: rayon::ThreadPool = rayon::ThreadPoolBuilder::new()
        .thread_name(|index| format!("brain-embed-intra-{index}"))
        .build()
        .expect("intra-op pool for an embedding blocking thread");
}

/// Report the width of the calling thread's intra-op pool. This is meaningful
/// only on a thread that embeds; calling it builds that thread's pool.
#[must_use]
pub fn intra_op_pool_threads() -> usize {
    INTRA_OP_POOL.with(rayon::ThreadPool::current_num_threads)
}

/// Run a passage embedding on Glommio's blocking pool. Only owned text and the
/// thread-safe dispatcher cross the boundary; operation contexts stay local to
/// the shard executor.
/// Awaited on a Glommio executor. The offload is the whole point of the call; a
/// caller with no reactor to protect wants `Dispatcher::embed` directly.
pub async fn embed_off_reactor(
    embedder: &Arc<dyn Dispatcher>,
    text: &str,
) -> Result<[f32; VECTOR_DIM], EmbedError> {
    let embedder = Arc::clone(embedder);
    let text = text.to_owned();
    glommio::executor()
        .spawn_blocking(move || INTRA_OP_POOL.with(|pool| pool.install(|| embedder.embed(&text))))
        .await
}

/// Run a retrieval-query embedding on Glommio's blocking pool. Query and
/// passage embeddings intentionally retain their asymmetric BGE prefixes.
/// Awaited on a Glommio executor. The offload is the whole point of the call; a
/// caller with no reactor to protect wants `Dispatcher::embed_query` directly.
pub async fn embed_query_off_reactor(
    embedder: &Arc<dyn Dispatcher>,
    text: &str,
) -> Result<[f32; VECTOR_DIM], EmbedError> {
    let embedder = Arc::clone(embedder);
    let text = text.to_owned();
    glommio::executor()
        .spawn_blocking(move || {
            INTRA_OP_POOL.with(|pool| pool.install(|| embedder.embed_query(&text)))
        })
        .await
}

/// Run a batch of passage embeddings as one forward pass on Glommio's blocking
/// pool. One pass over thirty-two texts costs about 20 ms where one text costs
/// about 15 ms, so the rows a caller can hand over together are close to free.
/// Only owned text and the thread-safe dispatcher cross the boundary.
/// Awaited on a Glommio executor.
pub async fn embed_batch_off_reactor(
    embedder: &Arc<dyn Dispatcher>,
    texts: Vec<String>,
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
    let embedder = Arc::clone(embedder);
    glommio::executor()
        .spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            INTRA_OP_POOL.with(|pool| pool.install(|| embedder.embed_batch(&refs)))
        })
        .await
}

/// Upper bound on the texts one request-path batch carries.
///
/// A shard admits a bounded number of requests at once — fifteen under the
/// default WAL group-commit budget — so the window cannot grow past that on a
/// default deployment. The cap matters on one configured wider: a batch pads
/// every row to the longest row's token count, so an unbounded window would
/// eventually pay more in padding than the shared matmul saves. Thirty-two is
/// the width the statement embed worker measured as CPU's sweet spot.
const MAX_BATCH_TEXTS: usize = 32;

/// How many of this shard's batches may be inside the model at once.
///
/// This is the knob that decides how wide the windows get, and it belongs with
/// the width of the shard's blocking pool. Refusing a request the right to open
/// its own pass is the only reason a queue ever forms, so a bound equal to the
/// pool's width keeps every pool thread fed while leaving every further request
/// to widen a batch instead of opening a pass that would only queue inside the
/// pool and amortise nothing. Glommio gives a shard one blocking thread pinned
/// to the shard's own core, so one pass at a time is what that pool can run;
/// raising the pool's width is what would justify raising this.
const MAX_BATCHES_IN_FLIGHT: usize = 1;

/// A request waiting on its vector.
struct PendingEmbed {
    text: String,
    /// The task that queued this text, parked until its own row comes back.
    respond: oneshot::Sender<Result<[f32; VECTOR_DIM], EmbedError>>,
    /// Fingerprint of the embedder this request arrived with. A shard owns one
    /// embedder, so every entry in one queue agrees; the collector asserts that
    /// rather than trusting it, because answering a request with a vector from
    /// a different model is invisible at the call site and silently poisons the
    /// index.
    model: [u8; 16],
}

thread_local! {
    /// Requests on this shard that have queued a text and not yet been answered.
    ///
    /// Shard-local by construction: one shard is one Glommio `LocalExecutor` is
    /// one thread, so a thread-local queue is a per-shard queue and no handle
    /// has to be threaded through `OpsContext`. Nothing in it is `Send` and
    /// nothing in it crosses a shard boundary.
    static EMBED_QUEUE: RefCell<VecDeque<PendingEmbed>> = const { RefCell::new(VecDeque::new()) };

    /// How many of this shard's batches are currently inside the model.
    ///
    /// A request that finds room under [`MAX_BATCHES_IN_FLIGHT`] collects a
    /// batch: it drains the queue, runs the forward pass, and answers everyone
    /// it drained, itself included. A request that finds no room has already
    /// queued its text and only waits, because a running collector will drain
    /// it when the pass it is inside returns. That is what makes the window
    /// adaptive without a timer — a batch is exactly what arrived during the
    /// previous forward pass, so a lone request under no load collects a batch
    /// of one and waits on nothing.
    static EMBED_BATCHES_IN_FLIGHT: Cell<usize> = const { Cell::new(0) };

    /// Batches run and texts carried on this shard, so a load test can report
    /// the window widths its traffic actually produced.
    static EMBED_WINDOW_STATS: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// Batches run and texts carried on the calling shard since the process
/// started. The ratio is the mean window this shard's load produced: at one
/// text per batch the requests never overlapped and the window bought nothing.
#[must_use]
pub fn embed_window_stats() -> (u64, u64) {
    EMBED_WINDOW_STATS.with(Cell::get)
}

/// Gives a collector's slot back however it leaves its loop.
///
/// Production request tasks are detached and run to completion, so a collector
/// always finishes its loop; the guard is here so that a caller which is
/// dropped mid-pass returns its slot instead of wedging every later write on a
/// count nobody will lower. Requests still queued behind a dropped collector
/// are picked up by the next request to arrive.
struct CollectorSlot;

impl Drop for CollectorSlot {
    fn drop(&mut self) {
        EMBED_BATCHES_IN_FLIGHT.with(|count| count.set(count.get().saturating_sub(1)));
    }
}

/// Embed a passage through the shard's batch window.
///
/// The caller queues its text and then either collects the next batch or waits
/// for a batch already running to collect it. Every text a shard has waiting
/// when the model frees up goes through together, which is where the throughput
/// comes from: a shard under load turns most of each forward pass into extra
/// rows instead of extra passes.
/// Awaited on a Glommio executor.
pub async fn embed_batched_off_reactor(
    embedder: &Arc<dyn Dispatcher>,
    text: &str,
) -> Result<[f32; VECTOR_DIM], EmbedError> {
    let (respond, answer) = oneshot::channel();
    EMBED_QUEUE.with(|queue| {
        queue.borrow_mut().push_back(PendingEmbed {
            text: text.to_owned(),
            respond,
            model: embedder.fingerprint(),
        });
    });
    // Reading the count and raising it with no await in between is what keeps
    // the bound exact: a shard runs one task at a time, so no other request can
    // observe room inside that gap.
    let collects = EMBED_BATCHES_IN_FLIGHT.with(|count| {
        let running = count.get();
        let room = running < MAX_BATCHES_IN_FLIGHT;
        if room {
            count.set(running + 1);
        }
        room
    });
    if collects {
        let _slot = CollectorSlot;
        run_batches(embedder).await;
    }
    answer.await.unwrap_or_else(|_| {
        Err(EmbedError::BatchFailed(
            "the batch collector went away before answering this request".to_owned(),
        ))
    })
}

/// Run this shard's batches until nothing is waiting.
///
/// The loop is what lets a window fill. Each pass answers the texts it drained,
/// and the requests that arrived while it was inside the model are already
/// queued when it comes back round, so they form the next batch.
async fn run_batches(embedder: &Arc<dyn Dispatcher>) {
    let model = embedder.fingerprint();
    loop {
        let batch: Vec<PendingEmbed> = EMBED_QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();
            let width = queue.len().min(MAX_BATCH_TEXTS);
            queue.drain(..width).collect()
        });
        if batch.is_empty() {
            return;
        }
        debug_assert!(
            batch.iter().all(|pending| pending.model == model),
            "every request on a shard embeds through that shard's one model",
        );
        EMBED_WINDOW_STATS.with(|stats| {
            let (batches, texts) = stats.get();
            stats.set((batches + 1, texts + batch.len() as u64));
        });
        let mut texts = Vec::with_capacity(batch.len());
        let mut responders = Vec::with_capacity(batch.len());
        for pending in batch {
            texts.push(pending.text);
            responders.push(pending.respond);
        }
        match embed_batch_off_reactor(embedder, texts).await {
            Ok(vectors) if vectors.len() == responders.len() => {
                for (respond, vector) in responders.into_iter().zip(vectors) {
                    // A requester that has gone away has nothing left to
                    // receive its row, which costs the rest of the batch
                    // nothing.
                    let _ = respond.send(Ok(vector));
                }
            }
            Ok(vectors) => {
                // The model returned a different number of rows than the batch
                // had texts, so no row can be matched to a requester with any
                // confidence. Pairing them up by position anyway is precisely
                // the silent corruption this path must never produce, so the
                // whole batch fails instead.
                let detail = format!(
                    "batch of {} texts returned {} vectors",
                    responders.len(),
                    vectors.len()
                );
                for respond in responders {
                    let _ = respond.send(Err(EmbedError::BatchFailed(detail.clone())));
                }
            }
            Err(error) => {
                // One forward pass is one fate. The specific variant is lost
                // for every request after the first because the error cannot be
                // cloned; the message it carried reaches the client either way.
                let detail = error.to_string();
                for respond in responders {
                    let _ = respond.send(Err(EmbedError::BatchFailed(detail.clone())));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Writes the embedded text's length into lane 0, which is enough to tell
    /// the passage path from the query path: the query path arrives carrying
    /// the BGE prefix, so its text is longer.
    struct LengthMarker;

    impl Dispatcher for LengthMarker {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            let mut v = [0.0_f32; VECTOR_DIM];
            v[0] = text.chars().count() as f32;
            Ok(v)
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0; 16]
        }
    }

    /// Both helpers complete on an executor, and the query helper keeps the
    /// asymmetric BGE prefix that makes its vector a retrieval query rather
    /// than a stored passage.
    #[test]
    fn both_helpers_embed_on_an_executor() {
        let embedder: Arc<dyn Dispatcher> = Arc::new(LengthMarker);
        let (passage, query) = glommio::LocalExecutor::default().run(async {
            let passage = embed_off_reactor(&embedder, "cue")
                .await
                .expect("passage embed");
            let query = embed_query_off_reactor(&embedder, "cue")
                .await
                .expect("query embed");
            (passage, query)
        });
        assert!(passage[0] > 0.0, "the dispatcher saw the passage text");
        assert!(
            query[0] > passage[0],
            "the query path prefixes before embedding",
        );
    }

    /// The offload is the contract, not an optimisation: with no reactor to
    /// move work off, the helper refuses loudly instead of quietly blocking
    /// the calling thread for the duration of a forward pass.
    #[test]
    #[should_panic(expected = "scoped thread local")]
    fn embedding_without_an_executor_panics() {
        let embedder: Arc<dyn Dispatcher> = Arc::new(LengthMarker);
        let _ = futures_lite::future::block_on(embed_off_reactor(&embedder, "cue"));
    }

    /// Writes the trailing serial of the text it embedded into lane 0, so a
    /// caller can tell its own vector from its neighbour's, and records the
    /// width of every batch it was handed.
    struct IdentityBatcher {
        widths: Mutex<Vec<usize>>,
    }

    impl IdentityBatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                widths: Mutex::new(Vec::new()),
            })
        }

        fn widths(&self) -> Vec<usize> {
            self.widths.lock().expect("batch width log").clone()
        }
    }

    impl Dispatcher for IdentityBatcher {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            let serial: f32 = text
                .rsplit(' ')
                .next()
                .and_then(|tail| tail.parse().ok())
                .ok_or_else(|| {
                    EmbedError::TokenizationFailed(format!("text carries no serial: {text}"))
                })?;
            let mut vector = [0.0_f32; VECTOR_DIM];
            vector[0] = serial;
            Ok(vector)
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            self.widths
                .lock()
                .expect("batch width log")
                .push(texts.len());
            texts.iter().map(|text| self.embed(text)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0; 16]
        }
    }

    /// Every requester is answered with the vector for its own text. A batch
    /// that handed requester `j` row `i` would be invisible at the call site —
    /// each memory would store a plausible vector belonging to someone else's
    /// text, and recall would decay with nothing to point at — so this pins the
    /// fan-out rather than only the throughput it buys.
    #[test]
    fn a_batch_answers_each_requester_with_its_own_vector() {
        const REQUESTS: usize = 8;
        let batcher = IdentityBatcher::new();
        let embedder: Arc<dyn Dispatcher> = Arc::clone(&batcher) as Arc<dyn Dispatcher>;
        let answers = glommio::LocalExecutor::default().run(async move {
            let mut tasks = Vec::with_capacity(REQUESTS);
            for serial in 0..REQUESTS {
                let embedder = Arc::clone(&embedder);
                tasks.push(glommio::spawn_local(async move {
                    let vector = embed_batched_off_reactor(&embedder, &format!("passage {serial}"))
                        .await
                        .expect("batched embed");
                    (serial, vector[0])
                }));
            }
            let mut answers = Vec::with_capacity(REQUESTS);
            for task in tasks {
                answers.push(task.await);
            }
            answers
        });
        for (serial, lane) in answers {
            assert_eq!(
                lane, serial as f32,
                "request {serial} was answered with another request's vector",
            );
        }
        let widths = batcher.widths();
        assert_eq!(
            widths.iter().sum::<usize>(),
            REQUESTS,
            "every text was embedded exactly once: widths {widths:?}",
        );
        assert!(
            widths.iter().any(|width| *width > 1),
            "the requests overlapped into a shared batch: widths {widths:?}",
        );
    }

    /// A write arriving alone does not wait for company. It collects a batch of
    /// one and returns after a single forward pass, which is what keeps an
    /// interactive client's latency where it was before the window existed.
    #[test]
    fn a_lone_request_embeds_without_waiting_for_a_batch() {
        let batcher = IdentityBatcher::new();
        let embedder: Arc<dyn Dispatcher> = Arc::clone(&batcher) as Arc<dyn Dispatcher>;
        let vector = glommio::LocalExecutor::default()
            .run(async move { embed_batched_off_reactor(&embedder, "passage 3").await })
            .expect("batched embed");
        assert_eq!(vector[0], 3.0);
        assert_eq!(
            batcher.widths(),
            vec![1],
            "one request alone is one batch of one",
        );
    }
}
