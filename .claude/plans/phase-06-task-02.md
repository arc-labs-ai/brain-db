# Sub-task 6.2 — Cost model + `pick_ef` + budget check

Pure functions, no I/O, no state. Consumed by 6.3 (recall planner) onwards.

Implements spec §08/07 byte-for-byte:
- §1–§2 — cost units + per-operation table.
- §3 — `ann_search_cost(n, ef) = 0.05 + ef * log2(n) * 0.001` formula.
- §4 — `pick_ef` with three biases (memory_count > 1M, tombstone_ratio, filter_selectivity).
- §5 — budget check raising `QueryTooExpensive` above `cost_budget_ms`.
- §8 — cross-shard overhead.
- §9 — `is_simple_recall` fast-path detection.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/07 §1 | Time is the primary cost unit; ms |
| §08/07 §2 | Per-op coefficients table — pin as constants |
| §08/07 §3 | `cost_recall` formula; `ann_search_cost(n, ef) = 0.05 + ef * log_2(n) * 0.001` |
| §08/07 §4 | `pick_ef` rules: baseline 64; ≥ 100 when n > 1M; * (1 + ratio*5) when tombstoned; / selectivity when filtered; cap at `max_ef_search` |
| §08/03 §5 | `over_factor = (1.0 / selectivity).max(1.0).min(8.0)`; `candidates = k * over_factor`; cap at 1000 |
| §08/07 §5 | budget: > 1000 ms → `QueryTooExpensive`; > 100 ms → warn-log |
| §08/07 §8 | cross-shard: `per_shard + 0.05 merge + 0.1 * n_shards serialisation` |
| §08/07 §9 | fast-path: `is_simple = k <= 20 && filter.is_minimal() && consistency == Eventual` |
| §08/07 §11 | Accuracy ±20% for simple queries; OK for parameter picking |
| §08/07 §15 | Intentional simplicity — no ML, no probing, no calibration |

## 1. Scope

**In scope for 6.2:**
- `crates/brain-planner/src/cost.rs` — pure functions only.
- Constants for every coefficient in the spec §07 §2 table, each named after the operation and tagged `pub(crate)` (the test suite reads them, but the public API is the functions, not the numbers).
- Per-operation cost functions:
  - `embedding_cost(cache_hit: bool) -> f32`
  - `ann_search_cost(memory_count: u64, ef: usize) -> f32`
  - `metadata_point_lookup_cost() -> f32`
  - `metadata_range_scan_cost(rows: usize) -> f32`
  - `wal_append_fsync_cost() -> f32`
  - `arena_io_cost() -> f32`
  - `hnsw_insert_cost() -> f32`
  - `cross_shard_overhead(n_shards: usize) -> f32`
- Selectivity helper: `estimate_filter_selectivity(rules: &[FilterRule]) -> f32` — hand-tuned per-variant coefficients, products bounded to `[0.001, 1.0]`.
- `over_factor(selectivity: f32) -> f32` — spec §03 §5's `(1/sel).max(1).min(8)`.
- `pick_ef(k: usize, selectivity: f32, ctx: &PlannerContext) -> usize` — spec §07 §4.
- Per-request total-cost functions:
  - `cost_recall(...) -> f32`
  - `cost_encode(cache_hit: bool, edge_count: usize) -> f32`
  - `cost_forget(hard: bool) -> f32`
  - `cost_path_placeholder()` / `cost_reason_placeholder()` — shells; 6.5 owns the real formulas.
- Budget check: `check_budget(estimated_cost_ms: f32, ctx: &PlannerContext) -> Result<(), PlanError>`.
- Fast-path detection: `is_simple_recall(...) -> bool`.
- Comprehensive unit tests against the spec values.

**NOT in scope:**
- Any *consumer* of the cost model — that's 6.3 onwards.
- Calibration / adaptive coefficients (spec §07 §14–§15 explicitly defer).
- Per-shard cost variation logic beyond what `ShardStats` already exposes (spec §07 §7 — handled inside `pick_ef`).
- Memory / disk-I/O cost units. Spec §07 §1 names them but §07 §3 onwards uses only time. We do the same.

