# 10 — Extractors

**Audience:** anyone reading an `extractor_audit` row, adding a
new extractor kind, debugging "why didn't this memory produce a
Statement," or thinking about LLM costs.

**Goal:** by the end you should know what each tier does, the
trade-off matrix between them, how an extractor stays
idempotent across replays, where the LLM tier's cache lives,
and what controls operators have over spend.

This chapter is the *who fills the knowledge tables* answer to
[chapter 09](09-knowledge-layer.md). The tables are storage;
extractors are the producer side.

---

## What an extractor is

An extractor turns unstructured memory text into typed Entities,
Statements, and Relations. The contract is one function shape:

```
fn extract(memory: &Memory, ctx: &Context) -> Vec<ExtractedItem>
```

…with a few specific properties:

- **Idempotent.** Same memory + same extractor version + same
  schema version ⇒ same output.
- **Audited.** Every invocation writes an `extractor_audit`
  row, success or failure.
- **Bounded.** Pattern and classifier extractors are
  microsecond-to-millisecond; LLM extractors have an explicit
  per-call cost budget.
- **Composable.** A schema can declare many extractors; they
  all run on each eligible memory; outputs merge.

The pipeline that runs them is one *registry* per shard, and the
registry holds `Arc<dyn Extractor>` instances of three concrete
kinds. The crate is `brain-extractors`
(`crates/brain-extractors/src/lib.rs:1`); the LLM transport
clients live in `brain-llm` (`crates/brain-llm/src/lib.rs:1`).
Both crates set `#![forbid(unsafe_code)]`.

---

## The three tiers

```
        cheap                                              expensive
        ──────────────────────────────────────────────────────────►

        Pattern               Classifier              LLM
        regex                 pinned model            cached call
        10-100 µs/memory      1-10 ms/memory          0.1-10 s/memory
        $0                    ~CPU                    $0.0001 to $0.10
        very high precision   high precision          high (with validation)
        low recall (narrow)   medium recall           high recall
        deterministic         deterministic           cached → deterministic
        fully sync            sync                    async (HTTP)
```

The pipeline composes them naturally: a single memory passes
through every registered extractor whose trigger fires, in some
ordering, producing a merged set of `ExtractedItem`s. Operators
configure each extractor in the Schema DSL — see
[chapter 09](09-knowledge-layer.md).

The unifying interface is `Extractor`
(`crates/brain-extractors/src/extractor.rs:33`):

```rust
pub trait Extractor: Send + Sync {
    fn id(&self) -> ExtractorId;
    fn kind(&self) -> ExtractorKind;
    fn name(&self) -> &str;
    fn extractor_version(&self) -> u32;
    fn run<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mem: &'a Memory,
    ) -> ExtractionFuture<'a>;
}
```

`run` returns a *boxed future* even though pattern and
classifier extractors are synchronous. The reason is the LLM
tier: an LLM extractor's `run` calls out over HTTP and must
`.await`. A uniform return type lets the registry hold all
three behind one trait object. Sync impls wrap their body in
`Box::pin(async move { ... })` — a one-time boxing cost per
call.

The output is a `Vec<ExtractedItem>`
(`crates/brain-extractors/src/item.rs`), with three variants
matching the three knowledge-layer object types:
`EntityMention`, `StatementMention`, `RelationMention`. The
materialiser turns these into row writes in the next stage.

---

## Tier 1: pattern

```
define extractor person_mentions {
    kind: pattern
    target: entity Person
    patterns [
        /\b([A-Z][a-z]+ [A-Z][a-z]+)\b/,
        /\b([A-Z]\. [A-Z][a-z]+)\b/,
    ]
    confidence: 0.7
}
```

`PatternExtractor`
(`crates/brain-extractors/src/pattern.rs:44`) compiles every
declared regex *once*, at registry construction, with
size-limit caps:

```rust
const REGEX_SIZE_LIMIT: usize = 1 << 20;  // 1 MiB NFA+DFA
```

(`crates/brain-extractors/src/pattern.rs:20`)

A pattern bigger than that fails `try_new` with
`ExtractorError::ResourceLimit`. The cap exists because regex
engines can blow up on adversarially-crafted patterns —
catastrophic backtracking, exponential DFAs. The size limit
keeps a misconfigured schema from taking the shard down.

### Semantics

