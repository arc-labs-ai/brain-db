//! One-shard Tier 1 / Tier 2 benchmark: export cost and scan latency.
//!
//! Seeds `BRAIN_BENCH_ROWS` memories (default 1 000 000) into one shard's
//! metadata DB, spread over 4 namespaces x 8 spaces x 30 days, exports a
//! snapshot, then times typical scan queries. Prints results directly
//! (no criterion): an export at this size is seconds, and the numbers
//! are meant to be extrapolated, not micro-compared.
//!
//! Run: `cargo bench -p brain-analytics --bench scan`
//!      `BRAIN_BENCH_ROWS=5000000 BRAIN_BENCH_THREADS=4 cargo bench -p brain-analytics --bench scan`

use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_analytics::{Analytics, AnalyticsConfig, ScanQuery};
use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
use brain_metadata::tables::agg::put_memory;
use brain_metadata::tables::memory::{
    space_timeline_key, MemoryMetadata, MEMORIES_BY_SPACE_TIMELINE_TABLE, MEMORIES_TABLE,
};
use brain_metadata::MetadataDb;
use redb::ReadableTable;

const DAY: u64 = 86_400_000_000_000;
const T0: u64 = 1_700_000_000_000_000_000;

fn space(b: u8) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[15] = b;
    s
}

fn seed(db: &MetadataDb, rows: u64) {
    let kinds = [
        MemoryKind::Episodic,
        MemoryKind::Semantic,
        MemoryKind::Consolidated,
    ];
    let mut x: u64 = 1;
    let mut rnd = |n: u64| {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (x >> 33) % n
    };
    for chunk in (0..rows).collect::<Vec<_>>().chunks(100_000) {
        let wtxn = db.write_txn().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let mut tl = wtxn.open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE).unwrap();
            for &slot in chunk {
                let r = MemoryMetadata::new_active(
                    MemoryId::pack(0, slot, 1),
                    NamespaceId::from(rnd(4) as u32),
                    SpaceId::from(space(rnd(8) as u8)),
                    SessionId(rnd(100)),
                    slot,
                    1,
                    kinds[rnd(3) as usize],
                    [0; 16],
                    rnd(1000) as f32 / 1000.0,
                    64,
                    T0 + rnd(30 * DAY),
                );
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
                let k = space_timeline_key(
                    r.namespace_id,
                    r.space_id_bytes,
                    r.created_at_unix_nanos,
                    r.session_id,
                    r.memory_id_bytes,
                );
                tl.insert(k.as_slice(), ()).unwrap();
            }
        }
        wtxn.commit().unwrap();
    }
}

fn time<T>(label: &str, runs: u32, mut f: impl FnMut() -> T) {
    let _ = f(); // warm the page cache
    let mut best = Duration::MAX;
    let mut total = Duration::ZERO;
    for _ in 0..runs {
        let t = Instant::now();
        let _ = f();
        let e = t.elapsed();
        best = best.min(e);
        total += e;
    }
    println!(
        "  {label:<58} best {:>8.2} ms   mean {:>8.2} ms",
        best.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e3 / f64::from(runs)
    );
}

fn main() {
    let rows: u64 = std::env::var("BRAIN_BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());

    let t = Instant::now();
    seed(&db, rows);
    println!(
        "seeded {rows} memories in {:.1} s",
        t.elapsed().as_secs_f64()
    );

    let a = Analytics::start(
        db.clone(),
        dir.path().join("analytics"),
        AnalyticsConfig {
            export_interval: Duration::from_secs(86_400),
            threads: std::env::var("BRAIN_BENCH_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            ..AnalyticsConfig::default()
        },
    )
    .unwrap();
    let snap = a.export_now().unwrap();
    println!(
        "export: {} rows, {:.1} MiB, {} ms ({:.0} rows/s, {:.1} B/row)",
        snap.rows,
        snap.bytes as f64 / (1 << 20) as f64,
        snap.elapsed_ms,
        snap.rows as f64 / (snap.elapsed_ms.max(1) as f64 / 1e3),
        snap.bytes as f64 / snap.rows.max(1) as f64
    );

    let base = ScanQuery {
        namespace_id: 1,
        space_id: space(3),
        ..ScanQuery::default()
    };
    let run = |q: ScanQuery| futures_lite::future::block_on(a.query(q)).unwrap();
    let queries = |title: &str| {
        println!("queries, {title} (one space = ~1/32 of rows):");
        time("MIN/MAX/AVG salience, one space", 10, || run(base.clone()));
        time("salience >= 0.8, one space, by kind", 10, || {
            run(ScanQuery {
                salience_min: Some(0.8),
                group_by_kind: true,
                ..base.clone()
            })
        });
        time("last 7 days, one space, by day", 10, || {
            run(ScanQuery {
                created_from_unix_nanos: Some(T0 + 23 * DAY),
                group_by_day: true,
                ..base.clone()
            })
        });
        time(
            "all statuses, one space, by kind x status x day",
            10,
            || {
                run(ScanQuery {
                    statuses: vec![0, 1, 2],
                    group_by_kind: true,
                    group_by_status: true,
                    group_by_day: true,
                    ..base.clone()
                })
            },
        );
    };
    queries("full snapshot");

    // Change 1% of memories (boost: re-anchor), then refresh incrementally.
    let changed = rows / 100;
    let t = Instant::now();
    let wtxn = db.write_txn().unwrap();
    {
        let batch = brain_metadata::tables::agg::begin_batch(&wtxn);
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for i in 0..changed {
                let id = MemoryId::pack(0, i * 97 % rows, 1).to_be_bytes();
                let Some(mut r) = mem.get(&id).unwrap().map(|g| g.value()) else {
                    continue;
                };
                let now = T0 + 40 * DAY;
                r.set_salience((r.salience_at(now) * 1.1).min(1.0), now);
                r.access_count += 1;
                put_memory(&wtxn, &mut mem, id, &r).unwrap();
            }
        }
        batch.finish(&wtxn).unwrap();
    }
    wtxn.commit().unwrap();
    println!(
        "changed {changed} memories in {:.1} s",
        t.elapsed().as_secs_f64()
    );
    let delta = a.refresh_now().unwrap();
    println!(
        "delta export: {} rows, {:.1} MiB, {} ms (vs {} ms full)",
        delta.rows,
        delta.bytes as f64 / (1 << 20) as f64,
        delta.elapsed_ms,
        snap.elapsed_ms
    );
    queries("base + 1% delta");

    // Change 15% more: the next refresh compacts (Parquet only).
    let wtxn = db.write_txn().unwrap();
    {
        let batch = brain_metadata::tables::agg::begin_batch(&wtxn);
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for i in 0..rows * 15 / 100 {
                let id = MemoryId::pack(0, (i * 7 + 3) % rows, 1).to_be_bytes();
                let Some(mut r) = mem.get(&id).unwrap().map(|g| g.value()) else {
                    continue;
                };
                r.access_count += 1;
                put_memory(&wtxn, &mut mem, id, &r).unwrap();
            }
        }
        batch.finish(&wtxn).unwrap();
    }
    wtxn.commit().unwrap();
    let compacted = a.refresh_now().unwrap();
    println!(
        "compaction (16% changed, Parquet only): {} rows, {:.1} MiB, {} ms (vs {} ms full export)",
        compacted.rows,
        compacted.bytes as f64 / (1 << 20) as f64,
        compacted.elapsed_ms,
        snap.elapsed_ms
    );
    queries("compacted snapshot");
}
