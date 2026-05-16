//! Relation-ops perf bench (sub-task 18.9b).
//!
//! Spec targets per [`spec/16_benchmarks_acceptance/02_latency_targets.md`](
//! ../../spec/16_benchmarks_acceptance/02_latency_targets.md) §2.4 at
//! 1M relations per shard (operator-run on the reference rig):
//!
//! - `RELATION_CREATE`:          p50 3 ms, p99 15 ms.
//! - `RELATION_GET`:              p50 0.5 ms, p99 2 ms.
//! - `RELATION_LIST_FROM`:        p50 2 ms, p99 10 ms.
//! - `RELATION_TRAVERSE` depth-1: p50 5 ms, p99 25 ms.
//!
//! This bench runs at 1024-relation scale for dev-time iteration.
//!
//! Run: `cargo bench -p brain-metadata --bench relation_ops`.

use brain_core::knowledge::{Entity, EntityType, Relation};
use brain_core::{EntityId, ExtractorId, RelationId, RelationTypeId};
use brain_metadata::entity_ops::{entity_put, normalize_name};
use brain_metadata::relation_ops::{
    relation_create, relation_get, relation_list_from, RelationListFilter,
};
use brain_metadata::relation_traversal::{traverse, TraversalConfig, TraversalDirection};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, Criterion};
use tempfile::TempDir;

const N_RELATIONS: usize = 1024;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    db: MetadataDb,
    seeded: Vec<(EntityId, RelationId)>,
    relation_type: RelationTypeId,
    _dir: TempDir,
}

fn build_fixture(n: usize) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let mut db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open db");
    let now = 1_700_000_000_000_000_000u64;

    // Resolve the built-in `brain:related_to` relation type id
    // (seeded at MetadataDb::open in 18.3).
    let related_to: RelationTypeId = {
        let rtxn = db.read_txn().expect("read_txn");
        let rt = brain_metadata::relation_type_lookup_by_qname(&rtxn, "brain", "related_to")
            .expect("lookup")
            .expect("seeded");
        rt.id
    };

    // Pre-seed: each iteration creates a fresh "subject" + "object"
    // entity and a relation between them.
    let mut seeded = Vec::with_capacity(n);
    {
        let wtxn = db.write_txn().expect("write_txn");
        for i in 0..n {
            let subj_id = EntityId::new();
            let subj_name = format!("rsubj_{i}");
            let subj = Entity::new_active(
                subj_id,
                EntityType::PERSON_ID,
                subj_name.clone(),
                normalize_name(&subj_name),
                now,
            );
            entity_put(&wtxn, &subj).expect("subj entity_put");

            let obj_id = EntityId::new();
            let obj_name = format!("robj_{i}");
            let obj = Entity::new_active(
                obj_id,
                EntityType::PERSON_ID,
                obj_name.clone(),
                normalize_name(&obj_name),
                now,
            );
            entity_put(&wtxn, &obj).expect("obj entity_put");

            let rel_id = RelationId::new();
            let r = Relation::new_root(
                rel_id,
                related_to,
                subj_id,
                obj_id,
                0.9,
                vec![],
                ExtractorId::from(0),
                now,
                /* symmetric */ false,
            );
            relation_create(&wtxn, &r, now).expect("relation_create");
            seeded.push((subj_id, rel_id));
        }
        wtxn.commit().expect("commit");
    }

    Fixture {
        db,
        seeded,
        relation_type: related_to,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// Benches.
// ---------------------------------------------------------------------------

fn bench_relation_create(c: &mut Criterion) {
    let mut fixture = build_fixture(N_RELATIONS);
    let now = 1_700_000_000_000_000_001u64;

    // Pre-allocate fresh entity pairs so each create has unique
    // endpoints (and doesn't trigger cardinality auto-supersede).
    let extras: Vec<(EntityId, EntityId)> = (0..1024)
        .map(|_| (EntityId::new(), EntityId::new()))
        .collect();
    {
        let wtxn = fixture.db.write_txn().expect("write_txn");
        for (i, (s, o)) in extras.iter().enumerate() {
            let s_name = format!("crsubj_{i}");
            let o_name = format!("crobj_{i}");
            let se = Entity::new_active(*s, EntityType::PERSON_ID, s_name.clone(), normalize_name(&s_name), now);
            let oe = Entity::new_active(*o, EntityType::PERSON_ID, o_name.clone(), normalize_name(&o_name), now);
            entity_put(&wtxn, &se).expect("se");
            entity_put(&wtxn, &oe).expect("oe");
        }
        wtxn.commit().expect("commit");
    }

    let rt = fixture.relation_type;
    let mut idx = 0usize;
    c.bench_function("relation_ops::create", |b| {
        b.iter(|| {
            let (s, o) = extras[idx % extras.len()];
            idx = idx.wrapping_add(1);
            let r = Relation::new_root(
                RelationId::new(),
                rt,
                s,
                o,
                0.9,
                vec![],
                ExtractorId::from(0),
                now,
                false,
            );
            let wtxn = fixture.db.write_txn().expect("write_txn");
            relation_create(&wtxn, black_box(&r), now).expect("create");
            wtxn.commit().expect("commit");
        });
    });
}

fn bench_relation_get(c: &mut Criterion) {
    let fixture = build_fixture(N_RELATIONS);
    let mut idx = 0usize;
    c.bench_function("relation_ops::get", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let id = fixture.seeded[idx % fixture.seeded.len()].1;
            idx = idx.wrapping_add(1);
            let r = relation_get(&rtxn, black_box(id)).expect("get");
            black_box(r);
        });
    });
}

fn bench_relation_list_from(c: &mut Criterion) {
    let fixture = build_fixture(N_RELATIONS);
    let mut idx = 0usize;
    c.bench_function("relation_ops::list_from_subject_filter", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let (subj, _) = fixture.seeded[idx % fixture.seeded.len()];
            idx = idx.wrapping_add(1);
            let filter = RelationListFilter {
                relation_type: Some(fixture.relation_type),
                current_only: true,
                limit: 10,
            };
            let rows =
                relation_list_from(&rtxn, subj, black_box(&filter)).expect("list_from");
            black_box(rows);
        });
    });
}

fn bench_relation_traverse_depth_1(c: &mut Criterion) {
    let fixture = build_fixture(N_RELATIONS);
    let mut idx = 0usize;
    let config = TraversalConfig {
        max_depth: 1,
        max_branching_factor: 1000,
        current_only: true,
    };
    c.bench_function("relation_ops::traverse_depth_1", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let (subj, _) = fixture.seeded[idx % fixture.seeded.len()];
            idx = idx.wrapping_add(1);
            let paths = traverse(
                &rtxn,
                subj,
                &[fixture.relation_type],
                TraversalDirection::Outgoing,
                black_box(&config),
            )
            .expect("traverse");
            black_box(paths);
        });
    });
}

// ---------------------------------------------------------------------------
// Setup sanity print.
// ---------------------------------------------------------------------------

fn print_corpus_summary() {
    let fixture = build_fixture(64);
    let rtxn = fixture.db.read_txn().expect("read_txn");
    let any = relation_get(&rtxn, fixture.seeded[0].1).expect("get").is_some();
    eprintln!(
        "relation_ops bench setup: seeded={} sanity_get={}",
        fixture.seeded.len(),
        any
    );
}

criterion_group!(
    name = relation_ops_benches;
    config = Criterion::default();
    targets =
        bench_relation_create,
        bench_relation_get,
        bench_relation_list_from,
        bench_relation_traverse_depth_1
);

fn main() {
    print_corpus_summary();
    relation_ops_benches();
    Criterion::default().configure_from_args().final_summary();
}