For each registered pattern, the extractor runs the regex
against the memory text, captures every match, and projects
each match to an `ExtractedItem` of the declared `target` type
(Entity, Statement, or Relation). The `confidence` field is
*fixed* — patterns don't compute per-match confidence; they
assert "this regex hit is N% likely to be a real mention."

The match span (`start`, `end` offsets within the memory text)
travels with the item so downstream tools can highlight or
re-anchor it.

### When patterns are right

- IDs, URLs, code identifiers, dates in well-known formats.
- Names with a stable surface form ("Acme Corp," "Bldg. 42").
- Triggers for the higher tiers (a pattern can pre-filter; if
  no match, save the LLM call).

### When patterns are wrong

- Paraphrase ("the manager," "she"). Anaphora resolution is
  not regex's job.
- Declension / morphology. "Priya's" should match Priya, but
  doesn't without an explicit pattern.
- Anything semantic. A regex doesn't know what "the meeting"
  refers to.

The doc string of every shipped pattern extractor includes a
note about which of these limits applies — operators reading
the registry can tell at a glance what they're getting.

---

## Tier 2: classifier

```
define extractor reporting_lines {
    kind: classifier
    target: relation reports_to
    model: "brain-reporting-line-classifier-v3"
    feature_extraction: builtin
    confidence_threshold: 0.8
    trigger: on encode where memory.text matches ".*report.*to.*"
}
```

`ClassifierExtractor`
(`crates/brain-extractors/src/classifier.rs:333`) wraps a
*pinned, deterministic* model — typically a fine-tuned
BERT-like, a small LSTM, or a logistic regression. The crate
ships a token classifier backed by candle
(`BertTokenClassifier`,
`crates/brain-extractors/src/classifier.rs:28`); operators bring
the model weights, the crate provides the runtime.

### Determinism

The "deterministic" claim in the table above relies on three
properties of the classifier setup:

- **Pinned weights.** Identical safetensors file across runs.
- **Pinned tokenizer.** Same `tokenizer.json`.
- **No sampling / temperature** in the inference loop —
  greedy decode only.

Together those produce **bit-exact** outputs across runs. A
classifier extractor with the same `model_id` always produces
the same output on the same input. That's what makes its
idempotency story tight without a cache.

### Where it sits in latency

1–10 ms per memory on CPU, dominated by the forward pass. Faster
than the BGE embedder (chapter 06) only because classifier
models are typically smaller — a token-classification head over
DistilBERT, say, instead of a full BERT-base for embedding.

The classifier reuses the same candle runtime patterns the
embedder uses, including the no-pickle / safetensors-only rule.

### Degraded mode

When the operator-provided model path isn't configured (env var
unset, file missing), the extractor materialiser
(`materialize_classifier_extractor`) constructs the extractor in
**degraded mode** — its `run` returns `Failure(reason)` on
every dispatch. The schema is still valid; the extractor is
still registered; you just see audit rows with failure status
and a clear reason. The shard doesn't refuse to spawn over a
missing classifier file.

This is the same pattern the LLM tier uses (next section) — a
configuration gap is loud-failure-per-invocation, not
boot-failure.

---

## Tier 3: LLM

```
define extractor preference_extraction {
    kind: llm
    target: statement Preference
    model: "claude-haiku-4-5"
    prompt: """..."""
    examples: [...]
    schema: { type: array, items: { ... } }
    cache: enabled
    cache_ttl: 90d
    confidence_threshold: 0.7
    cost_budget: "$0.001 per memory"
    trigger: on encode where memory.kind = episodic
}
```

`LlmExtractor`
(`crates/brain-extractors/src/llm.rs:118`) is the
heavyweight tier. Per call:

1. Look up the (input_hash, extractor_id, extractor_version,
   model_id) tuple in `llm_cache.redb`. **Cache hit ⇒ return
   the cached response** — no HTTP traffic, no spend, no
   non-determinism.
2. **Cache miss ⇒** estimate the cost. If it exceeds
   `cost_budget.per_call_micro_usd`, status is
   `SkippedBudget`; no call made.
3. **Within budget ⇒** dispatch the request via `LlmClient`
   (Anthropic or OpenAI; see below).
4. **Validate the response** against the declared JSON schema.
   If invalid, retry once with the validation error fed back
   into the prompt
   (`crates/brain-extractors/src/llm.rs:9`). Still invalid ⇒
   drop and log.
