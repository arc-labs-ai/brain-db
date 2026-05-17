//! `LlmClient` trait. Spec §22/09 §1.
//!
//! Object-safe (no `async fn` in the trait itself; concrete impls
//! return `Pin<Box<dyn Future + Send + 'a>>`) so multiple
//! provider implementations can co-exist behind `Arc<dyn LlmClient>`.

use std::future::Future;
use std::pin::Pin;

use crate::error::LlmError;
use crate::types::{LlmRequest, LlmResponse};

/// Future returned by [`LlmClient::complete`].
pub type LlmFuture<'a> =
    Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>>;

pub trait LlmClient: Send + Sync {
    /// Send the request to the provider and return the
    /// normalised response. Errors map to spec §22/09 §9.
    fn complete<'a>(&'a self, request: LlmRequest) -> LlmFuture<'a>;

    /// Model identifier this client serves (e.g.,
    /// `"anthropic/claude-haiku-4-5"`). The provider router uses
    /// this for routing diagnostics; the LLM extractor uses
    /// [`Self::model_id_hash`] for the cache key.
    fn model(&self) -> &str;

    /// 64-bit BLAKE3-low of [`Self::model`]. Cache key component
    /// per spec §22/09 §3 + §15.4.
    fn model_id_hash(&self) -> u64;
}

/// Compute the cache-key model_id hash from a model identifier.
/// Centralised so providers + the cache lookup agree.
#[must_use]
pub fn model_id_hash(model: &str) -> u64 {
    let h = blake3::hash(model.as_bytes());
    let b = h.as_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    // We can't use blake3 here directly without adding the dep.
    // Add it to the crate so model_id_hash compiles. Test it.
    #[test]
    fn model_id_hash_is_stable() {
        let a = model_id_hash("claude-haiku-4-5");
        let b = model_id_hash("claude-haiku-4-5");
        assert_eq!(a, b);
        let c = model_id_hash("claude-sonnet-4-6");
        assert_ne!(a, c);
    }
}
