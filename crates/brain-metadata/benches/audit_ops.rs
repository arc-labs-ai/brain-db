//! Audit-ops perf bench.
//!
//! Latency targets at a single dispatch over a 4 KiB memory:
//!
//! - Audit-row write (primary + 3 indexes, single wtxn): p50 200 µs, p99 1 ms.
//! - `audit_by_memory` (limit 100): p50 500 µs, p99 2 ms.
//! - `audit_by_extractor` (limit 100): p50 500 µs, p99 2 ms.
//!
//! Run: `cargo bench -p brain-metadata --bench audit_ops`.

use brain_core::{AuditId, MemoryId};
use brain_metadata::audit::ops::{audit_by_extractor, audit_by_memory, audit_write};
use brain_metadata::tables::audit::{output_kind, ExtractionAudit, OutputRef};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, Criterion};
use tempfile::TempDir;

const SEED_ROWS: u64 = 4_096;

fn open_db(dir: &TempDir) -> MetadataDb {
    MetadataDb::open(dir.path().join("audit.redb")).expect("open")
}

fn success_row(memory: MemoryId, extractor_id: u32, started_at: u64) -> ExtractionAudit {
    ExtractionAudit::success(
        AuditId::new(),
        memory,
        extractor_id,
        1,
        1,
        started_at,
        started_at + 100,
        vec![OutputRef {
            kind: output_kind::ENTITY,
            id: [1u8; 16],
        }],
        [0u8; 32],
    )
}

fn build_seeded_fixture() -> (TempDir, MetadataDb, MemoryId, u32) {
    let dir = TempDir::new().expect("tmp");
    let db = open_db(&dir);
    let target_memory = MemoryId::pack(1, 0, 42);
    let target_extractor: u32 = 1;
    let wtxn = db.write_txn().expect("wtxn");
    for i in 0..SEED_ROWS {
        let mem = if i % 8 == 0 {
            target_memory
        } else {
            MemoryId::pack(1, 0, i as u32)
        };
        let ext = if i % 4 == 0 {
            target_extractor
        } else {
            ((i % 16) as u32).saturating_add(2)
        };
        let row = success_row(mem, ext, 1_000 + i);
        audit_write(&wtxn, &row).expect("write");
    }
    wtxn.commit().expect("commit");
    (dir, db, target_memory, target_extractor)
}

fn bench_audit_write(c: &mut Criterion) {
    c.bench_function("audit_write (primary + 3 indexes)", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().expect("tmp");
                let db = open_db(&dir);
                (dir, db)
            },
            |(dir, db)| {
                let row = success_row(MemoryId::pack(1, 0, 0), 1, 1_000);
                let wtxn = db.write_txn().expect("wtxn");
                audit_write(&wtxn, &row).expect("write");
                wtxn.commit().expect("commit");
                drop(dir);
            },
        );
    });
}

fn bench_audit_by_memory(c: &mut Criterion) {
    let (_dir, db, memory, _) = build_seeded_fixture();
    c.bench_function("audit_by_memory (limit 100)", |b| {
        b.iter(|| {
            let rtxn = db.read_txn().expect("rtxn");
            let rows = audit_by_memory(&rtxn, memory, 100).expect("by_memory");
            black_box(rows);
        });
    });
}

fn bench_audit_by_extractor(c: &mut Criterion) {
    let (_dir, db, _, ext) = build_seeded_fixture();
    c.bench_function("audit_by_extractor (limit 100)", |b| {
        b.iter(|| {
            let rtxn = db.read_txn().expect("rtxn");
            let rows = audit_by_extractor(&rtxn, ext, 100).expect("by_extractor");
            black_box(rows);
        });
    });
}

fn print_corpus_summary() {
    let (_dir, db, memory, ext) = build_seeded_fixture();
    let rtxn = db.read_txn().expect("rtxn");
    let by_mem = audit_by_memory(&rtxn, memory, 1_000).expect("by_memory");
    let by_ext = audit_by_extractor(&rtxn, ext, 1_000).expect("by_extractor");
    eprintln!(
        "audit_ops bench setup: seeded={} per_memory_hits={} per_extractor_hits={}",
        SEED_ROWS,
        by_mem.len(),
        by_ext.len()
    );
}

criterion_group!(
    name = audit_ops_benches;
    config = Criterion::default();
    targets = bench_audit_write, bench_audit_by_memory, bench_audit_by_extractor
);

fn main() {
    print_corpus_summary();
    audit_ops_benches();
    Criterion::default().configure_from_args().final_summary();
}
