//! Input hash for the LLM extractor response cache.

/// BLAKE3 over the tenant, the active schema version, and the fully
/// materialized LLM prompt-context body — the input component of the
/// LLM extractor cache key.
///
/// The prompt body already carries every value that materially changes
/// the model's output (memory text, anchor date, declared entity types,
/// declared kinds, candidate predicates, prior entities, and bounded
/// neighbor/summary context, all substituted in). Hashing it — rather
/// than the memory text alone — guarantees that any change to that
/// context yields a distinct key, so a cached response is only ever
/// served for the exact context that produced it. The `space` component
/// keys the entry to its owning tenant, so two tenants can never share a
/// cache entry even for byte-identical text; the `schema_version`
/// component invalidates the entry the moment the active schema changes.
///
/// Components are domain-separated (fixed-width `space` and
/// `schema_version`, then a length-prefixed body) so no two distinct
/// inputs can collide by concatenation.
#[must_use]
pub fn hash_prompt_context(space: [u8; 16], schema_version: u32, prompt_body: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&space);
    h.update(&schema_version.to_le_bytes());
    let body = prompt_body.as_bytes();
    h.update(&(body.len() as u64).to_le_bytes());
    h.update(body);
    h.finalize().into()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_context_hash_is_deterministic() {
        let a = hash_prompt_context([7u8; 16], 3, "the body");
        let b = hash_prompt_context([7u8; 16], 3, "the body");
        assert_eq!(a, b);
    }

    #[test]
    fn prompt_context_hash_varies_on_every_component() {
        let base = hash_prompt_context([7u8; 16], 3, "the body");
        // Different tenant.
        assert_ne!(base, hash_prompt_context([8u8; 16], 3, "the body"));
        // Different schema version.
        assert_ne!(base, hash_prompt_context([7u8; 16], 4, "the body"));
        // Different prompt body (anchor date / declared types / etc. all
        // land in the body).
        assert_ne!(base, hash_prompt_context([7u8; 16], 3, "the BODY"));
    }

    #[test]
    fn prompt_context_hash_is_unambiguous_across_component_boundaries() {
        // Length-prefixing means a shift of bytes between the fixed-width
        // prefix and the body can't produce a collision.
        let a = hash_prompt_context([0u8; 16], 0, "ab");
        let b = hash_prompt_context([0u8; 16], 0, "a");
        assert_ne!(a, b);
    }
}
