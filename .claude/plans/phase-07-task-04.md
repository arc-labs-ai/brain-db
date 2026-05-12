# Sub-task 7.4 — RECALL handler

Thin glue layer that replaces the 7.1 stub. The planner (6.5) and
executor (6.5) are complete — this sub-task wires them through
`brain-ops::dispatch` and maps `brain_planner::RecallResult` into the
wire `RecallResponseFrame`.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/03 §1 | RECALL: embed cue → ANN search → filter → return top-K |
| §09/03 §3 | Response: `Vec<RecallResult>` sorted by score, descending |
| §09/03 §4 | Score = `1 - cosine_distance`; range typically [0, 1] |
| §09/03 §5 | Fewer than K is normal; not an error |
| §09/03 §6 | Zero results is normal; empty list, not an error |
| §09/03 §12 | Multi-shard recall fans out (v1 = single shard) |
| §09/03 §15 | `include_text=true` adds ~50 µs/result (wire flag missing in v1) |
| §03/07/3 | Wire `RecallRequest` shape (already implemented) |
| §03/08/3 | Wire `RecallResponseFrame` (streaming) |

## 1. Scope

**In scope for 7.4:**

- Replace `crates/brain-ops/src/recall.rs::handle_recall` stub with a
  real implementation: `plan_recall_inner` → `execute_recall` →
  map `RecallResult` → `RecallResponseFrame`.
- Map `RecallHit` → wire `MemoryResult` (per-result), filling fields
  that the executor doesn't carry (see §2.2).
- Integration tests in `crates/brain-ops/tests/recall.rs` covering
  full pipeline, empty result, filter rejection, k-truncation,
  confidence floor, BGE-gated end-to-end.

**NOT in scope:**

- Streaming. Wire spec §08/3 frames are streaming-capable; v1 emits a
  single frame with `is_final=true`. Phase 9 server adds the chunker.
- Cross-shard fan-out (spec §09/03 §12). v1 is single-shard.
- `last_accessed_at_unix_nanos` — `RecallHit` doesn't carry it. Set
  to `created_at_unix_nanos` for v1. Spec §09/03 §10 says access
  tracking is filter-only, not part of ranking. Tracking-on-recall is
  a future worker job.
- `vector_offset` / `vector_dim` — arena exposure is server-side
  (Phase 9). Set to `0` / `0` for v1. The wire `include_vectors` flag
  is forwarded but no vector bytes are returned yet.
- `edges` — Set to `None` for v1. The wire `include_edges` flag is
  forwarded but edge fetch is not yet implemented; the planner pins
  `metadata_lookup.include_extra = include_edges` already, but the
  executor doesn't read edge rows. A future sub-task wires the fetch.
- `text` — `RecallHit.text` is always `None` today (the executor has
  no `TextFetchStep`). Wire `MemoryResult.text` defaults to empty
  string. The wire request struct has no `include_text` flag yet, so
  there's nothing to gate on. Document the gap.

## 2. Implementation decisions

### 2.1 Handler body

```rust
pub async fn handle_recall(
    req: RecallRequest,
    ctx: &OpsContext,
) -> Result<RecallResponseFrame, OpError> {
    let plan = plan_recall_inner(&req, &ctx.planner_ctx)?;
    let result = execute_recall(plan, &ctx.executor).await?;

    let results: Vec<MemoryResult> = result.hits.into_iter().map(hit_to_wire).collect();
    let cumulative_count = u32::try_from(results.len()).unwrap_or(u32::MAX);

    Ok(RecallResponseFrame {
        results,
        is_final: true,
        cumulative_count,
        estimated_remaining: None,
    })
}
```

`?` propagates `PlanError` + `ExecError` through `OpError`'s `#[from]`
impls. The dispatcher wraps the success side in
`ResponseBody::Recall`.

### 2.2 `RecallHit` → `MemoryResult` mapping

```rust
fn hit_to_wire(hit: RecallHit) -> MemoryResult {
    MemoryResult {
        memory_id: hit.memory_id.into(),
        text: hit.text.unwrap_or_default(),
        similarity_score: hit.score,
        confidence: hit.score,                      // v1: == similarity
        salience: hit.salience,
        kind: hit.kind.into(),
        context_id: hit.context_id.into(),
        created_at_unix_nanos: hit.created_at_unix_nanos,
        last_accessed_at_unix_nanos: hit.created_at_unix_nanos, // v1 gap
        vector_offset: 0,                           // v1 gap
        vector_dim: 0,                              // v1 gap
        edges: None,                                // v1 gap
    }
}
```

`ContextId → WireContextId`: `WireContextId = u64`, `ContextId(u64)`.
Need `hit.context_id.into()`; check that the `From` impl exists in
brain-core.

### 2.3 Empty-result semantics

Spec §09/03 §6 says zero results is normal, not an error. The
executor already returns `Ok(RecallResult { hits: vec![] })` in that
case. Wire frame carries `results: vec![]` + `is_final: true`.

### 2.4 K-truncation

`plan_recall_inner` clamps `final_top` to `k`; the executor truncates
to `final_top`. The handler just maps; no additional truncation.

### 2.5 Confidence floor

`req.confidence_threshold > 0.0` is plumbed via the plan's
`merge.confidence_min`; the executor drops below-floor hits. No
handler work needed.

