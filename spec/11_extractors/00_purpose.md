# 11. Extractors

> **TL;DR.** Three-tier pipeline that derives Entities, Statements, and Relations from Memories. **Extractors are always wired** — every shard runs them on every ENCODE regardless of whether a user schema is declared. All three tiers run **asynchronously** in the per-shard extractor worker — ENCODE enqueues the memory and acknowledges, then the worker runs pattern (regex, tens of microseconds, free) → classifier (pinned model, milliseconds, cheap) → LLM (cached, hundreds of milliseconds to seconds, dollar-significant) over it, with strict cost budgets, schema-validated output, and a per-call cache keyed by `(input_hash, extractor_version, model_version)`. No extractor output exists when ENCODE returns. Persistence is **per-entity gated**: an extracted entity / statement / relation whose type exists in some active schema namespace is persisted; one whose type is undeclared is silently dropped (extraction is best-effort). Tier-level enable flags live in `config.toml`; an enabled tier that fails to load at shard spawn → `ShardError::ExtractorInitFailed`. All tiers are required to be idempotent. Built-in extractors ship for common entity types, temporal expressions, and basic relations.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the extractor pipeline; schema authors writing extractor definitions; operators tuning budgets |
| Voice | Hybrid (rationale + normative) |
| Depends on | [02. Data Model](../02_data_model/00_purpose.md), [03. Schema DSL](../03_schema/00_purpose.md), [07. Embedding Layer](../07_embedding/00_purpose.md), [10. Metadata + Graph Store](../10_metadata/00_purpose.md) |
| Referenced by | [13. Retrievers](../13_retrievers/00_purpose.md), [15. Background Workers](../15_background_workers/00_purpose.md) |

## What this spec defines

The three-tier extractor pipeline that turns raw Memory text into typed Entities, Statements, and Relations: pattern matching (Tier 1) → GLiNER classifier (Tier 2) → LLM (Tier 3). Includes the resolver gauntlet (surface name → `EntityId`), per-extraction audit, idempotency cache, prompt-caching scaffolding, and the plugin surface (enricher + connector hooks).

The pipeline's design goal: **reject ~80-90% of naive candidates before storage**. The cheaper tiers handle the bulk; the LLM tier only sees what survives. This is the cost moat against extract-everything-via-LLM systems.

## Always-on, persistence-gated

Extractors are **not** a schema-gated capability. They run on every ENCODE, on every shard, regardless of how many user namespaces are active. The model is:

```
ENCODE → extract (pattern → classifier → LLM tiers per config)
       → per-candidate persistence check:
           candidate.type ∈ some active schema namespace
             → persist into typed-graph tables
           else
             → drop silently (best-effort)
```

The seeded `brain:` system namespace already declares the common entity types (Person, Place, Organization) the built-in extractors target, so even shards without any user `SCHEMA_UPLOAD` produce useful typed-graph rows. Adding a user namespace declaring `Project` (say) extends the persisted set — extracted Person candidates still land via `brain:Person`, and extracted Project candidates now also land via `acme:Project`.

### Operator-controlled tier gates

Each tier is independently enabled via `config.toml`:

```toml
[extractors.pattern]
enabled = true

[extractors.classifier]
enabled = true

[extractors.llm]
enabled = true     # set false to skip the LLM tier on this shard
```

- A **disabled** tier is skipped silently — the operator chose to opt out; this is not a degradation, no warning is logged.
- An **enabled** tier that fails to load at shard spawn (model file missing, classifier weights corrupt, LLM client init error) is a hard spawn failure: `ShardError::ExtractorInitFailed { tier, source }`. The shard refuses to start rather than running with a quietly-missing tier.

There is no `has_llm_extractor` planning input. The pipeline always runs whichever tiers are loaded; clients call `GET_CAPABILITIES` (§04/03) to introspect which tiers are live on the connected shard.

## Purpose

An extractor is a pipeline that derives structured data (Entities, Statements, Relations) from unstructured Memories. Extractors are declared in the schema and run as background workers.

Three kinds, in increasing order of capability and cost:

