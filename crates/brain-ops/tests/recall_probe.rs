//! RECALL PROBE — the go/no-go gate for the per-space index redesign.
//!
//! This is an **isolated measurement harness**. It imports the production
//! index (`brain_index::HnswIndex`) exactly as the live retriever uses it,
//! but it does **not** modify any production path. Nothing here is wired
//! into the shard, the retriever, or the write path.
//!
//! ## What it measures
//!
//! The tenancy design (`.claude/plans/tenancy_industry_scale.md` §6) claims
//! that Brain's current model — one shared per-shard HNSW plus a *post-hoc
//! scope filter* (`semantic_retriever.rs::memory_row_passes`) — **misses a
//! sparse space's own results at high selectivity**: when one space owns 10
//! memories out of ~1M on the shard, filtered approximate-NN never explores
//! into that space's region and the answer is silently lost. The proposed
//! fix is an **exact per-space brute-force** over just that space's vectors.
//!
//! Two arms, same planted queries:
//!
//!   * **Arm (a) — shared-filtered** (what ships today):
//!     `HnswIndex::search(query, k, ef=64, |id| space_of(id) == target)`.
//!     Identical call shape to `BrainSemanticRetriever::search_memory`
//!     (default `ef_search = 64`, `ef_search_max = 500`, the over-fetch /
//!     ef-escalation bailout loop, the `Fn(MemoryId) -> bool` scope closure).
//!
//!   * **Arm (b) — per-space brute-force** (the prototype fix):
//!     gather the target space's vectors, exact cosine, top-k. In production
//!     the gather range-scans `MEMORIES_BY_SPACE_TIMELINE_TABLE`'s
//!     `space_timeline_prefix_space(namespace, space)` prefix → `memory_id`s
//!     → arena slots; here the space's vectors are held in a `Vec`, which is
//!     geometrically identical (recall is a property of *which* vectors are
//!     scored, not where they are stored).
//!
//! ## The adversarial corpus (the "two Johns")
//!
//! Random unit-norm 384-d vectors (BGE-small dimensionality). For each
//! sampled SPARSE target space (M = 10) we plant:
//!   * exactly **one** in-space near-duplicate of the query at
//!     `cosine = TRUE_COSINE` — the ground-truth answer for that space;
//!   * the space's other 9 vectors at `cosine ≈ 0` (random) — so the
//!     in-space answer is unambiguously the space's best match, and
//!     brute-force over the space is trivially correct;
//!   * **competitors** in OTHER spaces at `cosine > TRUE_COSINE` — memories
//!     that are *globally better* matches but belong to other users. The
//!     scope filter correctly rejects them; the question is whether the
//!     shared index still surfaces the in-space answer despite them.
//!
//! The number of competitors per query is `occupancy × COMPETITOR_DENSITY`
//! — a single physical knob ("what fraction of the shard outranks the
//! caller's own best memory for this query"). Because the true hit lands at
//! global rank ≈ `competitor_count + 1`, and the shared search explores at
//! most `ef_search_max = 500` nodes, the failure is predicted precisely at
//! `occupancy × DENSITY ≳ 500`. That crossover is occupancy-driven for any
//! fixed density — which is the whole thesis. We do NOT tune the outcome per
//! occupancy; we fix one density and let the sweep reveal the crossover.
//!
//! ## Running
//!
//! The heavy sweeps are `#[ignore]` (they build 100K–1M-node graphs). A
//! light `probe_smoke` runs in the normal suite to keep the harness honest.
//!
//! ```text
//! # inside the linux container (repo mounted at /workspaces/brain):
//! cargo test -p brain-ops --test recall_probe -- --ignored --nocapture
//! # or a single arm:
//! cargo test -p brain-ops --test recall_probe probe_recall_1m -- --ignored --nocapture
//! ```
//!
//! The 10M sweep is additionally gated behind `BRAIN_PROBE_10M=1` because it
//! needs ~16 GB RAM for the HNSW's internal vector copy.

use std::time::Instant;

use brain_core::MemoryId;
use brain_index::{HnswIndex, IndexParams, VECTOR_DIM};