## 2. Module surface

```rust
// crates/brain-planner/src/cost.rs

use crate::context::PlannerContext;
use crate::error::PlanError;
use crate::plan::FilterRule;

// ---------------------------------------------------------------------------
// Coefficients pinned from spec §08/07 §2's table.
// ---------------------------------------------------------------------------

pub(crate) const EMBED_CACHE_HIT_MS: f32           = 0.005;
pub(crate) const EMBED_CACHE_MISS_MS: f32          = 7.5;  // mid of 5–10 ms
pub(crate) const ANN_SEARCH_BASELINE_MS: f32       = 0.05;
pub(crate) const ANN_SEARCH_PER_EF_LOGN_MS: f32    = 0.001;
pub(crate) const METADATA_POINT_LOOKUP_MS: f32     = 0.005;
pub(crate) const METADATA_RANGE_SCAN_PER_ROW_MS: f32 = 0.0004; // 30–50 µs / 100 rows ≈ 0.4 µs / row
pub(crate) const WAL_FSYNC_GROUP_MS: f32           = 0.3;
pub(crate) const ARENA_IO_MS: f32                  = 0.001;
pub(crate) const HNSW_INSERT_MS: f32               = 1.25; // mid of 0.5–2 ms
pub(crate) const NETWORK_INTRA_SHARD_MS: f32       = 0.1;

// ---------------------------------------------------------------------------
// Per-operation cost.
// ---------------------------------------------------------------------------

pub fn embedding_cost(cache_hit: bool) -> f32;
pub fn ann_search_cost(memory_count: u64, ef: usize) -> f32;
pub fn metadata_point_lookup_cost() -> f32;
pub fn metadata_range_scan_cost(rows: usize) -> f32;
pub fn wal_append_fsync_cost() -> f32;
pub fn arena_io_cost() -> f32;
pub fn hnsw_insert_cost() -> f32;
pub fn cross_shard_overhead(n_shards: usize) -> f32;

// ---------------------------------------------------------------------------
// Selectivity + ef picking.
// ---------------------------------------------------------------------------

/// Crude per-rule heuristic. Spec §08/07 §15 says simple is good
/// enough; we ship hand-tuned coefficients and let 6.3+ feed them in.
pub fn estimate_filter_selectivity(rules: &[FilterRule]) -> f32;

/// Spec §08/03 §5: `over_factor = (1/selectivity).clamp(1, 8)`.
pub fn over_factor(selectivity: f32) -> f32;

/// Spec §08/07 §4.
pub fn pick_ef(k: usize, selectivity: f32, ctx: &PlannerContext) -> usize;

// ---------------------------------------------------------------------------
// Per-request total cost.
// ---------------------------------------------------------------------------

pub fn cost_recall(
    k: usize,
    selectivity: f32,
    cache_hit: bool,
    ctx: &PlannerContext,
) -> f32;

pub fn cost_encode(cache_hit: bool, edge_count: usize) -> f32;
pub fn cost_forget(hard: bool) -> f32;

/// Placeholder; 6.5 owns the real formula.
pub fn cost_path_placeholder() -> f32;
pub fn cost_reason_placeholder() -> f32;

// ---------------------------------------------------------------------------
// Budget + fast-path.
// ---------------------------------------------------------------------------

/// Spec §08/07 §5. Hard error above `ctx.config.cost_budget_ms`.
/// Warn-log threshold (100 ms) is hardcoded per spec; not configurable.
pub fn check_budget(estimated_cost_ms: f32, ctx: &PlannerContext) -> Result<(), PlanError>;

/// Spec §08/07 §9.
pub fn is_simple_recall(k: usize, no_filter: bool, eventual_consistency: bool) -> bool;
```