| Kind | Latency | Cost | Determinism | Recall | Precision |
|---|---|---|---|---|---|
| **Pattern** | 10-100 µs | ~0 | Yes | Low (narrow) | Very high |
| **Classifier** | 1-10 ms | Low | Yes (pinned model) | Medium | High |
| **LLM** | 100 ms - 10 s | High (per-call) | No (cached) | High | High (with validation) |

Brain runs them as a pipeline: pattern first (fast, free), then classifier (slow, cheap), then LLM (slowest, expensive). Each tier can be configured to either *replace* or *supplement* the previous tier's output.

## Pattern extractors

```
define extractor person_mentions {
    kind: pattern
    target: entity Person
    patterns [
        /\b([A-Z][a-z]+ [A-Z][a-z]+)\b/,    # First Last
        /\b([A-Z]\. [A-Z][a-z]+)\b/         # F. Last
    ]
    confidence: 0.7
    on_match: resolve_entity
}
```

Pattern semantics:
- Run on every ENCODE (enqueued to the per-shard extractor worker; never inline).
- Apply regex to memory text.
- For each match: invoke `resolve_entity` (or `resolve_predicate`, etc., per the target).
- Confidence is fixed at the declared value (patterns don't compute per-match confidence).

Cost: negligible. Latency: tens of microseconds per memory.

Limitations: regex is brittle. Can't handle paraphrase, declension, or semantic intent. Best for IDs, exact terms, and well-structured names.

## Classifier extractors

```
define extractor reporting_lines {
    kind: classifier
    target: relation reports_to
    model: "brain-reporting-line-classifier-v3"
    feature_extraction: builtin     # builtin | custom_function_id
    confidence_threshold: 0.8
    trigger: on encode where memory.text matches ".*report.*to.*|.*manager.*"
}
```

Classifier semantics:
- A pinned, deterministic model. Could be a fine-tuned BERT-like, a small LSTM, or a logistic regression. The model lives in `models/` directory of Brain.
- Inputs: memory text + extracted features (NER results, etc.).
- Output: structured prediction with confidence.
- Determinism: pinned weights + pinned tokenizer + pinned random seed → identical output across runs.

Cost: bounded by model size and CPU. Typically 1-10 ms per memory on a CPU. It runs in the background extractor worker (after the pattern tier), never on the ENCODE path.

The tier is **live in v1.0** — both the entity-mention head and the statement-kind head (Fact / Preference / Event) run a real forward pass over the shared GLiNER encoder. Earlier phases shipped the head wired but degraded; v1.0 is the first cut where the statement-kind classification gate fires before the LLM tier on the hot path.

Limitations: needs training data. Brain doesn't train classifiers; users bring pre-trained models (or use defaults shipped with Brain for common cases like NER and sentiment).

## LLM extractors

```
define extractor preference_extraction {
    kind: llm
    target: statement Preference
    model: "claude-haiku-4-5"           # or local "mistral-7b-instruct"
    prompt: """..."""
    examples: [...]                      # few-shot examples
    schema: {                            # JSON schema for output validation
        type: array
        items: {
            type: object
            required: [subject, predicate, object, confidence]
            properties: { ... }
        }
    }
    cache: enabled
    cache_ttl: 90d
    confidence_threshold: 0.7
    cost_budget: "$0.001 per memory"     # per-call budget; over-budget extractions skipped
    trigger: on encode where memory.kind = episodic
}
```

LLM semantics:
- Memory text is composed into the prompt (along with examples).
- The LLM is called via a pluggable backend: API (Anthropic, OpenAI) or local (llama.cpp, vLLM).
- Output must validate against the declared JSON schema. If it doesn't, retry once (with the validation error in the prompt). If still invalid, drop and log.
- Output is parsed into Entities/Statements/Relations per the target declaration.
- Result is cached by `(input_hash, extractor_version, model_version)`.

Cost: dollar-significant. The `cost_budget` field is a hard cap; Brain tracks per-memory cost and skips extraction if the projected cost exceeds the budget.

Determinism: not bit-exact, but heavily mitigated:
- `temperature = 0` (default; configurable).
- Schema validation rejects malformed.
- Cache means repeat calls return identical output.
- Replays use the cache; no re-LLM-calling unless the cache is invalidated.

## Idempotency

All three extractor kinds are required to be idempotent under their declared inputs:

```
fn extract(memory: &Memory, extractor: &Extractor) -> ExtractionResult
```

Inputs: `(memory.id, memory.text_hash, extractor.id, extractor.version, schema.version)`.

Outputs: the same set of Entities, Statements, Relations, with identical IDs (if the resolution paths are deterministic).

Brain uses this to support:
- Replay: re-run an extractor over a memory; if anything is missing, fill in.
- Migration: re-run a new extractor version; results compared to old; supersession applied.

For LLM extractors, idempotency is enforced via cache. If the cache is dropped or expired, the LLM may produce a slightly different output, which is treated as supersession.

## Audit log

Every extraction (success or failure) writes an audit record:

```rust
struct ExtractionAudit {
    id: AuditId,
    memory_id: MemoryId,
    extractor_id: ExtractorId,
    extractor_version: u32,
    schema_version: u32,
    started_at: u64,
    completed_at: u64,
    status: ExtractionStatus,        // Success | Failure | Skipped (cost budget) | Skipped (filter)
    outputs: Vec<OutputRef>,         // produced statement / entity / relation IDs
    cost: f32,                       // estimated cost in USD (0 for pattern/classifier)
    error: Option<String>,
    model_metadata: Option<ModelMetadata>,   // for LLM: model version, token counts
}
```

Audits are queryable: "show me all extractions for memory X" or "show me all failures of extractor Y in the last 24h."

Retention: 90 days default. Configurable.

## Cost controls

For LLM extractors, three layers of cost control:

1. **Per-call budget**: declared in the extractor. Skip extraction if projected cost exceeds.
2. **Per-deployment budget**: global daily/weekly/monthly cap. Enforced by a global counter.
3. **Selective triggering**: extractors declare `trigger` conditions to skip irrelevant memories.

Operators can inspect cost dashboards and adjust extractor configs to stay within budget.

## Triggering conditions

```
trigger: on encode where memory.kind = episodic
trigger: on encode where memory.text matches ".*meeting.*"
trigger: on demand                    # not triggered by writes; user invokes
trigger: on schema_change             # run on schema upgrades for migration
trigger: periodic at "0 0 * * *"      # cron, for batch jobs
```

Multiple triggers per extractor are AND-ed. Brain evaluates them in the worker loop before scheduling extraction.

## Extractor lifecycle

1. **Declared**: schema uploaded; extractor registered.
2. **Active**: triggers fire; extractor runs.
3. **Updated**: schema reuploaded with modified extractor (e.g., new prompt). Version bumps. Old outputs flagged stale. Migration plan generated.
4. **Disabled**: extractor set inactive in schema. Stops running. Existing outputs retained.
5. **Removed**: extractor deleted from schema. Outputs retained or tombstoned per migration plan.

## Built-in extractors

Brain ships built-in extractors that users can enable:

- `brain.entity_mentions` — pattern + classifier for common entity types (Person, Place, Organization, Date) using built-in NER.
- `brain.temporal_expressions` — extracts date/time expressions ("Tuesday", "last week") and emits Events.
- `brain.basic_relations` — pattern-matches common relation phrases ("X works for Y", "X manages Y") and emits Relations.

These are optional; users enable them in the schema:

```
use brain.entity_mentions
use brain.temporal_expressions
```

When used, they run alongside user-declared extractors.

## Concurrency and ordering

Multiple extractors can run on the same memory. All extraction is asynchronous — ENCODE enqueues the memory and the per-shard extractor worker runs the tiers in order when it drains the queue:

1. Pattern extractors run first (fast, deterministic).
2. Classifier extractors run next (pinned model, low per-memory latency).
3. LLM extractors run last (high latency, cached).

No extractor output exists when ENCODE returns. Pattern and classifier outputs typically appear within one worker cycle; LLM outputs appear within seconds to minutes.

Ordering matters when later extractors depend on earlier ones' outputs:

```
define extractor preference_with_resolved_person {
    kind: llm
    depends_on: [person_mentions]    # waits for person_mentions to complete
    ...
}
```

The worker scheduler honors `depends_on` chains.
