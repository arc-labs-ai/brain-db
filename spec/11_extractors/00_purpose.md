# 11. Extractors

> **TL;DR.** Three-tier pipeline that derives Entities, Statements, and Relations from Memories. **Extractors are always wired** — every shard runs them on every ENCODE regardless of whether a user schema is declared. All three tiers run **asynchronously** in the per-shard extractor worker — ENCODE enqueues the memory and acknowledges, then the worker runs pattern (regex, tens of microseconds, free) → classifier (pinned model, milliseconds, cheap) → LLM (cached, hundreds of milliseconds to seconds, dollar-significant) over it, with strict cost budgets, schema-validated output, and a per-call cache keyed by `(input_hash, extractor_version, model_version)`. No extractor output exists when ENCODE returns. Persistence is **per-entity gated**: an extracted entity / statement / relation whose type exists in some active schema namespace is persisted; one whose type is undeclared is silently dropped (extraction is best-effort). All three tiers are always-on (no per-tier enable flag — extraction is architectural, like the embedder); a tier that fails to load at shard spawn → `ShardError::ExtractorInitFailed`. All tiers are required to be idempotent. Built-in extractors ship for common entity types, temporal expressions, and basic relations.

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

### All tiers are always-on (no gate)

All three tiers run on every shard — there is **no per-tier enable flag**. Extraction
populates the typed graph that reads fuse, so a write with extraction disabled would
leave graph-backed reads silently incoherent; extraction is therefore architectural
(always-on), like the embedder and write-time HyPE, not a deploy-time toggle.

- A tier that fails to load at shard spawn (pattern regex compile error, classifier model file missing / weights corrupt) is a hard spawn failure: `ShardError::ExtractorInitFailed { tier, source }`. The shard refuses to start rather than running with a quietly-missing tier. The classifier's model (GLiNER) is a hard boot requirement, exactly like the embedder, and the shipped image bundles it for that reason.
- **The LLM tier's credential is not a boot-time concern.** It is resolved *per namespace*, from the broker, at the moment a request names its namespace — so the serving credential is not knowable at startup and a keyless boot is the correct shape. What the startup gate checks for is the **broker** (`[llm.broker]`), not a key. See §"The LLM tier's credential" below.
- There is no `has_llm_extractor` planning input and no `[extractors.<tier>].enabled` config. `GET_CAPABILITIES` (§04/03) reports extraction as always-live.

Extraction cannot be paused at runtime either: the former `EXTRACTOR_DISABLE` / `EXTRACTOR_ENABLE` admin wire ops were removed, so there is no deploy-time *and* no runtime way to turn extraction off (write/read coherence). `EXTRACTOR_LIST` (§04/03) remains as read-only introspection over the always-on extractors.

## The LLM tier's credential

**Brain never holds a provider credential. BYOK through the broker is the only way an LLM call is paid for, and there is no second path.** This is a hard invariant, not a default — there is no configuration that turns it off, no deployment shape that bypasses it, and no fallback when the broker is unreachable.

### The invariant

| | |
|---|---|
| **MUST NOT** | accept, resolve, or construct a client from a provider API key, from any source — config, environment, request field, or schema declaration |
| **MUST NOT** | write anything credential-shaped to disk: not to the WAL, the metadata store, a snapshot, a backup, the response cache, a log line, or a diagnostic |
| **MUST** | resolve every LLM call's authority per namespace, from `[llm.broker]`, as a namespace-scoped grant |

The storage half of that is guaranteed structurally rather than by discipline: **a provider key cannot leak from Brain's data at rest because one never arrives there.** There is no redaction rule to get right, no field to remember to exclude from a snapshot, and no audit of what a log might have printed. An invariant enforced by absence does not have exceptions.

### Why there is no shard-wide key

A shard-wide provider key serves every namespace on the shard. On a shard that also hosts real tenants it therefore stands in for each tenant's own registered key: their extraction is billed to whoever owns the shard, and their text is sent upstream under that account. Nothing reports it, because from Brain's side the key resolves fine.

