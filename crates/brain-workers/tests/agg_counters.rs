#![allow(clippy::arc_with_non_send_sync)] // OpsContext is !Send
//! Aggregate-counter worker: online build after an upgrade and drift
//! repair, driven through real worker cycles on a Glommio executor.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::agg::{self, MaintenanceStats};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_workers::{AggCountersKnobs, AggCountersWorker, Worker, WorkerContext};

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct NopDispatcher;
impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

struct Fixture {
    ctx: Arc<OpsContext>,
    metadata: SharedMetadataDb,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor)),
        metadata,
        _tempdir: tempdir,
    }
}

fn raw_insert(metadata: &SharedMetadataDb, slots: std::ops::Range<u64>, salience: f32) {
    // As a pre-counter binary (or a buggy path) would write.
    let wtxn = metadata.write_txn().unwrap();
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for slot in slots {
            let r = MemoryMetadata::new_active(
                MemoryId::pack(0, slot, 1),
                NamespaceId::from((slot % 2) as u32),
                SpaceId::default(),
                SessionId(1),
                slot,
                1,
                MemoryKind::Semantic,
                [0; 16],
                salience,
                1,
                1_700_000_000_000_000_000,
            );
            t.insert(&r.memory_id_bytes, &r).unwrap();
        }
    }
    wtxn.commit().unwrap();
}

fn consistent(metadata: &SharedMetadataDb) -> bool {
    agg::verify(
        &metadata.read_txn().unwrap(),
        brain_metadata::salience::now_unix_nanos(),
    )
    .unwrap()
    .is_clean()
}

async fn cycle(worker: &AggCountersWorker, ops: &Arc<OpsContext>) {
    let wctx = WorkerContext {
        ops: ops.clone(),
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    worker.run_cycle(&wctx).await.unwrap();
}

#[test]
fn builds_counters_in_batches_then_repairs_drift() {
    glommio_run(|| async {
        let fix = build_fixture();
        raw_insert(&fix.metadata, 0..25, 0.5);
        let wtxn = fix.metadata.write_txn().unwrap();
        assert!(agg::backfill_if_missing(&wtxn).unwrap());
        wtxn.commit().unwrap();

        let stats = Arc::new(MaintenanceStats::default());
        let worker = AggCountersWorker::new(
            AggCountersKnobs {
                build_batch_rows: 10,
                verify_interval: Duration::from_secs(3600),
                first_verify_delay: Duration::ZERO,
            },
            stats.clone(),
            None,
        );

        // 25 rows at 10 per tick: building for three ticks.
        for _ in 0..3 {
            cycle(&worker, &fix.ctx).await;
            assert!(consistent(&fix.metadata), "partial build stays consistent");
        }
        assert_eq!(stats.build_rows.load(Ordering::Relaxed), 25);
        cycle(&worker, &fix.ctx).await; // idle tick: ready, starts a verify
        assert!(stats.counters_ready.load(Ordering::Relaxed));

        // Let the first (clean) verify land.
        for _ in 0..200 {
            cycle(&worker, &fix.ctx).await;
            if stats.verify_runs.load(Ordering::Relaxed) == 1 {
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(stats.verify_runs.load(Ordering::Relaxed), 1);
        assert_eq!(stats.drifted_buckets.load(Ordering::Relaxed), 0);

        // A bypassing write, then a verify that finds and repairs it.
        raw_insert(&fix.metadata, 100..103, 0.9);
        assert!(!consistent(&fix.metadata));
        let worker = AggCountersWorker::new(
            AggCountersKnobs {
                build_batch_rows: 10,
                verify_interval: Duration::from_secs(3600),
                first_verify_delay: Duration::ZERO,
            },
            stats.clone(),
            None,
        );
        for _ in 0..200 {
            cycle(&worker, &fix.ctx).await;
            if stats.verify_runs.load(Ordering::Relaxed) == 2 {
                cycle(&worker, &fix.ctx).await; // nothing pending; no-op
                break;
            }
            glommio::timer::sleep(Duration::from_millis(5)).await;
        }
        assert!(stats.drifted_buckets.load(Ordering::Relaxed) > 0);
        assert!(consistent(&fix.metadata), "drift repaired");
    });
}

#[test]
fn prunes_the_snapshot_change_log_up_to_the_published_sequence() {
    use brain_metadata::tables::snapshot_changes;
    glommio_run(|| async {
        let fix = build_fixture();
        let wtxn = fix.metadata.write_txn().unwrap();
        snapshot_changes::set_tracking(&wtxn, true).unwrap();
        wtxn.commit().unwrap();
        // Logged writes: through the counters, not raw.
        let wtxn = fix.metadata.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for slot in 0..30u64 {
                let r = MemoryMetadata::new_active(
                    MemoryId::pack(0, slot, 1),
                    NamespaceId::from(0),
                    SpaceId::default(),
                    SessionId(1),
                    slot,
                    1,
                    MemoryKind::Semantic,
                    [0; 16],
                    0.5,
                    1,
                    1_700_000_000_000_000_000,
                );
                agg::put_memory(&wtxn, &mut t, r.memory_id_bytes, &r).unwrap();
            }
        }
        wtxn.commit().unwrap();
        let seq = snapshot_changes::current_seq(&fix.metadata.read_txn().unwrap()).unwrap();
        let upto = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let worker = AggCountersWorker::new(
            AggCountersKnobs {
                build_batch_rows: 10,
                verify_interval: Duration::ZERO,
                ..AggCountersKnobs::default()
            },
            Arc::default(),
            None,
        )
        .with_change_log_pruning(upto.clone());
        cycle(&worker, &fix.ctx).await;
        let rtxn = fix.metadata.read_txn().unwrap();
        assert_eq!(
            snapshot_changes::changed_since(&rtxn, 0, 100)
                .unwrap()
                .unwrap()
                .len(),
            30
        );
        drop(rtxn);

        upto.store(seq, Ordering::Relaxed);
        for _ in 0..3 {
            cycle(&worker, &fix.ctx).await; // 10 per tick
        }
        let rtxn = fix.metadata.read_txn().unwrap();
        assert!(snapshot_changes::changed_since(&rtxn, 0, 100)
            .unwrap()
            .unwrap()
            .is_empty());
        assert!(!snapshot_changes::prune_due(&rtxn, seq).unwrap());
    });
}

#[test]
fn idle_ticks_do_not_commit() {
    glommio_run(|| async {
        let fix = build_fixture();
        let worker = AggCountersWorker::new(
            AggCountersKnobs {
                verify_interval: Duration::ZERO,
                ..AggCountersKnobs::default()
            },
            Arc::default(),
            None,
        );
        // Any commit rewrites redb's header, so the file must be unchanged.
        let path = fix._tempdir.path().join("metadata.redb");
        let before = std::fs::read(&path).unwrap();
        for _ in 0..20 {
            cycle(&worker, &fix.ctx).await;
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "idle ticks wrote to the DB"
        );
    });
}

fn glommio_run<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    glommio::LocalExecutorBuilder::default()
        .name("worker-test")
        .spawn(move || async move { f().await })
        .expect("spawn glommio test executor")
        .join()
        .expect("test executor join")
}
