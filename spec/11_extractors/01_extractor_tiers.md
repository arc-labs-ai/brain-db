# 11.01 Extractor Tiers

The three extractor tiers — pattern (regex), classifier (pinned model), and LLM. Each tier trades latency, cost, and recall differently; together they form a layered pipeline where cheaper tiers screen the bulk of inputs and the LLM handles the residual ambiguity.

## Pattern Extractor

Pattern extractors apply regex matches to memory text and emit typed outputs (entity mentions / statements / relations). They are the **first tier** — fast (~10–100 µs per memory), deterministic, zero-cost. Like the other tiers they run asynchronously in the per-shard extractor worker (pattern is the first tier the worker runs per memory), not inline in ENCODE.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — three-tier overview.
- [`./02_triggers.md`](./02_triggers.md) — when patterns run.
- [`./03_resolver.md`](./03_resolver.md) — entity resolution on match.
- [`./04_audit.md`](./04_audit.md) — audit log shape.
- [`../03_schema/02_ast.md`](../03_schema/02_ast.md) §5 —
  `ExtractorField::Patterns` AST node.

### 1. Surface

```rust
pub struct PatternExtractor {
    pub id: ExtractorId,
    pub name: String,                       // qname, e.g. "acme:person_mentions"
    pub target: ExtractorTarget,            // Entity / Statement / Relation
    pub patterns: Vec<CompiledRegex>,
    pub confidence: f32,                    // fixed per-match value
    pub trigger: TriggerExpr,
    pub depends_on: Vec<ExtractorId>,
}
```

`PatternExtractor` is constructed once at schema-apply time by
compiling each `ExtractorField::Patterns(Vec<String>)` entry. The
compiled struct is cached in the per-shard `ExtractorRegistry`.

### 2. Compilation

```rust
fn compile_patterns(raw: &[String]) -> Result<Vec<CompiledRegex>, PatternError> {
    raw.iter()
        .map(|p| CompiledRegex::new(p))
        .collect()
}
```

`CompiledRegex` wraps `regex::Regex` with a compile-time complexity
cap (DFA size) and a per-match runtime cap. Both come from the
`regex` crate's built-in `RegexBuilder::size_limit` and
`dfa_size_limit` settings.

**Caps (v1, conservative):**
- DFA size limit: 1 MiB per pattern.
- NFA size limit: 1 MiB per pattern.
- Match-time backtracking budget: 10 000 steps per pattern per text.

Patterns that exceed any cap fail compilation with `PatternError::
ResourceLimit`. The extractor never registers.

### 3. Execution

```rust
fn run(&self, mem: &Memory) -> Vec<ExtractedItem> {
    let mut out = Vec::new();
    for r in &self.patterns {
        for cap in r.captures_iter(&mem.text) {
            let text = cap_text(&cap);
            out.push(self.project(mem, &text, cap.range()));
        }
    }
    out
}
```

Per-extractor invariants:
- Walks all patterns in source order.
- For each match, emits exactly one `ExtractedItem`. Overlap is
  allowed (one extractor may produce overlapping mentions from
  different patterns).
- Captures use the first capture group if any; else the full match.
- `range` is byte-offset into `mem.text`; UTF-8-safe.

### 4. Output projection

Per the `target` enum from [`../03_schema/02_ast.md`](../03_schema/02_ast.md):

| `ExtractorTarget` | Output kind |
|---|---|
| `Entity { entity_type }` | `EntityMention { entity_type, text, range }` — picked up by the entity-resolver worker (§11/03). |
| `Statement { kind }` | `StatementMention { kind, text, range, confidence }` — post-v1 promotes to a full `Statement` via a follow-on extractor or resolver. |
| `Relation { relation_type }` | Requires two capture groups (`subject`, `object`); emits `RelationMention { relation_type, subject_text, object_text, confidence }`. |
| `EntityOrStatement` | Best-effort — emits `EntityMention` if the match looks like a name, else `StatementMention`. v1 always emits `EntityMention`. |

