//! Schema-ops perf bench (sub-task 19.10b).
//!
//! Spec targets per `spec/16_benchmarks_acceptance/02_latency_targets.md`
//! §2.6 at a typical 50-definition schema (operator-run on the
//! reference rig):
//!
//! - `SCHEMA_UPLOAD`   (parse + validate + persist): p50 5 ms, p99 30 ms.
//! - `SCHEMA_VALIDATE` (parse + validate only):     p50 3 ms, p99 20 ms.
//! - `SCHEMA_GET`      (by version):                p50 1 ms, p99 5 ms.
//! - `SCHEMA_LIST`     (per-namespace):             p50 2 ms, p99 10 ms.
//!
//! This bench runs at a 50-definition fixture (10 entity types +
//! 30 predicates + 10 relation types) for dev-time iteration.
//!
//! Run: `cargo bench -p brain-metadata --bench schema_ops`.

use brain_metadata::schema_store::{schema_get, schema_list, schema_upload};
use brain_metadata::MetadataDb;
use brain_protocol::schema::{parse_schema, validate, ValidatedSchema};
use criterion::{black_box, criterion_group, Criterion};
use std::fmt::Write;
use std::path::PathBuf;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

const ENTITY_TYPES: usize = 10;
const PREDICATES: usize = 30;
const RELATION_TYPES: usize = 10;

/// Renders the bench fixture (50 definitions) directly as DSL text.
/// Avoids an SDK dev-dep and keeps the bench self-contained.
fn fixture_text() -> String {
    let mut s = String::from("namespace bench\n");
    for i in 0..ENTITY_TYPES {
        write!(
            s,
            "\ndefine entity_type Entity{i:02} {{\n    attributes {{\n        label: text optional\n        score: number optional\n    }}\n}}\n"
        )
        .unwrap();
    }
    for i in 0..PREDICATES {
        let kind = match i % 3 {
            0 => "Fact",
            1 => "Preference",
            _ => "Event",
        };
        let object = if i % 4 == 0 {
            "Value<text>"
        } else {
            "Value<number>"
        };
        write!(
            s,
            "\ndefine predicate pred_{i:02} {{\n    kind: {kind}\n    object: {object}\n}}\n"
        )
        .unwrap();
    }
    for i in 0..RELATION_TYPES {
        write!(
            s,
            "\ndefine relation_type rel_{i:02} {{\n    from: Any\n    to: Any\n    cardinality: many-to-many\n}}\n"
        )
        .unwrap();
    }
    s
}

struct UploadedFixture {
    _dir: TempDir,
    db: MetadataDb,
    text: String,
    validated: ValidatedSchema,
}

fn build_uploaded_fixture() -> UploadedFixture {
    let dir = TempDir::new().expect("tempdir");
    let path: PathBuf = dir.path().join("metadata.redb");
    let mut db = MetadataDb::open(&path).expect("open");
    let text = fixture_text();
    let parsed = parse_schema(&text).expect("parse fixture");
    let validated = validate(&parsed).expect("validate fixture");
    let wtxn = db.write_txn().expect("wtxn");
    schema_upload(&wtxn, &validated, 0).expect("upload");
    wtxn.commit().expect("commit");
    UploadedFixture {
        _dir: dir,
        db,
        text,
        validated,
    }
}

// ---------------------------------------------------------------------------
// Benches.
// ---------------------------------------------------------------------------

fn bench_parse_validate(c: &mut Criterion) {
    let text = fixture_text();
    c.bench_function("schema_validate (parse + validate)", |b| {
        b.iter(|| {
            let parsed = parse_schema(black_box(&text)).expect("parse");
            let _ = validate(black_box(&parsed)).expect("validate");
        });
    });
}

fn bench_upload(c: &mut Criterion) {
    let text = fixture_text();
    c.bench_function("schema_upload (parse + validate + persist)", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().expect("tempdir");
                let path = dir.path().join("metadata.redb");
                let db = MetadataDb::open(&path).expect("open");
                (dir, db)
            },
            |(dir, mut db)| {
                let parsed = parse_schema(black_box(&text)).expect("parse");
                let validated = validate(&parsed).expect("validate");
                let wtxn = db.write_txn().expect("wtxn");
                let _ = schema_upload(&wtxn, &validated, 0).expect("upload");
                wtxn.commit().expect("commit");
                drop(dir);
            },
        );
    });
}

fn bench_get(c: &mut Criterion) {
    let f = build_uploaded_fixture();
    c.bench_function("schema_get by version", |b| {
        b.iter(|| {
            let rtxn = f.db.read_txn().expect("read_txn");
            let row = schema_get(&rtxn, "bench", 1).expect("get");
            black_box(row);
        });
    });
}

fn bench_list(c: &mut Criterion) {
    let f = build_uploaded_fixture();
    c.bench_function("schema_list per-namespace", |b| {
        b.iter(|| {
            let rtxn = f.db.read_txn().expect("read_txn");
            let rows = schema_list(&rtxn, "bench").expect("list");
            black_box(rows);
        });
    });
}

fn print_corpus_summary() {
    let f = build_uploaded_fixture();
    eprintln!(
        "schema_ops bench setup: items={} text_bytes={} validator_v={}",
        f.validated.as_schema().items.len(),
        f.text.len(),
        1
    );
}

criterion_group!(
    name = schema_ops_benches;
    config = Criterion::default();
    targets = bench_parse_validate, bench_upload, bench_get, bench_list
);

fn main() {
    print_corpus_summary();
    schema_ops_benches();
    Criterion::default().configure_from_args().final_summary();
}
