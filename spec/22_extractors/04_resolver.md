# 22.04 Resolver Integration

How pattern and classifier extractor outputs become persisted
entities / statements / relations. The resolver is the bridge
between **mentions** (per-text spans) and **canonical knowledge
records**.

Cross-references:
- [`./01_pattern_extractor.md`](./01_pattern_extractor.md) — emits
  `EntityMention` / `StatementMention` / `RelationMention`.
- [`./02_classifier_extractor.md`](./02_classifier_extractor.md) —
  same output shape.
- [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md)
  — substrate entity-resolution tiers (phase 16.4 / 16.5).

## 1. The resolver pipeline

```text
ExtractorRun → Vec<ExtractedItem>
                  │
                  ▼
  for each item:
    match item {
      EntityMention { entity_type, text, .. } →
        resolve_entity_tier_1_2_3 → EntityId (existing or new)
      StatementMention { kind, subject_text, ... } →
        resolve_subject → resolve_predicate → statement_create
      RelationMention { relation_type, subject_text, object_text, ... } →
        resolve_subject → resolve_object → relation_create
    }
                  │
                  ▼
  Persisted Entity / Statement / Relation rows + audit log
```

The pipeline runs **synchronously after the extractor returns** in
phase 20. Per-item resolver work shares the shard's foreground
budget (§27/01) — for pattern extractors this fits inside ENCODE's
P99 budget; for classifier extractors the resolver tier may push
the extractor to near-foreground.

## 2. Entity resolution (tier 1–3)

Pre-existing from phase 16.4 / 16.5:

| Tier | Path |
|---|---|
| 1 | Exact match on `normalized_name`. |
| 2 | Alias match. |
| 3 | Trigram + Jaccard ≥ threshold. |

Phase 20 extractor outputs feed tier-1/2/3 with the
`EntityMention.text` field. Resolver returns one of:

```rust
pub enum ResolutionOutcome {
    Resolved { entity_id, confidence },
    Created { entity_id },
    Ambiguous { candidates: Vec<EntityId> },
}
```

- `Resolved` → mention linked to the existing entity; an
  `entity_mentions` row is written.
- `Created` → entity-create called automatically; mention linked
  to the new entity.
- `Ambiguous` → no link written; `ExtractionAudit.status =
  Skipped(reason: "ambiguous resolution")`. v1 doesn't surface
  ambiguity events; phase 22+ adds an admin queue.

## 3. Predicate / relation-type resolution

For `StatementMention` / `RelationMention`, the resolver looks up
the canonical qname:

```rust
fn resolve_predicate(name: &str, ns: &str) -> Option<PredicateId>;
fn resolve_relation_type(name: &str, ns: &str) -> Option<RelationTypeId>;
```

Both consult the schema-applied registries from §19.7. Unknown
qnames produce `ExtractionAudit.status = Failure { error:
"unknown predicate" }`.

## 4. Auto-creation policy

Phase 20 ships **auto-create for entities only**. Predicates +
relation types are NOT auto-created — they must exist in the
applied schema. Rationale: predicates encode meaning; auto-coining
them creates schema drift.

The schema author can pre-declare `brain:mentions` (§19.7 system
schema) for catch-all surfacing of pattern-only mentions before
they're typed.

## 5. Confidence chaining

The audit row records both the extractor's confidence and the
resolver's:

```rust
ExtractionAudit {
    extractor_confidence: f32,        // from ExtractedItem
    resolver_confidence: f32,         // from ResolutionOutcome::Resolved
    final_confidence: f32,            // extractor * resolver
    ...
}
```

Downstream consumers (statement_create, relation_create) use
`final_confidence` as the per-evidence confidence input (§17/04
noisy-OR aggregation).

## 6. Idempotency

A re-run of the same extractor over the same memory:
- Skips the resolve step entirely if the audit row already exists
  for `(memory_id, extractor_id, extractor_version)` AND no
  `replay = true` flag is set. The cached audit row's
  `outputs: Vec<OutputRef>` is returned unchanged.
- With `replay = true`, the resolver runs again; new outputs are
  diffed against the cached ones; supersession applies per §17/01.

## 7. Errors

```rust
pub enum ResolverError {
    UnknownPredicate { qname: String },
    UnknownRelationType { qname: String },
    SubjectResolutionFailed { reason: String },
    ObjectResolutionFailed { reason: String },
    AmbiguousEntity { candidates: Vec<EntityId> },
}
```

All map to `ExtractionAudit.status = Failure | Skipped` with the
appropriate reason. No resolver error fails the surrounding
ENCODE.

## 8. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q-ambiguity-queue — admin surface for `Ambiguous` outcomes.
- Q-auto-predicate — should pattern extractors targeting unknown
  predicates auto-create them under the extractor's namespace?
  (post-v1).
- Q-cross-shard-resolve — entity-mention text resolves on its own
  shard only in v1; cross-shard resolution post-v1.
