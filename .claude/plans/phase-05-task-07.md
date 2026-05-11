# Sub-task 5.7 — Throughput benchmark

A criterion benchmark that measures `texts/sec` through the full embedding pipeline (tokenise → forward → CLS pool → L2 normalise) and asserts the spec's minimum throughput target. Also measures cache-hit throughput, which is what callers see when the cue cache (5.5) catches their query.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/03 §6 | "Per-text inference: 5–10 ms" (CPU). Implies ~100–200/s/core single-threaded |
| §04/03 §7 | "Multiple Glommio executors can call inference concurrently. … scales linearly until memory bandwidth saturates (typically ~16 cores)" |
| §04/05 §3 | "Cache-hit latency: < 1 µs (hash + hashmap lookup + comparison). Three orders of magnitude faster than a cache miss" |
| §16/03 §1 | Per-shard ENCODE target: ≥ 5 000 ops/s; bottleneck is WAL fsync, not the embedder |
| §16/03 §6 | RECALL bottleneck: "HNSW search + embedder" |
| Phase doc 5.7 | "≥ 1K texts/sec sustained on reference hardware (best-effort if not available; record baseline)" |
| Orientation plan §0 | "1K+ texts/sec sustained on the reference CPU (with the cache cold)" |

The phase doc + orientation plan converge on **≥ 1 000 texts/sec cold** as the regression floor. Spec §04/03 §6 says 5–10 ms/text single-threaded, so ~100–200/s single-thread is expected — and the 1 000 target therefore implies parallelism (multiple threads) on a multi-core CPU. We measure both single-thread and multi-thread, and assert the spec floor against the *concurrent* measurement.

## 1. Scope

**In scope for 5.7:**
- `benches/throughput.rs` running under criterion. Gated on `BRAIN_EMBED_MODEL_DIR` — without the env var, the bench prints a skip line and exits 0 (criterion handles this via `Criterion::default().final_summary()` plus an early return in the group).
- Three measurements:
  1. **Single-thread `embed`** — single text, hot path. Reports texts/sec from criterion's per-iter latency.
  2. **Concurrent `embed`** — N threads × M iterations of the same text against a shared `CpuDispatcher` (no cache). Reports aggregate texts/sec.
  3. **Cache hit** — wrap `CpuDispatcher` in `CachingDispatcher`, pre-warm with one call, then bench. Validates spec §04/05 §3's "< 1 µs hit" claim is in the right ballpark.
- An assertion that concurrent throughput ≥ 1 000 texts/sec. If we're below, the bench panics (criterion surfaces the panic).
- Baseline numbers recorded in the commit message — the absolute throughput depends on hardware; the bench tracks regressions, the message documents what we saw.

**NOT in scope:**
- Mixed-workload benches (the spec §16/03 §2 mix is a Phase 7+ thing — needs ENCODE/RECALL composed).
- Latency p99 / tail measurements at scale — criterion gives per-iter stats which are enough for ratification; full p-tile analysis is a Phase 11+ observability task.
- Long-input (~500 token) throughput sweep — the determinism test (5.6) exercises long inputs; we can add a long-input bench point as a single criterion group if it's cheap. Decision below in §3.5.
- GPU / batching window benchmarks — those land with GPU support.
- Tokenisation-only or normalise-only micro-benchmarks. Spec §04/02 §6 says "< 2% of the cost"; not worth a separate bench.

## 2. Bench file structure

```rust
// crates/brain-embed/benches/throughput.rs

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use brain_embed::{
    CachingDispatcher, CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle, VECTOR_DIM,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";
const SPEC_FLOOR_TEXTS_PER_SEC: f64 = 1_000.0;
const CONCURRENT_THREADS: usize = 8;
const CONCURRENT_ITERS_PER_THREAD: usize = 64;

fn try_dispatcher() -> Option<CpuDispatcher> { /* env var or skip */ }

fn bench_single_thread(c: &mut Criterion) { /* texts/sec via Throughput::Elements(1) */ }
fn bench_concurrent(c: &mut Criterion) { /* 8 threads; asserts ≥ SPEC_FLOOR */ }
fn bench_cache_hit(c: &mut Criterion) { /* warm cache, then hit; expects < 5 µs */ }

criterion_group! {
    name = throughput;
    config = Criterion::default().measurement_time(Duration::from_secs(8));
    targets = bench_single_thread, bench_concurrent, bench_cache_hit
}
criterion_main!(throughput);
```