Re-exports from `lib.rs` are minimal — most call sites are inside the crate (6.3–6.6). Public API: `pick_ef`, `over_factor`, `cost_recall`/`cost_encode`/`cost_forget`, `check_budget`, `is_simple_recall`, `estimate_filter_selectivity`.

## 3. Implementation decisions

### 3.1 `ann_search_cost` formula

Spec §07 §3 is explicit:

```
ann_search_cost(n, ef) = 0.05 + ef * log2(n) * 0.001
```

For `n = 0` (empty shard, log2(0) = -∞), we clamp the log term:

```rust
let log_n = if memory_count <= 1 { 0.0 } else { (memory_count as f32).log2() };
```

`memory_count <= 1` covers the bench/test cases where the shard is empty or has one entry; `log2(1) = 0`, `log2(0)` is undefined. Either reading produces 0.0, and that's what we want — an empty shard's ANN search is essentially the baseline.

### 3.2 `pick_ef` rule order

Spec §07 §4 pseudocode:

```
baseline = 64
if n > 1M:           ef = max(ef, 100)
if tombstone_ratio > 0:  ef = ef * (1 + ratio * 5)
if selectivity < 0.5:    ef = ef / selectivity
return min(ef, max_ef_search)
```

We apply the spec literally, except: the tombstone bias uses `> 0.0`, not `> 0.1` (the spec is internally inconsistent — §03 §4 says no tombstone branch, §07 §4 says yes). Spec wins: use `§07 §4`'s wording. With `tombstone_ratio = 0`, the multiplier is `1.0` (no-op) so the branch is effectively gated on a non-zero ratio anyway.

Also: the spec doesn't say *which* selectivity to use when both K and filter push ef up. We multiply biases (don't take max), then cap. Document this as a deliberate reading.

For the K-large case (spec §03 §4 `ef = max(ef, K * 4)`), we add that branch too — even though §07 §4 omits it, the recall planner section is normative for the recall planner.

### 3.3 `over_factor` clamping

Spec §03 §5: `(1.0 / selectivity).max(1.0).min(8.0)`. We use `f32::clamp(1.0, 8.0)` for clarity. The lower bound `1.0` matters: if selectivity > 1 (impossible but defensive), we'd otherwise get a sub-1 factor.

Guard against `selectivity <= 0.0` (would divide by zero). We clamp the *input* selectivity to `[1e-3, 1.0]` before dividing — keeps the output bounded and matches `estimate_filter_selectivity`'s clamping.

### 3.4 `estimate_filter_selectivity` heuristics

Hand-tuned per the spec's "simple is good enough" line (§07 §15). Each rule maps to a single selectivity factor; the product is the overall estimate:

| FilterRule | Per-rule selectivity heuristic | Rationale |
|---|---|---|
| `KindIn(v)` | `v.len() as f32 / 3.0` | 3 MemoryKind variants; assume uniform |
| `ContextIn(v)` | `(v.len() as f32 / 10.0).min(1.0)` | rough: 10 typical contexts per agent |
| `SalienceFloor(f)` | `(1.0 - f).max(0.05)` | most memories above 0.5; floor at 5% |
| `AgeBound { .. }` | `0.5` | half-window default; can be refined later |
| `ConfidenceFloor(f)` | `(1.0 - f).max(0.05)` | similar to salience |

Product clamped to `[0.001, 1.0]`. An empty rule list → `1.0` (no filter).

These numbers are intentionally crude. The unit tests assert the *bounds* and the *ordering* (more rules ⇒ lower selectivity), not specific magic values. If 6.3+ shows the heuristic is wrong, we update here.

### 3.5 `cost_recall` formula

Spec §07 §3 pseudocode, transliterated:

```rust
pub fn cost_recall(k, selectivity, cache_hit, ctx) -> f32 {
    let n = ctx.stats.memory_count;
    let ef = pick_ef(k, selectivity, ctx);
    let factor = over_factor(selectivity);
    let candidates = ((k as f32) * factor) as usize;
    let candidates = candidates.min(ctx.config.max_candidates_per_search);

    let mut ms = 0.0;
    ms += embedding_cost(cache_hit);
    ms += ann_search_cost(n, ef);
    ms += (k as f32) * metadata_point_lookup_cost();
    if !rules_were_empty {                                    // ← caller passes a flag
        ms += (candidates as f32) * metadata_point_lookup_cost();
    }
    ms
}
```

We split into two function calls — `cost_recall` takes `selectivity` (which encodes whether filters apply) and *also* `has_filter: bool` for the optional filter-cost term. Slight redundancy, but clearer than threading rule lists through the cost function.

Actually we can derive `has_filter` from `selectivity < 1.0` since the empty-rules case returns exactly 1.0. We do that to keep the signature small.

### 3.6 `cost_encode` formula

Sum of spec §04 §16's table:

```
idempotency:  0.0075 ms  (5-10 µs)
embed:        7.5 ms (miss) or 0.005 ms (hit)
context:      0.005 ms
slot alloc:   0.001 ms
WAL fsync:    0.3 ms (group)
apply:        ARENA_IO_MS + 0.5 ms (metadata write) + HNSW_INSERT_MS
edges/each:   0.05 ms (in same metadata txn so amortised; 10 edges ≈ 0.5 ms)
response:     0.05 ms
```

We bake the constants into `cost_encode`:

```rust
pub fn cost_encode(cache_hit: bool, edge_count: usize) -> f32 {
    0.0075                            // idempotency
    + embedding_cost(cache_hit)
    + 0.005                           // context
    + 0.001                           // slot alloc
    + wal_append_fsync_cost()
    + arena_io_cost()
    + 0.5                             // metadata write
    + hnsw_insert_cost()
    + (edge_count as f32) * 0.05
    + 0.05                            // response
}
```

### 3.7 `check_budget` warning behaviour

Spec §07 §5 says "log_warning('Slow query plan', req)" above 100 ms. We don't have a logging stream wired here. Options:

- Use `tracing::warn!` directly. Requires `tracing` as a dep (already a workspace dep; add to brain-planner Cargo.toml).
- Return a `Result<bool, PlanError>` where the bool means "was warned" — over-engineered.
- Just return `Result<(), PlanError>` and accept that 6.7's executor instrumentation will log latency at the request level; the planner skips the warn step for now.

**Choice: use `tracing::warn!` directly.** Adds `tracing` to brain-planner's deps (small; already a workspace dep). The 100 ms warn threshold is a named constant `BUDGET_WARN_MS = 100.0`, hardcoded per spec.

### 3.8 `is_simple_recall` boolean test

Spec §07 §9:

```rust
fn is_simple(req: &RecallRequest) -> bool {
    req.k <= 20
        && req.filter.is_minimal()
        && req.consistency == Consistency::Eventual
}
```

We don't yet have the request struct shape pinned in the planner (that's wire-level, handled in 6.3). For 6.2 we expose primitives:

```rust
pub fn is_simple_recall(k: usize, no_filter: bool, eventual_consistency: bool) -> bool {
    k <= 20 && no_filter && eventual_consistency
}
```

6.3 builds the boolean inputs from the actual `RecallRequest`. This keeps 6.2 free of any wire-type dependency beyond what's already imported (`FilterRule`).

### 3.9 Why no per-shard cost variation here

Spec §07 §7 mentions "different shards have different costs". `ShardStats.memory_count` already carries per-shard variation; `cost_recall` reads it via `ctx.stats`. So per-shard variation is automatic — no extra plumbing needed.

### 3.10 Test strategy

Three layers:

1. **Constant sanity** — every `pub(crate) const` falls in the spec's range. Quick sanity that we haven't typo'd a coefficient.
2. **Function determinism** — same input → same output. f32 ops are deterministic.
3. **Spec-pinned values** — `pick_ef` returns 64 for the default (K=10, sel=1, default stats); returns ≥ 100 for `memory_count = 2_000_000`; respects `max_ef_search` cap; bias multiplies (not maxes) when both tombstones + filters apply. `ann_search_cost(1_000_000, 64) ≈ 1.33 ms` per spec §07 §2's "HNSW search (1M, ef=64): 1-2 ms".
4. **Budget check** — `cost > budget` → `PlanError::QueryTooExpensive { estimated_ms, budget_ms }`.
5. **`over_factor` clamping** — selectivity = 0.5 → factor = 2.0; selectivity = 0.1 → factor = 8.0 (capped); selectivity = 1.5 → factor = 1.0 (also clamped); selectivity = 0.0 → uses `1e-3` floor → factor = 8.0.
6. **`estimate_filter_selectivity`** — empty rules → 1.0; one `KindIn` of 1 variant → 0.33; two rules → product (no double-counting).

## 4. Risks

- **Constant drift**: spec §07 §2 may evolve; our coefficients become stale. Mitigation: tests pin them; spec changes flag the diff loudly.
- **`f32` log accuracy**: `(2_000_000u64 as f32).log2()` is fine (no overflow up to ~2^24). For shards over 16M memories we'd start losing precision — irrelevant at v1 scale.
- **Negative cost from over-multiplied biases**: `pick_ef`'s multiplications can blow up if `tombstone_ratio = 0.9` and `selectivity = 0.01`. The `min(max_ef_search)` cap saves us; we add an assertion in tests.

## 5. Files written / changed

```
crates/brain-planner/Cargo.toml          [edit: + tracing.workspace = true]
crates/brain-planner/src/cost.rs         [new]
crates/brain-planner/src/lib.rs          [edit: pub mod cost + selective re-exports]
```

`tracing` is already a workspace dep — just declare it in brain-planner.

## 6. Verify checklist

- `cargo build -p brain-planner` clean.
- `cargo test -p brain-planner` — existing 6 + ~15 new (one per function + budget + clamping + spec-pinned values).
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 7. Commit message (draft)

```
feat(brain-planner): cost model + pick_ef + budget check (sub-task 6.2)

Pure functions; consumed by 6.3–6.6's planners. Implements spec
§08/07 byte-for-byte:

- Coefficients pinned as crate-private consts (spec §07 §2 table).
- ann_search_cost(n, ef) = 0.05 + ef * log2(n) * 0.001 (spec §07 §3).
- pick_ef rules: baseline 64; ≥100 when n>1M; * (1 + ratio*5) when
  tombstoned; / selectivity when filtered; cap at max_ef_search
  (spec §07 §4). Also the spec §03 §4 K-large branch (max with K*4).
- over_factor = (1/selectivity).clamp(1, 8) per spec §03 §5.
- estimate_filter_selectivity: hand-tuned per-FilterRule coefficients
  multiplied; product clamped to [0.001, 1.0]. Crude on purpose
  (spec §07 §15: simplicity priority).
- cost_recall / cost_encode / cost_forget: sum of phase costs from
  spec §04 §16 + §07 §3.
- check_budget raises PlanError::QueryTooExpensive above
  ctx.config.cost_budget_ms; tracing::warn! above 100 ms warn floor.
- is_simple_recall(k, no_filter, eventual_consistency) → bool, the
  fast-path predicate (spec §07 §9).
- Placeholders for cost_path / cost_reason; 6.5 fills them.

New brain-planner dep declaration: tracing (workspace dep already).

Verify: cargo build/test/clippy -p brain-planner.
```

## 8. Out-of-scope flags

- No calibration loop. Spec §07 §14 explicitly defers.
- No memory / disk-I/O cost units. §07 §1 names them; §07 §3 uses only time.
- No real `cost_path` / `cost_reason`. 6.5 owns them; we ship constant placeholders so 6.7's executor signature stabilises.
- No request-struct knowledge. 6.3 maps the wire `RecallRequest` to the primitives this module exposes.

---

PLAN READY.
