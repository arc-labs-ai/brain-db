//! Aggregate-counter overhead bench.
//!
//! Measures what the counters add to the memory write path, and what an
//! aggregate query costs:
//!
//! - 1 000 new-memory inserts in one txn: raw `insert` vs `put_memory`,
//!   immediate and inside a txn batch (as the writer runs it).
//! - 1 000 salience boosts in one txn (the access-boost shape): raw vs
//!   `put_memory_deferred` + one `flush`.
//! - `agg::query` for one `(namespace, space)` grouped by kind.
//!
//! Run: `cargo bench -p brain-metadata --bench agg_ops`.

use std::cell::Cell;

use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
use brain_metadata::tables::agg::{
    self, put_memory, put_memory_deferred, AggDeltas, AggFilter, AggGroupBy,
};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, Criterion};
use redb::ReadableTable;
use tempfile::TempDir;

const BATCH: u64 = 1_000;
const SEED: u64 = 20_000;

fn row(slot: u64) -> MemoryMetadata {
    let mut sp = [0u8; 16];
    sp[15] = (slot % 8) as u8;
    let kind = [
        MemoryKind::Episodic,
        MemoryKind::Semantic,
        MemoryKind::Consolidated,
    ][(slot % 3) as usize];
    MemoryMetadata::new_active(
        MemoryId::pack(0, slot, 1),
        NamespaceId::from((slot % 4) as u32),
        SpaceId::from(sp),
        SessionId(1),
        slot,
        1,
        kind,
        [0; 16],
        (slot % 1000) as f32 / 1000.0,
        64,
        1_700_000_000_000_000_000,
    )
}

fn seeded() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().expect("tmp");
    let db = MetadataDb::open(dir.path().join("agg.redb")).expect("open");
    let wtxn = db.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
        for slot in 0..SEED {
            let r = row(slot);
            put_memory(&wtxn, &mut t, r.memory_id_bytes, &r).expect("put");
        }
    }
    wtxn.commit().expect("commit");
    (dir, db)
}

fn bench_insert(c: &mut Criterion) {
    let (_dir, db) = seeded();
    let next = Cell::new(SEED);
    let mut g = c.benchmark_group("insert 1000 new memories (one txn)");
    g.bench_function("raw insert", |b| {
        b.iter(|| {
            let wtxn = db.write_txn().expect("wtxn");
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
                for _ in 0..BATCH {
                    let r = row(next.replace(next.get() + 1));
                    t.insert(&r.memory_id_bytes, &r).expect("insert");
                }
            }
            wtxn.commit().expect("commit");
        });
    });
    g.bench_function("put_memory, one txn batch (with counters)", |b| {
        b.iter(|| {
            let wtxn = db.write_txn().expect("wtxn");
            let batch = agg::begin_batch(&wtxn);
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
                for _ in 0..BATCH {
                    let r = row(next.replace(next.get() + 1));
                    put_memory(&wtxn, &mut t, r.memory_id_bytes, &r).expect("put");
                }
            }
            batch.finish(&wtxn).expect("finish");
            wtxn.commit().expect("commit");
        });
    });
    g.bench_function("put_memory (with counters)", |b| {
        b.iter(|| {
            let wtxn = db.write_txn().expect("wtxn");
            {
                let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
                for _ in 0..BATCH {
                    let r = row(next.replace(next.get() + 1));
                    put_memory(&wtxn, &mut t, r.memory_id_bytes, &r).expect("put");
                }
            }
            wtxn.commit().expect("commit");
        });
    });
    g.finish();
}

fn bench_salience_batch(c: &mut Criterion) {
    let (_dir, db) = seeded();
    let mut g = c.benchmark_group("boost salience on 1000 memories (one txn)");
    let update = |deferred: bool| {
        let wtxn = db.write_txn().expect("wtxn");
        let mut deltas = AggDeltas::default();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
            for slot in 0..BATCH {
                let key = MemoryId::pack(0, slot, 1).to_be_bytes();
                let mut m = t.get(&key).expect("get").expect("row").value();
                let now = 1_700_000_100_000_000_000;
                m.set_salience((m.salience_at(now) * 1.1).min(1.0), now);
                if deferred {
                    put_memory_deferred(&mut t, key, &m, &mut deltas).expect("put");
                } else {
                    t.insert(&key, &m).expect("insert");
                }
            }
        }
        deltas.flush(&wtxn).expect("flush");
        wtxn.commit().expect("commit");
    };
    g.bench_function("raw insert", |b| b.iter(|| update(false)));
    g.bench_function("deferred + flush (with counters)", |b| {
        b.iter(|| update(true))
    });
    g.finish();
}

fn bench_query(c: &mut Criterion) {
    let (_dir, db) = seeded();
    let mut sp = [0u8; 16];
    sp[15] = 1;
    let filter = AggFilter {
        namespace_id: Some(1),
        space_id: Some(sp),
        ..AggFilter::default()
    };
    let group_by = AggGroupBy {
        kind: true,
        ..AggGroupBy::default()
    };
    c.bench_function("agg::query (one space, by kind)", |b| {
        b.iter(|| {
            let rtxn = db.read_txn().expect("rtxn");
            black_box(
                agg::query(&rtxn, &filter, group_by, 1_700_000_100_000_000_000).expect("query"),
            );
        });
    });
}

criterion_group!(
    name = agg_ops_benches;
    config = Criterion::default().sample_size(20);
    targets = bench_insert, bench_salience_batch, bench_query
);

fn main() {
    agg_ops_benches();
    Criterion::default().configure_from_args().final_summary();
}
