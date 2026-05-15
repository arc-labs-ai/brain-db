//! Entity-resolver perf bench (sub-task 16.9.4).
//!
//! Spec targets per [`spec/16_benchmarks_acceptance/02_latency_targets.md`](
//! ../../spec/16_benchmarks_acceptance/02_latency_targets.md) §2.2 at
//! 100K entities:
//!
//! - tier-1 exact lookup: p50 ≤ 1 ms, p99 ≤ 2 ms.
//! - tier-1 alias lookup: p50 ≤ 1 ms, p99 ≤ 2 ms.
//! - tier-2 trigram candidates: p50 ≤ 5 ms, p99 ≤ 30 ms.
//! - tier-2 full resolve (candidates + Jaccard scoring): p50 ≤ 5 ms,
//!   p99 ≤ 30 ms.
//!
//! Run with: `cargo bench -p brain-metadata --bench entity_resolve`.
//!
//! Phase scope (16.9): benches are operator-run; CI regression
//! thresholds land in phase 14. Tier-3 (embedding) bench lands in
//! phase 21 alongside the entity HNSW wiring.

use brain_core::{Entity, EntityId, EntityType, EntityTypeId};
use brain_metadata::entity_ops::{
    entity_lookup_by_alias, entity_lookup_by_canonical_name, entity_put, normalize_name,
};
use brain_metadata::trigram_ops::{
    candidates_for_query, extract_trigrams, jaccard, trigrams_of_components,
};
use brain_metadata::MetadataDb;
use criterion::{black_box, criterion_group, Criterion};
use tempfile::TempDir;

const N_ENTITIES: usize = 100_000;
const PERSON: EntityTypeId = EntityType::PERSON_ID;
const FUZZY_THRESHOLD: f32 = 0.85;

// ---------------------------------------------------------------------------
// Fixture: populate a fresh redb db with N_ENTITIES `Person` rows.
//
// Each entity:
//   - canonical_name = "person_<N>" (predictable, distinct trigram set).
//   - one alias "alias_<N>".
//   - no attributes.
//
// `_dir` ties the TempDir lifetime to the fixture; dropping the
// fixture wipes the on-disk db.
// ---------------------------------------------------------------------------

struct Fixture {
    db: MetadataDb,
    ids: Vec<EntityId>,
    _dir: TempDir,
}

fn build_fixture(n: usize) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let mut db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open db");

    let mut ids = Vec::with_capacity(n);
    let now = 1_700_000_000_000_000_000u64;
    let wtxn = db.write_txn().expect("write_txn");
    for i in 0..n {
        let id = EntityId::new();
        let name = format!("person_{i}");
        let normalized = normalize_name(&name);
        let mut entity = Entity::new_active(id, PERSON, name, normalized, now);
        entity.aliases = vec![format!("alias_{i}")];
        entity_put(&wtxn, &entity).expect("entity_put");
        ids.push(id);
    }
    wtxn.commit().expect("commit");

    Fixture { db, ids, _dir: dir }
}

// ---------------------------------------------------------------------------
// Tier 1 — exact canonical_name lookup.
// ---------------------------------------------------------------------------

fn bench_tier1_exact_lookup(c: &mut Criterion) {
    let fixture = build_fixture(N_ENTITIES);
    let queries: Vec<String> = (0..1024).map(|i| format!("person_{}", i * 97 % N_ENTITIES)).collect();
    let mut idx = 0usize;

    c.bench_function("entity_resolve::tier1_exact_lookup", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let q = &queries[idx % queries.len()];
            idx = idx.wrapping_add(1);
            let result =
                entity_lookup_by_canonical_name(&rtxn, PERSON, black_box(q)).expect("lookup");
            black_box(result);
        });
    });
}

// ---------------------------------------------------------------------------
// Tier 1 — alias lookup.
// ---------------------------------------------------------------------------

fn bench_tier1_alias_lookup(c: &mut Criterion) {
    let fixture = build_fixture(N_ENTITIES);
    let queries: Vec<String> = (0..1024).map(|i| format!("alias_{}", i * 97 % N_ENTITIES)).collect();
    let mut idx = 0usize;

    c.bench_function("entity_resolve::tier1_alias_lookup", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let q = &queries[idx % queries.len()];
            idx = idx.wrapping_add(1);
            let result = entity_lookup_by_alias(&rtxn, PERSON, black_box(q)).expect("lookup");
            black_box(result);
        });
    });
}

