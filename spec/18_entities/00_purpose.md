# Entities

## Purpose

An **Entity** is a canonical reference to a noun: a person, place, project, organization, document, or any other "thing" the substrate needs to track identity for.

Entities are the join points of the knowledge graph. Every Statement has an Entity as its subject. Every Relation has Entities as its endpoints. Memories *mention* Entities through extracted references.

Entities are identity-stable. When "Priya Patel" gets married and becomes "Priya Singh," the canonical name changes but the EntityId is the same. Existing Statements and Relations continue to point to the same identity.

## Entity record schema

```rust
struct Entity {
    id: EntityId,                       // UUIDv7, 128 bits
    entity_type: EntityTypeId,          // user-declared type (see schema DSL)
    canonical_name: String,             // primary display name
    aliases: Vec<String>,               // alternative spellings, nicknames
    attributes: BTreeMap<String, Value>,// arbitrary typed key-value
    mention_count: u32,                 // memories referencing this entity
    created_at: u64,                    // unix micros
    updated_at: u64,                    // last canonical_name or attribute change
    merged_into: Option<EntityId>,      // None unless this entity has been merged
    embedding: Option<[f32; 384]>,      // computed from canonical_name + context
    embedding_version: u32,             // for re-embedding when model changes
}
```

### Field semantics

- **`id`**: Immutable. Once assigned, never changes. Survives renames.
- **`entity_type`**: References a user-declared entity type (`Person`, `Project`, `Document`, etc.). One per Entity. Mutation requires explicit `RETYPE_ENTITY` operation.
- **`canonical_name`**: The "primary" display name. Used in extraction matching and in rendered output. Mutable; old values move into `aliases`.
- **`aliases`**: Alternative names that resolve to this entity. Includes old canonical names, nicknames, abbreviations, common misspellings. Mutable; bounded length (default 32).
- **`attributes`**: Free-form key-value. Constrained by the entity type's declared attribute schema (see `21_schema_dsl/`). Common attributes for a Person: `email`, `role`, `team`. Constraints enforced on write.
- **`mention_count`**: How many Memories mention this entity. Maintained by the entity resolver. Used for ranking and pruning decisions.
- **`merged_into`**: Set if this entity has been merged into another. Queries through this entity transparently redirect.
- **`embedding`**: Embedding of `canonical_name + " " + entity_type.name + " " + top_attributes`. Used by the entity resolver for embedding-similarity matching. Re-computed when the embedding model version changes.

## Entity types

Users declare entity types in the schema DSL:

```
define entity_type Person {
    attributes {
        email:       text optional unique
        role:        text optional
        team:        text optional
        timezone:    text optional
    }
}

define entity_type Project {
    attributes {
        slug:        text required unique
        repo_url:    text optional
        owner:       ref<Person> optional
    }
}
```

Entity types control:
- Which attributes are allowed.
- Which attributes are required vs optional.
- Which attributes are unique (used as alternate keys for resolution).
- Which attributes reference other entities (`ref<EntityType>`).

The type system is enforced at Entity creation and update time. Invalid attributes are rejected.

## Identity resolution

When an extractor mentions an entity in text, the resolver runs to determine: existing entity, or new entity?

The pipeline runs in tiers, each cheaper than the next, with early termination:

```
Input: candidate_name, context (surrounding text), entity_type_hint?

Tier 1 — Exact match:
    Look up canonical_name in entities index (case-insensitive, whitespace-normalized).
    Look up each alias.
    If unique hit: return entity. Confidence = 1.0.
    If multiple hits: proceed to tier 2 with multi-candidate set.
    If no hits: proceed.

Tier 2 — Fuzzy match (trigram):
    Compute trigram similarity between candidate and all entity canonical_names
    of compatible entity_type.
    If single match above SIMILARITY_THRESHOLD (default 0.85): return entity. Confidence = similarity.
    Else: proceed with top-K candidates (default K=5).

Tier 3 — Embedding similarity:
    Embed `candidate_name + " " + context_snippet (≤ 100 chars)`.
    Search entity embedding HNSW within entity_type filter, top-K (default 5).
    If single match above EMBEDDING_THRESHOLD (default 0.78): return entity. Confidence = cosine.
    Else: proceed with top-K candidates.

Tier 4 (optional, schema-configurable) — LLM:
    Prompt: "Given the candidate '{candidate}' in context '{context}', is it the same as any of these entities: {top_K}? Reply with the entity ID or 'none'."
    Pinned model, temperature=0, schema-validated output.
    If single match: return entity. Confidence = LLM's reported probability.
    Else: no resolution.

Tier 5 — Create:
    No tier above resolved. Create new entity with:
        canonical_name = candidate_name (normalized)
        entity_type = inferred from extractor / hint / default
        aliases = []
        attributes = {} or populated from extraction
        embedding = computed
    Confidence = 0.6 (default for fresh entities; lower than any match).
```

