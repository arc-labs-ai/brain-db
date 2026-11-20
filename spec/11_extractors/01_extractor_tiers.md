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
  Rust function. Deferred.

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

- Exact CONLL-NER checkpoint, licensing, candle compatibility.
- Multi-memory batching (post-v1).
- User-supplied feature extractors (post-v1).

## LLM Extractor

The third extractor tier: LLM-driven extraction with cache,
bounded retry, per-call cost accounting, and provider-agnostic
transport. Slowest
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
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    confidence_threshold: f32,
    cache_ttl: Duration,
    /// `None` on a degraded extractor, which carries only `degraded_reason`.
    inner: Option<Arc<LlmExtractorInner>>,
    degraded_reason: Option<String>,
    /// When this extractor fires on the ENCODE path; `OnEncode` when the
    /// declaration omits a trigger.
    trigger: TriggerExpr,
}

/// Fully-wired state. There is no `client` field: the credential — and
/// therefore the client — depends on whose memory is being extracted, which
/// is not known until dispatch (§19.1).
pub struct LlmExtractorInner {
    pub resolver: Arc<dyn LlmCredentialResolver>,
    /// The declared `model:`, passed to the resolver as a preference.
    pub model: String,
    pub cache: Option<Arc<Mutex<LlmCacheDb>>>,
    pub prompt: String,
    pub examples: Option<Value>,
    pub response_schema: Option<Value>,
    /// Compiled draft-7 validator; `None` in free-form response mode.
    pub schema_compiled: Option<JSONSchema>,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout: Duration,
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
- `AnthropicClient` (claude-* models).
- `OpenAIClient` (gpt-*, o1-*, o3-*), with JSON-schema structured output mode.

Both address the provider directly, constructed with the key the namespace
registered and opened for the duration of the call.

The provider is the one stored with the credential, not a guess from the model
id. A tenant on a provider-specific model name is therefore never routed to the
wrong wire dialect.

The schema-declared `model:` field is a **preference, not a command**. The
resolver serves the model the namespace registered a key for, which may be a
different one. That is not a limitation to work around: a tenant's key can only pay for
what it is a key for, so honouring an extractor's declared model over the
tenant's registration would mean issuing a call that cannot be billed to anyone.

Unknown patterns fail at `LlmExtractor` construction (the materializer logs a
warn-level diagnostic and registers the extractor in degraded mode).

#### 19.1 The credential is per namespace, and there is no other kind

A tier that needs an LLM asks an `LlmCredentialResolver` for the client belonging to a **namespace**. There is no global router to fall back to, no shard-startup provider key, and therefore no `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env-var gate on which providers are "enabled": the provider is whatever that namespace registered, decided when a request arrives.

This is the only path. A deployment holds no provider credential of its own and no configuration can give it one — the full invariant is in [`./00_purpose.md`](00_purpose.md) §"The LLM tier's credential", along with the self-invalidating client cache and the `Missing`-vs-`Unavailable` split.

Two consequences for this tier specifically:

- **The client is built from the namespace's own key**, opened from the credential store for the call and dropped after it. The two client implementations are reused verbatim.
- **A namespace with no key never reaches this tier.** ENCODE is refused at admission with `LlmCredentialMissing`, because HyPE needs the same key. When a credential merely could not be *resolved*, the write lands and the tier reports `StageOutcome::Failed` with a reason on the `StageCompleted` event and in the encode trace, while the pattern and classifier tiers still contribute — the caller gets entities without statements, and is told why.

#### 19.2 Reasoning models need a budget that covers reasoning

A completion budget sized for the visible answer is not large enough for a model that spends tokens thinking first. On such a model a small `max_tokens` is consumed entirely by reasoning, the response comes back **empty**, and — because an empty response looks like a transient failure — the call is retried and billed again. The tier therefore sizes the completion budget to include reasoning headroom, and when it cannot, it says so in the stage's failure reason rather than returning an empty extraction. The budget is also sized for what a memory *says* rather than how long it is: the tier asks for a memory's whole typed graph as one JSON document, so a single dense paragraph — eight entities with their statements and relations — runs past a small ceiling. `[extractors.llm] max_tokens` defaults to **4096** when the operator sets none.

A reply the provider stopped at `max_tokens` is terminal on the first attempt, not routed to the §21 validation retry. The prompt is unchanged, so the model regenerates the same prefix and stops at the same ceiling; the operator would pay twice to be told "schema validation failed twice" about a response that never had a schema problem. The tier fails once with `ExtractionFailureClass::Permanent` and names the ceiling in the reason.

A billed reply is billed whether or not it is usable. Both providers price the response from its reported token usage **before** checking whether it carried any content, so a reply that cost money and parsed to nothing still records its cost on the audit row rather than reporting zero spend (§22).

Some models also refuse an explicitly-set `temperature`. The tier retries once without the parameter rather than treating the refusal as a provider failure.

Provider completions run on a Tokio runtime, **not** on the shard executor. The shard's executor is a thread-per-core Glommio runtime with no business blocking on a multi-second HTTP round trip.

Local LLM backends (llama.cpp / vLLM) are post-v1.

### 20. Cache integration

```text
LlmCacheKey = (input_hash, extractor_id, extractor_version, model_id, credential_scope)
```

**`input_hash` here is not a hash of the memory text.** It is
`cache_input_hash` — BLAKE3 over the owning tenant's space, the active
schema version, and the **fully materialized prompt-context body**: the
text, the anchor date, the declared entity types and statement kinds, the
candidate predicates, the prior entities, and the bounded neighbor and
summary context, all substituted in. The components are domain-separated —
fixed-width space and schema version, then a length-prefixed body — so no
two distinct inputs collide by concatenation.

Hashing the materialized body rather than the text alone is what makes a
cache hit mean "the exact context that produced this response has recurred".
Anything that would change the model's output changes the key, so a stale
response can never be served for a context that has moved on. It also means
a cache hit is *rarer* than text-identity would suggest: a grown
neighborhood, a new declared type, a different candidate-predicate set or a
schema-version bump all miss, and each of those is a case where serving the
old answer would be wrong.

The credential scope is what keeps one tenant's paid completion from being served to another tenant that hashes to the same input. It is derived from the namespace, never from key material — a scope is hashed into persisted rows and shown in diagnostics, and neither is a place for a key. The scope is **last** in the key so the TTL sweeper can still resolve a hint back to a prefix range. Tenant isolation is therefore enforced twice over: the space is already folded into `input_hash`, so two tenants cannot share an entry even for byte-identical text.

`predict` flow:

```text
1. Build LlmCacheKey with cache_input_hash(ctx, memory) as its
   input component (see above), plus id, extractor_version,
   model_id_hash and credential_scope.
2. cache.get(key):
   - Some(row) → decode `response_blob` per response_schema;
     skip the LLM call; mark `model_metadata.cache_hit = true`.
   - None     → proceed to step 3.
3. Record the projected spend for the call
   (`ModelPricing::estimate_request_micro_usd`) at `debug`.
   An observation, never a gate — see §22.
4. response = client.complete(&request).await.
   `response.cost_micro_usd` is priced from the provider's reported
   usage, before any check on whether content came back.
5. validate(response.content, response_schema):
   - Ok(parsed) → continue.
   - Err(_) and `response.truncated` → `Failure(PERMANENT)` naming
     `max_tokens`; no retry (§19.2).
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
audit row's `cost_micro_usd`, and the cached row's token count
reflects every token spent, so a per-extractor cost report does not
undercount a retried extraction.

### 22. Cost accounting

Brain is BYOK: the key that pays for a call belongs to the tenant,
not to the deployment. Spend is therefore **tracked, never
rationed**. No estimate, declared figure or running total can refuse
a call, and no audit status means "refused on cost".

#### Rates are nano-USD per token

Every field Brain *stores* spend in is micro-USD (1e-6 USD) — an
audit row's `cost_micro_usd`, the
`brain_extractor_llm_micro_usd_spent_total` metric, the `llm_spend`
ledger. A **rate** cannot live in that unit: the cheapest models are
priced below one dollar per million tokens, so gpt-4o-mini input at
$0.15/MTok is 0.15 micro-USD per token, which rounds either to
nothing or to more than four times the real price.

Rates are held per model in **nano-USD (1e-9 USD) per token** —
exactly the published dollars-per-million-tokens figure times one
thousand — and a call's total is converted to micro-USD once, at the
end, rounding to nearest. A call's rounding error is bounded by half
a micro-dollar instead of accumulating one rounding per token class.

```rust
pub struct ModelPricing {
    pub input_nano_usd_per_token: u64,
    pub output_nano_usd_per_token: u64,
    /// Anthropic's 5-minute ephemeral cache-WRITE price. Equal to the
    /// input rate on a provider that does not bill writes separately.
    pub cache_write_nano_usd_per_token: u64,
    /// Server-side cache reads: a tenth of the live input rate on
    /// Anthropic, a quarter on gpt-4.1. Billing these at the input
    /// rate would overstate every cache hit, which is the steady
    /// state for Brain's prompts (§06_prompt_caching).
    pub cache_read_nano_usd_per_token: u64,
}
```

The rate table is **embedded, not configurable** — there is no
pricing config file. It is keyed by model-id *prefix*, because
providers append a dated snapshot to the model string they report
back (`claude-haiku-4-5-20240307`, `gpt-4o-mini-2024-07-18`) and the
rate is the same across snapshots. The rows are ordered so a longer
prefix is tested before any prefix of it — `claude-sonnet-5-5` ahead
of `claude-sonnet-5`, `gpt-4o-mini` ahead of `gpt-4o` — since
reversing either pair silently prices a model at its larger
sibling's rate. Claude Haiku / Sonnet / Opus and the gpt-4.1 and
gpt-4o families are priced.

**An unknown model has no price.** The lookup returns `None` rather
than a house default: a plausible-looking fallback rate is
indistinguishable from a real one once it is summed into a ledger,
so it corrupts every total downstream of it silently. The token
counts are recorded, the cost is reported as zero, and a warning
names the model to add.

#### A billed call is priced before it is judged usable

Both providers compute `cost_micro_usd` from the reported usage and
only then check whether the response carried content. A reply that
was billed but parsed to nothing surfaces as `LlmError::EmptyContent
{ billed, .. }` carrying the priced response, so the spend is
recorded rather than lost. Pricing after the content check would
report zero for exactly the calls an operator most needs to see.

#### Where a priced call lands

Durable spend lives in two stores, and a total that reads one of
them is not a total:

- **Extraction tiers** record `cost_micro_usd` on their
  `ExtractionAudit` row, keyed by audit id and tied to the one
  memory and extractor that spent it.
- **HyPE generation, the HyPE refresh sweep and entity
  disambiguation** produce no audit row, so they accumulate into the
  durable `llm_spend` table, keyed `(namespace, stage)`.

Durable total spend is the sum of the two. Summing only the ledger
misses every extraction; folding extraction costs into the ledger
double-counts them against the audit rows that already hold them.
Neither subsumes the other: an audit row answers "what did
extracting *this memory* cost", which a `(namespace, stage)` counter
cannot express, and the ledger answers "what has this tenant spent
on a stage that writes no audit row at all".

Both are keyed on the same spelling of the tenant, which is what
makes the sum well-defined. `namespace_total_micro_usd(rtxn, ns)`
performs it: the ledger rows for `ns`, plus the `cost_micro_usd` of
every audit row attributed to `ns`. The **empty** namespace is the
one name that owns nothing — unattributed audit rows carry it, and
answering with their costs would report spend for a tenant that does
not exist — so it reads as zero. A tenant that has spent nothing
also reads as zero, not as an error. `llm_spend_iter` returns every
`(namespace, stage, micro_usd)` triple in key order: the
reconciliation read an operator runs against a provider invoice.
Both totals saturate rather than wrap, so an implausible figure pins
the counter instead of reporting a tenant as having spent almost
nothing.

Ledger stage bytes are stable and never reassigned:
`HYPE_GENERATE = 1`, `HYPE_REFRESH = 2`,
`ENTITY_DISAMBIGUATION = 3`.

#### The projected figure

Before a call the tier logs
`ModelPricing::estimate_request_micro_usd` at `debug`, so an
operator watching a tenant's key can see what is about to be billed
against it. It prices `approx_input_tokens()` (characters / 4) at
the live input rate and the full `max_tokens` at the output rate, so
it is an upper bound that never reads low. It reads the same rate
row the billed figure reads, so an estimate and a recorded cost
cannot disagree about the rate. It is an observation and nothing
branches on it.

#### `cost_budget` in the schema DSL

The brainschema grammar accepts a `cost_budget:` field on an
extractor declaration and the materializer reads it, but **nothing
acts on it**. A declaration carrying one is admitted and logged at
`warn` — "the field is not enforced — spend is recorded, not
capped" — so an operator who wrote one is told rather than left
believing a ceiling is in force.

After the call:
- `audit.cost_micro_usd = actual_cost_from_response_metadata`.
- `audit.namespace` names the tenant billed, spelled as the
  `llm_spend` ledger spells it.
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
    /// The provider completed and billed the call but returned no usable
    /// content. The priced response rides on the error, because spend
    /// dropped on the floor is spend the tenant was charged for and
    /// nothing in Brain ever recorded.
    EmptyContent { reason: String, billed: LlmResponse },
    Transport { source: reqwest::Error },
    Auth { provider: &'static str },
    RateLimit { retry_after_ms: u64 },
    InvalidRequest { reason: String },
    ProviderError { status: u16, message: String },
    /// The provider HTTP runtime could not be built, so nothing was sent
    /// and nothing was billed.
    ProviderRuntimeUnavailable,
    /// The task carrying the request never produced a result — it
    /// panicked, or the runtime cancelled it on shutdown.
    RequestTaskFailed { reason: String },
    Timeout,
    OutputDecodeFailed { reason: String },
}
```

Mapping to audit `status`:
- `Transport` / `Timeout` / `ProviderError` (5xx) → `Failure`, classified `TRANSIENT`: retried under the bounded ladder in [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md) §"Bounded extraction retries".
- `RateLimit` → `Failure` with `retry_after_ms` in `status_reason`. The tier does not read the hint to schedule its own retry; the ladder's own backoff governs. Adaptive retry is a future enhancement.
- `Auth` → `Failure`, classified `PERMANENT` with an operator-actionable reason. Terminal on the first failure — a missing or rejected key does not fix itself, and retrying hides it.
- `InvalidRequest` → `Failure`, `PERMANENT`; prompt / schema bug.
- `OutputDecodeFailed` → `Failure`; sometimes recoverable via the §21 retry.
- `EmptyContent` → `Failure`, **and the billed cost is recorded**. The response was paid for, so the audit row carries its `cost_micro_usd` rather than zero (§22).
- `ProviderRuntimeUnavailable` → `Failure`. The runtime is built at most once per process and its outcome is cached, failure included, so no retry inside this process can reach a provider. A restart is the only recovery.
- `RequestTaskFailed` → `Failure`. A panic is a defect in the request path that repeats on the same input. The round trip may already have reached the provider and been billed, and that usage is unrecoverable: the task's own result is what would have carried it back.

Two of these name spend that cannot be reconciled from Brain's side — `RequestTaskFailed` loses a billed call's usage outright, and an `EmptyContent` reply is the failure most likely to repeat, since the same prompt against the same ceiling returns empty and is billed again. The refresh gate in the HyPE sweep exists for exactly that second case; see [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md).

### 27. LLM performance budget

Per [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| `LlmExtractor::predict` (cache hit) | 1 ms | 5 ms |
| `LlmExtractor::predict` (cache miss, claude-haiku) | 600 ms | 3 s |
| `LlmExtractor::predict` (cache miss, gpt-4o-mini) | 800 ms | 4 s |
| Degraded-extractor short-circuit (no provider call) | 200 µs | 1 ms |

The last row is the cost of writing one audit row and returning —
the same figure [`./04_audit.md`](./04_audit.md) §7 gives for
`write_audit_row`. It is the only no-call path the tier has: spend
is never a reason to short-circuit (§22).

These targets are dominated by external API latency and aren't
strictly enforceable on CI smoke benches against
mock HTTP servers. Production deployments operating against real
providers should set their own SLOs and instrument via the
`brain_extractors::audit` table.

### 28. LLM tests

Per [`./00_purpose.md`](./00_purpose.md) + the cache-key contract:

- Cache-hit returns cached items + sets `model_metadata.cache_hit`.
- Cache-miss writes through to the cache.
- Schema-validation failure → one retry with error in prompt;
  validation passes on retry → `Success`.
- Schema-validation failure twice → `Failure(reason: "schema
  validation failed twice")`.
- Response truncated at `max_tokens` → one `Failure(PERMANENT)`
  naming the ceiling; **no** second call (§19.2).
- A billed reply with no usable content still reports its cost.
- `LlmError::RateLimit { retry_after_ms }` → `Failure` audit with
  the retry-after info in `status_reason`.
- A model with no row in the rate table → token counts recorded,
  cost reported as zero, warning names the model.
- Unwired extractor (no credential resolvable, unknown model) →
  `SkippedDisabled` audit carrying the degraded reason; no call.
- A declared `cost_budget` changes no outcome; the materializer
  warns that it is unenforced.

Integration tests in `brain-server` use a mock `LlmClient`
injected into the registry — CI does not hit live providers.

### 29. Bounded context

The LLM tier receives a **bounded** context window — not the space's full history. Two constraints define it:

- `top_m_neighbors = 10` — the ten semantically nearest memories surfaced by the SemanticRetriever at extraction time. The neighbors carry whatever entities and statements the prior extraction passes resolved, so the LLM has anchors to reuse instead of resolving from scratch.
- **Rolling summary** — a single condensed paragraph produced by the summarizer worker. The summary covers the space's recent activity at a much higher compression ratio than the raw memories.

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

The bounded context is **the cost moat** vs the "send everything" pattern. Unbounded-history extractors hit per-call costs an order of magnitude higher and ENCODE p99 budgets they can't honor; the bounded pattern holds the LLM tier's per-call cost down while still giving the model enough context to resolve coreferents and dedupe near-duplicate statements against recent ones.

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

- Adaptive rate-limit retry (currently `Failure` on 429).
- Proper tokenizer integration for the projected-spend figure
  (`approx_input_tokens` uses `chars / 4`).
- Local LLM backends (post-v1).
- `STATEMENT_ADD_EVIDENCE` for richer per-call confidence.
