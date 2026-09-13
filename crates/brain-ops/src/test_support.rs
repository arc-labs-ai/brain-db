//! Test helpers shared between brain-ops's unit tests and its
//! integration tests under `tests/`. Linux-only — the runtime
//! production targets (Glommio) is Linux-only, and so are the
//! tests that exercise it.
//!
//! Keep this surface minimal. Test-specific helpers that belong to
//! one file should stay in that file; only put things here when they
//! are needed in two places.

#[cfg(target_os = "linux")]
pub use linux::run_in_glommio;

use std::path::Path;
use std::sync::Arc;

use brain_embed::Dispatcher;
use brain_index::{
    GraphRetriever, LexicalRetriever, SemanticRetriever, SharedHnsw, TantivyLexicalRetriever,
    TantivyShard,
};
use brain_metadata::MetadataDb;
use brain_protocol::envelope::response::ResponseBody;

use crate::DispatchOutcome;

/// Unwrap a non-streaming `dispatch` outcome. The vast majority of
/// integration-test call sites assert against a single response frame;
/// expecting a `Stream` here is a test-author bug, not a runtime case
/// to handle.
pub fn single_body(outcome: DispatchOutcome) -> ResponseBody {
    match outcome {
        DispatchOutcome::Single(b) => b,
        DispatchOutcome::Stream(_) => {
            panic!("expected DispatchOutcome::Single, got Stream")
        }
    }
}

/// Build a real `LexicalRetriever` backed by a `TantivyShard` opened
/// against `dir`. Tests pass a `tempfile::TempDir` so the indexes live
/// for the duration of the test only.
///
/// Production shards never construct retrievers this way — they go
/// through `spawn_shard`, which propagates open + recovery failures
/// through `ShardError`. Tests can `expect()` here because a failing
/// open on a fresh tempdir is a bug in the helper, not a runtime
/// concern under test.
#[must_use]
pub fn tantivy_for_tests(dir: &Path) -> Arc<dyn LexicalRetriever> {
    let startup = TantivyShard::open(dir).expect("TantivyShard::open(tempdir) must succeed");
    Arc::new(
        TantivyLexicalRetriever::new(startup.shard)
            .expect("TantivyLexicalRetriever::new must succeed on a fresh tempdir"),
    )
}

/// Rebuild the memory-text lexical index from redb and return a fresh
/// retriever over it. Fixtures encode through the writer (redb + HNSW)
/// but no background text-indexer runs in a unit test, so the lexical
/// lane would otherwise stay empty. Production keeps it populated (the
/// indexer worker, or `rebuild_memory_text` on cold start), and the read
/// path's structural abstention relies on that: a stored memory's own
/// words confirm it lexically, so a genuine hit is never dropped as
/// unanchored semantic noise. Call this after seeding and before recall
/// so the fixture matches a fully-indexed shard.
#[must_use]
pub fn reindex_memory_lexical_for_tests(
    dir: &Path,
    metadata: &MetadataDb,
) -> Arc<dyn LexicalRetriever> {
    crate::index::text_indexer::rebuild_memory_text(dir, metadata)
        .expect("rebuild_memory_text on a tempdir must succeed");
    tantivy_for_tests(dir)
}

/// Build a real `SemanticRetriever` over the in-memory HNSW. The
/// statement-side HNSW is left `None` — the integration tests that
/// exercise statement-semantic paths build the index themselves.
#[must_use]
pub fn semantic_for_tests(
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw,
    metadata: Arc<MetadataDb>,
) -> Arc<dyn SemanticRetriever> {
    Arc::new(
        crate::index::semantic_retriever::BrainSemanticRetriever::new(
            embedder,
            memory_index,
            None,
            metadata,
        ),
    )
}

/// Build a real `GraphRetriever` over the redb metadata store.
#[must_use]
pub fn graph_for_tests(metadata: Arc<MetadataDb>) -> Arc<dyn GraphRetriever> {
    Arc::new(crate::index::graph_retriever::BrainGraphRetriever::new(
        metadata,
    ))
}

/// Build all three retrievers in one call. Most fixtures use this so
/// each test's set-up stays a one-liner.
#[must_use]
pub fn retrievers_for_tests(
    dir: &Path,
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw,
    metadata: Arc<MetadataDb>,
) -> (
    Arc<dyn LexicalRetriever>,
    Arc<dyn SemanticRetriever>,
    Arc<dyn GraphRetriever>,
) {
    (
        tantivy_for_tests(dir),
        semantic_for_tests(embedder, memory_index, metadata.clone()),
        graph_for_tests(metadata),
    )
}

/// One-shot helper: build a default `OpsContext` whose three retriever
/// slots are wired to real impls backed by `dir`. Pulls the embedder /
/// memory HNSW / metadata out of the already-built `ExecutorContext`
/// so the test only has to pass one `ExecutorContext` value.
#[must_use]
pub fn ops_context_for_tests(
    executor: brain_planner::ExecutorContext,
    dir: &Path,
) -> crate::OpsContext {
    let embedder = executor.embedder.clone();
    let memory_index = executor.index.clone();
    let metadata = executor.metadata.clone();
    let (lexical, semantic, graph) = retrievers_for_tests(dir, embedder, memory_index, metadata);
    crate::OpsContext::new(executor, lexical, semantic, graph)
}

/// Drop-in shorthand for tests that don't keep a tempdir handle in
/// scope — leaks a fresh tempdir for the tantivy index so the test
/// process can pretend the storage doesn't exist. Use this only when
/// the test has no `TempDir` to thread through.
///
/// Production code never calls this — there's no `Path` argument, so
/// it can't accidentally pollute a real shard's directory.
#[must_use]
pub fn ops_context_for_tests_owning_tempdir(
    executor: brain_planner::ExecutorContext,
) -> crate::OpsContext {
    let tempdir = tempfile::tempdir().expect("tempdir for ops_context_for_tests_owning_tempdir");
    let ctx = ops_context_for_tests(executor, tempdir.path());
    // Test lifecycle: the tantivy `Index` keeps the underlying directory
    // pinned via its inner `Arc`. Leaking the `TempDir` lets the
    // directory survive until the test process exits, matching the
    // pre-pivot behavior where tantivy slots stayed `None` and there
    // was no directory to keep around.
    std::mem::forget(tempdir);
    ctx
}

#[cfg(target_os = "linux")]
mod linux {
    /// Run an async test body inside a fresh Glommio executor on a
    /// dedicated OS thread. Mirrors the `glommio_run` helper used
    /// across `brain-workers/tests/*`.
    ///
    /// Tests that drive any code which production runs on a shard
    /// (e.g. anything reached through `brain_ops::dispatch`, or any
    /// worker spawned via `glommio::spawn_local`) must use this so
    /// the runtime under test matches the runtime in production.
    pub fn run_in_glommio<F, Fut, T>(f: F) -> T
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + 'static,
        T: Send + 'static,
    {
        glommio::LocalExecutorBuilder::default()
            .name("brain-ops-test")
            .spawn(move || async move { f().await })
            .expect("spawn glommio test executor")
            .join()
            .expect("test executor join")
    }
}