5. **Project the validated JSON** to `EntityMention` /
   `StatementMention` / `RelationMention` per the extractor's
   `target` field.
6. **Write the response to the cache** keyed by the input tuple
   above.

The cache is the single most important reliability feature in
this tier. A *cache hit means determinism*: same memory + same
extractor version + same model ⇒ identical output, exactly
like pattern and classifier. The HTTP-call-side
non-determinism only exists on a cold key.

### `CostBudget` and `Pricing`

`CostBudget` is a per-call ceiling in *micro-USD*
(`crates/brain-extractors/src/llm.rs:47`):

```rust
pub struct CostBudget {
    pub per_call_micro_usd: u64,
}
```

`Pricing` is per-model
(`crates/brain-extractors/src/llm.rs:57`):

```rust
pub struct Pricing {
    pub input_micro_usd_per_token: f64,
    pub output_micro_usd_per_token: f64,
}
```

`estimate_cost`
(`crates/brain-extractors/src/llm.rs:103`) computes the
worst-case price assuming the response fills `max_tokens`:

```rust
in_tokens * pricing.input_micro_usd_per_token
  + out_tokens * pricing.output_micro_usd_per_token
```

…rounded up. If that exceeds the per-call cap, the call is
skipped. **A skipped call still writes an audit row** with
status `SkippedBudget` and the projected cost — operators see
exactly what would have been spent, which makes budget tuning
informed rather than blind.

Built-in pricing covers Claude Haiku, Claude Sonnet, GPT-4o
mini (`Pricing::for_model`,
`crates/brain-extractors/src/llm.rs:77`); unknown models fall
back to a conservative default. Operators override the table
per-deployment.

### `ModelRouter`

The mapping from a model string to a provider is
`brain_llm::ModelRouter`
(`crates/brain-llm/src/router.rs:57`):

```rust
pub struct ModelRouter {
    anthropic: Option<Arc<dyn LlmClient>>,
    openai: Option<Arc<dyn LlmClient>>,
}
```

Built at shard startup from env vars (`ANTHROPIC_API_KEY` /
`OPENAI_API_KEY`). `Provider::classify` is a prefix match
(`crates/brain-llm/src/router.rs:24`):

- `claude-*` or `anthropic/*` → Anthropic.
- `gpt-*`, `openai/*`, `o1-*`, `o3-*` → OpenAI.
- Else `Unknown`.

A model string the router can't classify, or one whose
provider key isn't configured, makes the materialiser
construct the LLM extractor in **degraded mode** — same loud-
failure-per-invocation pattern as the classifier tier. Brain
boots; the extractor refuses each call with a clear reason.

### Per-shard cache: `llm_cache.redb`

A *separate redb file* per shard:
`<data_dir>/<shard_id>/llm_cache.redb`
([chapter 03](03-arena-and-wal.md) §"What's actually on disk").
Why a separate file?

- The cache is heavy (10 GiB cap by default).
- Cache writes shouldn't compete with hot substrate writes.
- A backup snapshot can omit the cache cheaply.

The cache key is `(input_hash, extractor_id, extractor_version,
model_id)`. Default TTL is **7 days**
(`crates/brain-extractors/src/llm.rs:42`); the
`llm_cache_sweeper` worker
([chapter 07](07-background-workers.md)) prunes expired
entries on its hourly cadence.

Bumping `extractor_version` or changing `model_id` doesn't
invalidate old entries explicitly — it just makes them never
hit again, and the sweeper ages them out.

---

## Idempotency, in detail

The contract is: same input ⇒ same output. The key is
`IdempotencyKey`
(`crates/brain-extractors/src/idempotency.rs:6`):

```rust
pub struct IdempotencyKey {
    pub memory_id: MemoryId,
    pub text_hash: [u8; 32],
    pub extractor_id: ExtractorId,
    pub extractor_version: u32,
    pub schema_version: u32,
}
```

The `text_hash` is BLAKE3 of the memory text bytes
(`crates/brain-extractors/src/idempotency.rs:36`). Why include
both `memory_id` and `text_hash`? They're the two reasons a
single logical extraction might come up again:

- A new `MemoryId` is a new memory; the extraction is fresh.
- A re-extraction triggered by a schema change uses the same
  `MemoryId` but possibly a different `extractor_version` —
  the key changes, the extraction reruns.