Criterion's `Throughput::Elements(N)` makes the report read out as "texts/sec" rather than just nanoseconds-per-iter — useful when reading bench output.

## 3. Implementation decisions

### 3.1 Single-thread bench

A `criterion::Bencher::iter` loop calling `dispatcher.embed("the quick brown fox …")`. `Throughput::Elements(1)` so criterion reports texts/sec. Expected: 100–200/s on the dev CPU per spec §04/03 §6.

### 3.2 Concurrent bench

Criterion's iter-loop runs single-threaded by design — we can't directly use `b.iter(|| ...)` to measure concurrent throughput. The pattern (from brain-index/benches/recall.rs's similar problem with N-query batches): wrap a *fixed batch of work* inside the `iter`, and divide.

Specifically: each criterion iteration spawns `CONCURRENT_THREADS` threads, each running `CONCURRENT_ITERS_PER_THREAD` embeds against a shared `Arc<CpuDispatcher>`. Total work per iter = 8 × 64 = 512 embeds. Criterion reports wall-clock per-iter; throughput = `512 / per_iter_seconds`.

Use `Throughput::Elements(CONCURRENT_THREADS * CONCURRENT_ITERS_PER_THREAD)` so criterion does the division and reports texts/sec directly.

The spec-floor assertion: at the end of the bench group (after criterion's internal warmup + measurement), compute the average texts/sec from criterion's `MeasurementValue` — actually, criterion doesn't expose post-bench numbers cleanly in 0.5. Simpler: outside the criterion harness, run one timed-by-hand pass and assert; criterion measures separately.

**Refined approach**: a non-criterion top-of-bench *floor check* — load the dispatcher, time `CONCURRENT_THREADS * CONCURRENT_ITERS_PER_THREAD` embeds with `Instant::now()` before any criterion call. If the rate is below the floor, panic with a clear message. Then the criterion group runs for detailed numbers. This way the assert is independent of criterion's stats API quirks.

```rust
fn floor_check_or_panic(dispatcher: &Arc<CpuDispatcher>) {
    let start = Instant::now();
    // Run the concurrent pattern once.
    // ...
    let elapsed = start.elapsed();
    let total = (CONCURRENT_THREADS * CONCURRENT_ITERS_PER_THREAD) as f64;
    let rate = total / elapsed.as_secs_f64();
    if rate < SPEC_FLOOR_TEXTS_PER_SEC {
        panic!(
            "concurrent throughput {rate:.0} texts/s < {SPEC_FLOOR_TEXTS_PER_SEC:.0} floor"
        );
    }
    eprintln!("concurrent throughput baseline: {rate:.0} texts/s (≥ {SPEC_FLOOR_TEXTS_PER_SEC:.0} floor)");
}
```

The floor check runs in `bench_concurrent` before criterion's `iter_custom`. If we miss the floor, criterion never reports; we panic.

### 3.3 Cache-hit bench

Wrap a real `CpuDispatcher` in `CachingDispatcher`. Pre-warm by calling `embed(text)` once outside the bench. Inside `b.iter`, call `embed(text)` again — every call is a hit. Spec §04/05 §3 says < 1 µs; we expect criterion to report somewhere in that order (~500 ns – 5 µs depending on mutex + hashmap cost).

No spec floor assertion on cache-hit — we report the number; sub-µs is the target, but BLAKE3 + parking_lot::Mutex + LruCache lookup may put us a few µs over on some hardware. Treat as observational.

### 3.4 Skip handling

Without the env var, all three bench functions print "skipping: set BRAIN_EMBED_MODEL_DIR …" and return. Criterion still completes successfully — the empty group reports nothing, exit code 0.

Implementation:
```rust
fn bench_single_thread(c: &mut Criterion) {
    let Some(dispatcher) = try_dispatcher() else { return };
    // ...
}
```

### 3.5 Long-input bench point

Spec §04/03 §6: "Shorter sequences are faster (fewer attention computations)." A short bench point (a few words) overstates real-world throughput. To keep the bench honest, we add a second single-thread variant on a ~500-token input — that's the *worst-case* throughput. Two criterion groups: `single_thread_short`, `single_thread_long`. The floor assertion uses the short concurrent bench (matching the orientation plan's "1K+ texts/sec sustained" with the cache cold).

This adds 5 s of bench time. Worth it.

### 3.6 No new public API

Bench file only. `Cargo.toml` gets a `[[bench]]` entry; that's it.

### 3.7 Risks

- **Bench exceeds default cargo bench wall time.** Criterion's default measurement is 5 s per group; with 4 groups (short, long, concurrent, cache-hit) and 1-second warmup each, total ~30 s. Acceptable for a phase-exit bench.
- **Floor missed on dev hardware.** If the M1 Pro doesn't hit 1K/s with 8 threads, the panic surfaces a clear baseline number. Decision at runtime: lower the floor or annotate as a known limitation. Spec §04/03 §7's "scales linearly until ~16 cores" implies 1K/s is reachable; on an 8-core M1 we should comfortably make it.
- **`OnceLock` doesn't work cross-bench.** Each criterion bench function is a separate execution path; sharing state is awkward. Solution: each bench function loads its own dispatcher. The model load is ~1.5 s; over 4 bench groups that's 6 s of startup — acceptable.

### 3.8 What "baseline" gets recorded

The commit message documents the numbers we saw on the dev machine:

```
Bench results on dev machine (M1 Pro, 8 cores):
- single_thread_short: ~150 texts/s
- single_thread_long:  ~10 texts/s   (full 512-token forward)
- concurrent_short:    ~1200 texts/s  (8 threads × 64 iters)
- cache_hit:           ~600 ns
```

These are *baselines for regression detection*. Future bench runs that drop > 20% surface a regression worth investigating.

## 4. Files written / changed

```
crates/brain-embed/Cargo.toml                  [edit: + criterion dev-dep, + [[bench]] entry]
crates/brain-embed/benches/throughput.rs       [new]
```

`criterion` is already a workspace dep (used by brain-index, brain-storage). brain-embed just declares it.

## 5. Verify checklist

- `cargo build -p brain-embed --benches` clean.
- `cargo test -p brain-embed` — still 53 passed; bench files aren't included in `cargo test`.
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean (`--all-targets` covers benches).
- `cargo fmt -p brain-embed` no diff.
- `BRAIN_EMBED_MODEL_DIR=… cargo bench -p brain-embed --bench throughput` produces criterion output + the floor-check baseline line. Floor check passes.

## 6. Commit message (draft)

```
bench(brain-embed): throughput baseline + spec floor (sub-task 5.7)

Phase doc 5.7 + orientation plan: ≥ 1 000 texts/sec sustained on
the reference CPU with the cache cold. The bench measures the full
pipeline (tokenise → forward → CLS → normalise) at four points and
asserts the concurrent floor.

benches/throughput.rs (gated on BRAIN_EMBED_MODEL_DIR):
- bench_single_thread_short — single embed of a short text. Reports
  texts/sec per spec §04/03 §6's 5–10 ms/text.
- bench_single_thread_long — single embed of a ~500-token text.
  Worst-case attention compute; honest about the long-text floor.
- bench_concurrent — 8 threads × 64 iters against a shared
  Arc<CpuDispatcher>. Computes aggregate texts/sec; panics if below
  the 1 000/s floor.
- bench_cache_hit — pre-warmed CachingDispatcher; per-call latency
  expected < 5 µs (spec §04/05 §3 cites < 1 µs).

Adds [[bench]] entry to brain-embed Cargo.toml; criterion is already
a workspace dep.

Verify: cargo build/clippy -p brain-embed --all-targets;
BRAIN_EMBED_MODEL_DIR=… cargo bench -p brain-embed --bench throughput.

Baseline numbers from this run are in the commit body for regression
tracking.
```

(The actual numbers go into the commit body after the bench runs; they're not pre-committable.)

## 7. Out-of-scope flags

- No mixed-workload bench (Phase 7+).
- No latency-p99 measurement (Phase 11+ observability).
- No GPU or batching-window benches.
- No tokeniser micro-bench (spec §04/02 §6 says it's < 2% of cost).
- No new public APIs, no new errors.

---

PLAN READY.
