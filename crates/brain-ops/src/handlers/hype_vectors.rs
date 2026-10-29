//! Teardown of the HyPE question-vectors a memory owns.
//!
//! A memory's hypothetical-question vectors outlive the memory unless
//! something removes them: the durable redb rows are what the in-RAM HyPE
//! HNSW is rebuilt from at boot, so a memory whose rows survive keeps
//! coming back as a probe target after it is gone. Two paths retire a
//! memory and so both have to do this — the non-transactional FORGET and
//! the TXN_COMMIT replay of a buffered forget — and the second reached
//! across into `forget.rs` for a `pub(crate)` helper. Retiring a memory
//! from HyPE is not FORGET's private business; FORGET is just the path
//! that needed it first.
//!
//! The module boundary is "what has to happen to a memory's HyPE rows
//! when the memory goes away", not "which verb removes the memory".

use brain_core::MemoryId;

use crate::context::OpsContext;

/// Remove every HyPE question-vector row owned by `memory_id` in a
/// dedicated write transaction. Idempotent: `hype_vectors_delete_memory`
/// returns 0 when the memory owns none, so a double-forget (or the
/// slot-reclaim backstop) is a harmless no-op. Best-effort: a redb error
/// is logged, not propagated — FORGET is lenient and the reclaim worker
/// re-runs this at grace.
pub(super) fn delete_hype_vectors(ctx: &OpsContext, memory_id: MemoryId) {
    let wtxn = match ctx.executor.metadata.write_txn() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                target: "brain_ops::forget",
                memory_id = ?memory_id,
                error = %e,
                "forget: HyPE vector delete write_txn failed; leaving rows for reclaim",
            );
            return;
        }
    };
    match brain_metadata::hype_vectors_delete_memory(&wtxn, memory_id) {
        Ok(removed) => {
            if let Err(e) = wtxn.commit() {
                tracing::warn!(
                    target: "brain_ops::forget",
                    memory_id = ?memory_id,
                    error = %e,
                    "forget: HyPE vector delete commit failed; leaving rows for reclaim",
                );
            } else if removed > 0 {
                tracing::debug!(
                    target: "brain_ops::forget",
                    memory_id = ?memory_id,
                    removed,
                    "forget: removed HyPE question-vectors",
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "brain_ops::forget",
                memory_id = ?memory_id,
                error = %e,
                "forget: HyPE vector delete failed; leaving rows for reclaim",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::tables::hype::HYPE_QUESTION_VECTORS_TABLE;
    use brain_metadata::{hype_vector_put, MetadataDb};
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use redb::ReadableTable;
    use std::sync::Arc;

    struct MockDispatcher;
    impl Dispatcher for MockDispatcher {
        fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(texts.iter().map(|_| [0.0; VECTOR_DIM]).collect())
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0xAB; 16]
        }
    }

    fn build_ctx() -> (tempfile::TempDir, OpsContext, SharedMetadataDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata"));
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).expect("hnsw");
        let writer = Arc::new(crate::writer::RealWriterHandle::new(
            metadata.clone(),
            hnsw_writer,
        ));
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata.clone(),
            writer as Arc<dyn WriterHandle>,
        );
        let ctx = crate::test_support::ops_context_for_tests(executor, dir.path());
        (dir, ctx, metadata)
    }

    fn mem(n: u128) -> MemoryId {
        MemoryId::from(n)
    }

    /// Seed `count` question vectors at slots `0..count`, each filled with a
    /// value derived from the memory so a cross-memory leak would be visible.
    fn seed(metadata: &SharedMetadataDb, memory_id: MemoryId, count: u8) {
        let wtxn = metadata.write_txn().expect("write txn");
        for i in 0..count {
            let v = [f32::from(i) + 1.0; 384];
            hype_vector_put(&wtxn, memory_id, i, &v).expect("put");
        }
        wtxn.commit().expect("commit");
    }

    /// Row count for one memory, read straight off the table rather than
    /// through `hype_has_vectors` — the tests need "how many", not "any".
    fn rows_for(metadata: &SharedMetadataDb, memory_id: MemoryId) -> usize {
        let rtxn = metadata.read_txn().expect("read txn");
        let t = rtxn
            .open_table(HYPE_QUESTION_VECTORS_TABLE)
            .expect("table is materialized at open");
        let prefix = memory_id.to_be_bytes();
        t.iter()
            .expect("iter")
            .filter(|e| {
                let (k, _) = e.as_ref().expect("row");
                k.value()[..16] == prefix
            })
            .count()
    }

    fn total_rows(metadata: &SharedMetadataDb) -> usize {
        let rtxn = metadata.read_txn().expect("read txn");
        let t = rtxn
            .open_table(HYPE_QUESTION_VECTORS_TABLE)
            .expect("table is materialized at open");
        t.iter().expect("iter").count()
    }

    /// A memory that was never put through HyPE owns no rows, and the
    /// delete leaves the table exactly as it found it. The common case:
    /// most memories never generate questions, and every FORGET calls this.
    #[test]
    fn memory_with_no_vectors_changes_nothing() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, mem(2), 3);
        let before = total_rows(&metadata);

        delete_hype_vectors(&ctx, mem(1));

        assert_eq!(rows_for(&metadata, mem(1)), 0);
        assert_eq!(
            total_rows(&metadata),
            before,
            "a memory with no rows must not disturb anyone else's",
        );
    }

    /// All of a memory's slots go, not just the first. The rows are keyed
    /// `memory_id ++ question_index`, so a delete that stopped at the first
    /// hit would leave slots 1..n probing for a forgotten memory.
    #[test]
    fn every_slot_goes() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, mem(1), 5);
        assert_eq!(rows_for(&metadata, mem(1)), 5);

        delete_hype_vectors(&ctx, mem(1));

        assert_eq!(rows_for(&metadata, mem(1)), 0);
    }

    /// The helper never reads MEMORIES_TABLE — it deletes by key prefix. An
    /// id that was never encoded at all is the same no-op as an encoded
    /// memory with no questions, which is what makes the TXN replay path
    /// safe to call unconditionally.
    #[test]
    fn unknown_memory_id_is_a_no_op() {
        let (_dir, ctx, metadata) = build_ctx();
        delete_hype_vectors(&ctx, mem(0xDEAD_BEEF));
        assert_eq!(total_rows(&metadata), 0);
    }

    /// Idempotent by contract: the non-txn FORGET, the TXN commit replay
    /// and the slot-reclaim worker at grace can all run this for the same
    /// memory, and the second and third runs must be harmless.
    #[test]
    fn deleting_twice_is_idempotent() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, mem(1), 3);
        seed(&metadata, mem(2), 2);

        delete_hype_vectors(&ctx, mem(1));
        delete_hype_vectors(&ctx, mem(1));

        assert_eq!(rows_for(&metadata, mem(1)), 0);
        assert_eq!(
            rows_for(&metadata, mem(2)),
            2,
            "the repeat run must not widen into a neighbour",
        );
    }

    /// The range is `memory_id ++ 0x00 ..= memory_id ++ 0xFF`, and ids sort
    /// adjacently in the same table — so the one way this could go wrong is
    /// running off the end of a memory's slots into the next memory's. Use
    /// neighbouring ids on both sides of the target to pin the bound.
    #[test]
    fn neighbouring_memories_keep_their_vectors() {
        let (_dir, ctx, metadata) = build_ctx();
        seed(&metadata, mem(1), 2);
        seed(&metadata, mem(2), 4);
        seed(&metadata, mem(3), 2);

        delete_hype_vectors(&ctx, mem(2));

        assert_eq!(rows_for(&metadata, mem(2)), 0);
        assert_eq!(rows_for(&metadata, mem(1)), 2, "lower neighbour intact");
        assert_eq!(rows_for(&metadata, mem(3)), 2, "upper neighbour intact");
        assert_eq!(total_rows(&metadata), 4);
    }

    /// The last slot is index 255, and the range is inclusive of it. An
    /// exclusive upper bound would strand the final slot — invisible with
    /// the handful of questions HyPE generates in practice.
    #[test]
    fn the_last_slot_is_inside_the_range() {
        let (_dir, ctx, metadata) = build_ctx();
        {
            let wtxn = metadata.write_txn().expect("write txn");
            hype_vector_put(&wtxn, mem(1), 0, &[1.0; 384]).expect("put");
            hype_vector_put(&wtxn, mem(1), u8::MAX, &[2.0; 384]).expect("put");
            wtxn.commit().expect("commit");
        }
        assert_eq!(rows_for(&metadata, mem(1)), 2);

        delete_hype_vectors(&ctx, mem(1));

        assert_eq!(rows_for(&metadata, mem(1)), 0);
    }
}
