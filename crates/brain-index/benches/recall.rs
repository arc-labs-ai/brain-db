//! Recall@10 + search-latency criterion benchmark (sub-task 4.7).
//!
//! Spec target (`§16/05 §2`): recall@10 ≥ 0.95 at default HNSW
//! parameters. Phase doc 4.7 calls for the measurement at 100K
//! vectors. The companion `tests/recall.rs` runs a fast 10K version
//! for CI; this bench is the on-demand real-numbers measurement.
//!
//! Run with: `cargo bench -p brain-index --bench recall`.
//!
//! Output:
//! - A line `recall@10 at N=100000, queries=100, defaults: 0.xxxx` (or
//!   similar) printed before the bench.
//! - An `assert!(recall >= 0.95)` failure if the target is missed —
//!   the bench aborts, surfacing the regression.
//! - Criterion's standard latency stats for `search_active` over 100
//!   queries.

use std::time::Duration;

use brain_core::MemoryId;
use brain_index::{HnswIndex, IndexParams};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

const RECALL_TARGET: f64 = 0.95;
const N_CORPUS: u64 = 100_000;
const N_QUERIES: usize = 100;
const K: usize = 10;
const VECTOR_DIM: usize = 384;

/// Number of cluster centers used to give the corpus structure (spec
/// `§16/05 §4`: targets assume "realistic data … with some structure").
const N_CLUSTERS: usize = 1000;

/// Per-component Gaussian noise stddev around the cluster centers.
/// See `tests/recall.rs` for the dimensional-analysis rationale.
const CLUSTER_NOISE_STDDEV: f32 = 0.05;

// ---------------------------------------------------------------------------
// Inline xorshift64* PRNG. Same pattern as `tests/recall.rs`. Spec
// §16/05 §16's "consistent ranking" property holds with a deterministic
// seed.
// ---------------------------------------------------------------------------

struct Xs(u64);
impl Xs {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x as u32) ^ ((x >> 32) as u32)
    }
}

fn random_unit_vector(rng: &mut Xs) -> [f32; VECTOR_DIM] {
    let mut v = [0f32; VECTOR_DIM];
    for x in v.iter_mut() {
        let bits = rng.next_u32();
        *x = (bits as i32 as f32) / (i32::MAX as f32);
    }
    normalise(&mut v);
    v
}

fn normalise(v: &mut [f32; VECTOR_DIM]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Sample a clustered unit vector: pick one of `centers` uniformly,
/// add Gaussian noise to each component, normalise. Spec `§16/05 §4`
/// targets assume realistic clustered data; uniform-random vectors in
/// 384 dim give recall ~0.5 (curse of dimensionality).
fn clustered_unit_vector(
    rng: &mut Xs,
    centers: &[[f32; VECTOR_DIM]],
    noise_stddev: f32,
) -> [f32; VECTOR_DIM] {
    let pick = (rng.next_u32() as usize) % centers.len();
    let centre = &centers[pick];
    let mut v = [0f32; VECTOR_DIM];
    for i in 0..VECTOR_DIM {
        let u1 = (rng.next_u32() as f64 + 1.0) / (u32::MAX as f64 + 2.0);
        let u2 = (rng.next_u32() as f64) / (u32::MAX as f64 + 1.0);
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        v[i] = centre[i] + (z as f32) * noise_stddev;
    }
    normalise(&mut v);
    v
}

fn dot(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    let mut s = 0f32;
    for i in 0..VECTOR_DIM {
        s += a[i] * b[i];
    }
    s
}

fn ground_truth(
    corpus: &[(MemoryId, [f32; VECTOR_DIM])],
    query: &[f32; VECTOR_DIM],
    k: usize,
) -> Vec<MemoryId> {
    let mut sims: Vec<(MemoryId, f32)> =
        corpus.iter().map(|(id, v)| (*id, dot(v, query))).collect();
    sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sims.into_iter().take(k).map(|(id, _)| id).collect()
}

type CorpusPair = (Vec<[f32; VECTOR_DIM]>, Vec<(MemoryId, [f32; VECTOR_DIM])>);

fn build_corpus() -> CorpusPair {
    let mut rng = Xs::new(0xCAFE);
    let centers: Vec<[f32; VECTOR_DIM]> = (0..N_CLUSTERS)
        .map(|_| random_unit_vector(&mut rng))
        .collect();
    let corpus: Vec<(MemoryId, [f32; VECTOR_DIM])> = (0..N_CORPUS)
        .map(|i| {
            (
                MemoryId::pack(1, i, 1),
                clustered_unit_vector(&mut rng, &centers, CLUSTER_NOISE_STDDEV),
            )
        })
        .collect();
    (centers, corpus)
}

fn build_index(corpus: &[(MemoryId, [f32; VECTOR_DIM])]) -> HnswIndex<VECTOR_DIM> {
    let (idx, _report) = HnswIndex::<VECTOR_DIM>::rebuild(
        IndexParams::default_v1(),
        corpus.iter().map(|(id, v)| (*id, *v)),
    )
    .expect("rebuild");
    idx
}

fn compute_recall(
    idx: &HnswIndex<VECTOR_DIM>,
    corpus: &[(MemoryId, [f32; VECTOR_DIM])],
    queries: &[[f32; VECTOR_DIM]],
) -> f64 {
    let mut total_overlap: usize = 0;
    for q in queries {
        let truth: std::collections::HashSet<MemoryId> =
            ground_truth(corpus, q, K).into_iter().collect();
        let hnsw: Vec<MemoryId> = idx
            .search_active(q, K, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        total_overlap += hnsw.iter().filter(|id| truth.contains(id)).count();
    }
    total_overlap as f64 / (K * queries.len()) as f64
}

fn bench_recall(c: &mut Criterion) {
    eprintln!("building {N_CORPUS}-vector index for recall bench...");
    let (centers, corpus) = build_corpus();
    let idx = build_index(&corpus);

    let mut q_rng = Xs::new(0xBEEF);
    let queries: Vec<[f32; VECTOR_DIM]> = (0..N_QUERIES)
        .map(|_| clustered_unit_vector(&mut q_rng, &centers, CLUSTER_NOISE_STDDEV))
        .collect();

    eprintln!("computing brute-force ground truth + measuring recall@{K}...");
    let recall = compute_recall(&idx, &corpus, &queries);
    println!(
        "recall@{K} at N={N_CORPUS}, queries={N_QUERIES}, defaults: {recall:.4} \
         (target ≥ {RECALL_TARGET})"
    );
    assert!(
        recall >= RECALL_TARGET,
        "recall@{K} = {recall:.4} fell below spec §16/05 §2 target {RECALL_TARGET}"
    );

    // Latency: measure 100-query batch (median per-query latency falls
    // out of criterion's per-iter stats divided by N_QUERIES).
    let mut group = c.benchmark_group("recall");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(20);
    group.bench_function("search_active_100k_default_ef", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for q in &queries {
                let results = idx.search_active(black_box(q), K, None);
                acc += results.len();
            }
            black_box(acc);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_recall);
criterion_main!(benches);
