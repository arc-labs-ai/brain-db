# 11.03 Resolver Integration

How pattern and classifier extractor outputs become persisted
entities / statements / relations. The resolver is the bridge
between **mentions** (per-text spans) and **canonical knowledge
records**.

Cross-references:
- [`./01_extractor_tiers.md`](./01_extractor_tiers.md) — emits
  `EntityMention` / `StatementMention` / `RelationMention`.
- [`./01_extractor_tiers.md`](./01_extractor_tiers.md) —
  same output shape.
- [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md)
  — entity-resolution tiers.

## 1. The resolver pipeline

```text
ExtractorRun → Vec<ExtractedItem>
                  │
                  ▼
  for each item:
    match item {
      EntityMention { entity_type, text, .. } →
        resolve_entity (four-tier gauntlet) → EntityId (existing or new)
      StatementMention { kind, subject_text, ... } →
        resolve_subject → resolve_predicate → statement_create
      RelationMention { relation_type, subject_text, object_text, ... } →
        resolve_subject → resolve_object → relation_create
    }
                  │
                  ▼
  Persisted Entity / Statement / Relation rows + audit log
```

The pipeline runs **synchronously after each extractor returns, inside the
extractor worker** (see [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)).
Resolver work shares the worker's background budget, not ENCODE's, so it
never widens ENCODE's p99.

## 2. Entity resolution — four-tier gauntlet

| Tier | Path | Cost | Notes |
|---|---|---|---|
| 1 | Exact match on `normalized_name` OR registered alias | trivial | scoped by entity-type when `type_constraint = Strict` |
| 2 | Trigram + Jaccard ≥ threshold (default 0.85) | cheap | catches typos and minor surface variation |
| 3 | Embedding HNSW similarity ≥ threshold (default 0.78) | medium | catches semantically equivalent surface forms ("Acme Inc" ≈ "Acme, Inc.") |
| 4 | LLM resolution (opt-in per extractor; default off) | expensive | last-resort tie-break with surrounding context |

When all four tiers miss with no ambiguity, the resolver falls back to **creating a new entity** (see §4 below). Detailed algorithm with thresholds and configuration is in §9.

Extractor outputs feed the gauntlet with the `EntityMention.text` field plus the surrounding context. Resolver returns one of:

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
  ambiguity events; post-v1 adds an admin queue.

## 3. Predicate / relation-type resolution

For `StatementMention` / `RelationMention`, the resolver looks up
the canonical qname:

```rust
fn resolve_predicate(name: &str, ns: &str) -> Option<PredicateId>;
fn resolve_relation_type(name: &str, ns: &str) -> Option<RelationTypeId>;
```

Both consult the schema-applied registries from
[`../03_schema/06_system_schema.md`](../03_schema/06_system_schema.md).
Unknown qnames produce `ExtractionAudit.status = Failure { error:
"unknown predicate" }`.

## 4. Auto-creation policy

Brain ships **auto-create for entities only**. Predicates +
relation types are NOT auto-created — they must exist in the
applied schema. Rationale: predicates encode meaning; auto-coining
them creates schema drift.

The schema author can pre-declare `brain:mentions` (see
[`../03_schema/06_system_schema.md`](../03_schema/06_system_schema.md))
for catch-all surfacing of pattern-only mentions before
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
`final_confidence` as the per-evidence confidence input (see
[`../02_data_model/07_statement.md`](../02_data_model/07_statement.md)
— noisy-OR aggregation).

## 6. Idempotency

A re-run of the same extractor over the same memory:
- Skips the resolve step entirely if the audit row already exists
  for `(memory_id, extractor_id, extractor_version)` AND no
  `replay = true` flag is set. The cached audit row's
  `outputs: Vec<OutputRef>` is returned unchanged.
- With `replay = true`, the resolver runs again; new outputs are
  diffed against the cached ones; supersession applies per
  [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md).

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

See [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md). Notably:

- Q-ambiguity-queue — admin surface for `Ambiguous` outcomes.
- Q-auto-predicate — should pattern extractors targeting unknown
  predicates auto-create them under the extractor's namespace?
  (post-v1).
