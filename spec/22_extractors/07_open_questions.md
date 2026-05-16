# 22.07 Open Questions

Extractor-specific deferrals. Wire-shape questions live in
[`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md).

## Active

### Q1 — Bundled NER model licensing + format

Phase 20.3 ships a built-in `brain.basic_ner` classifier. The
exact CONLL-trained checkpoint, its licence, and its candle
compatibility are open at spec-write time. Fallback: a deterministic
rule-based classifier that satisfies the trait but doesn't beat
the pattern tier on recall.

**Target:** phase 20.3 implementation. **Status:** open.

---

### Q2 — Custom feature extractors

[`./02_classifier_extractor.md`](./02_classifier_extractor.md) §3
defines a `FeatureExtractor::Custom { id }` variant for user-
supplied feature pipelines. v1 phase 20 implements `Builtin` only.

**Target:** post-v1. **Status:** deferred.

---

### Q3 — `OnDemand` / `OnSchemaChange` / `Periodic` triggers

[`./03_triggers.md`](./03_triggers.md) §1 lists five trigger types;
phase 20 implements two (`OnEncode`, `OnEncodeWhere`). The other
three parse and persist but never fire — they produce `Skipped`
audit rows.

**Target:** phase 22+. **Status:** deferred.

---

### Q4 — Multi-extractor batching

A classifier model with batch inference could process N memories
at once. Phase 20 dispatches one-at-a-time. Batching land in a
future worker version that buffers near-foreground queue items
into mini-batches.

**Target:** phase 22+. **Status:** deferred.

---

### Q5 — Ambiguous-entity admin queue

[`./04_resolver.md`](./04_resolver.md) §2 — `ResolutionOutcome::
Ambiguous` writes `Skipped(reason: "ambiguous")` audit rows but
doesn't surface them via a dedicated admin op. Operators have to
query audit failures by hand.

**Target:** phase 22+. **Status:** deferred.

---

### Q6 — Auto-predicate creation

[`./04_resolver.md`](./04_resolver.md) §4 — phase 20 auto-creates
entities but not predicates / relation types. A pattern extractor
that emits a `StatementMention` for an unknown predicate fails
with `UnknownPredicate`. Some users may want lax mode where the
predicate auto-coins under the extractor's namespace.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
new `auto_create_predicates: bool` extractor field.

---

### Q7 — Cross-shard mention dedup

Two ENCODEs of structurally-identical text on different shards
produce two `EntityMention` entries; resolver tier 3 on each shard
may decide differently whether to dedupe. Cross-shard mention
dedup is post-v1.

**Target:** post-v1. **Status:** deferred.

---

### Q8 — Content-addressed output IDs

[`./06_idempotency.md`](./06_idempotency.md) §4 — output IDs are
UUIDv7; idempotency relies on the audit row's `outputs` cache.
Some output kinds (e.g., `EntityMention`) could be content-addressed
by `BLAKE3(memory_id || extractor_id || span)`, making them truly
deterministic.

**Target:** phase 22+. **Status:** deferred.

---

### Q9 — Audit row output overflow

[`./05_audit.md`](./05_audit.md) §9 — extreme extractors might
produce ≥64 outputs per memory. v1 caps at 64; overflow goes to a
follow-on `extractor_audit_overflow` row keyed by the original
`audit_id`. Phase 20 implements the cap but not the overflow
mechanism.

**Target:** phase 22+. **Status:** deferred.

---

### Q10 — Cost tracking units

[`./05_audit.md`](./05_audit.md) §1 — `cost_micro_usd: u64`. v1
phase 20 stores 0 (pattern + classifier are zero-cost). Phase 21
LLM uses this field. Question: should we track in dollar
micro-units, token counts, or both? v1 is dollars; phase 21 may
add a `tokens: u64` companion.

**Target:** phase 21. **Status:** open.

---

### Q11 — Deterministic diamond-dependency ordering

[`./03_triggers.md`](./03_triggers.md) §6 — `depends_on` chains
admit diamonds, but phase 20's scheduler dispatches diamond legs
in arbitrary order. Two extractors both depending on a third may
see each other's outputs or not, non-deterministically.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
topological-sort with stable tie-breaking on `ExtractorId`.

---

### Q12 — Resolver tier-4 (LLM-assisted)

[`../18_entities/01_resolution.md`](../18_entities/01_resolution.md)
mentions a tier-4 LLM-assisted resolver. Phase 20 stops at tier 3.
Tier 4 lands alongside phase 21's LLM tier.

**Target:** phase 21. **Status:** deferred.

## Resolved

- Audit-row vs ERROR-frame split — pattern / classifier failures
  write `Failure` audits and don't surface as wire errors. Resolved
  in [`./05_audit.md`](./05_audit.md) §3.
- Extractor fan-out from schema_upload — phase 19.7 deferred the
  `SchemaItem::Extractor` arm; phase 20.5 fleshes it out.
