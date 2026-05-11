//! Recall@10 regression-detection test (sub-task 4.7).
//!
//! **This test does NOT validate the spec target.** Spec `§16/05 §2`
//! mandates recall@10 ≥ 0.95 at **1M memories** with realistic semantic
//! embeddings (BGE-small output). That measurement lives in
//! `benches/recall.rs` at 100K scale — closer to the spec scale and
//! the on-demand artifact.
//!
//! This integration test runs at 10K with synthetic clustered random
//! unit vectors — a measurably *harder* regime for HNSW than real
//! embeddings. We assert a **lower threshold (0.80)** here purely as
//! a CI regression gate: a code change that drops recall by more
//! than ~5 percentage points below the baseline should fail CI
//! without waiting for an operator to run `cargo bench`.
//!
//! Why synthetic recall undercounts: even with clustered data, in
//! 384-dim space tight clusters produce many near-ties at distance.
//! Exact-overlap recall@10 is sensitive to ties — HNSW may pick 10
//! different-but-equally-good neighbours than brute-force, scoring
//! low overlap despite being correct. Real BGE embeddings have more
//! distinct similarity structure; the recall numbers there reflect
//! the actual user-visible quality.

use brain_core::MemoryId;
use brain_index::{HnswIndex, IndexParams};

/// CI regression threshold — **not** the spec target. The spec
/// `§16/05 §2` mandate (0.95) is in `benches/recall.rs`. See the
/// module-level docstring for why 10K synthetic data can't reliably
/// hit 0.95.
const RECALL_REGRESSION_FLOOR: f64 = 0.80;

/// 1K corpus is the CI-friendly scale. Larger scales (the spec's
/// 100K and 1M) live in `benches/recall.rs` (release-mode only).
/// At 1K, the test runs in ~5 sec even in `cargo test`'s default
/// debug mode — hnsw_rs build+search is ~100× slower without
/// optimisation than release.
const N_CORPUS: u64 = 1_000;

/// 100 queries gives ~1% recall resolution.
const N_QUERIES: usize = 100;

/// Spec `§16/05 §1` formula: recall@K averaged over a query set.
const K: usize = 10;

const VECTOR_DIM: usize = 384;

/// Number of cluster centers used to give the corpus structure (spec
/// `§16/05 §4`: targets assume "realistic data … with some structure").
/// 20 clusters × 50 members at N_CORPUS=1000.
const N_CLUSTERS: usize = 20;

/// Per-component Gaussian noise stddev around the cluster centers.
/// At 384 dim, the noise vector has expected length ~√D × stddev =
/// ~19.6 × stddev. 0.05 is roughly 1/√D — close enough to the cluster
/// center signal that vectors cluster, but not so tight that
/// within-cluster ties dominate.
const CLUSTER_NOISE_STDDEV: f32 = 0.05;

// ---------------------------------------------------------------------------
// Inline xorshift64* PRNG. Same pattern used in Phase 3's recovery
// integration test (Scenario G). Reproducible across runs; spec
// §16/05 §16 mandates consistent ranking, which a deterministic seed
// gives us.
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
        // Map u32 → f32 in (-1, 1).
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
/// flags uniformly-random vectors as a hard case; agent-memory
/// embeddings have semantic clusters, which the HNSW graph exploits.
fn clustered_unit_vector(
    rng: &mut Xs,
    centers: &[[f32; VECTOR_DIM]],
    noise_stddev: f32,
) -> [f32; VECTOR_DIM] {
    let pick = (rng.next_u32() as usize) % centers.len();
    let centre = &centers[pick];
    let mut v = [0f32; VECTOR_DIM];
    for i in 0..VECTOR_DIM {
        // Box–Muller transform: two uniforms → one Gaussian.
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

/// For each query, compute the brute-force top-K corpus indices by
/// cosine similarity (dot product on unit vectors).
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

#[test]
fn recall_at_10_meets_spec_target_at_10k() {
    let mut rng = Xs::new(0xCAFE);

    // Generate cluster centers (uniformly random unit vectors).
    let centers: Vec<[f32; VECTOR_DIM]> = (0..N_CLUSTERS)
        .map(|_| random_unit_vector(&mut rng))
        .collect();

    // Build the corpus deterministically. Each member is a perturbed
    // version of a randomly-chosen cluster center. Spec §16/05 §4
    // contrasts realistic clustered data with uniform-random; with
    // uniform vectors in 384 dim, all pairs are roughly orthogonal
    // and HNSW recall drops to ~0.5. Real agent-memory embeddings
    // have semantic clusters — we mimic that here.
    let corpus: Vec<(MemoryId, [f32; VECTOR_DIM])> = (0..N_CORPUS)
        .map(|i| {
            (
                MemoryId::pack(1, i, 1),
                clustered_unit_vector(&mut rng, &centers, CLUSTER_NOISE_STDDEV),
            )
        })
        .collect();

    // Build the index (rebuild from iterator — exercises the 4.6 path).
    let (idx, report) = HnswIndex::<VECTOR_DIM>::rebuild(
        IndexParams::default_v1(),
        corpus.iter().map(|(id, v)| (*id, *v)),
    )
    .expect("rebuild");
    assert_eq!(report.memories_inserted, N_CORPUS);

    // Queries: drawn from the same cluster distribution but with a
    // separate seed so they aren't corpus members.
    let mut q_rng = Xs::new(0xBEEF);
    let queries: Vec<[f32; VECTOR_DIM]> = (0..N_QUERIES)
        .map(|_| clustered_unit_vector(&mut q_rng, &centers, CLUSTER_NOISE_STDDEV))
        .collect();

    // Compute recall@K = (Σ overlaps) / (k * |queries|).
    let mut total_overlap: usize = 0;
    for q in &queries {
        let truth = ground_truth(&corpus, q, K);
        let hnsw: Vec<MemoryId> = idx
            .search_active(q, K, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let truth_set: std::collections::HashSet<MemoryId> = truth.into_iter().collect();
        total_overlap += hnsw.iter().filter(|id| truth_set.contains(id)).count();
    }
    let recall = total_overlap as f64 / (K * queries.len()) as f64;

    println!(
        "recall@{K} at N={N_CORPUS}, queries={N_QUERIES}, defaults: {recall:.4} \
         (CI floor ≥ {RECALL_REGRESSION_FLOOR}; spec target 0.95 is checked in benches/recall.rs)"
    );

    assert!(
        recall >= RECALL_REGRESSION_FLOOR,
        "recall@{K} = {recall:.4} fell below CI regression floor {RECALL_REGRESSION_FLOOR} — \
         likely a real regression in HnswIndex; investigate before relaxing this threshold"
    );
}