- Q-cross-shard-resolve — entity-mention text resolves on its own
  shard only in v1; cross-shard resolution post-v1.

## 9. Resolver algorithm — four tiers plus create fallback

The resolver runs a four-tier gauntlet (Exact, Fuzzy, Embedding, LLM), each more expensive than the previous, with early termination on a unique hit above threshold. When all four miss without raising an `Ambiguous` outcome, the resolver creates a new entity as a fallback action — represented in the `ResolverTier` enum as `Created` for audit purposes, but not a resolution tier itself.

### 9.1 Inputs and outputs

```rust
fn resolve_entity(
    candidate: &str,                  // raw string from extraction
    context: &str,                    // surrounding text, ≤ 200 chars
    entity_type_hint: Option<EntityTypeId>,
    config: &ResolverConfig,
) -> ResolutionOutcome

enum ResolutionOutcome {
    Resolved { entity: EntityId, confidence: f32, tier: ResolverTier },
    Ambiguous { audit_id: AuditId, candidates: Vec<(EntityId, f32)> },
    Created { entity: EntityId },
}

enum ResolverTier { Exact, Fuzzy, Embedding, LLM, Created }
```

### 9.2 Configuration

```rust
struct ResolverConfig {
    enable_exact: bool,              // default true
    enable_fuzzy: bool,              // default true
    fuzzy_threshold: f32,            // default 0.85
    enable_embedding: bool,          // default true
    embedding_threshold: f32,        // default 0.78
    embedding_top_k: usize,          // default 5
    enable_llm: bool,                // default false
    llm_model: Option<ModelId>,      // required if enable_llm
    llm_threshold: f32,              // default 0.85
    create_confidence: f32,          // default 0.6
    type_constraint: TypeConstraint, // Strict | Hint | None
}
```

`TypeConstraint`:
- `Strict`: candidates must match `entity_type_hint`. Cross-type matches not considered.
- `Hint`: prefer matching type; fall back across types if no in-type match.
- `None`: ignore type hint.

### 9.3 Algorithm

```rust
fn resolve_entity(candidate, context, hint, config) -> ResolutionOutcome {
    let normalized = normalize(candidate);  // trim, lowercase, collapse whitespace

    // Tier 1: Exact match
    if config.enable_exact {
        let exact_hits = entity_index.lookup_exact(&normalized, hint, &config.type_constraint);
        match exact_hits.len() {
            1 => return Resolved {
                entity: exact_hits[0],
                confidence: 1.0,
                tier: Exact
            },
            0 => { /* proceed */ },
            _ => { /* multiple hits; fall through to tier 2 with this candidate set */ }
        }
    }

    // Tier 2: Fuzzy (trigram)
    if config.enable_fuzzy {
        let fuzzy_hits = entity_index.lookup_fuzzy(
            &normalized,
            hint,
            &config.type_constraint,
            config.fuzzy_threshold,
        );
        match fuzzy_hits.len() {
            1 if fuzzy_hits[0].similarity >= config.fuzzy_threshold => {
                return Resolved {
                    entity: fuzzy_hits[0].entity,
                    confidence: fuzzy_hits[0].similarity,
                    tier: Fuzzy,
                }
            },
            _ => { /* keep top-K for tier 3 */ }
        }
    }

    // Tier 3: Embedding similarity
    let emb_candidates = if config.enable_embedding {
        let candidate_emb = embed(&format!("{} {}", candidate, &context[..min(100, context.len())]));
        let hits = entity_hnsw.search(
            &candidate_emb,
            config.embedding_top_k,
            &type_filter(hint, &config.type_constraint),
        );
        let above_threshold: Vec<_> = hits.iter()
            .filter(|h| h.score >= config.embedding_threshold)
            .collect();
        match above_threshold.len() {
            1 => return Resolved {
                entity: above_threshold[0].entity,
                confidence: above_threshold[0].score,
                tier: Embedding,
            },
            _ => hits,  // pass top-K to tier 4
        }
    } else { vec![] };

    // Tier 4: LLM resolution
    if config.enable_llm {
        let llm_result = llm_resolve(
            candidate,
            context,
            &emb_candidates,
            config.llm_model.unwrap(),
        );
        if let Some(resolved) = llm_result {
            if resolved.confidence >= config.llm_threshold {
                return Resolved {
                    entity: resolved.entity,
                    confidence: resolved.confidence,
                    tier: LLM,
                };
            }
        }
    }

    // Check for ambiguity: multiple high-confidence candidates from any tier?
    let all_candidates = collect_candidates(...);  // top-K with score >= threshold/2
    if all_candidates.len() >= 2 && top_two_close(&all_candidates) {
        let audit_id = write_audit(candidate, context, &all_candidates);
        return Ambiguous { audit_id, candidates: all_candidates };
    }

    // Fallback: Create new entity (no resolution tier matched and no ambiguity)
    let new_entity = create_entity(
        candidate,
        hint.unwrap_or_else(|| infer_type_from_context(context)),
        embed_for_storage(candidate, context),
    );
    Created { entity: new_entity }
}
```

