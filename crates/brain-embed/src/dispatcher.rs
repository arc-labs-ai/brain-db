//! Dispatch surface for text → vector.
//!
//! v1 (CPU-only) ships a [`Dispatcher`] trait + a [`CpuDispatcher`]
//! pass-through. There is **no** window-and-batch machinery on CPU —
//! spec `04/03 §7` and `04/03 §14` are both explicit:
//!
//! > "The substrate doesn't internally batch CPU inference. Each
//! > request goes through the model independently."
//!
//! The trait exists so:
//! - 5.5's cache wraps any `Dispatcher`, not just `CpuDispatcher`.
//! - Phase 7's ops can test against a mock `Dispatcher` without
//!   loading ~130 MiB of BGE-small weights per test.
//! - A future GPU sub-task plugs `GpuDispatcher` into the same trait
//!   without re-spelling the surface (spec `04/06` covers the
//!   window+batch design for that path).
//!
//! Concurrency: `Dispatcher: Send + Sync`. Per spec `04/03 §7`,
//! "multiple Glommio executors can call inference concurrently. Each
//! call runs on the current core. The model's weights are shared
//! across all callers via `Arc<Model>`." `CpuDispatcher` owns
//! `Arc<ModelHandle>` so clones are cheap and many shards can share
//! a single loaded model.

use std::sync::Arc;

use crate::error::EmbedError;
use crate::forward::{embed_batch, embed_text, VECTOR_DIM};
use crate::model::ModelHandle;

/// Sync, thread-safe text-to-vector dispatch.
///
/// `embed_batch` accepts a caller-provided batch. Per spec `04/03 §7`
/// the substrate does **not** assemble batches by waiting for more
/// requests — but it does honour batches the caller already has,
/// because that lets one BertModel forward pass amortise across the
/// rows.
pub trait Dispatcher: Send + Sync {
    /// Embed a single text. Returns a 384-dim L2-normalised vector.
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>;

    /// Embed a caller-provided batch in one forward pass.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;

    /// 16-byte BLAKE3-truncated model fingerprint (spec `04/07 §3`).
    /// Stable for the process lifetime; used by 5.5's cache key and
    /// Phase 7's ENCODE path to stamp stored vectors.
    fn fingerprint(&self) -> [u8; 16];
}

/// CPU dispatcher. Pure pass-through to 5.3's [`embed_text`] /
/// [`embed_batch`]; no queue, no window, per spec `04/03 §7 + §14`.
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

    /// Per spec `04/03 §7`, the substrate does not assemble batches
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

    /// Object-safety check: `dyn Dispatcher` must be constructible.
    /// The cache in 5.5 and Phase 7's ops both need this.
    #[test]
    fn dispatcher_trait_is_object_safe() {
        fn _accepts(_d: &dyn Dispatcher) {}
        // Compile-only: if the trait stops being object-safe (added a
        // generic method, used `Self` in a return position, etc.),
        // this fails to type-check. No runtime body needed beyond the
        // function existing.
    }

    #[test]
    fn cpu_dispatcher_is_send_sync_and_clone() {
        fn require_send_sync<T: Send + Sync>() {}
        fn require_clone<T: Clone>() {}
        require_send_sync::<CpuDispatcher>();
        require_clone::<CpuDispatcher>();
    }

    /// A small mock Dispatcher proves the trait is usable without a
    /// real model — Phase 7's tests will lean on this pattern.
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
}