- A storage-layer corruption or a `MIGRATE_EMBEDDING` could
  give us "same memory_id, different text" — the `text_hash`
  catches that.

### How each tier enforces idempotency

- **Pattern.** Deterministic by construction. No cache needed.
  Replay produces the same output.
- **Classifier.** Deterministic by pinned-weights + pinned
  tokenizer + greedy decode. No cache needed.
- **LLM.** Cached. Hit ⇒ deterministic. Miss ⇒
  `temperature = 0` and schema validation make repeat calls
  *very likely* identical, but the spec treats LLM output as
  non-deterministic and the cache is the contract.

This means three replay scenarios:

- **Replay an extraction over an unchanged memory** (e.g., the
  backfill worker rediscovering it) — pattern and classifier
  recompute identically; LLM hits the cache and returns the
  same response.
- **Re-extract after a bumped `extractor_version`** — all three
  tiers compute fresh. LLM cache miss because the key changed.
  Old Statements are flagged stale; the new ones supersede
  Preferences and add to Facts.
- **Re-extract after a schema upgrade that adds a new
  extractor** — only the new extractor runs (the old
  extractors don't see a key change). Memories that already
  had outputs get new ones from the new extractor.

The `stale_extraction_detector` worker
([chapter 07](07-background-workers.md)) is what flags
"memories that should be reprocessed because their last
extraction was against an older version."

---

## The extractor registry

One per shard
(`crates/brain-extractors/src/registry.rs:16`):

```rust
pub struct ExtractorRegistry {
    by_id: HashMap<ExtractorId, Arc<dyn Extractor>>,
    enabled: HashSet<ExtractorId>,
}
```

Built from `EXTRACTORS_TABLE` rows at shard open. Updated when
`SCHEMA_UPLOAD` lands new extractors or when
`EXTRACTOR_ENABLE` / `EXTRACTOR_DISABLE` flips a flag. The
registry is wrapped in a `RwLock` inside `OpsContext`
([chapter 01](01-system-architecture.md)) so admin wire ops can
mutate it on the shard executor.

`is_enabled` / `set_enabled` are the runtime toggles. A
disabled extractor stays registered (so `EXTRACTOR_LIST` can
report it) but doesn't run on incoming memories.

### The dispatcher

The dispatcher (lives in `brain-ops`, not `brain-extractors`) is
what wires this together on ENCODE. After a memory is durably
written ([chapter 03](03-arena-and-wal.md)), the dispatcher:

1. Iterates `registry.iter_enabled()`.
2. Evaluates each extractor's trigger against the memory.
3. For each fired extractor, calls `extractor.run(&ctx, mem)`.
4. Writes the audit row.
5. Hands the `Vec<ExtractedItem>` to the *materialiser*.

The dispatcher applies dependency ordering when extractors
declare `depends_on` — an extractor that needs another's
output waits for the dep's items before running.

Pattern extractors run **synchronously during ENCODE**.
Classifier extractors are usually synchronous too. LLM
extractors run in **background workers** because they
`.await` HTTP and can't sit on the ENCODE hot path.

---

## The materialiser

`materialize::*`
(`crates/brain-extractors/src/materialize.rs`) turns the
generic `Vec<ExtractedItem>` into actual writes to
[chapter 09](09-knowledge-layer.md)'s tables:

- **`EntityMention`** ⇒ run the entity resolver to find or
  create the entity, write a row to `entity_mentions`.
- **`StatementMention`** ⇒ resolve subject + object, intern
  the predicate, write the row to `statements` and all six
  secondary indexes.
- **`RelationMention`** ⇒ resolve both endpoints, write to
  `relations` and the direction indexes.

The resolver is the hard part (it's the four-table fan-out
described in [chapter 09](09-knowledge-layer.md)). Same set of
tables, same resolution order: exact canonical name → alias
→ trigram → embedding similarity → ambiguous-pending audit row.

The materialiser is where idempotency on the storage side gets
applied. If an item's resolved subject already has a
non-superseded Statement with the same predicate and object,
the materialiser handles the supersession (for Preferences) or
the contradiction (for Facts) per
[chapter 09](09-knowledge-layer.md)'s rules.

---

## Triggers

Triggers gate *whether* an extractor runs on a given memory.
The Schema DSL covers three forms:

```
trigger: on encode                                # run on every ENCODE
trigger: on encode where memory.kind = episodic   # filtered
trigger: on demand                                # only operator-invoked
trigger: on schema_change                         # run during migration
trigger: periodic at "0 0 * * *"                  # cron-like
```

For an `on encode` trigger, the dispatcher evaluates the
`where` clause against the memory's metadata before running
the extractor. A miss results in an audit row with status
`SkippedFilter` — same audit shape, different status.

`on demand` extractors don't fire from the dispatcher; they're
invoked by an admin RPC, typically as part of a one-off
backfill.

`on schema_change` extractors fire from the `schema_migration`
worker ([chapter 07](07-background-workers.md)) when a new
`extractor_version` lands. They re-run over relevant memories
and produce fresh outputs.

`periodic` extractors fire from a cron scheduler. v1 supports
the syntax but the scheduler integration is light — operators
should use external orchestration (cron, systemd timers) plus
admin RPCs in deployments that need predictable cadence.

---

## Audit trail

Every invocation writes an `extractor_audit` row, success or
failure ([chapter 09](09-knowledge-layer.md)). The fields:

```rust
struct ExtractionAudit {
    id: AuditId,
    memory_id: MemoryId,
    extractor_id: ExtractorId,
    extractor_version: u32,
    schema_version: u32,
    started_at: u64,
    completed_at: u64,
    status: ExtractionStatus,
    outputs: Vec<OutputRef>,
    cost: f32,                     // micro-USD, 0 for pattern/classifier
    error: Option<String>,
    model_metadata: Option<...>,   // LLM: model, token counts
}
```

`ExtractionStatus`
(`crates/brain-extractors/src/extractor.rs:120`) is a *wire-
stable* `u8` discriminator — adding a variant appends, never
renumbers:

| Status | Byte | Means |
|---|---|---|
| `Success` | 1 | Items produced (possibly empty list). |
| `Failure` | 2 | Extractor errored. |
| `SkippedBudget` | 3 | LLM tier hit `cost_budget`. |
| `SkippedFilter` | 4 | Trigger `where` clause didn't fire. |
| `SkippedDuplicate` | 5 | Idempotency cache hit. |
| `SkippedDisabled` | 6 | Extractor was disabled at dispatch time. |

The byte-stable encoding matters because the audit table is a
read-mostly history that operators query weeks later. Renumbering
would orphan old rows.

The audit log is queryable by `memory_id`, `extractor_id`, or
time range — see the audit table indexes in
[chapter 09](09-knowledge-layer.md). Default retention 90 days
(`audit_log_sweeper`, [chapter 07](07-background-workers.md)).

---

## Cost controls

Three layers:

1. **Per-call budget** in the extractor declaration
   (`cost_budget`). Enforced at `estimate_cost` vs
   `CostBudget::per_call_micro_usd`. The cheapest knob —
   prevents one over-large memory from costing a multiple of
   your typical extraction.
2. **Per-deployment budget** (future). v1 ships per-call only
   (`crates/brain-extractors/src/llm.rs:44`); a global
   daily/weekly cap is deferred.
3. **Trigger conditions** in the schema. The cheapest call is
   the one you don't make. A well-tuned `where` clause skips
   the bulk of memories that aren't worth extracting from.

The audit log makes (1) observable per memory and the cost-
sweeper makes (3) tunable — operators see which extractors run
on the largest fraction of memories and adjust triggers.

---

## Failure modes

**Pattern compilation fails at schema load.** Schema validation
catches it and reports the offending pattern's index back to
the client. The schema isn't applied.

**Pattern regex exceeds the 1 MiB size limit.**
`ExtractorError::ResourceLimit`. Same handling — schema rejects.

**Classifier model file missing or wrong shape.** Materialiser
constructs the extractor in degraded mode; every invocation
audits `Failure("model not found / failed to load")`. Schema
upload succeeds; the operator fixes the model and restarts.

**LLM provider unreachable.** Per-call HTTP error. The
extractor audits `Failure(http_error)`. The cache stays warm
for memories that have hit before; new ones get the failure.
A retry policy at the call site (configurable) handles
transient failures.

**LLM response fails JSON schema validation.** One retry with
the validation error fed back into the prompt
(`crates/brain-extractors/src/llm.rs:9`). If still invalid,
`Failure("schema validation failed")`. No partial output is
written.

**LLM response exceeds `cost_budget` at estimate time.**
`SkippedBudget` audit; no call.

**Cache write fails (disk full).** Logged; the LLM call's
output is *still returned to the materialiser* — we don't
penalise the current request for a cache-write failure. The
next call will miss again and re-spend.

**Trigger evaluation throws.** Rare — triggers are simple
expressions — but if it does, the dispatcher logs and treats
the trigger as "didn't fire," with a `SkippedFilter` audit
carrying the eval error.

---

## Configuration & tuning

| Knob | Where | Default | Notes |
|---|---|---|---|
| `cache_ttl` | per LLM extractor (or default) | 7 days | Per-key. The sweeper enforces. |
| `cost_budget.per_call_micro_usd` | per LLM extractor | (schema-defined) | Hard ceiling per call. |
| `confidence_threshold` | per extractor | (schema-defined) | Items below threshold drop. |
| LLM provider keys | env (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) | (unset) | Unset ⇒ degraded mode for matching extractors. |
| `llm_cache.redb` size cap | TOML | 10 GiB | LRU after cap; sweeper for TTL. |
| Regex size limit | code | 1 MiB | NFA + DFA combined. |

Operational rules:

- **Use patterns for everything you can.** They're free and
  deterministic. Promote to classifier only when patterns
  can't capture the surface; promote to LLM only when the
  classifier can't capture the semantics.
- **Watch the `extractor_audit` failure rate per extractor.** A
  pattern with a high failure rate is over-broad; a classifier
  with high failures is mismatched to the data; an LLM
  extractor with high failures is producing malformed JSON
  (tighten the prompt or schema).
- **Tune triggers before raising `cost_budget`.** Filtering out
  ineligible memories saves money predictably; raising the
  per-call cap only helps the few large memories that
  legitimately need more tokens.
- **Don't disable `llm_cache_sweeper`** — without it, the
  cache grows past the LRU cap and you pay more than you
  budgeted.
- **`temperature = 0` is the default; keep it that way** for
  any extractor whose audit trail matters. The cache promise
  rests on it.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Crate root, exports | `crates/brain-extractors/src/lib.rs` |
| `Extractor` trait, `ExtractionContext`, `ExtractionResult` | `crates/brain-extractors/src/extractor.rs` |
| `ExtractedItem` (Entity / Statement / Relation mention) | `crates/brain-extractors/src/item.rs` |
| `ExtractionStatus` byte mapping | `crates/brain-extractors/src/extractor.rs` |
| `IdempotencyKey`, `hash_memory_text` | `crates/brain-extractors/src/idempotency.rs` |
| `ExtractorRegistry` | `crates/brain-extractors/src/registry.rs` |
| `PatternExtractor`, `CompiledRegex`, size limit | `crates/brain-extractors/src/pattern.rs` |
| `ClassifierExtractor`, `BertTokenClassifier`, BIO decoding | `crates/brain-extractors/src/classifier.rs`, `labels.rs` |
| Candle runtime shared by classifier + LLM | `crates/brain-extractors/src/candle_runtime.rs` |
| `LlmExtractor`, `CostBudget`, `Pricing`, `estimate_cost` | `crates/brain-extractors/src/llm.rs` |
| `materialize_*_extractor` | `crates/brain-extractors/src/materialize.rs` |
| `LlmClient` trait, `LlmRequest`/`LlmResponse` | `crates/brain-llm/src/{client.rs, types.rs}` |
| `ModelRouter`, `Provider::classify` | `crates/brain-llm/src/router.rs` |
| Anthropic backend | `crates/brain-llm/src/anthropic.rs` |
| OpenAI backend | `crates/brain-llm/src/openai.rs` |
| LLM cache table | `crates/brain-metadata/src/llm_cache.rs` |

---

## Further reading

- [09 — Knowledge layer](09-knowledge-layer.md) for the tables
  extractors write into.
- [11 — Hybrid retrieval (RRF)](11-hybrid-retrieval-rrf.md) for
  how the entities and statements extractors produce are
  read back.
- [07 — Background workers](07-background-workers.md) for the
  `llm_cache_sweeper`, `schema_migration`, `stale_extraction_detector`,
  `backfill`, and `audit_log_sweeper` workers that complete the
  extractor lifecycle.
- [06 — Embedding pipeline](06-embedding-pipeline.md) for the
  candle-runtime patterns the classifier tier reuses.
