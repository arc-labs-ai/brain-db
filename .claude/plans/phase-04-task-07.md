# Phase 4 — Task 4.7: Recall@10 benchmark fixture

**Classification:** simple. Two files: a criterion benchmark for the real 100K-vector measurement, and a fast integration test for CI quality gating. Spec anchor: `spec/16_benchmarks_acceptance/05_recall_quality.md`.

## 1. Scope

In:

- `crates/brain-index/benches/recall.rs` (new) — criterion bench. Builds a 100K-vector index with seeded RNG; measures HNSW search latency for 100 queries; also computes recall@10 vs exhaustive ground truth and asserts ≥ 0.95.
- `crates/brain-index/tests/recall.rs` (new) — fast integration test (~5 sec). Builds a 10K-vector index, measures recall@10 against exhaustive ground truth, asserts ≥ 0.95. Runs as part of `cargo test` for CI gating.
- `crates/brain-index/Cargo.toml` — add `criterion.workspace = true` dev-dep + `[[bench]] name = "recall" harness = false`.

Out (deferred):

- **Latency targets per spec §16/02** — measured by criterion as a side-effect, but the assert-on-threshold pattern is recall-only. Latency p50/p99 numbers are recorded but not gated; Phase 11 (observability) wires them into CI proper.
- **Recall@1 / Recall@100 targets** (spec §16/05 §2) — additional metrics; v1 4.7 ships recall@10 only. Adding the other two is a one-line bench addition later.
- **Real BGE-small embeddings** — the bench uses synthetic random unit vectors. Phase 5 (embedding) introduces real embeddings; a follow-up bench at that point can substitute real cues for richer recall numbers (spec §16/05 §13 "semantic match quality").
- **CI bench-runner** — `cargo bench` isn't on the `just verify` path. Phase 11 decides if/how to add it; 4.7 only needs the bench to *exist* and pass when run.

## 2. Spec quotes that bind the design

> **§16/05 §1 (recall formula):**
> ```
> recall@K = |HNSW_top_K ∩ exhaustive_top_K| / K
> ```
>
> **§16/05 §2 (targets):**
> | Metric | Target |
> |---|---|
> | Recall@10 (default settings) | ≥ 0.95 |
>
> **§16/05 §3 (conditions):** "1M memories per shard. Default HNSW parameters (M=16, ef_construction=200, ef_search=64)."
>
> **§16/05 §16 ("consistent ranking"):** "Within a single substrate state: the same query returns the same results in the same order. … HNSW's randomness is at build time, not search time."
> → Deterministic seeded RNG produces a reproducible build; the recall numbers are stable across runs.

## 3. Design decisions

### 3.1 Two files, two scales

`tests/recall.rs` runs fast enough for CI (~5 sec total: build + brute-force + search). `benches/recall.rs` is the real-numbers measurement at 100K (~30-60 sec). Both assert ≥ 0.95; the bench is the spec's literal "100K vectors" call.

