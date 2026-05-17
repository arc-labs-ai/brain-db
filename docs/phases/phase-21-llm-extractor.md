# Phase 21: LLM Extractor ✓

## Goal

Implement the LLM extractor kind: cache, schema validation, retry-once, cost budgeting, audit. Background worker integrates with the queue subsystem. Resolver tier 4 (LLM) becomes operational.

## Status

**Complete** — tag `phase-21-complete`. The original 9-subtask layout in this file was collapsed into a tighter 8-task delivery (`.claude/plans/phase-21-task-0[0-7].md`) because the LLM cache + schema validation + retry-once + cost budget all live inside the `LlmExtractor` impl. Sub-tasks 21.7 (Resolver tier 4) and 21.8 (built-in `brain.preferences_llm`) were explicitly scope-cut to phase 22+ / post-v1 — see [`ROADMAP.md`](../../ROADMAP.md) §"Phase 21" Deferred for the full list.

The actual landed mapping:

| Phase-doc sub-task | Where it landed |
|---|---|
| 21.1 LLM client trait + backend | `.claude/plans/phase-21-task-01.md` (Anthropic) + `phase-21-task-02.md` (OpenAI) |
| 21.2 LLM extractor worker | `phase-21-task-03.md` (LlmExtractor + async trait) + `phase-21-task-05.md` (server router + per-shard cache wiring) |
| 21.3 LLM cache | reused phase-17 `LlmCacheDb`; threaded in `phase-21-task-03.md` |
| 21.4 Schema validation | `phase-21-task-03.md` §schema validation (jsonschema crate) |
| 21.5 Retry-once | `phase-21-task-03.md` §retry-once |
| 21.6 Cost budgeting | `phase-21-task-03.md` + `phase-21-task-04.md` (`CostBudget` materializer translation) |
| **21.7 Resolver tier 4 (LLM)** | **Deferred** — phase 22+ (§22/07 Q12). Entity resolver currently stops at tier 3; tier-4 LLM-assisted disambig lands after tantivy. |
| **21.8 Built-in `brain.preferences_llm`** | **Deferred** — post-v1. Operators declare their own LLM extractors; the system schema ships only the phase-20 pattern + classifier built-ins. |
| 21.9 Tests | `phase-21-task-06.md` (mock-client integration suite + wire smoke) |

## Prerequisites

- Phase 20 complete.

## Reading list

- `22_extractors/00_purpose.md` (LLM extractor section)
- `22_extractors/09_llm_extractor.md` (spec backfill landed in 21.0)
- `16_benchmarks_acceptance/02_latency_targets.md` §2.8 (LLM perf targets, backfilled in 21.0)
- `27_knowledge_workers/00_purpose.md`
- `18_entities/01_resolution.md` (tier 4 — deferred)

## Outputs

- [x] LLM client abstraction — `LlmClient` trait + Anthropic + OpenAI backends.
- [x] LLM extractor framework with cache, validation, retry, budget.
- [x] Server-side router + per-shard cache wiring (background-worker integration parked alongside the synchronous post-ENCODE pipeline from phase 20; same dispatch surface).
- [ ] **Deferred:** Resolver tier 4 active — phase 22+.
- [ ] **Deferred:** Built-in `brain.preferences_llm` — post-v1.

## Sub-tasks

### 21.1 LLM client trait and at least one backend ✓

**Reads:** `22_extractors/00_purpose.md` (LLM section); `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-llm/src/lib.rs`, `crates/brain-llm/src/anthropic.rs`, `crates/brain-llm/src/openai.rs`.
**Landed in:** `.claude/plans/phase-21-task-01.md` (Anthropic) + `phase-21-task-02.md` (OpenAI).
**Done when:** `LlmClient::complete(LlmRequest) -> LlmFuture<'a>` works; two backends implemented (Anthropic Messages, OpenAI Chat Completions).

### 21.2 LLM extractor worker ✓

**Reads:** `22_extractors/00_purpose.md`; `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-extractors/src/llm.rs`.
**Landed in:** `phase-21-task-03.md` (`LlmExtractor` + async `Extractor::run`) + `phase-21-task-05.md` (server-side router + per-shard `OpsContext.llm_cache`).
**Done when:** dispatch runs LlmExtractor logic (cache check, call, validate, decode), writes outputs. v1 reuses the phase-20 synchronous post-ENCODE pipeline; a dedicated background worker is parked.

### 21.3 LLM cache ✓