### 9.4 Index requirements

The resolver depends on three indexes:

1. **Exact / alias index**: a trigram-or-btree index on lowercase entity names and aliases, scoped by entity type.
2. **Fuzzy index**: pg_trgm-style trigram similarity index. For Rust/redb, this is a separate `entity_trigrams` table storing (trigram, entity_id) pairs and computing Jaccard or Dice similarity at query time.
3. **Embedding HNSW**: a per-shard HNSW index of entity embeddings. Smaller than the memory HNSW (entity count is typically 10-100x smaller than memory count). Tier 3 — the embedding HNSW tie-break — is **live in v1.0**: the resolver embeds `candidate + context_snippet`, searches the entity HNSW with `embedding_top_k = 5`, and resolves to the unique candidate above `embedding_threshold` (default 0.78). Multiple in-band hits fall through to Tier 4 (LLM, opt-in) or to ambiguity-queue + new-entity creation; this is what stops the entity graph fragmenting on near-duplicate surface forms ("Acme Inc" vs "Acme, Inc.").

All three are maintained by a worker on every entity create/update.

### 9.5 Per-extractor resolver configuration

The resolver is invoked by extractors. Each extractor declares its resolver configuration:

```
define extractor person_extractor {
    target: entity_type Person
    resolver {
        type_constraint: Strict
        enable_llm: false           // cost control
        fuzzy_threshold: 0.80       // looser for people (typos common)
        embedding_threshold: 0.82
    }
}
```

This lets cost-sensitive extractors disable LLM resolution while accuracy-critical ones enable it.

### 9.6 Audit and observability

Every resolution writes an audit record:

```rust
struct ResolutionAudit {
    id: AuditId,
    timestamp: u64,
    candidate: String,
    context_snippet: String,
    type_hint: Option<EntityTypeId>,
    outcome: ResolutionOutcome,
    tier_results: BTreeMap<ResolverTier, Vec<(EntityId, f32)>>,
    config_snapshot: ResolverConfig,
}
```

Metrics:
- `entity_resolution_total{tier}` — count of resolutions by terminal tier.
- `entity_resolution_ambiguous_total` — count of ambiguous outcomes.
- `entity_resolution_created_total` — count of new entities created.
- `entity_resolution_latency_seconds{tier}` — latency histogram per tier.
- `entity_resolution_llm_cost_usd_total` — cost tracking when LLM tier is enabled.

Operators can inspect audits via `ADMIN_GET_RESOLUTION_AUDIT`.

### 9.7 Handling ambiguity proactively

When a resolution is `Ambiguous`, the system writes the statement with a `Pending(audit_id)` subject. A background `ambiguity_resolver` worker periodically:

1. Lists pending audits.
2. For each, checks if more context has accumulated (the same audit might have been written multiple times for the same candidate).
3. Re-runs resolution with the accumulated context (more signal).
4. If still ambiguous: queues for human review (admin interface lists pending audits).
5. If resolved: updates all pending statements with the resolved EntityId.

Human review is the escape hatch: operators see a list of "Priya — 3 possible matches" and pick one (or merge two if they realize the candidates are actually duplicates).
