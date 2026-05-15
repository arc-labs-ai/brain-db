# Entity Resolution Algorithm

## Inputs and outputs

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

## Configuration

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

## Algorithm

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
    
    // Tier 5: Create new entity
    let new_entity = create_entity(
        candidate,
        hint.unwrap_or_else(|| infer_type_from_context(context)),
        embed_for_storage(candidate, context),
    );
    Created { entity: new_entity }
}
```

## Index requirements

The resolver depends on three indexes:

1. **Exact / alias index**: a trigram-or-btree index on lowercase entity names and aliases, scoped by entity type.
2. **Fuzzy index**: pg_trgm-style trigram similarity index. For Rust/redb, we implement this via a separate `entity_trigrams` table storing (trigram, entity_id) pairs and computing Jaccard or Dice similarity at query time.
3. **Embedding HNSW**: a per-shard HNSW index of entity embeddings. Smaller than the memory HNSW (entity count is typically 10-100x smaller than memory count).

All three are maintained by a worker on every entity create/update.

## Per-extractor resolver configuration

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

## Audit and observability

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

## Handling ambiguity proactively

When a resolution is `Ambiguous`, the system writes the statement with a `Pending(audit_id)` subject. A background `ambiguity_resolver` worker periodically:

1. Lists pending audits.
2. For each, checks if more context has accumulated (the same audit might have been written multiple times for the same candidate).
3. Re-runs resolution with the accumulated context (more signal).
4. If still ambiguous: queues for human review (admin interface lists pending audits).
5. If resolved: updates all pending statements with the resolved EntityId.

Human review is the escape hatch: operators see a list of "Priya — 3 possible matches" and pick one (or merge two if they realize the candidates are actually duplicates).
