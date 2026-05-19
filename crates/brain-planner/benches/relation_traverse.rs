//! Performance gate for the unified edge layer (Phase C).
//!
//! Validates that collapsing the substrate `edges_out`/`edges_in`
//! tables and the typed-relation `relations`/`by_from`/`by_to` tables
//! into one `EDGES_TABLE` + sidecar did not regress the
//! `RELATION_TRAVERSE` latency below spec §16/02 §2.4 targets:
//!
//! - depth=1 : p50 ≤ 5  ms
//! - depth=2 : p50 ≤ 15 ms
//! - depth=3 : p50 ≤ 30 ms
//!
//! Also validates that the substrate-only memory-anchored walk
//! (`walk_memory_edges` from Phase A) has not regressed under the
//! same unification.
//!
//! Fixture:
//! - 1000 entities + 5000 typed relations across 3 distinct relation
//!   types (random graph).
//! - 1000 memories + 5000 substrate edges of 4 distinct EdgeKinds
//!   (random graph).
//!
//! Run: `cargo bench -p brain-planner --bench relation_traverse`.

use std::time::{Duration, Instant};

use brain_core::knowledge::{Entity, EntityType, Relation};
use brain_core::{
    EdgeKind, EdgeKindRef, EntityId, ExtractorId, MemoryId, NodeRef, RelationId, RelationTypeId,
};
use brain_metadata::entity_ops::{entity_put, normalize_name};
use brain_metadata::relation_ops::relation_create;
use brain_metadata::relation_traversal::{traverse, TraversalConfig, TraversalDirection};
use brain_metadata::tables::edge::{
    derived_by, link, origin, walk_outgoing, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE,
    EDGES_TABLE,
};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

const N_ENTITIES: usize = 1000;
const N_TYPED_RELATIONS: usize = 5000;
const N_MEMORIES: usize = 1000;
const N_SUBSTRATE_EDGES: usize = 5000;

// Spec §16/02 §2.4 p50 targets in milliseconds.
const P50_TARGET_MS_D1: f64 = 5.0;
const P50_TARGET_MS_D2: f64 = 15.0;
const P50_TARGET_MS_D3: f64 = 30.0;
const P50_TARGET_MS_SUBSTRATE: f64 = 5.0; // Substrate walk should match depth=1 target.

const T0: u64 = 1_700_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

struct EntityFixture {
    db: MetadataDb,
    entities: Vec<EntityId>,
    relation_types: [RelationTypeId; 3],
    _dir: TempDir,
}

fn build_entity_fixture() -> EntityFixture {
    let dir = TempDir::new().expect("tempdir");
    let mut db = MetadataDb::open(dir.path().join("md.redb")).expect("open db");

    // Use the seeded `brain:related_to` plus two ad-hoc types.
    let related_to: RelationTypeId = {
        let rtxn = db.read_txn().expect("read_txn");
        brain_metadata::relation_type_lookup_by_qname(&rtxn, "brain", "related_to")
            .expect("lookup")
            .expect("seeded")
            .id
    };

    // Intern two more relation types so the bench exercises type
    // diversity (mimics a real schema with several predicates).
    use brain_metadata::relation_type_ops::relation_type_intern_or_get;
    let (rt_b, rt_c) = {
        let wtxn = db.write_txn().expect("write_txn");
        let b = relation_type_intern_or_get(&wtxn, "brain", "follows", 0, T0).expect("intern_b");
        let c = relation_type_intern_or_get(&wtxn, "brain", "cites", 0, T0).expect("intern_c");
        wtxn.commit().expect("commit");
        (b, c)
    };
    let relation_types = [related_to, rt_b, rt_c];

    // Seed entities.
    let mut entities = Vec::with_capacity(N_ENTITIES);
    {
        let wtxn = db.write_txn().expect("write_txn");
        for i in 0..N_ENTITIES {
            let id = EntityId::new();
            let name = format!("e_{i}");
            let e = Entity::new_active(
                id,
                EntityType::PERSON_ID,
                name.clone(),
                normalize_name(&name),
                T0,
            );
            entity_put(&wtxn, &e).expect("entity_put");
            entities.push(id);
        }
        wtxn.commit().expect("commit");
    }

    // Seed relations using a deterministic linear-congruential walk
    // so the graph topology is reproducible across runs without
    // pulling in `rand`.
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let mut step = || -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    {
        let wtxn = db.write_txn().expect("write_txn");
        for _ in 0..N_TYPED_RELATIONS {
            let s = entities[(step() as usize) % N_ENTITIES];
            let o = entities[(step() as usize) % N_ENTITIES];
            if s == o {
                continue;
            }
            let rt = relation_types[(step() as usize) % 3];
            let r = Relation::new_root(
                RelationId::new(),
                rt,
                s,
                o,
                0.8,
                vec![],
                ExtractorId::from(0),
                T0,
                false,
            );
            // Cardinality conflicts silently auto-supersede or error;
            // both are fine for the bench fixture (we just want
            // realistic row counts).
            let _ = relation_create(&wtxn, &r, T0);
        }
        wtxn.commit().expect("commit");
    }

    EntityFixture {
        db,
        entities,
        relation_types,
        _dir: dir,
    }
}