// ---------------------------------------------------------------------------
// Deterministic PRNG (no `rand` dependency — splitmix64).
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn uniform(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Standard normal via Box–Muller.
    fn normal(&mut self) -> f32 {
        let u1 = self.uniform().max(1e-9);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

// ---------------------------------------------------------------------------
// Vector helpers (384-d, unit-norm).
// ---------------------------------------------------------------------------

type Vec384 = [f32; VECTOR_DIM];

fn dot(a: &Vec384, b: &Vec384) -> f32 {
    // Scalar cosine (inputs are unit-norm, so dot == cosine). This is the
    // brute-force prototype's inner loop; the production prototype would
    // widen this to `wide::f32x8` lanes (8-wide FMA over the 384 dims =
    // 48 vector ops/pair) — a ~4–8× speedup that only *lowers* arm (b)
    // latency, so the scalar numbers here are a conservative upper bound.
    let mut s = 0.0f32;
    for i in 0..VECTOR_DIM {
        s += a[i] * b[i];
    }
    s
}

fn normalize(v: &mut Vec384) {
    let mut n = 0.0f32;
    for &x in v.iter() {
        n += x * x;
    }
    let n = n.sqrt().max(1e-12);
    for x in v.iter_mut() {
        *x /= n;
    }
}

fn random_unit(rng: &mut Rng) -> Vec384 {
    let mut v = [0.0f32; VECTOR_DIM];
    for x in v.iter_mut() {
        *x = rng.normal();
    }
    normalize(&mut v);
    v
}

/// A fresh unit vector at *exactly* cosine `c` to `q`:
/// `v = c·q + sqrt(1−c²)·r⊥`, with `r⊥` a random unit vector orthogonal to
/// `q`. `v·q == c` by construction.
fn at_cosine(q: &Vec384, c: f32, rng: &mut Rng) -> Vec384 {
    let mut r = random_unit(rng);
    let d = dot(&r, q);
    for i in 0..VECTOR_DIM {
        r[i] -= d * q[i];
    }
    normalize(&mut r);
    let s = (1.0 - c * c).sqrt();
    let mut v = [0.0f32; VECTOR_DIM];
    for i in 0..VECTOR_DIM {
        v[i] = c * q[i] + s * r[i];
    }
    v
}

// ---------------------------------------------------------------------------
// MemoryId ⇄ (space, local) packing.
//
// The scope filter needs a cheap "which space owns this id" check. We encode
// the space in the high bits of the 48-bit slot field and the per-space local
// index in the low 24 bits — so the filter is an allocation-free bit-shift,
// faithful to a real per-row scope check (which reads `space_id_bytes`).
// ---------------------------------------------------------------------------

const LOCAL_BITS: u64 = 24;
const LOCAL_MASK: u64 = (1 << LOCAL_BITS) - 1;

/// Space ids. Targets are `0..num_queries`; competitors and background live
/// in disjoint high ranges so a space id never collides with a target.
const COMP_SPACE: u32 = 1 << 22;
const BG_SPACE: u32 = 1 << 23;

fn mid(space: u32, local: u32) -> MemoryId {
    let slot = ((space as u64) << LOCAL_BITS) | (local as u64 & LOCAL_MASK);
    MemoryId::pack(0, slot, 1)
}

fn space_of(id: MemoryId) -> u32 {
    (id.slot() >> LOCAL_BITS) as u32
}

// ---------------------------------------------------------------------------
// Brute-force top-k (arm (b) inner kernel).
// ---------------------------------------------------------------------------

fn brute_force_topk(query: &Vec384, vecs: &[(MemoryId, Vec384)], k: usize) -> Vec<(MemoryId, f32)> {
    let mut scored: Vec<(MemoryId, f32)> =
        vecs.iter().map(|(id, v)| (*id, dot(v, query))).collect();
    scored.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

// ---------------------------------------------------------------------------
// Recall probe.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct RecallConfig {
    occupancy: usize,
    num_queries: usize,
    sparse_m: usize,
    competitor_density: f32,
    true_cosine: f32,
    comp_cos_lo: f32,
    comp_cos_hi: f32,
    seed: u64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            occupancy: 100_000,
            num_queries: 30,
            sparse_m: 10,
            competitor_density: 0.001, // 0.1% of the shard outranks the caller
            true_cosine: 0.55,
            comp_cos_lo: 0.60,
            comp_cos_hi: 0.85,
            seed: 0xB0BA_CAFE,
        }
    }
}

