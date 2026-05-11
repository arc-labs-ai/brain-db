# Sub-task 6.3 — Recall planner + Recall executor

The first sub-task with real logic. Wires 6.1's `RecallPlan` + 6.2's `cost_recall` / `pick_ef` / `is_simple_recall` to produce a plan from a wire `RecallRequest`, then executes it end-to-end across `brain-embed` (5.4 `Dispatcher`) + `brain-index` (4.8 `SharedHnsw`) + `brain-metadata` (Phase 3).

This is the longest 6.x sub-task by code volume. Decisions made here pin patterns that 6.4–6.6 reuse: how the planner maps wire types, how the executor structures its async stages, how the test harness composes the real storage components.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/03 §1 | `RecallRequest { cue_text, agent_id, k, filter, confidence_min, include_text, include_metadata, consistency, request_id }` |
| §08/03 §2 | Single-shard for typical agents (Phase 12 adds cross-shard) |
| §08/03 §3 | Embed once, reuse vector across shards |
| §08/03 §4 | `pick_ef` rules — already in 6.2 |
| §08/03 §5 | `candidates_to_request = k * over_factor`, capped at 1000 |
| §08/03 §6 | PreFilter (tombstone, fingerprint) vs PostFilter (kind, context, salience, age, confidence) |
| §08/03 §7 | `confidence_min` is a post-merge filter |
| §08/03 §11 | `include_text` toggles a metadata text-fetch step |
| §08/03 §12 | Full plan example — already mirrored in 6.1's `RecallPlan` shape |
| §08/03 §13 | Plan validity: `ef ≥ K`, `ef ≤ max_ef_search`, `candidates ≤ 1000`, valid filter rules |
| §08/08 §2 | `execute_recall` orchestration: embed → search → merge → text_fetch → response |
| §08/08 §3 | Cooperative yields during long ops (deferred until 6.7 wires Glommio) |
| §08/08 §4 | `?` propagates errors; on failure cancel + build error response |

## 1. Scope

**In scope for 6.3:**
- `crates/brain-planner/src/recall.rs` — the planner side. Takes `&brain_protocol::request::RecallRequest` + `&PlannerContext` → `Result<RecallPlan, PlanError>`.
- `crates/brain-planner/src/executor/mod.rs` + `executor/recall.rs` — the executor side. Takes a `RecallPlan` + an `ExecutorContext` → `Result<RecallResult, ExecError>`.
- `ExecutorContext` struct (in `executor/context.rs`) — bag of `Arc<dyn Dispatcher>`, `SharedHnsw<384>`, `Arc<MetadataDb>`. The full struct will grow over 6.4–6.7; we ship what 6.3 needs.
- `ExecError` enum (in `executor/error.rs`) — variants needed by Recall: `EmbedFailed`, `IndexSearchFailed`, `MetadataReadFailed`, `MemoryNotFound`, plus a forward-compat `Internal`. 6.4+ extend.
- `RecallResult` struct + `RecallHit` row — the planner-side result type. Phase 9 wraps this into a wire `RecallResponseBody`; for now it's a Rust-side value.
- Planner unit tests: a few `RecallRequest` shapes → expected `RecallPlan` shapes. No I/O.
- Executor integration tests: real `MetadataDb` (via `tempdir`), real `SharedHnsw`, mock `Dispatcher` (returns deterministic vectors per text). End-to-end recall against an in-memory pre-populated index. Asserts ordering, K, `confidence_min` filter, `include_text` text-fetch.
- One BGE-gated integration test using `CpuDispatcher` for the embedder.

