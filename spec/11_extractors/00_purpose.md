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
- **The LLM tier's credential is not a boot-time concern.** It is resolved *per namespace*, from Brain's own credential store, at the moment a request names its namespace — so the serving credential is not knowable at startup and a keyless boot is the correct shape: no shard refuses to start over a missing tenant key. Two things do happen at startup — opening the credential store and loading its sealing key is fatal, because a server that came up unable to hold a credential would accept registrations it could not persist; and extractor materialization probes the system namespace so a deployment with no key at all still gets an operator diagnostic, registering the extractor degraded with a reason rather than failing per write. See §"The LLM tier's credential" below.
- There is no `has_llm_extractor` planning input and no `[extractors.<tier>].enabled` config. `GET_CAPABILITIES` (§04/03) reports extraction as always-live.

Extraction cannot be paused at runtime either: the former `EXTRACTOR_DISABLE` / `EXTRACTOR_ENABLE` admin wire ops were removed, so there is no deploy-time *and* no runtime way to turn extraction off (write/read coherence). `EXTRACTOR_LIST` (§04/03) remains as read-only introspection over the extractors the caller can actually be served by: its own namespace's declarations plus the system built-ins, resolved at the caller's effective space so a space overlay shadows the namespace base for the same name.

## The LLM tier's credential

**A deployment holds no provider credential of its own. A key registered by a namespace is the only way an LLM call is paid for, and there is no second path.** This is a hard invariant, not a default — there is no configuration that turns it off, no deployment shape that bypasses it, and no fallback when a namespace has not registered one.

### The invariant

| | |
|---|---|
| **MUST NOT** | accept, resolve, or construct a client from a deployment-wide provider API key, from any source — config, environment, request field, or schema declaration |
| **MUST NOT** | write a provider key in plaintext anywhere: not to the WAL, the metadata store, a snapshot, a backup, the response cache, a log line, or a diagnostic |
| **MUST** | resolve every LLM call's authority per namespace, from the credential store, keyed by the namespace of the data it is about to act on |
| **MUST** | reject a registration that arrives over a connection whose transport does not protect its contents |

The first clause is structural: there is no field to set, so there is nothing to get wrong. The second is enforced by sealing plus hand-written redaction — a key enters the store encrypted under a key-encryption key, and the types that carry one render as `[redacted]` rather than deriving `Debug`. That is a weaker guarantee than absence and is stated as such: see §"What is stored, and what that protects".

### Why there is no shard-wide key

A shard-wide provider key serves every namespace on the shard. On a shard that also hosts real tenants it therefore stands in for each tenant's own registered key: their extraction is billed to whoever owns the shard, and their text is sent upstream under that account. Nothing reports it, because from Brain's side the key resolves fine.

That is not a tuning problem to be left as an operator's choice. It is a correctness and custody failure that is **invisible from inside the system**, which is precisely the kind of thing a configuration flag must not be able to re-enable. The key is therefore not configurable at all.

### What is configured