struct RecallResult {
    cfg_occupancy: usize,
    competitors_per_query: usize,
    // Arm (a): the production `HnswIndex::search` path (filter closure +
    // over-fetch / ef-escalation bailout), exactly as `search_memory` calls
    // it. Recall AND cost — the bailout preserves recall by escalating toward
    // a full scan, so its per-query latency is the real signal.
    shared_recall_at_10: f32,
    shared_avg_us: f64,
    shared_p99_us: f64,
    // Arm (a'): "naive filtered ANN" = retrieve top-N unfiltered, then keep
    // only in-space hits. Models a bounded-budget filtered search (no
    // exhaustive fallback). This is where the geometric high-selectivity
    // recall collapse the plan predicts actually shows up.
    naive_recall_budget_10: f32,
    naive_recall_budget_50: f32,
    naive_recall_budget_500: f32,
    // Arm (b): exact per-space brute-force (the proposed fix).
    brute_recall_at_10: f32,
    brute_avg_us: f64,
}

/// Build one shared HNSW at `cfg.occupancy`, plant `cfg.num_queries` sparse
/// target spaces, and measure recall@10 for both arms.
fn run_recall(cfg: RecallConfig) -> RecallResult {
    let mut rng = Rng::new(cfg.seed);
    let competitors_per_query = ((cfg.occupancy as f32) * cfg.competitor_density).round() as usize;

    let params = IndexParams::default_v1(); // M=16, ef_c=200, ef_search=64, max=500
    let mut index = HnswIndex::new(params).expect("valid params");

    // Per-query state we retain (tiny): the query vector, its ground-truth
    // in-space hit id, and the target space's own vectors (for arm (b)).
    let mut queries: Vec<Vec384> = Vec::with_capacity(cfg.num_queries);
    let mut truth: Vec<MemoryId> = Vec::with_capacity(cfg.num_queries);
    let mut space_vecs: Vec<Vec<(MemoryId, Vec384)>> = Vec::with_capacity(cfg.num_queries);

    // We do NOT retain the full corpus (that would double peak RAM against
    // the HNSW's internal copy). Vectors are dropped right after insertion.
    let mut inserted: usize = 0;

    // 1. Target spaces + their in-space true hit + 9 random decoys.
    for q in 0..cfg.num_queries {
        let query = random_unit(&mut rng);
        let mut this_space: Vec<(MemoryId, Vec384)> = Vec::with_capacity(cfg.sparse_m);

        // slot 0: the in-space near-duplicate (ground truth).
        let hit_id = mid(q as u32, 0);
        let hit_vec = at_cosine(&query, cfg.true_cosine, &mut rng);
        index.insert(hit_id, &hit_vec).expect("insert hit");
        this_space.push((hit_id, hit_vec));
        inserted += 1;

        // slots 1..M: random decoys (cosine ≈ 0 to the query).
        for local in 1..cfg.sparse_m {
            let id = mid(q as u32, local as u32);
            let v = random_unit(&mut rng);
            index.insert(id, &v).expect("insert decoy");
            this_space.push((id, v));
            inserted += 1;
        }

        queries.push(query);
        truth.push(hit_id);
        space_vecs.push(this_space);
    }

    // 2. Competitors: for each query, `competitors_per_query` vectors in
    //    OTHER spaces at cosine > true_cosine (globally-better, filtered out).
    let mut comp_local: u32 = 0;
    for query in &queries {
        for _ in 0..competitors_per_query {
            let c = cfg.comp_cos_lo + (cfg.comp_cos_hi - cfg.comp_cos_lo) * rng.uniform();
            let v = at_cosine(query, c, &mut rng);
            let id = mid(COMP_SPACE, comp_local);
            comp_local = comp_local.wrapping_add(1);
            index.insert(id, &v).expect("insert competitor");
            inserted += 1;
        }
    }

    // 3. Background filler: random vectors (cosine ≈ 0 to every query) up to
    //    occupancy — makes the graph genuinely large without adding
    //    competitors. Split across local ids within one background space.
    let mut bg_local: u32 = 0;
    while inserted < cfg.occupancy {
        let v = random_unit(&mut rng);
        let id = mid(BG_SPACE, bg_local);
        bg_local = bg_local.wrapping_add(1);
        index.insert(id, &v).expect("insert background");
        inserted += 1;
    }

    let ef = params.ef_search; // 64, same default the retriever passes

    // ---- Arm (a): production HnswIndex::search (filter + bailout). ----
    // Recall + per-query latency. The over-fetch loop escalates fetch_k up
    // to total_nodes when it cannot collect `k` filter-passing hits, so at
    // high selectivity this silently approaches a full-shard scan — which is
    // exactly what the latency samples expose.
    let mut shared_hits_10 = 0usize;
    let mut shared_us: Vec<f64> = Vec::with_capacity(cfg.num_queries);
    for (q, query) in queries.iter().enumerate() {
        let target = q as u32;
        let filter = |id: MemoryId| space_of(id) == target;
        let t = Instant::now();
        let r10 = index.search(query, 10, Some(ef), filter);
        shared_us.push(t.elapsed().as_secs_f64() * 1e6);
        if r10.iter().any(|(id, _)| *id == truth[q]) {
            shared_hits_10 += 1;
        }
    }

    // ---- Arm (a'): naive filtered ANN (bounded budget, post-filter). ----
    // `search_all(query, budget, ef)` returns the true top-`budget` by cosine
    // (no exhaustive fallback, since every candidate passes `|_| true`); we
    // then keep only in-space ids. If the in-space answer is not in the
    // unfiltered top-`budget`, it is lost. This is the classic
    // retrieve-then-filter failure the per-space index is meant to fix.
    let naive_recall = |budget: usize| -> f32 {
        let mut hits = 0usize;
        for (q, query) in queries.iter().enumerate() {
            let target = q as u32;
            let top = index.search_all(query, budget, Some(ef));
            if top
                .iter()
                .any(|(id, _)| space_of(*id) == target && *id == truth[q])
            {
                hits += 1;
            }
        }
        hits as f32 / cfg.num_queries as f32
    };
    let naive_recall_budget_10 = naive_recall(10);
    let naive_recall_budget_50 = naive_recall(50);
    let naive_recall_budget_500 = naive_recall(500);

    // ---- Arm (b): per-space exact brute-force (the fix). ----
    let mut brute_hits_10 = 0usize;
    let mut brute_us: Vec<f64> = Vec::with_capacity(cfg.num_queries);
    for (q, query) in queries.iter().enumerate() {
        let t = Instant::now();
        let r = brute_force_topk(query, &space_vecs[q], 10);
        brute_us.push(t.elapsed().as_secs_f64() * 1e6);
        if r.iter().any(|(id, _)| *id == truth[q]) {
            brute_hits_10 += 1;
        }
    }

    let n = cfg.num_queries as f32;
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let p99 = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() as f64 * 0.99) as usize).min(v.len() - 1)]
    };
    let shared_avg_us = avg(&shared_us);
    let shared_p99_us = p99(&mut shared_us);
    RecallResult {
        cfg_occupancy: cfg.occupancy,
        competitors_per_query,
        shared_recall_at_10: shared_hits_10 as f32 / n,
        shared_avg_us,
        shared_p99_us,
        naive_recall_budget_10,
        naive_recall_budget_50,
        naive_recall_budget_500,
        brute_recall_at_10: brute_hits_10 as f32 / n,
        brute_avg_us: avg(&brute_us),
    }
}

