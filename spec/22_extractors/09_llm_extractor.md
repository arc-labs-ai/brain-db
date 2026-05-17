# 22.09 LLM Extractor

The third extractor tier: LLM-driven extraction with cache,
retry, cost budget, and provider-agnostic transport. Slowest
(~100 ms – 10 s per call); most expensive; highest recall on
unstructured text. Runs on the background queue (§27/01 §3).

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"LLM extractors" — overview.
- [`./02_classifier_extractor.md`](./02_classifier_extractor.md) §1
  — sibling tier surface; LLM mirrors it.
- [`./05_audit.md`](./05_audit.md) §1 — `cost_micro_usd` +
  `model_metadata` fields the LLM tier populates.
- [`./06_idempotency.md`](./06_idempotency.md) — cache enforces
  idempotency for the LLM tier.
- [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md)
  — `llm_cache.redb` shape.

## 1. Surface

```rust
pub struct LlmExtractor {
    pub id: ExtractorId,
    pub name: String,
    pub target: ExtractorTarget,
    pub extractor_version: u32,
    pub client: Arc<dyn LlmClient>,
    pub cache: Option<Arc<LlmCacheDb>>,
    pub prompt: String,
    pub examples: Option<serde_json::Value>,
    pub response_schema: Option<serde_json::Value>,
    pub confidence_threshold: f32,
    pub cost_budget: Option<CostBudget>,
    pub cache_ttl: Duration,
}

pub trait LlmClient: Send + Sync {
    fn complete(
        &self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + '_>>;
    /// Pinned identifier (e.g., `"anthropic/claude-haiku-4-5"`).
    fn model(&self) -> &str;
    /// 64-bit BLAKE3-low hash of `model()`. Used as the
    /// `model_id` component of [`LlmCacheKey`].
    fn model_id_hash(&self) -> u64;
}
```

`LlmCacheDb` is the phase-17 / spec §15.4 file-per-shard cache
keyed by `(input_hash, extractor_id, extractor_version,
model_id)`. The phase-21 wiring just reads + writes through it.

## 2. Provider routing

Two transports ship in v1:
- `AnthropicClient` (claude-* models) — POST
  `https://api.anthropic.com/v1/messages`.
- `OpenAIClient` (gpt-*, o1-*, o3-*) — POST
  `https://api.openai.com/v1/chat/completions` with JSON-schema
  structured output mode.

A `ModelRouter` maps the schema-declared `model:` field to one
of these clients via prefix matching:

| Prefix | Provider |
|---|---|
| `claude-*`, `anthropic/*` | Anthropic |
| `gpt-*`, `o1-*`, `o3-*`, `openai/*` | OpenAI |

Unknown patterns fail at `LlmExtractor` construction (the
materializer logs a warn-level diagnostic and registers the
extractor in degraded mode).

API keys via env vars at shard startup:
- `ANTHROPIC_API_KEY` — enables Anthropic.
- `OPENAI_API_KEY` — enables OpenAI.

Missing keys produce a `None` for that provider; extractors
configured against an unconfigured provider register as degraded.
Local LLM backends (llama.cpp / vLLM) are post-v1.

## 3. Cache integration

Spec §22/06 §1: `IdempotencyKey { memory_id, text_hash,
extractor_id, extractor_version, schema_version }`. The LLM cache
adds the `model_id` byte to the key (§15.4 `LlmCacheKey =
(input_hash, extractor_id, extractor_version, model_id)`).

`predict` flow:

```text
1. Build LlmCacheKey { hash_memory_text(memory.text), id,
                       extractor_version, model_id_hash }.
2. cache.get(key):
   - Some(row) → decode `response_blob` per response_schema;
     skip the LLM call; mark `model_metadata.cache_hit = true`.
   - None     → proceed to step 3.
3. cost_estimate = client.estimate_cost(&request).
   if cost_estimate > cost_budget.per_call: write
   `Failure(SkippedBudget)` audit and return.
4. response = client.complete(&request).await.
5. validate(response.content, response_schema):
   - Ok(parsed) → continue.
   - Err(e) → retry once with the validation error included
     in the prompt (see §4); if second response also fails,
     return `Failure(reason: "schema validation failed twice")`.
6. cache.put(key, response, cache_ttl).
7. Project parsed JSON to ExtractedItem[] per `target`.
```

Steps 2 and 6 are no-ops when `cache: None` (operators may
disable the cache via `cache: disabled` in the schema DSL).

## 4. Retry-once on validation failure

Spec §22/00: "Output must validate against the declared JSON
schema. If it doesn't, retry once (with the validation error in
the prompt). If still invalid, drop and log."

Implementation:

```rust
match validate(&response, schema) {
    Ok(p) => p,
    Err(e) => {
        let retry_prompt = format!(
            "{original_prompt}\n\n\
             Your previous response did not match the expected \
             schema. Error: {e}. Please retry with valid JSON.",
        );
        let response2 = client.complete(retry_prompt).await?;
        match validate(&response2, schema) {
            Ok(p) => p,
            Err(_) => return Ok(failure_with_reason(
                "schema validation failed twice",
            )),
        }
    }
}
```

The retry **doubles** the cost. Both calls are counted in the
audit row's `cost_micro_usd`. Operators with tight budgets
should pre-flight extractor prompts on representative inputs.

## 5. Cost budget

`CostBudget { per_call_micro_usd: u64 }`. Phase 21 ships
per-call only; per-deployment global budget is post-v1
(§22/07 deferred).

Pre-call estimate:

```rust
pub fn estimate_cost(request: &LlmRequest, model_pricing: &Pricing) -> u64 {
    let in_tokens = char_count_approx(&request.combined_prompt()) / 4;
    let out_tokens = request.max_tokens.unwrap_or(MAX_TOKENS_DEFAULT);
    in_tokens * model_pricing.input_micro_usd_per_token
        + out_tokens * model_pricing.output_micro_usd_per_token
}
```

Pricing is operator-provided via a `pricing.toml` config file
(default values for known models ship in v1; operators override
for negotiated rates). Phase 21 ships an embedded default table
for `claude-haiku-4-5` / `claude-sonnet-4-6` / `gpt-4o-mini`;
unknown models default to `100` µ$/1K input + `300` µ$/1K
output as a conservative guess.

When `cost_estimate > cost_budget.per_call_micro_usd`:
- Audit row written: `status = SkippedBudget`, `status_reason =
  "estimated $X exceeds per-call budget $Y"`.
- No LLM call. No charge.
- ENCODE unaffected.

After the call:
- `audit.cost_micro_usd = actual_cost_from_response_metadata`.
- `audit.model_metadata` (rkyv-archived) carries token counts +
  cache_hit flag.

## 6. Schema validation

The `response_schema:` field in the extractor's `define
extractor { ... }` block is parsed as `serde_json::Value` (§19.2
AST `ExtractorField::Schema`). On response:

1. Parse response.content as JSON (`serde_json::from_str`).
2. Validate against the schema using a JSON-Schema validator
   (`jsonschema` crate, draft 7 default).
3. On validation failure, route to §4 retry.

If `response_schema: None`, the response is treated as a free-
form string and projected as a single `StatementMention` /
`EntityMention` per `target` (best effort).

## 7. Output projection

Per `target`:

- `Statement { kind }` — expect a JSON array of objects, each
  with `subject`, `predicate`, `object`, `confidence` keys. One
  `StatementMention` emitted per array element. Schema typically
  pins this shape.
- `Entity { entity_type }` — expect a JSON array of strings (entity
  names) or `{name, confidence}` objects. One `EntityMention`
  per element.
- `Relation { relation_type }` — expect a JSON array of `{from,
  to, confidence}` objects. One `RelationMention` per element.
- `EntityOrStatement` — heuristic: if the response is an array
  of strings, treat as entities; if of objects with `predicate`,
  treat as statements.

Items below `confidence_threshold` are skipped.

## 8. Determinism

Per spec §22/00:
- `temperature = 0` (default; configurable via extractor field).
- Schema validation rejects malformed; same input + cache hit →
  byte-identical output.
- Cache invalidation = drift event; the substrate treats
  uncached re-runs as supersession.

The `model_metadata` audit field carries `model_version` (from
the response's model field, e.g., `"claude-haiku-4-5-20240307"`)
so downstream readers can detect provider-side rolling deploys.

## 9. Error model

```rust
pub enum LlmError {
    Transport { source: reqwest::Error },
    Auth { provider: &'static str },
    RateLimit { retry_after_ms: u64 },
    InvalidRequest { reason: String },
    ProviderError { status: u16, message: String },
    Timeout,
    OutputDecodeFailed { reason: String },
}
```

Mapping to audit `status`:
- `Transport` / `Timeout` / `ProviderError` (5xx) → `Failure`.
- `RateLimit` → `Failure` with `retry_after_ms` in
  `status_reason`. Adaptive retry (later) lands in phase 22+.
- `Auth` → `Failure` with operator-actionable reason.
- `InvalidRequest` → `Failure`; prompt / schema bug.
- `OutputDecodeFailed` → `Failure`; sometimes recoverable via §4
  retry (the loop already exercises this).

## 10. Performance budget

Spec §16/02 §2.7 (phase 21 extension):

| Operation | p50 | p99 |
|---|---|---|
| `LlmExtractor::predict` (cache hit) | 1 ms | 5 ms |
| `LlmExtractor::predict` (cache miss, claude-haiku) | 600 ms | 3 s |
| `LlmExtractor::predict` (cache miss, gpt-4o-mini) | 800 ms | 4 s |
| Cost-budget skip path | 200 µs | 1 ms |

These targets are dominated by external API latency and aren't
strictly enforceable on CI; phase 21 ships smoke benches against
mock HTTP servers. Production deployments operating against real
providers should set their own SLOs and instrument via the
`brain_extractors::audit` table.

## 11. Tests (phase 21)

Per spec §22/00 + the cache-key contract:

- Cache-hit returns cached items + sets `model_metadata.cache_hit`.
- Cache-miss writes through to the cache.
- `cost_estimate > budget` → `SkippedBudget` audit; no LLM call.
- Schema-validation failure → one retry with error in prompt;
  validation passes on retry → `Success`.
- Schema-validation failure twice → `Failure(reason: "schema
  validation failed twice")`.
- `LlmError::RateLimit { retry_after_ms }` → `Failure` audit with
  the retry-after info in `status_reason`.
- Unknown model prefix → degraded extractor (audit
  `Failure(reason: "no client configured for model X")`).
- Provider key unset → matching extractor is degraded; audit
  surfaces the key-not-set reason.

Integration tests in `brain-server` use a mock `LlmClient`
injected into the registry — phase 21 doesn't hit live providers
in CI.

## 12. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q-llm-1 — per-deployment global cost budget (only per-call in
  v1).
- Q-llm-2 — adaptive rate-limit retry (currently `Failure` on
  429).
- Q-llm-3 — proper tokenizer integration for cost estimation
  (v1 uses `chars / 4`).
- Q-llm-4 — local LLM backends (post-v1).
- Q-llm-5 — `STATEMENT_ADD_EVIDENCE` for richer per-call
  confidence (phase 22).