// ---------------------------------------------------------------------------
// Tier 2 — trigram candidate set only (no Jaccard).
// ---------------------------------------------------------------------------

fn bench_tier2_trigram_candidates_only(c: &mut Criterion) {
    let fixture = build_fixture(N_ENTITIES);
    // Fuzzy queries — slight typos so trigrams overlap multiple
    // candidate entities (more realistic than exact matches).
    let queries: Vec<String> = (0..1024)
        .map(|i| format!("persoon_{}", i * 97 % N_ENTITIES))
        .collect();
    let mut idx = 0usize;

    c.bench_function("entity_resolve::tier2_trigram_candidates", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let q = &queries[idx % queries.len()];
            idx = idx.wrapping_add(1);
            let candidates =
                candidates_for_query(&rtxn, PERSON, &normalize_name(black_box(q))).expect("trigram");
            black_box(candidates);
        });
    });
}

// ---------------------------------------------------------------------------
// Tier 2 — full resolve (candidates + per-candidate Jaccard scoring +
// threshold filter). Closest match to what the wire-side
// ENTITY_RESOLVE handler runs.
// ---------------------------------------------------------------------------

fn bench_tier2_full_resolve(c: &mut Criterion) {
    let fixture = build_fixture(N_ENTITIES);
    let queries: Vec<String> = (0..1024)
        .map(|i| format!("persoon_{}", i * 97 % N_ENTITIES))
        .collect();
    let mut idx = 0usize;

    c.bench_function("entity_resolve::tier2_full_resolve", |b| {
        b.iter(|| {
            let rtxn = fixture.db.read_txn().expect("read_txn");
            let q = &queries[idx % queries.len()];
            idx = idx.wrapping_add(1);
            let q_norm = normalize_name(q);
            let q_trigrams = extract_trigrams(&q_norm);
            let candidates =
                candidates_for_query(&rtxn, PERSON, &q_norm).expect("trigram");

            let mut scored = Vec::with_capacity(candidates.len());
            for cand in candidates {
                // Re-fetch the candidate's name to compute its trigram set.
                let cand_entity = brain_metadata::entity_ops::entity_get(&rtxn, cand)
                    .expect("entity_get");
                if let Some(e) = cand_entity {
                    let cand_trigrams = trigrams_of_components(&e.canonical_name, &e.aliases);
                    let score = jaccard(&q_trigrams, &cand_trigrams);
                    if score >= FUZZY_THRESHOLD {
                        scored.push((cand, score));
                    }
                }
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            black_box(scored);
        });
    });
}

// ---------------------------------------------------------------------------
// Hit-rate sanity check (printed once before benches run).
// ---------------------------------------------------------------------------

fn print_corpus_summary() {
    let fixture = build_fixture(1024); // tiny sample for the sanity print
    let rtxn = fixture.db.read_txn().expect("read_txn");
    let probe = "person_42";
    let exact = entity_lookup_by_canonical_name(&rtxn, PERSON, probe)
        .expect("exact")
        .is_some();
    let probe_alias = "alias_42";
    let alias = entity_lookup_by_alias(&rtxn, PERSON, probe_alias).expect("alias");
    let probe_fuzzy = "persoon_42";
    let candidates =
        candidates_for_query(&rtxn, PERSON, &normalize_name(probe_fuzzy)).expect("trigram");
    eprintln!(
        "entity_resolve bench setup: 1024-entity sample probe shows exact={exact} alias_hits={} fuzzy_candidates={}",
        alias.len(),
        candidates.len()
    );
    // Drop ids reference to silence unused warning.
    let _ = fixture.ids.first();
}

criterion_group!(
    name = entity_resolve_benches;
    config = Criterion::default();
    targets =
        bench_tier1_exact_lookup,
        bench_tier1_alias_lookup,
        bench_tier2_trigram_candidates_only,
        bench_tier2_full_resolve
);

fn main() {
    print_corpus_summary();
    entity_resolve_benches();
    Criterion::default().configure_from_args().final_summary();
}
