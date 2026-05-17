# Phase 21 — LLM extractor

Wires the third extractor tier: the LLM extractor. Anthropic +
OpenAI HTTP clients behind a single trait; cache-backed
idempotency via the phase-17 `LlmCacheDb`; retry-once on schema
validation failure; per-call cost budget enforcement; cost
tracked in the audit row's `cost_micro_usd` field.

## Confirmed scope (user decisions)

1. **Anthropic + OpenAI APIs**. Two transports behind a single
   `LlmClient` trait. Provider selection via model-name prefix
   (`claude-*` → Anthropic; `gpt-*` / `o*` → OpenAI). Local LLMs
   (llama.cpp, vLLM) are post-v1.
2. **Reuse phase-17 `LlmCacheDb`**. Already in brain-metadata with
   the right cache key `(input_hash, extractor_id, extractor_version,
   model_id)`. Phase 21 wires the LLM extractor to it.
3. **Phase 20.7b BertRuntime is in place** (merged before this
   work). Classifier tier production-complete; LLM tier slots in
   cleanly.

## Branch

`feature/phase-21-llm-extractor` (off `dev`).

## Spec-first discipline — §22 backfill required

Phase 20.0's backfill brought §22 to 8 files but left LLM-specific
behavior threaded through §22/00 and §22/06. Phase 21 lands a
dedicated `§22/09_llm_extractor.md` covering the client trait,
cache integration, retry semantics, cost budget, determinism
contract. `§22/08_references.md` gets updated.

## Sub-tasks

### 21.0 — §22/09 spec backfill + master plan

**Reads:** §22/00 + §22/02 (classifier as the structural twin) +
§22/05 (audit cost field) + §22/06 (idempotency / cache key).
**Writes:**
- `spec/22_extractors/09_llm_extractor.md` — full LLM tier spec at
  §03-substrate depth (~7-8 sections).
- `spec/22_extractors/08_references.md` — link 09 into the file map.
- `.claude/plans/phase-21.md` — this file.

### 21.1 — `brain-llm` crate + Anthropic client

**Writes:**
- `crates/brain-llm/` new crate.
- `LlmClient` trait — object-safe, `async`-trait-free (returns
  boxed future), `complete(request) -> Result<LlmResponse, LlmError>`.
- `LlmRequest { model, system, messages, response_schema:
  Option<serde_json::Value>, temperature, max_tokens }`.
- `LlmResponse { content: String, tokens_in, tokens_out,
  cost_micro_usd, model_version }`.
- `AnthropicClient` — POST to `api.anthropic.com/v1/messages`.
  Reads `ANTHROPIC_API_KEY` from env at construction. JSON body +
  per-call timeout (30 s default).
- `LlmError { Transport, Auth, RateLimit { retry_after_ms },
  InvalidRequest, ProviderError, Timeout, OutputDecodeFailed }`.
- `model_router` — maps model name prefix to provider; rejects
  unknown patterns at construction (caller decides whether to
  panic / log).

`workspace deps`: reqwest (already present), serde_json,
thiserror, tokio, tracing.

### 21.2 — OpenAI client

**Writes:** `OpenAIClient` — POST to `api.openai.com/v1/chat/completions`
with structured-output mode (response_format: json_schema) when
`response_schema` present.

### 21.3 — `LlmExtractor` impl in brain-extractors

**Writes:** `crates/brain-extractors/src/llm.rs` — `LlmExtractor:
Extractor` with:
- `Arc<dyn LlmClient>` injected at construction.
- `LlmCacheDb` reference (operator-provided via `OpsContext`).
- Prompt template from `ExtractorField::Prompt`.
- Optional examples (`ExtractorField::Examples`).
- Optional output schema (`ExtractorField::Schema`).
- `cost_budget` enforcement before each call.
- Cache lookup before the LLM call.
- Retry once on schema-validation failure (validation error
  included in the retry prompt).
- Audit row's `cost_micro_usd` + `model_metadata` populated.

**Spec §22/09 §4 retry-once semantics**: malformed output →
single retry with the validation error appended; persistent
malformed → `Failure { reason: "schema validation failed twice" }`.

### 21.4 — Materialise + register LLM extractors

**Writes:** extend `brain-extractors::materialize` to construct
`LlmExtractor` instances when an `ExtractorDefinition.kind ==
Llm`. The materializer takes an `Option<Arc<dyn LlmClient>>` (or
a `model_router`) so deployments without API keys produce
degraded LLM extractors that write `Failure(reason: "llm client
not configured")` audit rows.

### 21.5 — Server-side wiring + cache hook

**Writes:** extend `brain-server/src/shard/mod.rs` to:
- Construct the LLM client(s) at shard startup based on env
  (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`); produce a
  `model_router` if at least one is set.
- Pass the router (or `None`) through to
  `build_registry_from_definitions`.
- Open the per-shard `llm_cache.redb` via the existing
  `LlmCacheDb::open` and pass to OpsContext.

`OpsContext` gains `llm_cache: Option<Arc<LlmCacheDb>>` field.
The LLM extractor's `predict` reads / writes through this when
present.

### 21.6 — Integration tests

**Writes:**
- `brain-extractors/tests/llm_mock.rs` — mock `LlmClient` with
  controllable responses. Covers cache hit, cost-budget skip,
  retry-on-validation-fail (success on retry), retry-on-validation-fail
  (failure on both attempts), audit row populated with
  cost_micro_usd + model_metadata.
- `brain-server/tests/knowledge_llm_extractor_wire.rs` — wire
  smoke. Not against live APIs; the test shard uses an injected
  mock client.

### 21.7 — Bench + ROADMAP + phase exit

**Writes:**
- `brain-llm/benches/anthropic_request.rs` — bench against a
  local HTTP stub (not real Anthropic) measuring serialization +
  request prep cost.
- ROADMAP phase 21 ✓.
- User-authorised tag `phase-21-complete`.

## Out of scope (per spec / explicit user direction)

- Local LLM backends (llama.cpp / vLLM) — post-v1.
- Per-deployment global cost budget (§22/00 §"Cost controls" §2)
  — phase 22+ admin work. Phase 21 ships per-call budget only.
- Cache sweeper (TTL eviction + LRU) — phase 22+ per spec
  §27/07 Q4.
- `OnDemand` / `OnSchemaChange` / `Periodic` triggers — phase 22+.
- `STATEMENT_ADD_EVIDENCE` for richer per-entry confidence
  recording — phase 22.
- Cross-shard LLM budget coordination — post-v1.
- LLM-assisted entity resolver (tier-4) — §22/07 Q12.

## Risks

- **API key surface**: production deployments will mount keys via
  env vars; CI tests must use mock clients. Spec §22/09 documents
  this.
- **Provider rate-limit semantics**: Anthropic / OpenAI return
  429s with `retry-after` headers; phase 21 surfaces this as
  `LlmError::RateLimit { retry_after_ms }` and the audit row
  marks `Failure`. Adaptive retry land in phase 22+.
- **Cost estimation**: pre-call estimation depends on tokenizer
  parity with the provider's actual usage. Phase 21 uses
  character-based proxies (`text.len() / 4` ≈ tokens); phase 22
  ships proper tokenizer integration.

## Suggested commit cadence

8 commits (21.0 through 21.7).

## Verification gate (per sub-task)

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
just docker cargo test -p brain-llm -p brain-extractors
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

## After phase 21

Phase 22 — Retrieval (hybrid query router + RRF fusion).
