//! Dispatch surface for text → vector.
//!
//! v1 (CPU-only) ships a [`Dispatcher`] trait + a [`CpuDispatcher`]
//! pass-through. There is **no** window-and-batch machinery on CPU:
//! the substrate doesn't internally batch CPU inference. Each
//! request goes through the model independently.
//!
//! The trait exists so:
//! - The cache wraps any `Dispatcher`, not just `CpuDispatcher`.
//! - Ops can test against a mock `Dispatcher` without
//!   loading ~130 MiB of BGE-small weights per test.
//! - A future GPU dispatcher plugs `GpuDispatcher` into the same trait
//!   without re-spelling the surface.
//!
//! Concurrency: `Dispatcher: Send + Sync`. Multiple Glommio executors
//! can call inference concurrently. Each call runs on the current core.
//! The model's weights are shared across all callers via `Arc<Model>`.
//! `CpuDispatcher` owns `Arc<ModelHandle>` so clones are cheap and many
//! shards can share a single loaded model.

use std::sync::Arc;

use crate::error::EmbedError;
use crate::model::ModelHandle;
use crate::model::{embed_batch, embed_text, VECTOR_DIM};

/// Sync, thread-safe text-to-vector dispatch.
///
/// `embed_batch` accepts a caller-provided batch. The substrate does
/// **not** assemble batches by waiting for more
/// requests — but it does honour batches the caller already has,
/// because that lets one BertModel forward pass amortise across the
/// rows.
pub trait Dispatcher: Send + Sync {
    /// Embed a *passage* (stored vector). Returns a 384-dim L2-normalised
    /// vector. Use this for ENCODE memory text, entity canonical names,
    /// statement text, and anywhere else the vector goes *into* the index.
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>;

    /// Embed a caller-provided batch in one forward pass.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;

    /// Embed a *query* (lookup vector). Returns a 384-dim L2-normalised
    /// vector with the BGE asymmetric-retrieval prefix
    /// ([`BGE_QUERY_PREFIX`]) applied. Use this for RECALL / PLAN /
    /// REASON cues and the retrieval SemanticRetriever's query embed.
    ///
    /// Default impl concatenates the prefix and delegates to `embed`;
    /// the prefix is applied *before* the cache lookup so query and
    /// passage entries for the same surface text are independent rows
    /// (no collision).
    fn embed_query(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut prefixed = String::with_capacity(BGE_QUERY_PREFIX.len() + text.len());
        prefixed.push_str(BGE_QUERY_PREFIX);
        prefixed.push_str(text);
        self.embed(&prefixed)
    }

    /// 16-byte BLAKE3-truncated model fingerprint.
    /// Stable for the process lifetime; used by the cache key and
    /// the ENCODE path to stamp stored vectors.
    fn fingerprint(&self) -> [u8; 16];
}

/// Asymmetric-retrieval prefix for `bge-small-en-v1.5` queries.
/// Concatenated directly with the query text — no
/// separator. Skipping this prefix degrades short-query recall badly;
/// it's what the upstream model card prescribes.
pub const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// CPU dispatcher. Pure pass-through to [`embed_text`] /
/// [`embed_batch`]; no queue, no window.
#[derive(Clone)]
pub struct CpuDispatcher {
    model: Arc<ModelHandle>,
}

impl CpuDispatcher {
    /// Wrap a freshly-loaded `ModelHandle` into a dispatcher.
    #[must_use]
    pub fn new(model: ModelHandle) -> Self {
        Self {
            model: Arc::new(model),
        }
    }

    /// Wrap an already-shared `Arc<ModelHandle>`. Useful when one
    /// loaded model serves multiple dispatchers (shards, caches).
    #[must_use]
    pub fn from_arc(model: Arc<ModelHandle>) -> Self {
        Self { model }
    }

    /// Borrow the inner shared handle. Escape hatch for callers that
    /// need raw access to the tokenizer or forward primitives.
    #[must_use]
    pub fn handle(&self) -> &Arc<ModelHandle> {
        &self.model
    }
}

impl Dispatcher for CpuDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        embed_text(&self.model, text)
    }

    /// The substrate does not assemble batches
    /// itself. If you call this with multiple texts, they run as one
    /// BertModel forward pass (cheaper than N serial single-text
    /// calls even on CPU, since matmul amortises across the rows).
    /// The "no batching" rule means no time-window queueing — not
    /// that batches are forbidden.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        embed_batch(&self.model, texts)
    }

    fn fingerprint(&self) -> [u8; 16] {
        self.model.fingerprint()
    }
}

// Compile-time guard against candle / tokenizers regressing on
// thread-safety. If either crate stops being Send + Sync, the build
// breaks here instead of mysteriously at the call site.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ModelHandle>();
    assert_send_sync::<CpuDispatcher>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A small mock Dispatcher proves the trait is usable without a
    /// real model.
    #[test]
    fn mock_dispatcher_implements_trait() {
        struct Mock {
            fp: [u8; 16],
        }
        impl Dispatcher for Mock {
            fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                self.fp
            }
        }
        let m = Mock { fp: [0x11; 16] };
        let dyn_disp: &dyn Dispatcher = &m;
        assert_eq!(dyn_disp.fingerprint(), [0x11; 16]);
        assert_eq!(dyn_disp.embed_batch(&["a", "b"]).unwrap().len(), 2);
    }

    /// `embed_query` must apply the BGE retrieval prefix before calling
    /// `embed`. This both verifies the asymmetric pipeline
    /// and proves the cache-collision invariant: query and passage
    /// for the same surface text route to different cache rows because
    /// the prefix is part of the lookup key.
    #[test]
    fn embed_query_prepends_bge_retrieval_prefix() {
        use std::sync::Mutex;
        struct Recorder {
            seen: Mutex<Vec<String>>,
        }
        impl Dispatcher for Recorder {
            fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
                self.seen.lock().unwrap().push(text.to_string());
                Ok([0.0; VECTOR_DIM])
            }
            fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
                Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
            }
            fn fingerprint(&self) -> [u8; 16] {
                [0; 16]
            }
        }
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        // Passage path: raw text.
        r.embed("priya works at stripe").unwrap();
        // Query path: prefix + same text.
        r.embed_query("priya works at stripe").unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], "priya works at stripe");
        assert_eq!(
            seen[1],
            format!("{}priya works at stripe", BGE_QUERY_PREFIX)
        );
        // The two strings differ → the dispatcher cache (keyed on
        // BLAKE3 of input text) treats them as independent entries.
        assert_ne!(seen[0], seen[1]);
    }
}
