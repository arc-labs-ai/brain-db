# Extractors

## Purpose

An extractor is a pipeline that derives structured data (Entities, Statements, Relations) from unstructured Memories. Extractors are declared in the schema and run as background workers.

Three kinds, in increasing order of capability and cost:

| Kind | Latency | Cost | Determinism | Recall | Precision |
|---|---|---|---|---|---|
| **Pattern** | 10-100 µs | ~0 | Yes | Low (narrow) | Very high |
| **Classifier** | 1-10 ms | Low | Yes (pinned model) | Medium | High |
| **LLM** | 100 ms - 10 s | High (per-call) | No (cached) | High | High (with validation) |

The substrate runs them as a pipeline: pattern first (fast, free), then classifier (slow, cheap), then LLM (slowest, expensive). Each tier can be configured to either *replace* or *supplement* the previous tier's output.

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
- Run on every ENCODE (foreground or queued background).
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
- A pinned, deterministic model. Could be a fine-tuned BERT-like, a small LSTM, or a logistic regression. The model lives in `models/` directory of the substrate.
- Inputs: memory text + extracted features (NER results, etc.).
- Output: structured prediction with confidence.
- Determinism: pinned weights + pinned tokenizer + pinned random seed → identical output across runs.

Cost: bounded by model size and CPU. Typically 1-10 ms per memory on a CPU. Latency: low; runs in foreground or near-foreground.

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

Cost: dollar-significant. The `cost_budget` field is a hard cap; the substrate tracks per-memory cost and skips extraction if the projected cost exceeds the budget.

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

The substrate uses this to support:
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

Multiple triggers per extractor are AND-ed. The substrate evaluates them in the worker loop before scheduling extraction.

## Extractor lifecycle

1. **Declared**: schema uploaded; extractor registered.
2. **Active**: triggers fire; extractor runs.
3. **Updated**: schema reuploaded with modified extractor (e.g., new prompt). Version bumps. Old outputs flagged stale. Migration plan generated.
4. **Disabled**: extractor set inactive in schema. Stops running. Existing outputs retained.
5. **Removed**: extractor deleted from schema. Outputs retained or tombstoned per migration plan.

## Built-in extractors

the knowledge layer ships with built-in extractors that users can enable:

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

Multiple extractors can run on the same memory. Default execution:

1. Pattern extractors run synchronously during ENCODE (foreground, fast).
2. Classifier extractors run synchronously or in near-foreground (low latency).
3. LLM extractors run in background workers (high latency, batched).

Pattern and classifier outputs are visible immediately after ENCODE returns. LLM outputs appear within seconds to minutes.

Ordering matters when later extractors depend on earlier ones' outputs:

```
define extractor preference_with_resolved_person {
    kind: llm
    depends_on: [person_mentions]    # waits for person_mentions to complete
    ...
}
```

The worker scheduler honors `depends_on` chains.
