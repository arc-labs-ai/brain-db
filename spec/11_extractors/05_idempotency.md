# 11.05 Idempotency

Every extractor MUST be idempotent on its declared inputs. A replay over
unchanged inputs — the same memory text and prompt context, the same extractor
at the same version, the same active schema version, the same model, and the
same paying credential — produces byte-identical outputs (or the same
deterministic skip).

Cross-references:
- [`./04_audit.md`](./04_audit.md) §8 — the two gates that stop repeated work.
- [`./01_extractor_tiers.md`](./01_extractor_tiers.md) §6 —
  per-tier determinism.
- [`./01_extractor_tiers.md`](./01_extractor_tiers.md) §2
  — classifier determinism contract.

## 1. The idempotency tuple

The LLM tier's response cache is keyed on a five-component tuple
(`crates/brain-extractors/src/llm/cache.rs:21-41`):

```rust
(input_hash, extractor_id, extractor_version, model_id_hash, credential_scope)
```

`input_hash` is `hash_prompt_context(space, schema_version, prompt_body)`
(`crates/brain-extractors/src/idempotency.rs:22-30`), where `prompt_body` is
the **fully materialized prompt** — memory text, anchor date, declared entity
types and kinds, candidate predicates, prior entities and the bounded
neighbor/summary context, all already substituted in
(`crates/brain-extractors/src/llm/extractor.rs:372-396`, reachable as
`brain_extractors::idempotency::hash_prompt_context`). Its components are
domain-separated — fixed-width `space` and `schema_version`, then a
length-prefixed body — so no two distinct inputs can collide by concatenation
(`idempotency.rs:18-20`, `:23-29`). How the lookup sits around the model call
is in [`./01_extractor_tiers.md`](./01_extractor_tiers.md) §20.

This tuple is also the **contract** every tier is held to: the inputs an
extractor is allowed to be a function of. It is strictly finer than the
memory-level identity a reader might expect, because the per-memory inputs are
inside it rather than beside it:

- The memory id and its text are both in `prompt_body`
  (`llm/extractor.rs:385-395`), so a change to either changes `input_hash`.
  An admin edit-in-place of memory text therefore invalidates the entry
  without needing a separate text-hash component.
- `schema_version` is its own hashed component, so the active schema is part
  of the key even when the extractor's config bytes are identical (§5).

Two components have no memory-level counterpart at all, and they are the
reason the cache key cannot be derived from the memory alone:

- `model_id_hash` — a model swap under an unchanged extractor config still
  misses, because the same prompt does not give the same completion from a
  different model.
- `credential_scope` — which credential paid for the row. Under BYOK a
  completion bought with one tenant's provider key must not be served to
  another tenant, so it is part of the key rather than a filter applied
  afterwards. A single-key deployment passes `CredentialScope::HOUSE` for
  every namespace and shares rows across them (`llm/cache.rs:8-12`).

`space` sits inside `input_hash` on top of that, so two tenants never share an
entry even for byte-identical text.

The memory's text hash is not a cache-key component; it is the **audit row's**
`input_hash`. That one is a BLAKE3 over the memory's source text read from
`TEXTS_TABLE` inside the open extraction write transaction, computed by
`memory_text_hash` calling `blake3::hash` directly
(`crates/brain-workers/src/workers/extractor.rs:3530-3541`) and written at
`:5225`. It is what an operator reads to tell whether a re-extraction ran over
edited text, and it returns the all-zero hash when the text row is missing
rather than failing the audit.

## 2. Determinism contract by tier

| Tier | Determinism source |
|---|---|
| Pattern | Pinned `regex` crate version + pattern source order. |
| Classifier | Pinned model weights + tokeniser + seed + math library. |
| LLM | Cache lookup; first uncached call may drift across binary versions; cache hit is bit-identical. |

When the source changes, `extractor_version` bumps. Old audit rows
remain queryable; the next ENCODE produces new outputs and a new
audit row.

## 3. Replay semantics

Replay is driven per shard and per plan, not per extractor call. Three
surfaces reach it, in ascending order of deliberateness:

1. **The queue-replay guard.** `[workers.extractor] skip_already_extracted`
   is on by default, so a memory whose extraction is terminal never runs
   again. Setting it to `false` is what allows a second pass over an
   already-audited memory; it is the switch a re-extraction scenario turns
   off, not a per-call flag.
2. **Schema-driven re-extraction.** `reextract_on_schema_change` — off by
   default, because it is one model call per affected memory — re-enqueues a
   namespace's stale memories when it uploads a new schema version (see
   [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)
   §"Schema migration re-extraction gate").
3. **Operator-triggered backfill.** `POST /v1/extract/backfill` runs the same
   work as a checkpointed plan, resumable mid-run and per-item resilient.
   This is the recovery path a memory lands on after its retry budget is
   spent.

Replay has no per-dispatch form. The dispatch surface carries no replay
option: `Extractor::run(&self, ctx, mem)` and `Extractor::run_batch(&self,
ctx, mems)` take only the extraction context and the memories
(`crates/brain-extractors/src/framework/extractor.rs:138`, `:150-154`), so
every replay comes from one of the three surfaces above and none of them is a
per-call flag. Nor does any path re-emit an audit row's stored outputs in
place of running an extractor: a replay re-runs the pipeline and leans on the
LLM cache and the apply layer's content idempotency (§4) instead of reading
outputs back.