Nothing that grants provider access. There is no `[llm] api_key`, no `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env gate, and no per-extractor key in the schema DSL. A deployment cannot supply a provider credential even if it wants to.

What an operator configures is how stored credentials are sealed:

| Field | What it is |
|---|---|
| `[llm] model` | **inert.** A registration always names its own model and the resolver serves that one, so nothing reads this. Grants nothing. |
| `[llm.credentials] kek` | the key-encryption key, 64 hex characters. Unset, Brain generates one into the data directory on first boot, so a fresh install seals without configuration. |

### How a namespace registers one

Over the wire, through `LLM_CREDENTIAL_PUT` / `GET` / `DELETE` / `VERIFY` (§04). The ops carry their own permission bit, deliberately separate from `ADMIN` — a tenant running their own extraction budget should not need operator powers — and separate from the write bundle, because a key that can store a memory must not also be able to replace the key that pays for processing it. The bit is not delegable: an `act_as` request can never register a credential.

`PUT` is the only shape that carries a plaintext key, and it is refused unless the connection is TLS, mTLS, or loopback. Sealing protects the key at rest; refusing the request is what protects it in flight. Nothing that comes back out can reconstruct it — a read answers with the provider, the model, and a display fingerprint.

### What is stored, and what that protects

The sealed key, in a redb file of its own, keyed `(namespace, space)`. Sealing is an AEAD under the key-encryption key, with the namespace and provider authenticated alongside the ciphertext. That binding is load-bearing: a stolen row moved into another tenant's slot fails to open rather than yielding a usable key, which is what keeps row-level write access to the store from becoming a way to spend someone else's provider budget.

**Stated precisely, because it is easy to overclaim.** A generated key-encryption key sits in the same directory as the rows it seals. That defends against the ways credential ciphertext ordinarily escapes — a database file copied out, a backup archive, a snapshot restored elsewhere, anything that greps the store — because none of those carry the key file with them. It does **not** defend against someone holding the whole data directory. An operator who needs that supplies the key themselves, so it lives somewhere Brain's disk is not.

Rotation needs no coordination: the id of the key that sealed a row travels in the clear beside its ciphertext, so a new key-encryption key can be introduced, new seals use it, and existing rows stay readable until they are re-sealed.

### Every LLM consumer resolves the same way

There is no privileged consumer of a *credential*. Write-time HyPE, the extractor's LLM tier, the entity disambiguator's second-opinion pass, and the consolidation summarizer's `llm` backend all resolve through the same `LlmCredentialResolver`, keyed by the namespace of the data they are about to act on. The summarizer's `ollama` backend is the one exception, and it is not one in substance: it calls an operator-run local endpoint that takes no key, so there is no authority to resolve and no tenant to bill.

A consumer that cannot name a namespace cannot make an LLM call. That is the intended consequence: if a piece of work is not attributable to a tenant, there is no one whose key should pay for it.

### Brain's own API keys are a different thing

Brain mints and stores credentials of its own — the client keys that authenticate a connection — and this invariant does not contradict that. Two reasons they are not the same object:

- They are **Brain's**, not a third party's. Revoking one costs a client its access to Brain; it cannot spend anyone's money upstream.
- The database holds `BLAKE3(secret)`, never the secret. The plaintext exists only in the mint response and on the wire; a stolen `api_keys` table yields no usable credential.

So the two stores hold different kinds of thing, and neither is a usable plaintext secret: a tenant's provider key arrives once and is persisted only sealed, and Brain's own keys are persisted only as `BLAKE3(secret)`.

### Caching, and why a rotation needs no invalidation

Resolution reads the store, which is local, so there is no TTL to tune and no window in which a revoked key keeps working. What is cached is the built provider client, so resolving does not construct an HTTP client per call.

That cache is keyed by the credential's registration timestamp alongside its namespace and model. Registering a new key moves the timestamp, so the previous entry becomes unreachable the moment the store changes and every shard picks up the new key on its next resolve. There is no generation counter to keep in step, and no shard can serve a key the store no longer holds.

### `Missing` is not `Unavailable`

The resolver keeps "this namespace has no key" separate from "I could not find out right now", and callers must branch on both. Conflating them turns a transient storage fault into a permanent-looking rejection and fails writes that should have been retried.

The write path is deliberately **asymmetric**:

- A **definitive** miss rejects the write up front with `LlmCredentialMissing` (`0x0066`, Conflict, terminal). Acking it would store a memory Brain cannot ground — HyPE and extraction are mandatory — so it would land as text with no questions, entities or statements, and recall would quietly degrade with nothing telling the caller why.
- An **unresolved** credential — the store could not answer, or the key that sealed the row is no longer loaded — **admits** the write and lets the derivation stages resolve and retry. Neither is evidence that the tenant has no key.
- With **no credential store at all**, the write is admitted and the derivation stages report that they could not run. Brain does not fall back to another credential, because there is no other credential to fall back to — a write that cannot be grounded is reported as such, never paid for out of someone else's key.

Because the store is local, a definitive answer costs one point read and needs no network round trip. The gate can therefore refuse a keyless namespace at the door with a reason, rather than admitting on "unknown" and discovering the problem several stages downstream.

A namespace with no registered key is not a boot-time concern. It is a **refused write**: the gate reads the local store before any derivation runs, gets a definitive answer, and returns `LlmCredentialMissing` with a reason the tenant can act on. HyPE and extraction both need the key, so admitting the write would store text with no questions, entities or statements — stored-looking, and quietly degrading recall.

The per-stage report — `StageOutcome::Failed` with a reason on the `StageCompleted` event — carries the *other* case: a write that was admitted and then degraded, which is an unresolvable credential or a deployment with no credential store at all.

### Cache isolation

The LLM response cache is keyed by `(input_hash, extractor_id, extractor_version, model)`, a tuple with no tenant component. Sharing a row across credentials means one tenant's key pays for a completion another tenant then gets free. A **credential scope** therefore enters the cache key, derived from the namespace that registered the credential — never from the key itself, which is hashed into persisted rows and shown in diagnostics.

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