`ExtractedItem` is the sum type carrying any of the above plus
provenance fields (`extractor_id`, `extracted_at_unix_nanos`,
`schema_version`).

### 5. Confidence

Pattern extractors **don't compute per-match confidence**. Every
emitted item carries `ExtractorDef.fields[Confidence(_)]` verbatim
(default `0.7` if the user omits the field, per
[`./00_purpose.md`](./00_purpose.md)).

The resolver tier downstream may multiply this by its own
confidence (e.g., low-confidence resolution drops the overall
score). The audit record retains both values.

### 6. Determinism

Pattern execution is bit-deterministic given:
- The same compiled `regex::Regex` (regex crate is deterministic).
- The same `mem.text` bytes.
- The same source-order of patterns.

The `regex` crate version is pinned at the workspace level (see
[`../09_indexing/`](../09_indexing/00_purpose.md) prior pin practice).
Upgrading the crate is a versioned event: every extractor that
relies on a particular regex feature gets a `schema_version` bump
on rebuild.

### 7. Errors

```rust
pub enum PatternError {
    InvalidRegex { index: usize, message: String },
    ResourceLimit { index: usize, limit: &'static str },
    EmptyPatterns,
}
```

- `InvalidRegex` — surfaces during `compile_patterns`; aborts the
  schema upload with `ExtractorInvalidConfig` (see
  [`../03_schema/03_validator.md`](../03_schema/03_validator.md)).
- `ResourceLimit` — same; the regex is too large to compile.
- `EmptyPatterns` — `pattern` extractor with no `patterns:` field;
  the validator already catches this (see
  [`../03_schema/03_validator.md`](../03_schema/03_validator.md))
  but the compiler asserts it defensively.

### 8. Performance budget