struct MemoryFixture {
    db: MetadataDb,
    memories: Vec<MemoryId>,
    _dir: TempDir,
}

fn build_memory_fixture() -> MemoryFixture {
    let dir = TempDir::new().expect("tempdir");
    let mut db = MetadataDb::open(dir.path().join("md.redb")).expect("open db");

    let memories: Vec<MemoryId> = (0..N_MEMORIES as u64)
        .map(|slot| MemoryId::pack(1, slot, 1))
        .collect();

    let mut state: u64 = 0x1234_5678_9ABC_DEF0;
    let mut step = || -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    let kinds = [
        EdgeKind::Caused,
        EdgeKind::FollowedBy,
        EdgeKind::SimilarTo,
        EdgeKind::References,
    ];
    {
        let wtxn = db.write_txn().expect("write_txn");
        {
            let mut e = wtxn.open_table(EDGES_TABLE).expect("open EDGES");
            let mut r = wtxn
                .open_table(EDGES_REVERSE_TABLE)
                .expect("open EDGES_REVERSE");
            for _ in 0..N_SUBSTRATE_EDGES {
                let a = memories[(step() as usize) % N_MEMORIES];
                let b = memories[(step() as usize) % N_MEMORIES];
                if a == b {
                    continue;
                }
                let k = kinds[(step() as usize) % kinds.len()];
                let data = EdgeData::new(0.5, origin::EXPLICIT, derived_by::CLIENT, T0);
                let _ = link(
                    &mut e,
                    &mut r,
                    NodeRef::Memory(a),
                    EdgeKindRef::Builtin(k),
                    NodeRef::Memory(b),
                    zero_disambiguator(),
                    &data,
                );
            }
        }
        wtxn.commit().expect("commit");
    }

    MemoryFixture {
        db,
        memories,
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// p50 helpers — direct timing so we can assert against the spec target
// even when criterion is invoked without the statistical-summary mode.
// ---------------------------------------------------------------------------

fn measure_p50<F: FnMut()>(mut f: F, samples: usize) -> Duration {
    // Warm-up to populate the redb page cache.
    for _ in 0..16 {
        f();
    }
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        f();
        times.push(t.elapsed());
    }
    times.sort();
    times[times.len() / 2]
}

// ---------------------------------------------------------------------------
// Benches + perf-gate assertions.
// ---------------------------------------------------------------------------

fn bench_relation_traverse_depths(c: &mut Criterion) {
    let fx = build_entity_fixture();
    let mut idx = 0usize;
    let next_start = |idx: &mut usize, ents: &[EntityId]| -> EntityId {
        let s = ents[*idx % ents.len()];
        *idx = idx.wrapping_add(1);
        s
    };

    let cfg_d1 = TraversalConfig {
        max_depth: 1,
        max_branching_factor: 1000,
        current_only: true,
    };
    let cfg_d2 = TraversalConfig {
        max_depth: 2,
        max_branching_factor: 1000,
        current_only: true,
    };
    let cfg_d3 = TraversalConfig {
        max_depth: 3,
        max_branching_factor: 1000,
        current_only: true,
    };

    // Depth-1 perf gate.
    let p50_d1 = measure_p50(
        || {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                black_box(&cfg_d1),
            )
            .expect("traverse");
        },
        128,
    );

    let p50_d2 = measure_p50(
        || {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                black_box(&cfg_d2),
            )
            .expect("traverse");
        },
        96,
    );

    let p50_d3 = measure_p50(
        || {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                black_box(&cfg_d3),
            )
            .expect("traverse");
        },
        64,
    );

    eprintln!("RELATION_TRAVERSE p50 actuals:");
    eprintln!(
        "  depth=1 : {:.3} ms (target ≤ {:.1} ms)",
        duration_ms(p50_d1),
        P50_TARGET_MS_D1
    );
    eprintln!(
        "  depth=2 : {:.3} ms (target ≤ {:.1} ms)",
        duration_ms(p50_d2),
        P50_TARGET_MS_D2
    );
    eprintln!(
        "  depth=3 : {:.3} ms (target ≤ {:.1} ms)",
        duration_ms(p50_d3),
        P50_TARGET_MS_D3
    );

    assert!(
        duration_ms(p50_d1) <= P50_TARGET_MS_D1,
        "depth=1 p50 {:.3} ms exceeds target {} ms",
        duration_ms(p50_d1),
        P50_TARGET_MS_D1,
    );
    assert!(
        duration_ms(p50_d2) <= P50_TARGET_MS_D2,
        "depth=2 p50 {:.3} ms exceeds target {} ms",
        duration_ms(p50_d2),
        P50_TARGET_MS_D2,
    );
    assert!(
        duration_ms(p50_d3) <= P50_TARGET_MS_D3,
        "depth=3 p50 {:.3} ms exceeds target {} ms",
        duration_ms(p50_d3),
        P50_TARGET_MS_D3,
    );

    // Also feed criterion for the standard statistical summary so a
    // future operator can graph the trend.
    let mut idx_c = 0usize;
    c.bench_function("relation_traverse_depth_1", |b| {
        b.iter(|| {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx_c, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                &cfg_d1,
            )
            .expect("traverse");
        });
    });
    let mut idx_c = 0usize;
    c.bench_function("relation_traverse_depth_2", |b| {
        b.iter(|| {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx_c, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                &cfg_d2,
            )
            .expect("traverse");
        });
    });
    let mut idx_c = 0usize;
    c.bench_function("relation_traverse_depth_3", |b| {
        b.iter(|| {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let s = next_start(&mut idx_c, &fx.entities);
            let _ = traverse(
                &rtxn,
                s,
                &fx.relation_types,
                TraversalDirection::Outgoing,
                &cfg_d3,
            )
            .expect("traverse");
        });
    });
}

