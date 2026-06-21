//! HyPE persistence: durable hypothetical-question vectors.
//!
//! The in-RAM HyPE HNSW (`brain_index::HypeHnswIndex`) is derived from
//! these rows and rebuilt on boot, so a restart never re-runs the LLM or
//! the embedder — it range-scans the table and re-inserts.

pub mod ops;