### 2.6 Validation errors

`plan_recall_inner` rejects:
- `top_k == 0` → `InvalidParameters`
- `top_k > max_top_k` (1000) → `InvalidParameters`
- `cue_text` empty → `InvalidParameters`
- `cue_text` too long → `InvalidParameters`

All propagate via `OpError::PlanError` → wire `InvalidRequest`.

### 2.7 No new error variants

Every failure path is already mapped (see 7.3 plan §2.5). Embedder
failures become `ExecError::EmbedFailed` → `InternalError` per spec
§09/01 §12.

## 3. Test harness

Reuse the `tests/encode.rs` fixture pattern: `MockDispatcher` +
tempdir `MetadataDb` + `SharedHnsw` + `RealWriterHandle` + `OpsContext`.

For RECALL we need *some* data in the index. Pre-populate by calling
`dispatch(RequestBody::Encode(...))` a few times before the recall.
This exercises the full encode → index → recall path.

### 3.1 Integration tests (6)

1. `recall_full_pipeline_returns_top_k` — encode 3 memories; recall
   k=2; assert 2 hits, sorted by score descending, fields populated.
2. `recall_empty_index_returns_empty_frame` — recall on empty index;
   assert `results.is_empty()`, `is_final=true`, `cumulative_count=0`.
3. `recall_k_truncation` — encode 5 memories; recall k=3; assert
   exactly 3 results.
4. `recall_kind_filter_rejects_off_kind_hits` — encode 2 Episodic +
   2 Semantic; recall with `kind_filter=[Semantic]`; assert only
   Semantic results.
5. `recall_confidence_floor_drops_low_score_hits` — encode 3; recall
   with `confidence_threshold=0.999` (very strict); assert near-empty
   result.
6. `recall_invalid_top_k_returns_plan_error` — `top_k=0`; assert
   `OpError::PlanError` + wire `InvalidRequest` code.

### 3.2 BGE-gated test (1)

7. `recall_with_real_embedder_end_to_end` — gated on
   `BRAIN_EMBED_MODEL_DIR`; encode 2 semantically distinct memories,
   recall with a cue close to one, assert the closer one ranks higher.

## 4. Files written / changed

```
crates/brain-ops/src/recall.rs       [edit: real handler body]
crates/brain-ops/tests/recall.rs     [new — 7 integration tests]
```

No new external deps. No `lib.rs` re-export changes (the planner
already re-exports `plan_recall_inner` + `execute_recall`).

## 5. Verify checklist

- `cargo build -p brain-ops` clean (in Linux dev container).
- `cargo test -p brain-ops` — 31 existing + ~6 new (6 mock + 1
  BGE-gated which skips). Final tally: ~37 tests.
- `cargo clippy -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-ops` no diff.

## 6. Commit message (draft)

```
feat(brain-ops): RECALL handler (sub-task 7.4)

Replaces the 7.1 stub with a real implementation that plumbs the
existing planner (6.5) + executor (6.5) through brain-ops::dispatch
and maps the result into the wire RecallResponseFrame.

- handle_recall: plan_recall_inner → execute_recall → map hits.
  PlanError + ExecError propagate via OpError's #[from] impls; the
  dispatcher wraps the success side in ResponseBody::Recall.
- Wire mapping (RecallHit → MemoryResult):
  - similarity_score ← score; confidence ← score (v1: identical
    for cosine; spec §09/03 §4)
  - text ← hit.text.unwrap_or_default() (executor has no
    TextFetchStep yet; spec §09/03 §15 gap)
  - last_accessed_at_unix_nanos ← created_at_unix_nanos (access
    tracking is a future worker; spec §09/03 §10)
  - vector_offset / vector_dim ← 0 / 0 (arena exposure deferred to
    Phase 9 server)
  - edges ← None (edge fetch deferred to a future sub-task)
- Single-frame response: is_final=true, no streaming yet
  (Phase 9 adds the chunker).

Tests: 6 mock-embedder integration tests pinning full pipeline,
empty-result handling, k-truncation, kind filter, confidence floor,
and invalid-top_k rejection. One BGE-gated end-to-end test using
CpuDispatcher that asserts the semantically-closer memory ranks
higher.

No new external deps. brain-planner unchanged.
```

## 7. Risks

- **Stale `last_accessed_at`**. Documented as a v1 gap. Recall doesn't
  bump access time today; a future worker (Phase 11) writes back. Wire
  field is filled with `created_at` until then.
- **Empty `text`**. `MemoryResult.text` is a `String`, not `Option`.
  Defaulting to empty string is wire-compatible but ambiguous (could
  mean "no text" or "text wasn't fetched"). The wire request has no
  `include_text` flag, so a future spec/wire iteration is the right
  place to disambiguate.
- **Confidence == similarity**. Spec §09/03 §4 only defines similarity;
  the wire `confidence` field is reserved for future use (e.g., when
  consolidated memories carry a separate confidence). v1 ties them.

## 8. Out-of-scope flags

- No streaming. Single frame only.
- No cross-shard fan-out.
- No edge view fetch on `include_edges=true`.
- No vector return on `include_vectors=true`.
- No text fetch (executor lacks the step).
- No access-time bump.

---

PLAN READY.