/// Memory-anchor walk on a substrate-only fixture. Verifies that
/// Phase A's `walk_memory_edges` (now backed by the unified
/// `EDGES_TABLE`) hasn't regressed against the depth=1 target.
fn bench_substrate_walk(c: &mut Criterion) {
    let fx = build_memory_fixture();
    let mut idx = 0usize;

    let p50 = measure_p50(
        || {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let m = fx.memories[idx % fx.memories.len()];
            idx = idx.wrapping_add(1);
            let _ = walk_outgoing(&rtxn, NodeRef::Memory(m), None).expect("walk_outgoing");
        },
        128,
    );

    eprintln!(
        "SUBSTRATE walk_outgoing p50 actual: {:.3} ms (target ≤ {:.1} ms)",
        duration_ms(p50),
        P50_TARGET_MS_SUBSTRATE,
    );
    assert!(
        duration_ms(p50) <= P50_TARGET_MS_SUBSTRATE,
        "substrate walk p50 {:.3} ms exceeds target {} ms",
        duration_ms(p50),
        P50_TARGET_MS_SUBSTRATE,
    );

    let mut idx_c = 0usize;
    c.bench_function("substrate_walk_outgoing", |b| {
        b.iter(|| {
            let rtxn = fx.db.read_txn().expect("read_txn");
            let m = fx.memories[idx_c % fx.memories.len()];
            idx_c = idx_c.wrapping_add(1);
            let _ = walk_outgoing(&rtxn, NodeRef::Memory(m), None).expect("walk_outgoing");
        });
    });
}

fn duration_ms(d: Duration) -> f64 {
    (d.as_nanos() as f64) / 1_000_000.0
}

criterion_group!(
    relation_traverse,
    bench_relation_traverse_depths,
    bench_substrate_walk,
);
criterion_main!(relation_traverse);