Spec §16/05 §3 targets are at *1M* memories. We bench at 100K (per the phase doc's explicit call) which is below spec scale; recall numbers tend to be **higher** at smaller N because the graph is denser relative to total nodes. So ≥ 0.95 at 100K is a necessary but not sufficient guard for the 1M target. Phase 11+ can add a 1M bench if needed.

### 3.2 Synthetic random unit vectors

Real BGE-small embeddings are Phase 5. For 4.7, generate vectors deterministically:

```rust
// 384 random f32 components from a seeded RNG, then L2-normalise.
fn random_unit_vector(rng: &mut Xs) -> [f32; 384] {
    let mut v = [0f32; 384];
    for x in v.iter_mut() {
        // u32::to_f32 in [-1, 1) via bit manipulation.
        *x = ((rng.next_u32() as i32) as f32) / (i32::MAX as f32);
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
    v
}
```

The same inline xorshift64* PRNG we used in Phase 3's integration test. No new deps.

**Note on synthetic-vs-real recall:** synthetic uniformly-random vectors are spec §16/05 §4's "harder" case ("uniformly random data: harder to achieve high recall"). If we hit 0.95 on synthetic, we'll be comfortably above on real clustered embeddings.

### 3.3 Brute-force ground truth

For each query, compute cosine similarity vs every indexed vector, sort, take top-K:

```rust
fn ground_truth(corpus: &[[f32; 384]], query: &[f32; 384], k: usize) -> Vec<usize> {
    let mut sims: Vec<(usize, f32)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (i, dot(query, v)))
        .collect();
    sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    sims.into_iter().take(k).map(|(i, _)| i).collect()
}
```

For L2-normalised vectors, cosine = dot product. Simple.

Cost: N × D FMA per query. At N=10K, D=384, 100 queries → 384M FMA ≈ ~2 sec sequential. At N=100K → 20-30 sec (still acceptable for an on-demand bench).

### 3.4 Query selection: random subset of the corpus

Queries are drawn from the same vector distribution as the index. Two valid strategies:

- **Existing-memory queries:** the query *is* one of the indexed vectors. Recall@10 includes self-match (similarity 1.0). Tests "can the index find what it stored?" (spec §16/05 §11 "exact-match recall").
- **Fresh queries:** the query is a new random vector. Recall@10 measures whether the HNSW's approximate search finds the same top-10 as exhaustive. Tests "approximate search quality."

We use **fresh queries** as the primary measurement — it's the harder test and matches the spec's recall-quality intent (§16/05 §1's formula doesn't say the query has to be in the index).

### 3.5 Reproducibility

Both files use a fixed seed (`0xCAFE` for the corpus, `0xBEEF` for queries). Spec §16/05 §16 mandates consistent ranking; with the same seed, recall numbers are stable across runs and machines (modulo f32 SIMD non-associativity, which spec §16/05 §16 doesn't address — we accept ±1 nuance).

### 3.6 Assert inside the bench

Criterion benches can panic. We assert recall ≥ 0.95 inside the bench's setup function. If the assert fires, the bench fails — which is the right signal: a code change that drops recall below the spec target should not silently pass `cargo bench`.

For users who want to see the actual number without panicking, the test prints recall via `println!` before the assert.

### 3.7 No HnswError surface changes

4.7 is pure observability. No new public API on `HnswIndex`.

## 4. Files touched

- `crates/brain-index/Cargo.toml` — add `criterion.workspace = true` to dev-deps + `[[bench]] name = "recall" harness = false`.
- `crates/brain-index/benches/recall.rs` (new) — ~150 LOC. 100K corpus, 100 queries, criterion bench + recall assert.
- `crates/brain-index/tests/recall.rs` (new) — ~80 LOC. 10K corpus, 100 queries, recall assert. CI runs this via `cargo test`.

No other crate changes.

## 5. What CI / verification will see

### `cargo test -p brain-index` (CI default)

Picks up the new integration test in `tests/recall.rs`. Adds ~5 sec to the test suite. **Fails if recall drops < 0.95 at 10K scale** — the CI quality gate.

### `cargo bench -p brain-index --bench recall` (manual / Phase 11)

Builds the 100K-vector index, runs the brute-force ground truth, asserts recall ≥ 0.95, and times the HNSW search via criterion. Output includes:
- Recall number (printed before bench).
- Latency p50/p99 per criterion's standard format.

This is the "real" measurement; runs on demand.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index && cargo bench -p brain-index --bench recall --no-run"
```

The `--no-run` on the bench checks that it compiles without running it (the full bench is too slow for the default test cycle).

Expected:
- brain-index test count: 65 → 66 (one new integration test).
- New bench compiles.
- Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5:

```
feat(brain-index): recall@10 benchmark + CI test (sub-task 4.7)
```

Body summarises: criterion bench at 100K + integration test at 10K, both seeded-deterministic, both assert recall@10 ≥ 0.95 at default params, brute-force ground truth via dot-product on L2-normalised synthetic vectors.

## 8. Done when

- [ ] `cargo bench -p brain-index --bench recall --no-run` compiles cleanly.
- [ ] `cargo test -p brain-index` includes the new recall test and passes (recall@10 ≥ 0.95 at 10K scale).
- [ ] `cargo bench -p brain-index --bench recall` (manually invoked) reports recall ≥ 0.95 at 100K scale and prints criterion latency stats.
- [ ] Workspace clippy clean.

PLAN READY.
