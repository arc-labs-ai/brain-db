//! Statement-ops perf bench (sub-task 17.10b).
//!
//! Spec targets per [`spec/16_benchmarks_acceptance/02_latency_targets.md`](
//! ../../spec/16_benchmarks_acceptance/02_latency_targets.md) §2.3 at
//! 1M statements per shard (operator-run on the 16-core / 64 GB /
//! NVMe reference rig):
//!
//! - `STATEMENT_CREATE` (Fact, 3 evidence): p50 2 ms, p99 10 ms.
//! - `STATEMENT_GET`:                       p50 0.5 ms, p99 2 ms.
//! - `STATEMENT_SUPERSEDE` (explicit):      p50 3 ms, p99 15 ms.
//! - `STATEMENT_LIST` (subject + predicate
//!   filter, current_only):                 p50 2 ms, p99 10 ms.
//!
//! This bench runs at 1024-statement scale for dev-time iteration;
//! full-corpus numbers are operator-run.
//!
//! Run: `cargo bench -p brain-metadata --bench statement_ops`.

use brain_core::knowledge::{StatementObject, SubjectRef};
use brain_core::{
    Entity, EntityId, EntityType, EntityTypeId, ExtractorId, PredicateId, StatementId,
    StatementKind,
};
use brain_metadata::entity_ops::{entity_put, normalize_name};
use brain_metadata::statement_ops::{
    statement_create, statement_get, statement_list, statement_supersede, StatementListFilter,
};
use brain_metadata::tables::knowledge::predicate::{PREDICATES_BY_QNAME_TABLE, PREDICATES_TABLE};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, Criterion};
use tempfile::TempDir;

const N_STATEMENTS: usize = 1024;
const PERSON: EntityTypeId = EntityType::PERSON_ID;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    db: MetadataDb,
    // (subject, predicate) per pre-seeded statement; queries draw from
    // these for deterministic hit rate.
    seeded: Vec<(EntityId, PredicateId, StatementId)>,
    _dir: TempDir,
}

fn build_fixture(n: usize) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let mut db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open db");
    let now = 1_700_000_000_000_000_000u64;

    // Resolve the built-in `brain:related_to` predicate id (seeded
    // at MetadataDb::open).
    let related_to: PredicateId = {
        let rtxn = db.read_txn().expect("read_txn");
        let idx = rtxn
            .open_table(PREDICATES_BY_QNAME_TABLE)
            .expect("open by_qname");
        let raw: u32 = idx
            .get("brain:related_to")
            .expect("get qname")
            .expect("brain:related_to seeded")
            .value();
        PredicateId::from(raw)
    };

    // Sanity: predicate exists in the primary table too.
    {
        let rtxn = db.read_txn().expect("read_txn");
        let t = rtxn.open_table(PREDICATES_TABLE).expect("open predicates");
        assert!(t.get(&related_to.raw()).expect("get").is_some());
    }

    // Pre-seed N_STATEMENTS Fact rows. Each Fact has subject = a
    // freshly-minted Person, object = another freshly-minted Person.
    let mut seeded = Vec::with_capacity(n);
    {
        let wtxn = db.write_txn().expect("write_txn");
        for i in 0..n {
            let subj_id = EntityId::new();
            let subj_name = format!("subj_{i}");
            let subj = Entity::new_active(
                subj_id,
                PERSON,
                subj_name.clone(),
                normalize_name(&subj_name),
                now,
            );
            entity_put(&wtxn, &subj).expect("subj entity_put");

            let obj_id = EntityId::new();
            let obj_name = format!("obj_{i}");
            let obj = Entity::new_active(
                obj_id,
                PERSON,
                obj_name.clone(),
                normalize_name(&obj_name),
                now,
            );
            entity_put(&wtxn, &obj).expect("obj entity_put");

            let stmt_id = StatementId::new();
            let s = brain_core::knowledge::Statement::new_root(
                stmt_id,
                StatementKind::Fact,
                SubjectRef::Entity(subj_id),
                related_to,
                StatementObject::Entity(obj_id),
                0.9,
                brain_core::knowledge::EvidenceRef::default(),
                ExtractorId::from(0),
                now,
                1,
            );
            statement_create(&wtxn, &s, now).expect("statement_create");
            seeded.push((subj_id, related_to, stmt_id));
        }
        wtxn.commit().expect("commit");
    }

    Fixture {
        db,
        seeded,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// statement_create — Fact (no evidence; wire shape).
// ---------------------------------------------------------------------------

fn bench_statement_create_fact(c: &mut Criterion) {
    let mut fixture = build_fixture(N_STATEMENTS);
    let now = 1_700_000_000_000_000_001u64;
    // Pre-allocate two pools of unused entities so each create has
    // valid subject/object.
    let extra_subjects: Vec<EntityId> = (0..1024).map(|_| EntityId::new()).collect();
    let extra_objects: Vec<EntityId> = (0..1024).map(|_| EntityId::new()).collect();
    {
        let wtxn = fixture.db.write_txn().expect("write_txn");
        for (i, id) in extra_subjects
            .iter()
            .chain(extra_objects.iter())
            .enumerate()
        {
            let name = format!("xfix_{i}");
            let e = Entity::new_active(*id, PERSON, name.clone(), normalize_name(&name), now);
            entity_put(&wtxn, &e).expect("entity_put");
        }
        wtxn.commit().expect("commit");
    }
    let related_to = fixture.seeded[0].1;
    let mut idx = 0usize;

    c.bench_function("statement_ops::create_fact", |b| {
        b.iter(|| {
            let i = idx % extra_subjects.len();
            idx = idx.wrapping_add(1);
            let s = brain_core::knowledge::Statement::new_root(
                StatementId::new(),
                StatementKind::Fact,
                SubjectRef::Entity(extra_subjects[i]),
                related_to,
                StatementObject::Entity(extra_objects[i]),
                0.9,
                brain_core::knowledge::EvidenceRef::default(),
                ExtractorId::from(0),
                now,
                1,
            );
            let wtxn = fixture.db.write_txn().expect("write_txn");
            statement_create(&wtxn, black_box(&s), now).expect("create");
            wtxn.commit().expect("commit");
        });
    });
}

// ---------------------------------------------------------------------------
// statement_get — point lookup.
// ---------------------------------------------------------------------------

fn bench_statement_get(c: &mut Criterion) {
    let fixture = build_fixture(N_STATEMENTS);
    let mut idx = 0usize;

    c.bench_function("statement_ops::get", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let id = fixture.seeded[idx % fixture.seeded.len()].2;
            idx = idx.wrapping_add(1);
            let s = statement_get(&rtxn, black_box(id)).expect("get");
            black_box(s);
        });
    });
}

