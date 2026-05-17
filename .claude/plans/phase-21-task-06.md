# 21.6 — Integration tests (mock LLM client + wire smoke)

End-to-end coverage of the LLM tier with deterministic mock
clients. Two complementary suites:

1. **`brain-extractors/tests/llm_pipeline.rs`** — exercises a
   real `LlmExtractor` instance backed by a mock `LlmClient`
   and a real on-disk `LlmCacheDb`. Verifies the cross-call
   behaviour the in-module unit tests (21.3) can only show one
   call at a time: cache populate → cache replay → re-arm
   after invalidation, plus retry sequencing.
2. **`brain-server/tests/knowledge_llm_extractor_wire.rs`** —
   wire smoke. SCHEMA_UPLOAD declaring an LLM extractor →
   `EXTRACTOR_LIST` includes the row → ENCODE produces an audit
   row from the LLM dispatch.

   v1 ships the no-keys-set variant: the audit row is a
   `Failure` with `"no llm clients configured"` reason. That
   confirms the schema → registry → ENCODE pipeline lights up
   the LLM tier without needing a mock HTTP server.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-extractors/tests/llm_pipeline.rs` | New: ~300 LOC mock-driven scenario tests. |
| `crates/brain-server/tests/knowledge_llm_extractor_wire.rs` | New: ~250 LOC wire smoke. Mirrors `knowledge_extractor_wire.rs` structure. |
| `crates/brain-extractors/Cargo.toml` | Move `tempfile` + `futures-lite` to `[dev-dependencies]` if not already there (already there from 21.3). No new deps expected. |
| `crates/brain-server/Cargo.toml` | No change — `support_harness` already lives in tests/. |

## `llm_pipeline.rs` scenarios

Mock `LlmClient` defined inline (~30 LOC) — same shape as the
21.3 in-module mock but with a richer `responses_queue`
discipline:

```rust
struct ScriptedClient {
    model: String,
    queue: parking_lot::Mutex<VecDeque<Result<LlmResponse, LlmError>>>,
    calls: AtomicUsize,
}
```

Tests:

1. **`cache_populates_then_replays`** — call 1 produces 1 LLM
   call + writes the cache row; calls 2 and 3 against the same
   memory hash short-circuit through the cache → 0 additional
   LLM calls. Confirms the cache row's `response_blob` parses
   back to the same projected items.
2. **`cache_row_invalidation_re_arms_call`** — manually delete
   the cache row between call 1 and call 2 (open the cache
   db's write txn, drop the key). Call 2 then makes a fresh
   LLM call.
3. **`schema_validation_retry_completes_in_two_calls`** — first
   response is malformed, second is correct → status=Success,
   audit reason carries no error, `calls == 2`.
4. **`schema_validation_failure_twice_costs_two_calls`** —
   both responses malformed → status=Failure with
   `"schema validation failed twice"`; `calls == 2`. Confirms
   the second failure does not loop a third call.
5. **`cost_budget_blocks_call`** — large prompt + tiny budget
   (`per_call_micro_usd: 1`) → status=SkippedBudget; the mock
   client records `calls == 0`. Verifies the budget gate runs
   strictly before the call.
6. **`projection_strips_below_threshold_items`** — response
   `[{name: Alice, conf 0.9}, {name: X, conf 0.1}]` with
   `confidence_threshold = 0.5` → exactly one
   `EntityMention` emitted.
7. **`response_blob_in_cache_persists_across_extractor_rebuilds`**
   — open cache → run extractor A → drop A → open a fresh
   extractor B with the same id + version + model → call 1
   on B hits A's cache row.

All seven scenarios drive `extractor.run(...).await` via
`futures_lite::future::block_on`. No tokio runtime needed
(reqwest is never hit; the mock client is sync-friendly).

## `knowledge_llm_extractor_wire.rs` scenarios

Mirrors `knowledge_extractor_wire.rs` (20.9). Wire prelude
(handshake / send_frame / read_one_frame) is copied verbatim.

The user-namespace schema declares an LLM extractor block:

```text
namespace acme
version 1
schema {
    define extractor llm_prefs {
        kind: llm
        target: statement Preference
        trigger: on encode
        model: "claude-haiku-4-5"
        prompt: "Extract preferences."
    }
}
```

Tests:

1. **`schema_upload_registers_llm_extractor_in_degraded_mode`** —
   SCHEMA_UPLOAD → `EXTRACTOR_LIST { include_disabled: false,
   namespace: "acme" }` returns one item with `kind == 2`
   (LLM) and `enabled == true`. The extractor is degraded
   internally (no API keys in CI) but visible on the wire as a
   normal enabled row.

2. **`encode_on_llm_extractor_in_degraded_mode_writes_failure_audit`**
   — ENCODE a memory whose `kind == episodic` so the
   `trigger: on encode` fires. After the call returns,
   `AUDIT_LIST` (or `extractor_dispatch_check` if available)
   shows one row stamped against the LLM extractor with status
   `Failure(reason: "no llm clients configured (...)")`. The
   ENCODE itself returns success — the LLM tier failure
   doesn't propagate to the client.

3. **`extractor_disable_then_enable_round_trip_for_llm_row`** —
   `EXTRACTOR_DISABLE(llm_prefs_id)` then `EXTRACTOR_ENABLE`,
   asserting the boolean fields flip. Confirms 20.8's wire ops
   work uniformly for LLM-kind rows.

## How we deal with missing AUDIT_LIST

If no audit-wire op is exposed yet (only `audit_write` on the
write path), test 2 verifies the audit row indirectly:

- Open the metadata redb via a side channel (the test already
  has a tempdir and can read the file post-run).
- Or stop the server cleanly and re-open the `metadata.redb`
  read-side to scan `EXTRACTION_AUDIT_TABLE`.

Look for the `knowledge_compat.rs` precedent — it reads
metadata directly post-shutdown.

If audit reading is too invasive, we **drop test 2** and ship
only tests 1 and 3 for the wire suite, keeping the
end-to-end audit observation in
`brain-extractors/tests/llm_pipeline.rs` (which gives full
audit visibility via direct method calls).

## Out of scope

- Mock HTTP server pointed at by real `AnthropicClient` /
  `OpenAIClient` — overkill for 21.6; the mock-client path
  through `LlmExtractor::run` already exercises the cache /
  retry / budget logic.
- Cross-shard cache coherency — single-shard tests only.
- Cache sweeper / TTL eviction — phase 24.
- Live-provider opt-in tests (gated by env vars) — post-v1.

## Single commit

`test(extractors,server): 21.6 — LLM tier integration tests`

## Verification

```
just docker cargo test -p brain-extractors --test llm_pipeline
just docker cargo test -p brain-server --test knowledge_llm_extractor_wire
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
```