fn report_recall(r: &RecallResult) {
    println!(
        "  occupancy={:>9}  competitors/query={:>6}",
        r.cfg_occupancy, r.competitors_per_query
    );
    println!(
        "    arm(a)  shared HNSW+filter (prod):  recall@10={:.3}   \
         latency avg={:>9.1} µs  p99={:>9.1} µs",
        r.shared_recall_at_10, r.shared_avg_us, r.shared_p99_us
    );
    println!(
        "    arm(a') naive retrieve-then-filter: recall@10  budget10={:.3}  \
         budget50={:.3}  budget500={:.3}",
        r.naive_recall_budget_10, r.naive_recall_budget_50, r.naive_recall_budget_500
    );
    println!(
        "    arm(b)  per-space brute-force:       recall@10={:.3}   \
         latency avg={:>9.1} µs",
        r.brute_recall_at_10, r.brute_avg_us
    );
}

// ---------------------------------------------------------------------------
// Latency probe (arm (b) p50/p99 by space size M).
// ---------------------------------------------------------------------------

fn run_latency(m: usize, iters: usize, seed: u64) -> (f64, f64) {
    let mut rng = Rng::new(seed);
    let vecs: Vec<(MemoryId, Vec384)> = (0..m)
        .map(|i| (mid(7, i as u32), random_unit(&mut rng)))
        .collect();
    let queries: Vec<Vec384> = (0..iters).map(|_| random_unit(&mut rng)).collect();

    // Warm the caches.
    let _ = brute_force_topk(&queries[0], &vecs, 10);

    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for q in &queries {
        let t = Instant::now();
        let r = brute_force_topk(q, &vecs, 10);
        let us = t.elapsed().as_secs_f64() * 1e6;
        std::hint::black_box(&r);
        samples.push(us);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    (p(0.50), p(0.99))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Fast, always-on sanity check that the harness geometry works: at a small
/// occupancy the crossover shows up in miniature, and brute-force is exact.
#[test]
fn probe_smoke() {
    let cfg = RecallConfig {
        occupancy: 20_000,
        num_queries: 30,
        competitor_density: 0.05, // 1000 competitors ≫ ef_max=500 → shared fails
        ..RecallConfig::default()
    };
    println!("\n=== RECALL PROBE — smoke ===");
    let r = run_recall(cfg);
    report_recall(&r);

    // Brute-force must be exact — the in-space answer is the space's best.
    assert!(
        r.brute_recall_at_10 >= 0.99,
        "brute-force must recall the in-space hit; got {:.3}",
        r.brute_recall_at_10
    );
    // The NAIVE bounded filtered search (retrieve top-N, then filter) must
    // collapse under high selectivity: with competitors ≫ budget, the
    // in-space answer is not in the unfiltered top-N. This is the geometric
    // claim the full sweep quantifies.
    assert!(
        r.naive_recall_budget_500 < r.brute_recall_at_10,
        "naive retrieve-then-filter should trail brute-force under high \
         selectivity (naive@500={:.3}, brute={:.3})",
        r.naive_recall_budget_500,
        r.brute_recall_at_10,
    );
    // The production path preserves recall but pays for it: its escalating
    // over-fetch approaches a full scan, so it must be dramatically slower
    // than the scoped brute-force.
    assert!(
        r.shared_avg_us > r.brute_avg_us,
        "shared-filtered (full-scan fallback) should cost more than scoped \
         brute-force (shared={:.1}µs, brute={:.1}µs)",
        r.shared_avg_us,
        r.brute_avg_us,
    );
}

#[test]
#[ignore = "heavy: builds a 100K-node HNSW"]
fn probe_recall_100k() {
    println!("\n=== RECALL PROBE — occupancy 100K (density 0.1%) ===");
    let r = run_recall(RecallConfig {
        occupancy: 100_000,
        ..RecallConfig::default()
    });
    report_recall(&r);
}

#[test]
#[ignore = "heavy: builds a 1M-node HNSW (minutes)"]
fn probe_recall_1m() {
    println!("\n=== RECALL PROBE — occupancy 1M (density 0.1%) ===");
    let r = run_recall(RecallConfig {
        occupancy: 1_000_000,
        num_queries: 20,
        ..RecallConfig::default()
    });
    report_recall(&r);
}

/// Density sensitivity at 1M: shows the crossover is governed by
/// `competitors/query` vs `ef_search_max`, not a cherry-picked constant.
#[test]
#[ignore = "heavy: three 1M-node HNSW builds"]
fn probe_recall_1m_density_sweep() {
    println!("\n=== RECALL PROBE — 1M, density sweep ===");
    for density in [0.0002_f32, 0.0005, 0.001, 0.002] {
        let r = run_recall(RecallConfig {
            occupancy: 1_000_000,
            competitor_density: density,
            ..RecallConfig::default()
        });
        report_recall(&r);
    }
}

#[test]
#[ignore = "very heavy: ~16 GB RAM; gated behind BRAIN_PROBE_10M=1"]
fn probe_recall_10m() {
    if std::env::var("BRAIN_PROBE_10M").ok().as_deref() != Some("1") {
        println!("\n=== RECALL PROBE — 10M SKIPPED (set BRAIN_PROBE_10M=1) ===");
        return;
    }
    println!("\n=== RECALL PROBE — occupancy 10M (density 0.1%) ===");
    let r = run_recall(RecallConfig {
        occupancy: 10_000_000,
        num_queries: 30,
        ..RecallConfig::default()
    });
    report_recall(&r);
}

#[test]
#[ignore = "measurement: brute-force latency by space size"]
fn probe_bruteforce_latency() {
    println!("\n=== BRUTE-FORCE LATENCY (scalar cosine; wide::f32x8 would lower these) ===");
    for &m in &[10usize, 200, 2_000, 20_000] {
        let iters = if m >= 20_000 { 500 } else { 2_000 };
        let (p50, p99) = run_latency(m, iters, 0x1A7E_u64.wrapping_add(m as u64));
        println!("  M={:>6}  p50={:>8.2} µs  p99={:>8.2} µs", m, p50, p99);
    }
}