That is not a tuning problem to be left as an operator's choice. It is a correctness and custody failure that is **invisible from inside the system**, which is precisely the kind of thing a configuration flag must not be able to re-enable. The key is therefore not configurable at all.

### What is configured

`[llm.broker]` is **required**, and it is the whole of the LLM credential surface. A configuration without it does not start. The section names:

| Field | What it is |
|---|---|
| `url` | the broker's base URL |
| `service_key` | **Brain's own identity to the broker.** Not a provider key: it authenticates *Brain*, so the broker will mint namespace grants for it. It buys no provider access by itself. |
| `ttl_secs` | how long a resolved grant stays cached (default 300) |

There is no `[llm] api_key`, no `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env gate, and no per-extractor key in the schema DSL. A deployment cannot supply a provider credential even if it wants to.

`service_key` is the one secret Brain is given, and it is given for the one thing a secret is unavoidable for: proving which service is asking. It lives in config or the environment — never in the database — and is redacted from `Debug` output and from `GET /v1/config`.

### What Brain holds

A namespace-scoped **grant**, in memory, for as long as its TTL. The broker tells Brain only which provider and model to build a client for; the request is re-signed at the edge on the way upstream. The tenant's actual provider key stays sealed at the broker and is never transmitted to Brain, in any form, at any point.

The provider clients are reused verbatim. They cannot set a custom header, so both routing inputs travel in the two channels they do control: the **namespace** in `base_url`, as `{edge}/llm/ns/{namespace}/{provider}`, and the **grant** as the bearer token in place of a provider key. Nothing in the provider layer knows a broker exists — which is also why no code path in it can be handed a real key by mistake.

### Every LLM consumer resolves the same way

There is no privileged consumer. Write-time HyPE, the extractor's LLM tier, the entity disambiguator's second-opinion pass, and the consolidation summarizer all resolve through the same `LlmCredentialResolver`, keyed by the namespace of the data they are about to act on.

A consumer that cannot name a namespace cannot make an LLM call. That is the intended consequence: if a piece of work is not attributable to a tenant, there is no one whose key should pay for it.

### Brain's own API keys are a different thing

Brain mints and stores credentials of its own — the client keys that authenticate a connection — and this invariant does not contradict that. Two reasons they are not the same object:

- They are **Brain's**, not a third party's. Revoking one costs a client its access to Brain; it cannot spend anyone's money upstream.
- The database holds `BLAKE3(secret)`, never the secret. The plaintext exists only in the mint response and on the wire; a stolen `api_keys` table yields no usable credential.

So "the database holds no credentials" is true in both directions: no provider key ever arrives, and Brain's own keys are stored only as hashes.

### Caching, and why the negative TTL is shorter

HyPE runs on every write, so an uncached resolve would put a request to the edge on the write path. Results are cached per namespace, and a **negative** result is cached far more briefly than a positive one:

| Entry | Default TTL | What the TTL bounds |
|---|---|---|
| Positive (`Ready`) | 300 s | how long a *revoked* key keeps working |
| Negative (`Missing`), for resolution | 10 s | how long a tenant who just registered a key keeps being told they have none |
| Negative, for the synchronous write gate | 300 s, with one probe admitted every 30 s | how long the gate keeps refusing before it lets a write through to re-probe |

The two negative TTLs differ because their consumers want opposite things. Resolution wants the entry to expire fast, so the next derivation notices a key the moment it is registered. The write gate wants it to persist, or it only ever fires for writes arriving within ten seconds of each other — which is to say never, on the sparse traffic of a sandbox, and a keyless namespace then degrades in silence exactly as the gate exists to prevent. The periodic probe is what keeps the long window from becoming self-sustaining: the gate runs *before* any derivation, so a namespace it refuses issues no resolve and would never fetch the `Ready` that clears it.

### `Missing` is not `Unavailable`

The resolver keeps "this namespace has no key" separate from "I could not find out right now", and callers must branch on both. Conflating them turns a broker blip into a permanent-looking rejection and fails writes that should have been retried.

The write path is deliberately **asymmetric**:

- A **definitive** miss rejects the write up front with `LlmCredentialMissing` (`0x0066`, Conflict, terminal). Acking it would store a memory Brain cannot ground — HyPE and extraction are mandatory — so it would land as text with no questions, entities or statements, and recall would quietly degrade with nothing telling the caller why.
- An **unresolved** credential (cold cache, broker blip) **admits** the write and lets the derivation stages resolve and retry. Rejecting here would fail every write for the first request after a restart.
- With **no broker reachable at all**, the write is admitted and the derivation stages report that they could not run. Brain does not fall back to another credential, because there is no other credential to fall back to — a write that cannot be grounded is reported as such, never paid for out of someone else's key.

A namespace with no registered key is not a boot-time concern and not a failed write in the general case: the write lands, the pattern and classifier tiers run, and the **LLM tier reports that it could not run**. That report — `StageOutcome::Failed` with a reason on the `StageCompleted` event — is the honest signal for a degraded write, not the gate, which is a cache-only optimisation that performs no I/O and therefore cannot promise to fire.

### Cache isolation

The LLM response cache is keyed by `(input_hash, extractor_id, extractor_version, model)`, a tuple with no tenant component. Sharing a row across credentials means one tenant's key pays for a completion another tenant then gets free. A **credential scope** therefore enters the cache key, derived from the grant or the tenant id.

It is **never derived from key material**, and under this invariant it could not be: no key material exists in the process to derive it from. The scope is hashed into persisted rows and surfaces in diagnostics, so even if a key were available it would be the wrong input — the rule and the structure agree, which is what makes it hold.

The scope is placed **last** in the key so the TTL sweeper can still resolve a hint back to a prefix range.

There is no cache migration. A cache file written before per-namespace credentials is **refused, not adopted**: its rows cannot say which credential paid for them, so serving them is exactly the cross-tenant reuse the scope exists to prevent.

## Purpose

An extractor is a pipeline that derives structured data (Entities, Statements, Relations) from unstructured Memories. Extractors are declared in the schema and run as background workers.

Three kinds, in increasing order of capability and cost:

| Kind | Latency | Cost | Determinism | Recall | Precision |
|---|---|---|---|---|---|
| **Pattern** | 10-100 µs | ~0 | Yes | Low (narrow) | Very high |
| **Classifier** | 1-10 ms | Low | Yes (pinned model) | Medium | High |
| **LLM** | 100 ms - 10 s | High (per-call) | No (cached) | High | High (with validation) |

Brain runs them as a pipeline: pattern first (fast, free), then classifier (slow, cheap), then LLM (slowest, expensive). Within one namespace every declared extractor across all tiers *supplements* the others — their outputs are merged, not replaced. There is no per-tier replace/supplement switch; the pattern/classifier tiers always contribute alongside the LLM tier.

The one replace-shaped rule is namespace-scoped and lives inside the LLM tier: a namespace that declares its own enabled LLM extractor(s) **replaces** the seeded `brain:` default LLM extractor for that namespace's memories (auto-suppress by declaration). The system default runs only for memories in namespaces that declare no LLM extractor of their own. Multiple LLM extractors declared in the *same* namespace supplement each other.

Extractor lifecycle notes:
- **`trigger` is honored on the encode path.** `on encode` and `on encode where <cond>` gate whether the extractor runs for a given memory, where `<cond>` supports `memory.text matches <regex>` / `memory.kind = <kind>` / `memory.kind in (…)`, combined with `and` / `or`. `on demand`, `periodic`, and `on schema_change` triggers do **not** run on the encode path.
- **The registry refreshes on `SCHEMA_UPLOAD`** — no shard restart. After the upload commits, the per-shard extractor worker rebuilds the in-memory registry from the persisted `EXTRACTORS_TABLE` rows on its next cycle, so a newly-declared extractor fires against subsequent encodes.
- **`depends_on` ordering is not yet honored** (known limitation / follow-up): all extractors in a namespace run and their outputs merge, but a declared inter-extractor dependency order is not currently enforced.

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