**NOT in scope:**
- `WriterHandle` trait — comes in 6.4 (encode needs writes; recall doesn't).
- Cross-shard fan-out. The `shards` Vec is always length 1 per orientation §4.7.
- `Consistency::ReadAfterWrite` — spec §03 §10 needs WAL LSN waiting; deferred until 6.7 ties LSN signalling together. For now, we accept either consistency value but only the eventual path is exercised; strong-consistency returns `PlanError::Unsupported`.
- Plan caching (spec §03 §14). Deferred.
- `ADMIN_EXPLAIN_PLAN` opcode (spec §03 §15). 6.8 builds the pretty-tree; opcode wiring is Phase 9.
- Cooperative yields. The async functions don't `.await` inside hot loops yet — fine until 6.7 picks the runtime.

## 2. Module layout

```
crates/brain-planner/src/
├── recall.rs                       [new — planner side]
└── executor/
    ├── mod.rs                      [new — re-exports]
    ├── context.rs                  [new — ExecutorContext]
    ├── error.rs                    [new — ExecError]
    ├── result.rs                   [new — RecallResult, RecallHit]
    └── recall.rs                   [new — executor side]
```

Plus `lib.rs` declares the modules + re-exports.

Integration tests:
```
crates/brain-planner/tests/
└── recall_end_to_end.rs            [new — real MetadataDb + real SharedHnsw + mock Dispatcher]
```

A BGE-gated test lives in the same file behind the `BRAIN_EMBED_MODEL_DIR` env var.

## 3. Planner-side design

### 3.1 Function signature

```rust
// crates/brain-planner/src/recall.rs

pub fn plan_recall(
    req: &brain_protocol::request::RecallRequest,
    ctx: &PlannerContext,
) -> Result<RecallPlan, PlanError>;
```

Pure. Single-shard. No async, no I/O.

### 3.2 Validation

Per spec §03 §13:
- `req.top_k > 0` — else `InvalidParameters { field: "top_k", reason: "must be > 0" }`.
- `req.top_k as usize <= ctx.config.max_k` — else `InvalidParameters`.
- `req.confidence_threshold ∈ [0, 1]` — else `InvalidParameters`.
- `req.salience_floor ∈ [0, 1]` — else `InvalidParameters`.
- Strong consistency is not yet supported. (`RecallRequest` doesn't have a `consistency` field on the wire yet — checked: spec §03 §1 lists it, but `brain-protocol`'s `RecallRequest` doesn't. So Phase 6 doesn't need to handle it; future wire bump adds it. No explicit check needed.)

### 3.3 FilterRule construction

Translate wire fields → `FilterRule` list:

| Wire field | Filter rule | Stage |
|---|---|---|
| `req.kind_filter: Option<Vec<MemoryKindWire>>` (if `Some` and non-empty) | `FilterRule::KindIn(kinds)` | PostFilter |
| `req.context_filter: Option<Vec<WireContextId>>` (if `Some`) | `FilterRule::ContextIn(ctx_ids)` | PostFilter |
| `req.salience_floor: f32` (if > 0) | `FilterRule::SalienceFloor(s)` | PostFilter |
| `req.age_bound_unix_nanos: Option<u64>` (if `Some`) | `FilterRule::AgeBound { not_older_than_unix_nanos }` | PostFilter |
| `req.confidence_threshold: f32` (if > 0) | applied at merge (spec §03 §7), not as a filter rule. Stored as `merge.confidence_min` |

`MemoryKindWire` → `MemoryKind` and `WireContextId` → `ContextId` conversions live in `brain-protocol::convert` (already shipped in Phase 1).

### 3.4 ef + over_factor + candidates

```rust
let post_rules: Vec<FilterRule> = build_filter_rules(req);
let selectivity = cost::estimate_filter_selectivity(&post_rules);
let ef = cost::pick_ef(req.top_k as usize, selectivity, ctx);
let factor = cost::over_factor(selectivity);
let candidates = ((req.top_k as f32 * factor) as usize)
    .min(ctx.config.max_candidates_per_search);
```

Validate: `ef >= req.top_k as usize` (spec §03 §13). If `pick_ef` returned below K (impossible given the spec rules; defensive), promote to K.

### 3.5 Cost + budget

```rust
let cache_hit_estimate = false; // The planner is pessimistic about cache; spec §07 §3 averages.
let estimated = cost::cost_recall(req.top_k as usize, selectivity, cache_hit_estimate, ctx);
cost::check_budget(estimated, ctx)?;
```

### 3.6 Plan assembly

```rust
RecallPlan {
    embedding: EmbeddingStep {
        text: req.cue_text.clone(),
        cache_lookup: true,
    },
    shards: vec![ShardSearchStep {
        shard_id: 0u16, // single shard for v1
        ann_search: AnnSearchStep { ef, candidates_to_request: candidates, pre_filter: vec![] },
        metadata_lookup: MetadataLookupStep { include_extra: req.include_edges },
        filter_apply: FilterStep { stage: FilterStage::PostFilter, rules: post_rules },
    }],
    merge: MergeStep {
        sort_by: SortKey::Score,
        final_top: req.top_k as usize,
        confidence_min: if req.confidence_threshold > 0.0 {
            Some(req.confidence_threshold)
        } else {
            None
        },
    },
    text_fetch: None, // 6.3 doesn't materialise text yet; deferred to 6.4-style; see §3.7
    response: ResponseStep {
        include_text: false,
        include_metadata: req.include_edges,
    },
    estimated_cost_ms: estimated,
}
```

### 3.7 What about `include_text`?

`RecallRequest` carries `include_vectors: bool` and `include_edges: bool` but **no `include_text`** in the current wire shape. Spec §03 §11 says there should be one; the wire shape doesn't yet have it. For 6.3, `text_fetch: None` always. When the wire adds the field, the plan picks it up — no breaking change.

`response.include_metadata = req.include_edges` is a reasonable proxy until the wire splits text from metadata. Document.

## 4. Executor-side design

### 4.1 `ExecutorContext`

```rust
// crates/brain-planner/src/executor/context.rs

#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw<384>,
    pub metadata: Arc<MetadataDb>,
    // Phase 6.4+ will add: writer: Arc<dyn WriterHandle>, arena: Arc<Arena>.
    // 6.7's full ExecutorContext is the union.
}
```

We don't include `WriterHandle` or `Arena` yet — recall is read-only. Adding them later is a struct field addition, not a signature break.

### 4.2 Function signature

```rust
// crates/brain-planner/src/executor/recall.rs

pub async fn execute_recall(
    plan: RecallPlan,
    ctx: &ExecutorContext,
) -> Result<RecallResult, ExecError>;
```

Async (per spec §08 §1). Owns the plan (consumed; no need to copy substeps).

### 4.3 Stages

Mirror spec §08/08 §2:

1. **Embed**. `let vector = ctx.embedder.embed(&plan.embedding.text)?;` — synchronous under the trait surface (5.4 chose sync API); we wrap in `tokio::task::spawn_blocking` later when the runtime is wired (6.7). For now, the call is sync and the `async fn` adds no `.await` here.

2. **Per-shard search**. Single shard. Acquire a read guard on the `SharedHnsw`:

   ```rust
   let hits = ctx.index.search_active(&vector, candidates_to_request, ef);
   ```

   `search_active` already skips tombstoned slots — that's the PreFilter step the spec mentions (tombstone bitmap filter is automatic).

   Returns `Vec<(MemoryId, f32)>` (id + distance/similarity). Spec §06 has cosine distance ≈ 1 − dot product; for unit vectors the score is the dot product.

3. **Metadata lookup**. For each hit, open a read txn on `MetadataDb` and fetch `MemoryMetadata`:

   ```rust
   let txn = ctx.metadata.read_txn()?;
   let table = txn.open_table(MEMORIES_TABLE)?;
   for (mid, _score) in &raw_hits {
       let meta = table.get(mid.as_bytes())?...
   }
   ```

   Spec §08 §3 says yield every ~100 rows — we skip the yield for now (no runtime), but structure the loop so adding it is one line.

4. **Apply post-filter rules**. For each (hit, metadata) pair, evaluate every `FilterRule`. Drop misses.

5. **Merge / sort / trim**. Single shard, so "merge" is a no-op. Sort by score descending; if `merge.confidence_min` is set, drop hits with `score < threshold`; take first `merge.final_top`.

6. **Text fetch**. Skipped for 6.3 (`plan.text_fetch == None`).

7. **Build `RecallResult`**.

### 4.4 `RecallResult` and `RecallHit`

```rust
// crates/brain-planner/src/executor/result.rs

#[derive(Debug, Clone)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
}

#[derive(Debug, Clone)]
pub struct RecallHit {
    pub memory_id: brain_core::MemoryId,
    pub score: f32,                 // similarity (higher = better)
    pub kind: brain_core::MemoryKind,
    pub context_id: brain_core::ContextId,
    pub salience: f32,
    pub created_at_unix_nanos: u64,
    pub text: Option<String>,        // None until 6.x adds include_text
}
```

Phase 9's server maps `RecallResult` → wire `ResponseBody::Recall(...)`. For Phase 6 the Rust value is the integration test's assertion target.

### 4.5 `ExecError`

```rust
// crates/brain-planner/src/executor/error.rs

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("embedding failed: {0}")]
    EmbedFailed(#[from] brain_embed::EmbedError),

    #[error("ANN search failed: {0}")]
    IndexSearchFailed(String),

    #[error("metadata read failed: {0}")]
    MetadataReadFailed(String),

    /// HNSW returned a MemoryId not present in the metadata. Indicates
    /// a desync between the two stores — surface loudly.
    #[error("metadata missing for HNSW hit: {memory_id:?}")]
    MemoryNotFound { memory_id: brain_core::MemoryId },

    /// Catch-all for not-yet-supported request shapes encountered at
    /// execute-time (different from `PlanError::Unsupported`, which
    /// fires at plan-time).
    #[error("unsupported at execute-time: {0}")]
    Unsupported(&'static str),

    /// Catch-all for internal bookkeeping errors.
    #[error("internal executor error: {0}")]
    Internal(String),
}
```

We don't `#[from]`-wrap `brain_metadata`'s errors yet because Phase 3 didn't expose a typed error in the read-txn path consistently; we keep it as a `String` for now and tighten when we know the shape.

## 5. Test plan

### 5.1 Pure planner tests

- `plan_recall_with_default_request` — builds a plan with ef=64, no filters, no confidence threshold.
- `plan_recall_rejects_zero_k` — `InvalidParameters { field: "top_k" }`.
- `plan_recall_rejects_k_over_max` — `InvalidParameters`.
- `plan_recall_picks_higher_ef_for_filters` — `kind_filter` lowers selectivity ⇒ `pick_ef` raises ef.
- `plan_recall_confidence_threshold_maps_to_merge` — `req.confidence_threshold = 0.7` ⇒ `merge.confidence_min = Some(0.7)`.
- `plan_recall_filter_rules_built` — `req.salience_floor = 0.5` + `kind_filter = [Episodic]` ⇒ two `FilterRule`s in the PostFilter step.

### 5.2 Executor integration tests

Harness:
- `MetadataDb::open(tempdir)`. Populate the `memories` + (optional) `texts` tables with N rows.
- `SharedHnsw::<384>::new(default_params)`. Insert N (`MemoryId`, vector) pairs via the `Writer`.
- A `MockDispatcher` (pattern from 6.5 of brain-embed): deterministic per-text vector. Used to embed the cue.

Tests:
- `recall_returns_top_k_in_score_order` — query a cue that's close to a known cluster; assert hit ordering matches expected.
- `recall_respects_top_k_limit` — request K=3 against 10 candidates; result has 3.
- `recall_filters_by_confidence` — `confidence_threshold = 0.99` against varied scores; only the very-top hits survive.
- `recall_post_filter_drops_wrong_kind` — `kind_filter = [Semantic]` but data is all `Episodic` ⇒ empty result.
- `recall_metadata_includes_kind_salience_context` — hits carry the metadata.

### 5.3 BGE-gated test (one)

`recall_with_real_embedder_end_to_end` — uses `CpuDispatcher`, embeds a few real texts, indexes them, queries with one of them, asserts it's the top hit. Gated on `BRAIN_EMBED_MODEL_DIR`.

## 6. Dependencies

`brain-planner/Cargo.toml` additions:
- `brain-embed = { path = "../brain-embed" }` (for `Dispatcher`)
- `brain-index = { path = "../brain-index" }` (for `SharedHnsw`)
- `brain-metadata = { path = "../brain-metadata" }` (for `MetadataDb`)
- `redb.workspace = true` (only if needed for table-open helpers; check during implementation)

Dev-deps:
- `tempfile.workspace = true` for the metadata + index test harness.

## 7. Risks

- **Metadata read API friction.** `MetadataDb::read_txn` returns a redb `ReadTransaction`; opening tables, decoding keys, etc. has a specific shape. We may need a thin helper inside `brain-metadata` to look up a memory by `MemoryId` — if the existing API forces boilerplate at the call site, we add a `MemoryReader` shim in 6.3 and remember to upstream it. Decision at implementation.
- **`SharedHnsw::search_active` return type.** Verify it returns `(MemoryId, f32)` and what the f32 represents (distance vs similarity). Adjust the comparator accordingly.
- **MockDispatcher placement.** Could live in `brain-embed::cache::tests` (already exists), but tests there are private. We replicate the small mock pattern inside `brain-planner/tests/recall_end_to_end.rs`. Five lines.
- **Async without a runtime.** The executor is `async fn` but the body has no `.await` (sync calls behind Dispatcher + sync HNSW + sync metadata reads). Tests use `futures::executor::block_on` or simply call `.now_or_never()` since the futures complete synchronously. Or — cleaner — add `tokio = { version = "1", features = ["macros", "rt"] }` as a dev-dep + use `#[tokio::test]`. Phase 9 picks the production runtime; the tests don't constrain that choice.
- **`brain-metadata::MetadataDb` requires `&mut self` for `write_txn`** but `read_txn` is `&self` — good; we share an `Arc<MetadataDb>` across executor tasks for reads. Confirm at build time.

## 8. Files written / changed

```
crates/brain-planner/Cargo.toml                         [edit: +3 deps + tokio dev-dep]
crates/brain-planner/src/lib.rs                         [edit: mod + re-exports]
crates/brain-planner/src/recall.rs                      [new]
crates/brain-planner/src/executor/mod.rs                [new]
crates/brain-planner/src/executor/context.rs            [new]
crates/brain-planner/src/executor/error.rs              [new]
crates/brain-planner/src/executor/result.rs             [new]
crates/brain-planner/src/executor/recall.rs             [new]
crates/brain-planner/tests/recall_end_to_end.rs         [new]
```

## 9. Verify checklist

- `cargo build -p brain-planner` clean.
- `cargo test -p brain-planner` — existing 29 + ~6 planner units + ~5 executor integration tests + 1 ignored BGE.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 10. Commit message (draft)

```
feat(brain-planner): Recall planner + executor (sub-task 6.3)

First sub-task with real logic. Maps a wire RecallRequest → RecallPlan
through 6.2's cost model, then executes it against the real
Dispatcher + SharedHnsw + MetadataDb.

Planner (recall.rs):
- plan_recall(&RecallRequest, &PlannerContext) → Result<RecallPlan>.
- Validates top_k ∈ (0, max_k], confidence_threshold + salience_floor
  ∈ [0, 1]; returns PlanError::InvalidParameters otherwise.
- Builds FilterRule list from kind/context/salience/age filters
  (PostFilter stage per spec §03 §6).
- Picks ef + candidates via 6.2's pick_ef + over_factor.
- Estimates cost via cost_recall; cost_budget check raises
  QueryTooExpensive when over budget.
- Single shard, single ShardSearchStep (Phase 12 adds fan-out).

Executor (executor/recall.rs + executor/{context,error,result}.rs):
- ExecutorContext { embedder: Arc<dyn Dispatcher>, index: SharedHnsw,
  metadata: Arc<MetadataDb> }. 6.4 adds writer/arena.
- execute_recall: embed → search_active → metadata lookup → post-
  filter → sort/trim → return RecallResult { hits: Vec<RecallHit> }.
- RecallHit carries memory_id, score, kind, context_id, salience,
  created_at; text stays None until a wire-level include_text lands.
- ExecError covers EmbedFailed (#[from] EmbedError), IndexSearchFailed,
  MetadataReadFailed, MemoryNotFound, Unsupported, Internal.

Tests:
- ~6 pure planner units pinning ef/candidates/filter-rule shape.
- ~5 integration tests using tempdir MetadataDb + in-memory SharedHnsw
  + a small mock Dispatcher: ordering, K limit, confidence_threshold,
  kind filter, metadata coverage.
- One BGE-gated end-to-end test using CpuDispatcher.

New deps in brain-planner Cargo.toml: brain-embed, brain-index,
brain-metadata. Dev-dep: tokio (macros + rt) for #[tokio::test];
tempfile already in dev-deps via 6.1.

Verify: cargo build/test/clippy -p brain-planner.
```

---

PLAN READY.
