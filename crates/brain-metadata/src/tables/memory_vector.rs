//! `memory_vectors` table: per-memory embedding vector, stored as raw
//! little-endian `f32` bytes for O(1) by-id resolution.
//!
//! ## Why this exists
//!
//! The full-precision embedding is also carried inside the JSON
//! `memory_artifacts` bundle, but that bundle bundles the graph, HyPE
//! questions, and keyword fields — so pulling the vector out of it means
//! JSON-parsing the whole blob. The hot single-space brute-force recall
//! lane resolves a vector *per candidate* (up to `SPACE_BRUTEFORCE_MAX`),
//! and JSON-parsing a graph-bearing bundle per candidate is far too slow
//! for a recall path. This table stores JUST the vector as a flat byte
//! run so a resolve is a single redb point lookup + a `from_le_bytes`
//! decode — no JSON, no allocation of the other bundle fields.
//!
//! ## What lives here
//!
//! - [`MEMORY_VECTORS_TABLE`] — `MemoryId` → the embedding as
//!   `VECTOR_DIM` little-endian `f32`s, concatenated.
//!
//! ## What does NOT live here
//!
//! - **The vector's other homes** — the in-RAM HNSW (search index) and,
//!   post-restart, the mmap'd arena (recovery image). This table is the
//!   durable by-id lookup the live path resolves from.
//! - **Alignment guarantees** — redb hands back an unaligned `&[u8]`, so
//!   readers decode via `f32::from_le_bytes` over `chunks_exact(4)`
//!   rather than a zero-copy `bytemuck` cast.

use redb::TableDefinition;

/// The `memory_vectors` table. Key is the `MemoryId`'s 16-byte raw form
/// (`MemoryId::to_be_bytes()`); value is `VECTOR_DIM` `f32`s written as
/// little-endian bytes, back-to-back (so `VECTOR_DIM * 4` bytes total).
///
/// `&[u8]` (redb's built-in variable-length type) rather than a rkyv
/// `Value`: the payload is a fixed flat byte run with no struct to
/// evolve, so rkyv would only add an encode/decode pass and the
/// `AlignedVec` copy for no benefit.
pub const MEMORY_VECTORS_TABLE: TableDefinition<'static, [u8; 16], &'static [u8]> =
    TableDefinition::new("memory_vectors");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn mid(byte: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[15] = byte;
        b
    }

    #[test]
    fn le_f32_bytes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("test.redb")).unwrap();
        let key = mid(7);
        let vector: Vec<f32> = vec![0.5, -1.25, 3.0, 0.0];

        // Encode as LE bytes (the shape the write path produces).
        let mut bytes = Vec::with_capacity(vector.len() * 4);
        for f in &vector {
            bytes.extend_from_slice(&f.to_le_bytes());
        }

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORY_VECTORS_TABLE).unwrap();
            t.insert(&key, bytes.as_slice()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORY_VECTORS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap();
        let decoded: Vec<f32> = got
            .value()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(decoded, vector);
    }
}
