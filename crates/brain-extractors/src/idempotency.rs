//! Idempotency primitives.

use brain_core::{ExtractorId, MemoryId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    pub memory_id: MemoryId,
    pub text_hash: [u8; 32],
    pub extractor_id: ExtractorId,
    pub extractor_version: u32,
    pub schema_version: u32,
}

impl IdempotencyKey {
    #[must_use]
    pub fn new(
        memory_id: MemoryId,
        text: &str,
        extractor_id: ExtractorId,
        extractor_version: u32,
        schema_version: u32,
    ) -> Self {
        Self {
            memory_id,
            text_hash: hash_memory_text(text),
            extractor_id,
            extractor_version,
            schema_version,
        }
    }
}

/// BLAKE3 of `memory.text` as raw bytes. Used by [`IdempotencyKey`]
/// and the audit-row `input_hash` field.
#[must_use]
pub fn hash_memory_text(text: &str) -> [u8; 32] {
    blake3::hash(text.as_bytes()).into()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips_eq_hash() {
        let m = MemoryId::pack(1, 0, 0);
        let k1 = IdempotencyKey::new(m, "hello", ExtractorId::from(7), 1, 3);
        let k2 = IdempotencyKey::new(m, "hello", ExtractorId::from(7), 1, 3);
        assert_eq!(k1, k2);

        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(k1);
        assert!(s.contains(&k2));
    }

    #[test]
    fn different_text_yields_different_key() {
        let m = MemoryId::pack(1, 0, 0);
        let k1 = IdempotencyKey::new(m, "a", ExtractorId::from(1), 1, 1);
        let k2 = IdempotencyKey::new(m, "b", ExtractorId::from(1), 1, 1);
        assert_ne!(k1, k2);
    }
}