// ---------------------------------------------------------------------------
// statement_list — subject + predicate + current_only.
// ---------------------------------------------------------------------------

fn bench_statement_list_subject_predicate(c: &mut Criterion) {
    let fixture = build_fixture(N_STATEMENTS);
    let mut idx = 0usize;

    c.bench_function("statement_ops::list_subject_predicate_current", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let (subj, pred, _) = fixture.seeded[idx % fixture.seeded.len()];
            idx = idx.wrapping_add(1);
            let filter = StatementListFilter {
                subject: Some(subj),
                predicate: Some(pred),
                kind: Some(StatementKind::Fact),
                current_only: true,
                min_confidence: None,
                limit: 10,
            };
            let rows = statement_list(&rtxn, black_box(&filter)).expect("list");
            black_box(rows);
        });
    });
}

// ---------------------------------------------------------------------------
// statement_supersede — explicit, one chain step at a time.
// ---------------------------------------------------------------------------

fn bench_statement_supersede(c: &mut Criterion) {
    let mut fixture = build_fixture(N_STATEMENTS);
    let related_to = fixture.seeded[0].1;
    let now = 1_700_000_000_000_000_002u64;
    let mut idx = 0usize;
    // Track the current head of each chain so successive supersedes
    // extend the same chain (more realistic than always superseding
    // the original).
    let mut heads: Vec<(EntityId, EntityId, StatementId)> = fixture
        .seeded
        .iter()
        .take(256)
        .map(|(subj, _pred, sid)| (*subj, *subj, *sid))
        .collect();

    c.bench_function("statement_ops::supersede", |b| {
        b.iter(|| {
            let i = idx % heads.len();
            idx = idx.wrapping_add(1);
            let (subj, obj, old_id) = heads[i];
            let new_id = StatementId::new();
            let new_stmt = brain_core::knowledge::Statement::new_root(
                new_id,
                StatementKind::Fact,
                SubjectRef::Entity(subj),
                related_to,
                StatementObject::Entity(obj),
                0.92,
                brain_core::knowledge::EvidenceRef::default(),
                ExtractorId::from(0),
                now,
                1,
            );
            let wtxn = fixture.db.write_txn().expect("write_txn");
            let written =
                statement_supersede(&wtxn, old_id, black_box(&new_stmt), now).expect("supersede");
            wtxn.commit().expect("commit");
            heads[i].2 = written;
        });
    });
}

// ---------------------------------------------------------------------------
// Setup-time corpus sanity print.
// ---------------------------------------------------------------------------

fn print_corpus_summary() {
    let fixture = build_fixture(64);
    let rtxn = fixture.db.read_txn().expect("read_txn");
    let any = statement_get(&rtxn, fixture.seeded[0].2)
        .expect("get")
        .is_some();
    eprintln!(
        "statement_ops bench setup: seeded={} sanity_get={}",
        fixture.seeded.len(),
        any
    );
}

criterion_group!(
    name = statement_ops_benches;
    config = Criterion::default();
    targets =
        bench_statement_create_fact,
        bench_statement_get,
        bench_statement_list_subject_predicate,
        bench_statement_supersede
);

fn main() {
    print_corpus_summary();
    statement_ops_benches();
    Criterion::default().configure_from_args().final_summary();
}