Per [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| `PatternExtractor::run` over a 4 KiB memory | 30 µs | 100 µs |

The pattern-extractor criterion bench runs N patterns × M memories
and asserts the p99 stays under cap.

### 9. Idempotency

Re-running the same `(memory_id, text_hash, extractor_id,
extractor_version)` produces byte-identical outputs. The audit
record's `input_hash` field carries the BLAKE3 of `mem.text`; a
re-run with matching hash + extractor version writes a duplicate
audit row only if the caller explicitly requested replay (see
[`./05_idempotency.md`](./05_idempotency.md)).

## Classifier Extractor

Classifier extractors run a pinned, deterministic model over
memory text + features and emit typed outputs. They are the
**second tier** — slower than patterns
(~1–10 ms per memory) but precise on inputs patterns can't catch.

### 10. Surface

```rust
pub struct ClassifierExtractor {
    pub id: ExtractorId,
    pub name: String,
    pub target: ExtractorTarget,
    pub model: Box<dyn ClassifierModel>,
    pub feature_extractor: FeatureExtractor,
    pub confidence_threshold: f32,
    pub trigger: TriggerExpr,
    pub depends_on: Vec<ExtractorId>,
}
```

`ClassifierModel` is an object-safe trait:

```rust
pub trait ClassifierModel: Send + Sync {
    fn predict(&self, features: &Features) -> Prediction;
    fn version(&self) -> &str;          // pinned; e.g. "brain-basic-ner-v1.0"
}
```

`Features` is an opaque newtype carrying whatever
`feature_extractor` produces — typically tokenised text + optional
NER tags from a preceding pattern pass.

### 11. Determinism contract

The classifier MUST be bit-deterministic across runs of the same
binary version:

| Source | Pinning |
|---|---|
| Model weights | Embedded via `include_bytes!` or shipped at a fixed `models/` path. |
| Tokeniser | Pinned (same crate version, same vocabulary file). |
| Random seed | Fixed (0). |
| Math library | Pinned (candle workspace version). |
| Float ops | Default precision; no opt-in fast-math. |

A change to any of the above is a **model version bump** —
`ClassifierModel::version()` returns a new string, every
`ExtractionAudit` row written after the bump carries the new
version, and downstream statements get a `schema_version` /
`extractor_version` bump that the stale-extraction detector
(see [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md))
notices.

### 12. Feature extraction

`FeatureExtractor` is one of:

- `Builtin` — uses the model's bundled tokeniser + featurizer.
  Standard path; Brain uses this for `brain.basic_ner`.
- `Custom { id: FeatureExtractorId }` — refers to a registered
  Rust function. Deferred; tracked in
  [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

```rust
pub enum FeatureExtractor {
    Builtin,
    Custom { id: FeatureExtractorId },
}
```

### 13. Execution

```rust
fn run(&self, mem: &Memory) -> Vec<ExtractedItem> {
    let features = self.feature_extractor.extract(mem);
    let pred = self.model.predict(&features);
    if pred.confidence < self.confidence_threshold {
        return vec![];
    }
    vec![self.project(mem, pred)]
}
```

Output projection mirrors §4 above:

| `ExtractorTarget` | Output kind |
|---|---|
| `Entity { entity_type }` | One `EntityMention` per detected span; predictions with multiple spans emit multiple items. |
| `Statement { kind }` | One `StatementMention` per high-confidence prediction. |
| `Relation { relation_type }` | One `RelationMention` per (subject, object) pair the model emits. |

`Prediction.confidence` carries through to `ExtractedItem.confidence`
verbatim — unlike pattern extractors, classifiers can produce
variable per-match confidence, and the value lands in the audit
row.

### 14. Performance budget

Per [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| `ClassifierExtractor::run` over a 4 KiB memory | 5 ms | 15 ms |

Budget includes feature extraction + inference. This is a
**worker-throughput** target, not part of ENCODE's latency:
extraction runs in the per-shard extractor worker (see
[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)),
so classifier cost never widens ENCODE's p99.

### 14.1 GLiNER backbone sharing

The statement-kind head (Fact / Preference / Event) shares the GLiNER encoder used by the entity-mention classifier — one BERT-shaped forward pass on the memory text feeds both heads. No second model load: the classifier registry hands the loaded `GLiNERBackbone` to whichever head needs an inference, and the head runs a small linear-projection-plus-softmax on the pooled token embeddings.

| Aspect | Setting |
|---|---|
| Backbone | Shared GLiNER encoder (loaded once per shard) |
| Inference dtype | F32 default; F16 path gated behind a feature flag |
| Label-set namespace | The label vocabulary is namespace-prefixed (`brain:Fact`, `brain:Preference`, `brain:Event`); the head strips the namespace prefix before emitting `StatementKind` to keep downstream code provider-agnostic |

A real forward pass is what makes the statement-kind head usable in the ENCODE hot path. Without it the pipeline falls through to the LLM tier for every statement, blowing the ENCODE p99 budget and the cost moat.

### 15. Built-in `brain.basic_ner`

Brain ships one built-in classifier:

```text
define extractor brain.basic_ner {
    kind: classifier
    target: entity Person
    model: "brain-basic-ner-v1"
    feature_extraction: builtin
    confidence_threshold: 0.6
    trigger: on encode
}
```

Model details:
- Architecture: small distilled BERT (with a fallback rule-based
  path; see the bundled-NER risk note).
- Weights: ≤ 30 MB compressed, bundled via `include_bytes!` in
  `crates/brain-extractors/`.
- Output classes: `PER`, `ORG`, `LOC`, `O`.
- v1 projects only `PER` spans into `EntityMention { entity_type:
  Person }`; `ORG` / `LOC` are dropped until post-v1 adds
  `Organization` / `Location` to the system schema.

### 16. Classifier errors

```rust
pub enum ClassifierError {
    ModelNotFound { id: String },
    FeatureExtractionFailed { reason: String },
    InferenceFailed { reason: String },
    OutputDecodeFailed { reason: String },
}
```

Classifier errors are captured in the audit row's `status =
Failure` + `error: Some(_)`. They DO NOT fail the surrounding
ENCODE; the extractor returns empty output and the audit row
records the failure.

### 17. Classifier open questions

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md). Notably:

- Q-classifier-model — exact CONLL-NER checkpoint, licensing,
  candle compatibility.
- Q-batching — multi-memory batching (post-v1).
- Q-feature-extractor-custom — user-supplied feature extractors
  (post-v1).

## LLM Extractor

The third extractor tier: LLM-driven extraction with cache,
retry, cost budget, and provider-agnostic transport. Slowest
(~100 ms – 10 s per call); most expensive; highest recall on
unstructured text. Runs on the background queue (see
[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)).

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"LLM extractors" — overview.
- [`./04_audit.md`](./04_audit.md) §1 — `cost_micro_usd` +
  `model_metadata` fields the LLM tier populates.
- [`./05_idempotency.md`](./05_idempotency.md) — cache enforces
  idempotency for the LLM tier.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md)
  — `llm_cache.redb` shape.

### 18. LLM surface

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

`LlmCacheDb` is the file-per-shard cache from
[`../10_metadata/02_table_layout.md`](../10_metadata/02_table_layout.md)
§ 16.3, keyed by `(input_hash, extractor_id, extractor_version,
model_id)`. The wiring just reads + writes through it.

### 19. Provider routing

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

### 20. Cache integration

Per [`./05_idempotency.md`](./05_idempotency.md) § 1: `IdempotencyKey { memory_id, text_hash,
extractor_id, extractor_version, schema_version }`. The LLM cache
adds the `model_id` byte to the key (see
[`../10_metadata/02_table_layout.md`](../10_metadata/02_table_layout.md)
§ 16.3 — `LlmCacheKey =
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
     in the prompt (see §21); if second response also fails,
     return `Failure(reason: "schema validation failed twice")`.
6. cache.put(key, response, cache_ttl).
7. Project parsed JSON to ExtractedItem[] per `target`.
```

Steps 2 and 6 are no-ops when `cache: None` (operators may
disable the cache via `cache: disabled` in the schema DSL).

### 21. Retry-once on validation failure

Per [`./00_purpose.md`](./00_purpose.md): "Output must validate
against the declared JSON schema. If it doesn't, retry once (with
the validation error in the prompt). If still invalid, drop and
log."

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

### 22. Cost budget

`CostBudget { per_call_micro_usd: u64 }`. Brain ships
per-call only; per-deployment global budget is deferred (see
[`./07_plugins.md`](./07_plugins.md) — open questions).

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
(default values for known models ship with Brain; operators override
for negotiated rates). Brain ships an embedded default table
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

### 23. Schema validation

The `response_schema:` field in the extractor's `define
extractor { ... }` block is parsed as `serde_json::Value` (see
[`../03_schema/02_ast.md`](../03_schema/02_ast.md) — AST
`ExtractorField::Schema`). On response:

1. Parse response.content as JSON (`serde_json::from_str`).
2. Validate against the schema using a JSON-Schema validator
   (`jsonschema` crate, draft 7 default).
3. On validation failure, route to §21 retry.

If `response_schema: None`, the response is treated as a free-
form string and projected as a single `StatementMention` /
`EntityMention` per `target` (best effort).

### 24. LLM output projection

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

### 25. LLM determinism

Per [`./00_purpose.md`](./00_purpose.md):
- `temperature = 0` (default; configurable via extractor field).
- Schema validation rejects malformed; same input + cache hit →
  byte-identical output.
- Cache invalidation = drift event; uncached re-runs are treated
  as supersession.

The `model_metadata` audit field carries `model_version` (from
the response's model field, e.g., `"claude-haiku-4-5-20240307"`)
so downstream readers can detect provider-side rolling deploys.

### 26. LLM error model

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
  `status_reason`. Adaptive retry is a future enhancement.
- `Auth` → `Failure` with operator-actionable reason.
- `InvalidRequest` → `Failure`; prompt / schema bug.
- `OutputDecodeFailed` → `Failure`; sometimes recoverable via §21
  retry (the loop already exercises this).

### 27. LLM performance budget

Per [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| `LlmExtractor::predict` (cache hit) | 1 ms | 5 ms |
| `LlmExtractor::predict` (cache miss, claude-haiku) | 600 ms | 3 s |
| `LlmExtractor::predict` (cache miss, gpt-4o-mini) | 800 ms | 4 s |
| Cost-budget skip path | 200 µs | 1 ms |

These targets are dominated by external API latency and aren't
strictly enforceable on CI smoke benches against
mock HTTP servers. Production deployments operating against real
providers should set their own SLOs and instrument via the
`brain_extractors::audit` table.

### 28. LLM tests

Per [`./00_purpose.md`](./00_purpose.md) + the cache-key contract:

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
injected into the registry — CI does not hit live providers.

### 29. Bounded context

The LLM tier receives a **bounded** context window — not the agent's full history. Two constraints define it:

- `top_m_neighbors = 10` — the ten semantically nearest memories surfaced by the SemanticRetriever at extraction time. The neighbors carry whatever entities and statements the prior extraction passes resolved, so the LLM has anchors to reuse instead of resolving from scratch.
- **Rolling summary** — a single condensed paragraph produced by the summarizer worker. The summary covers the agent's recent activity at a much higher compression ratio than the raw memories.

Combined input cap: ~2k tokens median. Hard cap before the prompt is sent: 4k tokens — over-cap inputs trigger summary trimming + neighbor pruning until the cap is met.

```rust
pub struct LlmExtractContext {
    pub top_m_neighbors: Vec<MemoryDigest>,   // ≤ 10
    pub rolling_summary: Option<String>,      // from summarizer worker
}

impl LlmExtractor {
    pub async fn extract_with_context(
        &self,
        memory: &Memory,
        context: &LlmExtractContext,
    ) -> Vec<ExtractedItem> { ... }
}
```

The bounded context is **the cost moat** vs the "send everything" pattern. Unbounded-history extractors hit per-call costs an order of magnitude higher and ENCODE p99 budgets they can't honor; the bounded pattern keeps the LLM tier in budget while still giving the model enough context to resolve coreferents and dedupe near-duplicate statements against recent ones.

### 30. Supersession judge

The LLM extractor surface doubles as the host for the **supersession judge** invoked by the Tier 2 band of the five-tier supersession ladder (see [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) §"Statement supersession"). The judge is a separate prompt shape that runs through the same `LlmClient` + cache infrastructure.

Inputs:

```rust
pub struct SupersessionJudgeInput {
    pub subject_canonical_name: String,
    pub predicate_qname: String,
    pub existing: StatementSummary,    // current row's surface + extracted_at
    pub candidate: StatementSummary,   // new row arriving at STATEMENT_CREATE
    pub recent_evidence: Vec<String>,  // up to 3 surface forms from cited memories
}
```

Output is a typed verdict — `SUPERSEDES`, `COEXISTS`, or `CONTRADICTS` — accompanied by a brief rationale (audit-only, never user-facing).

Prompt structure splits into three cacheable blocks:

1. **Role block** — the judge persona + decision protocol (stable across all judge calls; `cache_control: ephemeral`, see [`./06_prompt_caching.md`](./06_prompt_caching.md)).
2. **Schema block** — the typed-verdict response schema (stable per validator version; same caching).
3. **Query block** — the per-call inputs above; not cached.

The role + schema blocks are the same across every Tier 2 invocation on a given shard. Steady-state cache_read ratio for the judge target ≥ 0.7.

Cost: typically one Claude Haiku call per Tier 2 hit. Cache hits on the role + schema blocks make the marginal cost dominated by the input query block (~200 tokens) plus the verdict response (~80 tokens). The five-tier ladder is engineered so that Tier 0/1/3 carry the bulk of decisions and only the ambiguous middle band pays the LLM cost.

### 31. LLM open questions

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md). Notably:

- Q-llm-1 — per-deployment global cost budget (only per-call in
  v1).
- Q-llm-2 — adaptive rate-limit retry (currently `Failure` on
  429).
- Q-llm-3 — proper tokenizer integration for cost estimation
  (v1 uses `chars / 4`).
- Q-llm-4 — local LLM backends (post-v1).
- Q-llm-5 — `STATEMENT_ADD_EVIDENCE` for richer per-call
  confidence.