## 4. Output identity

For idempotency to hold, **output IDs must be deterministic** given
the tuple in §1. The current allocation rules:

| Output kind | ID source | Deterministic? |
|---|---|---|
| `EntityMention` | None (transient; not persisted) | n/a |
| Entity (auto-create) | UUIDv7 | NO — allocation alone gives a fresh ID. |
| Statement (create) | UUIDv7 | NO — same. |
| Relation (create) | UUIDv7 | NO — same. |

ID allocation is therefore not where idempotency comes from. Three mechanisms
stand in front of it, and the last is the one that holds when the others do
not:

1. **The re-run usually does not happen.** The queue-replay guard drops a
   memory whose extraction is terminal, so the common case never reaches an
   allocator (see [`./04_audit.md`](./04_audit.md) §8).
2. **The model call is cached.** A repeated extraction of unchanged text with
   an unchanged extractor and model reads `llm_cache.redb` instead of paying
   for a completion.
3. **The apply layer is content-idempotent.** A retry *does* re-run the whole
   pipeline, which re-applies the pattern- and classifier-tier rows that
   already committed — and that is safe on its own terms. For single-valued
   statement kinds an incoming row supersedes the current one rather than
   joining it. For cumulative kinds — Fact, Event — an active row identical on
   `(subject, predicate, object, event_at)` makes `statement_create` a no-op
   that returns the **existing** id; `relation_create` does the same for a
   byte-identical active `(from, type, to)`. Distinct objects (multi-valued
   Facts) and distinct event times (separate Events) never match, so genuine
   cumulation is preserved and only exact re-applications collapse.

Mechanism 3 is what makes the bounded retry ladder safe to have at all: a
user-declared pattern Relation with `Many` cardinality has no cardinality
conflict to catch a repeat, so content dedup is the only thing standing
between a retry and a duplicated edge.

Content-addressed IDs for some kinds may be introduced later.

## 5. Cache invalidation

Both `extractor_version` and `schema_version` invalidate the LLM cache, by
different routes: `extractor_version` is a component of the key tuple
directly, and `schema_version` is folded into the input hash. Two scenarios:

1. **Extractor changed** (config or model bump). Schema upload bumps
   `extractor_version`, which is a key component, so the lookup misses, the
   extractor runs, and new outputs are written. Old outputs remain, and the
   stale-extraction detector counts them (see
   [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)
   §"Stale extraction detector").

2. **Schema changed but the extractor didn't.** The namespace version bumps
   while the extractor's config bytes stay identical, so `extractor_version`
   is unchanged — but the active `schema_version` is hashed into the input
   component, so the lookup **still misses**. A cosmetic schema bump
   therefore costs a re-run.

That cost is the deliberate trade. A key that tried to hit across schema
versions would have to decide which version's declared types the cached
response was still valid under, and a wrong answer there serves a response
shaped by a vocabulary the namespace no longer has. Missing is cheap and
self-correcting; a stale hit is neither. The audit row's `schema_version`
records the version the row was written under, not the current namespace
version, which is what lets the detector find the gap.

## 6. Concurrency

Two ENCODEs of the same memory text on the same shard race only
through the single-writer-per-shard discipline (see
[`../14_concurrency/00_purpose.md`](../14_concurrency/00_purpose.md)).
One write transaction commits first. The second does not find a probe to
hit — it finds the first one's rows: single-valued statement kinds supersede,
and byte-identical cumulative rows and relations no-op onto the existing ids
(§4). The guarantee comes from the apply layer, which is why it holds whether
or not anything consulted a cache first. No duplicate outputs.

Across shards: the same memory text on different shards is allowed
to produce distinct outputs — Brain does not deduplicate cross-shard.

## 7. Tests

Unit + integration tests:

- A memory whose extraction is terminal is dropped from the queue rather than
  re-run, with `skip_already_extracted` on.
- With the guard off, a second pass over the same memory commits no duplicate
  rows: identical cumulative statements and relations return the existing
  ids, and single-valued kinds supersede per
  [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md).
- Two distinct objects for the same `(subject, predicate)` Fact both survive
  — content dedup must not collapse legitimate multi-value cumulation.
- Two Events differing only in `event_at` both survive.
- Bumping `extractor_version` between runs misses the LLM cache and writes a
  fresh `Success` audit.
- An unchanged memory under a bumped `schema_version` also misses, since the
  active version is folded into the cache's input hash.
- `hash_prompt_context` is deterministic: two calls on the same `(space,
  schema_version, prompt_body)` return the same hash
  (`crates/brain-extractors/src/idempotency.rs:40-45`).
- `hash_prompt_context` varies on every component — a different tenant, a
  different schema version, or a different prompt body each yield a distinct
  hash (`idempotency.rs:47-57`).
- Component boundaries are unambiguous: length-prefixing the body means
  shifting bytes between the fixed-width prefix and the body cannot collide
  (`idempotency.rs:59-66`).