**Reads:** `26_knowledge_storage/00_purpose.md` (LLM cache).
**Writes:** thread the phase-17 `LlmCacheDb` through `MaterializeDeps` + `OpsContext`.
**Landed in:** `phase-21-task-03.md` (cache wiring in the extractor) + `phase-21-task-05.md` (per-shard `LlmCacheDb::open` under the shard data dir).
**Done when:** cache get/put by `(input_hash, extractor_id, version, model_id_hash)`; TTL / eviction inherited from phase-17.

### 21.4 Schema validation for LLM output ✓

**Reads:** `22_extractors/00_purpose.md` (schema validation).
**Writes:** validation step inside `LlmExtractor::run`.
**Landed in:** `phase-21-task-03.md` §schema validation (`jsonschema` crate against operator-declared `output_schema_json`).
**Done when:** LLM output JSON parsed and validated; malformed → retry-once path.

### 21.5 Retry-once with error feedback ✓

**Reads:** `22_extractors/00_purpose.md` (idempotency).
**Writes:** in `crates/brain-extractors/src/llm.rs`.
**Landed in:** `phase-21-task-03.md` §retry-once.
**Done when:** on validation failure, retry with the validation error appended to the prompt; if second attempt fails, drop with `ExtractionStatus::SchemaInvalid` and log.

### 21.6 Cost budgeting ✓

**Reads:** `22_extractors/00_purpose.md` (cost controls).
**Writes:** `crates/brain-extractors/src/cost.rs` (the `CostBudget` type) + extractor short-circuit.
**Landed in:** `phase-21-task-03.md` + `phase-21-task-04.md` (materializer translates the persisted budget into `CostBudget { per_call_micro_usd }`).
**Done when:** per-call budget enforced; over-budget extractions skipped with `ExtractionStatus::SkippedBudget` and zero LLM calls issued. Global / cross-shard budget deferred post-v1.

### 21.7 Resolver tier 4 (LLM) — **DEFERRED**

**Reads:** `18_entities/01_resolution.md` (tier 4).
**Status:** deferred to phase 22+ (§22/07 Q12). Tier 4 LLM-assisted entity disambiguation depends on tantivy lexical retrieval landing in phase 22; the v1 resolver stops at tier 3.

### 21.8 Built-in `brain.preferences_llm` — **DEFERRED**

**Reads:** `22_extractors/00_purpose.md` (built-ins).
**Status:** deferred post-v1. Operators declare their own LLM extractors; the system schema ships only the phase-20 pattern + classifier built-ins (`brain.entity_mentions`, `brain.basic_ner`).

### 21.9 Tests ✓

**Writes:** `tests/knowledge_llm_extractor.rs` + `tests/knowledge_llm_extractor_wire.rs`.
**Landed in:** `phase-21-task-06.md`.
**Done when:** mock LLM backend drives deterministic tests; cache hit/miss flows; schema validation positive/negative; budget enforcement; degraded fallback when env-driven router cannot construct a client. Real-provider tests remain opt-in / post-v1.

## Done-when (phase)

- [x] LLM extractor framework end-to-end (mock-client pipeline green).
- [x] Cache works; second call to same input is a cache hit.
- [x] Schema validation rejects malformed outputs; retries once.
- [x] Cost budget enforced.
- [ ] **Deferred:** Resolver tier 4 active — phase 22+.
- [x] Audit log captures LLM-specific metadata (model, tokens, cost).

## Phase exit

- [x] Sub-tasks 21.1–21.6 + 21.9 landed (renumbered to `.claude/plans` 21.0–21.6).
- [x] 21.7 + 21.8 explicitly scope-cut and recorded in ROADMAP §"Deferred".
- [x] `llm_pipeline` criterion bench added (`crates/brain-extractors/benches/llm_pipeline.rs`); `pattern_extract` bench restored after the async-trait refactor. Bench wall-time capture deferred to phase-22 pre-flight to keep the implementation loop moving.
- [x] Workspace verify suite green at tag time (see commit history for `cargo zigbuild` + `just docker cargo test` evidence).
- [x] Tag `phase-21-complete` cut.

## Pitfalls

- Real LLM testing should be opt-in via env var, not in default CI runs.
- Different LLM backends have different rate limits; respect them.
- LLM extractor latency dominates wall-time when active; the v1 synchronous post-ENCODE pipeline (inherited from phase 20) is acceptable because LLM extractors are best-effort and never propagate errors to ENCODE — but they do hold the dispatch loop. A dedicated background worker queue lands phase 22+.
