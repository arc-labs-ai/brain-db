# Phase 21: LLM Extractor

## Goal

Implement the LLM extractor kind: cache, schema validation, retry-once, cost budgeting, audit. Background worker integrates with the queue subsystem. Resolver tier 4 (LLM) becomes operational.

## Prerequisites

- Phase 20 complete.

## Reading list

- `22_extractors/00_purpose.md` (LLM extractor section)
- `27_knowledge_workers/00_purpose.md` (LLM extractor section)
- `18_entities/01_resolution.md` (tier 4)

## Outputs

- LLM client abstraction (pluggable for Anthropic API, local Llama-like, etc.).
- LLM extractor framework with cache, validation, retry, budget.
- Background worker integrated with priority queue.
- Resolver tier 4 active.
- Optional built-in extractor: `brain.preferences_llm`.

## Sub-tasks

### 21.1 LLM client trait and at least one backend

**Reads:** `22_extractors/00_purpose.md` (LLM section); `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-llm/src/lib.rs`, `crates/brain-llm/src/anthropic.rs`.
**Done when:** `LlmClient::complete(prompt) -> CompletionResult` works; one backend implemented (Anthropic API).
**Pitfalls:** API key handling (env var, never logged); timeout per request; rate limiting via the backend.

### 21.2 LLM extractor worker

**Reads:** `22_extractors/00_purpose.md`; `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-extractors/src/llm.rs`.
**Done when:** worker consumes from queue, runs LLM extractor logic (cache check, call, validate, decode), writes outputs.
**Pitfalls:** Worker yields between memories; respects priority budget; integrates with existing worker scheduler.

### 21.3 LLM cache

**Reads:** `26_knowledge_storage/00_purpose.md` (LLM cache).
**Writes:** `crates/brain-metadata/src/llm_cache_ops.rs`.
**Done when:** cache get/put by `(input_hash, extractor_id, version, model)`; TTL respected; eviction by capacity (LRU).

### 21.4 Schema validation for LLM output

**Reads:** `22_extractors/00_purpose.md` (schema validation).
**Writes:** `crates/brain-extractors/src/schema_validate.rs`.
**Done when:** LLM output JSON parsed and validated against the extractor's declared output schema.
**Pitfalls:** Use `serde_json` + `jsonschema` crate (or equivalent). Test malformed inputs.

### 21.5 Retry-once with error feedback

**Reads:** `22_extractors/00_purpose.md` (idempotency).
**Writes:** in `crates/brain-extractors/src/llm.rs`.
**Done when:** on validation failure, retry with the validation error appended to the prompt; if second attempt fails, drop and log.

### 21.6 Cost budgeting

**Reads:** `22_extractors/00_purpose.md` (cost controls).
**Writes:** `crates/brain-extractors/src/cost.rs`.
**Done when:** per-extractor and global budgets enforced; extractions skipped over budget with metric.
**Pitfalls:** Estimate before call (token count heuristic); track actuals.

### 21.7 Resolver tier 4 (LLM)

**Reads:** `18_entities/01_resolution.md` (tier 4).
**Writes:** `crates/brain-core/src/resolver.rs` (extending phase 16).
**Done when:** when tier 3 candidates are ambiguous and the extractor's resolver config enables LLM, call LLM with candidates, parse response, resolve or fail.

### 21.8 Optional built-in: `brain.preferences_llm`

**Reads:** `22_extractors/00_purpose.md` (built-ins).
**Writes:** `crates/brain-extractors/src/builtin/preferences_llm.rs`.
**Done when:** prompt template, examples, JSON schema for output, declaration string ready to use in user schemas.

### 21.9 Tests

**Writes:** `tests/knowledge_extractors_llm.rs`.
**Done when:** mock LLM backend used for deterministic tests; cache hit/miss flows; schema validation positive/negative; budget enforcement; resolver tier 4 with mock LLM.
**Pitfalls:** Don't hit real LLM in CI; use mock backend.

## Done-when (phase)

- LLM extractor framework end-to-end.
- Cache works; second call to same input is a cache hit.
- Schema validation rejects malformed outputs; retries once.
- Cost budget enforced.
- Resolver tier 4 active behind extractor config.
- Audit log captures LLM-specific metadata (model, tokens).

## Pitfalls

- Real LLM testing should be opt-in via env var, not in default CI runs.
- Different LLM backends have different rate limits; respect them.
- LLM extractor latency dominates wall-time when active; ensure other workers can yield.