Each tier has a configurable threshold. Disabling a tier (e.g., not running tier 4 in cost-sensitive deployments) shifts unresolved candidates to tier 5.

## Resolution ambiguity

If multiple candidates pass the threshold at the same tier, the resolver does *not* pick one. Instead:

1. The statement (or relation) is written with a *pending* subject reference: `EntityId::Pending(audit_id)`.
2. An audit record is written to `entity_resolution_audit` with: candidate name, context, top-K matches with confidences, the timestamp.
3. A worker (configurable, possibly LLM-assisted) attempts to resolve. Until resolved, the statement is queryable by predicate and confidence but excluded from graph joins on the subject.

Users can list pending resolutions via `ADMIN_LIST_PENDING_RESOLUTIONS` and manually decide. The substrate does not guess.

## Merging entities

Two entities are merged when the resolver determines they are the same:

```rust
fn merge_entity(
    survivor: EntityId,
    merged: EntityId,
    confidence: f32,
    actor: Actor,
) -> MergeOutcome
```

Mechanics:

1. `merged.merged_into = Some(survivor)`.
2. `merged.aliases` are added to `survivor.aliases` (deduplicated).
3. `merged.attributes` are merged into `survivor.attributes`, with conflict resolution per the entity type's merge rules (default: survivor wins, with conflicts logged).
4. All statements with `subject = merged` updated to `subject = survivor`.
5. All statements with `object = Entity(merged)` updated to `Entity(survivor)`.
6. All relations with `from = merged` updated to `from = survivor`.
7. All relations with `to = merged` updated to `to = survivor`.
8. `survivor.mention_count += merged.mention_count`.
9. Merge audit record written.
10. The substrate emits a `MERGED` event on the SUBSCRIBE channel for any subscribers.

Reversibility: merges are reversible within a grace period (default 7 days) by `UNMERGE_ENTITY`. After that, the redirect is permanent. Hard unmerge requires manual statement re-routing.

Confidence thresholds:
- `confidence >= 0.95`: autonomous merge allowed (with audit).
- `0.7 <= confidence < 0.95`: queued for human review.
- `confidence < 0.7`: not a merge candidate.

## Renaming entities

A rename is a soft operation:

```rust
fn rename_entity(
    entity: EntityId,
    new_canonical_name: String,
    move_old_to_alias: bool,
) -> ()
```

- `canonical_name` is updated.
- If `move_old_to_alias`, the old name is appended to `aliases`.
- `updated_at` is bumped.
- Embedding is recomputed (asynchronously by a worker).
- No statements or relations are touched.

## Entity garbage collection

An entity becomes a candidate for garbage collection when:

- No active (non-superseded) Statement has this entity as subject or object.
- No active Relation has this entity as endpoint.
- `mention_count = 0` (no memory mentions).
- Last update is older than the GC grace period (default 90 days).

The GC worker (optional, off by default) tombstones such entities. They are reversible during their own grace period.

The default is off because entities are cheap and re-creating them after hard deletion loses identity. Operators with long-running deployments and high entity churn (test data, etc.) can enable GC.

## Entity attributes vs Statements about entities

A common question: should `email: priya@example.com` be a Person attribute on the Entity, or a Fact statement `(Priya, has_email, "priya@example.com")`?

Guidance:

| Use entity attribute when... | Use Fact when... |
|---|---|
| Value is intrinsic to identity (slug, email, primary role) | Value can change over time |
| Value is required for the entity type | Value has uncertain confidence |
| Value is used in resolution/dedup | Value needs provenance |
| Schema enforces format/uniqueness | Value derives from multiple memories |

Practically: attributes are for *small set of identifying / required fields*. Facts are for *everything else*. The entity type schema controls which attributes exist; everything not in the schema becomes a Fact.
