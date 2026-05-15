# Phase 16 — Entity Layer

> Phase-level plan. Covers sub-tasks 16.1–16.9. The pattern follows
> Phase 15: this file is the phase overview; each sub-task gets its
> own `.claude/plans/phase-16-task-NN.md` before implementation
> starts.

## Goal

Make entities real. After this phase: entities can be created, looked
up, resolved (tiers 1–3), renamed, merged, and unmerged via wire +
SDK. The substrate-only regression suite continues to pass.

This is the first "real behavior" phase of the knowledge layer.
Phase 15 laid the storage; Phase 16 makes it queryable.

## What Phase 15.1 already shipped

Most of the *types* are in place. Phase 16 mostly adds *operations*.

From 15.1 (`brain-core::knowledge`, `brain-metadata::tables::knowledge`):

- `EntityId` (UUIDv7), `EntityTypeId` (u32), `AuditId`, `MergeId`.
- `EntityMetadata` rkyv value (with most spec-aligned fields, plus
  `aliases_blob`/`attributes_blob` as opaque `Vec<u8>` placeholders).
- `MentionMetadata` rkyv value.
- `EntityTypeDefinition` rkyv value.
- `MergeRecord` rkyv value.
- `ResolutionAudit` rkyv value.
- redb tables: `entities`, `entity_by_canonical_name`,
  `entity_aliases`, `entity_trigrams`, `entity_mentions`,
  `entity_types`, `merge_log`, `entity_resolution_audit`.

What Phase 16 must add on top:

- Typed CRUD over those tables (16.2).
- Promote `aliases_blob` to a typed `Vec<String>` field (16.1).
  `attributes_blob` stays opaque until Phase 19 (schema DSL).
- The four behavioral pieces: entity HNSW (16.3), trigram index +
  Jaccard scoring (16.4), the multi-tier resolver (16.5), merge +
  unmerge (16.7).
- Wire + SDK surface (16.6, 16.8).
- Tests (16.9).

## Spec references (phase-wide reading)

1. `spec/18_entities/00_purpose.md` — Entity record schema, type
   system, lifecycle.
2. `spec/18_entities/01_resolution.md` — Tier-1/2/3 algorithm,
   resolver config, ambiguity handling.
3. `spec/18_entities/02_storage.md` — Table layout, HNSW parameters,
   read/write paths.
4. `spec/28_knowledge_wire_protocol/00_purpose.md` — Wire opcodes
   0x30–0x38.
5. `spec/29_knowledge_sdk/00_purpose.md` — SDK shape.
6. `spec/06_ann_index/02_parameters.md` — HNSW parameter precedent
   (entity HNSW is a smaller variant).

## Pre-flight findings

### F-1 — No schema DSL yet (phase 19)

`EntityType` is supposed to be user-declared (spec §18 + §21). Phase
19 ships the schema DSL parser; until then, Phase 16 needs **at least
one hardcoded entity type** so its tests can run.

**Decision:** ship a built-in `Person` `EntityTypeDefinition` seeded
into `entity_types` at `MetadataDb::open` with a stable
`EntityTypeId(1)`. Phase 19 replaces this with user-declared types
from `SCHEMA_UPLOAD`. The built-in row is harmless once real types
arrive — IDs are unique; the user's first declared type gets
`EntityTypeId(2)+`.

Pull this into 16.1 — it's a one-table seed during open, not a
behavior change.

### F-2 — `aliases` and `attributes` field shapes

`EntityMetadata` from 15.1 stores both as `Vec<u8>` blobs. Two
options for Phase 16:

- **`aliases`** → promote to `Vec<String>`. Simple, no schema-DSL
  dependency, the resolver needs to scan aliases for tier-1 lookups.
- **`attributes`** → keep as `Vec<u8>` blob. The typed `Value` union
  needs `EntityType`'s attribute schema (phase 19) to validate. For
  Phase 16, `attributes_blob` is just stored opaquely; CRUD passes
  bytes through.

This is a one-row schema bump on `EntityMetadata` (rkyv supports
field additions backward-compatibly). `type_name` versioning bumps
to `::v2`.

### F-3 — Entity embedding lifecycle

Spec §18/02 §"Write paths" step 5: "Embed and write to entity HNSW
(async, doesn't block)." Two implementation paths:

- **Synchronous in 16.3:** entity create blocks on embedding. Simpler
  but adds 1–5 ms to entity-create latency.
- **Async worker:** create completes immediately; a background
  worker embeds + inserts into HNSW within seconds. Spec-mandated
  but adds a worker.

For Phase 16, **synchronous is OK** — entity-create rate is much
lower than ENCODE rate (entities are merged across many memories).
Phase 21 (LLM extractor) is where embedding throughput becomes
material; an entity-embedding worker can land then.

### F-4 — `BGE-small-en-v1.5` is the embedding model

Same model as memory embeddings (`brain-embed`). The entity embedding
text per spec §18/00: `canonical_name + " " + entity_type.name + " "
+ top_attributes`. Phase 16 uses `canonical_name + " " +
entity_type.name` only (attributes are blobs in 16 — typed
attribute extraction lands in phase 19).

### F-5 — `hnsw_rs` reuse for entity HNSW

The existing `brain-index::HnswIndex` is parameterized by `M`,
`ef_construction`, `ef_search`. Entity HNSW uses different
parameters per spec §18/02:

| Index | M | ef_construction | ef_search |
|---|---|---|---|
| Memory HNSW (existing) | 16 | 200 | 100 |
| Entity HNSW (16.3) | 16 | 100 | 64 |

Reuse the same crate; instantiate with the entity-specific config.

### F-6 — Trigram extraction

Spec §18/02: `entity_trigrams` keyed by `(entity_type_id, [u8; 3],
EntityId)`. Trigrams are 3-byte windows of the normalized name. For
"priya patel" (11 chars), the trigrams are `_pr`, `pri`, `riy`,
`iya`, `ya_`, `a_p`, `_pa`, `pat`, `ate`, `tel`, `el_` (with `_`
padding for word boundaries).

Plan: standard pg_trgm-style with leading/trailing space padding.
Jaccard similarity = `|intersect(A, B)| / |union(A, B)|`. Phase
16.4 implements this purely in Rust; no external trigram crate
needed.

### F-7 — Merge + unmerge with grace period

Spec §18/00 §"Merge" + §01 §"Handling ambiguity": merging two
entities `A` and `B` into survivor `S` means:

1. Update every reference to `B` (statements, relations, mentions)
   to point to `S`. Bulk redb transaction.
2. Set `B.merged_into = Some(S)`.
3. Write a `MergeRecord` to `merge_log` with grace period
   (default 30 days).
4. During the grace period, `UNMERGE` reverses everything.

Phase 16 ships this with a stub for "every reference" — statements
and relations don't exist yet (phases 17, 18), so the bulk redirect
covers mentions + the merged-entity flag. The full cascade lands
when statements and relations exist; that's tracked as a 17/18
follow-up.

### F-8 — Resolver returns an enum the wire surface understands

`ResolutionOutcome` is a tagged enum: `Resolved | Ambiguous |
Created`. The wire layer for `ENTITY_RESOLVE` (opcode 0x36 — not
listed in 16's checklist but spec'd in §28) returns this directly.
For Phase 16 we don't ship 0x36 yet (the 9-sub-task checklist stops
at 0x35 UNMERGE); resolver use is internal-only. **Decision:**
implement 0x36 in Phase 20 (extractors) where it'll be called by
the extractor pipeline.

### F-9 — Derive macro (16.8) is heavy

The phase doc says "typed entity SDK works for at least one example
entity type (Person, with derive macro)." Derive macros are a
substantial feature (proc-macro crate setup + parsing entity-type
metadata + generating From/Into/SchemaInfo impls).

**Decision:** defer the derive macro to Phase 19 (schema DSL) where
the user-declared schema is the natural source. For Phase 16, ship a
non-derive SDK API: `client.entity_create(EntityTypeId(1),
"Priya", attrs)` + a typed wrapper `Person::create(client, "Priya",
{ email: "...", role: "..." }).await?` written by hand for the
hardcoded `Person` type. This proves the SDK works end-to-end
without committing to the macro before the schema language exists.

## Sub-task plan

| # | Title | Files | Effort |
|---|---|---|---|
| 16.1 | Entity record types + alias promotion + Person bootstrap | `brain-core::knowledge::entity` (new), `brain-metadata::tables::knowledge::entity` (rev), `brain-metadata::tables::knowledge::entity_type` (seed) | S |
| 16.2 | redb entity CRUD operations | `brain-metadata::entity_ops` (new) | M |
| 16.3 | Entity HNSW per shard | `brain-index::entity_hnsw` (new) | M |
| 16.4 | Trigram index + Jaccard scoring | `brain-metadata::trigram` (new) | M |
| 16.5 | Resolver tiers 1+2+3 | `brain-core::resolver` (new) | L |
| 16.6 | Wire opcodes 0x30–0x33 (CREATE/GET/UPDATE/RENAME) | `brain-protocol::knowledge::entity` (new), `brain-server::handlers::knowledge::entity` (new) | L |
| 16.7 | Merge + unmerge (0x34/0x35) with grace-period rollback | `brain-core::merge` (new), `brain-server::handlers::knowledge::entity_merge` (new) | L |
| 16.8 | SDK helpers (hand-written Person; macro deferred) | `brain-sdk-rust::knowledge::entity` (new) | S |
| 16.9 | Integration tests | `crates/brain-server/tests/knowledge_entities.rs` (new) | M |

S = ~0.5 day, M = ~1 day, L = ~1.5 days. Total: ~7–9 days. Matches
the phase doc's "7–10 days" estimate.

## Cross-cutting concerns

- **Substrate regression** — every sub-task must keep
  `knowledge_compat.rs` (15.5) green. The schema-off assertion is
  binding for the lifetime of the project. We re-run the test at the
  end of every sub-task and in the phase exit.
- **Entity-type bootstrap** — the seeded `Person` row appears at
  `MetadataDb::open`. Phase 19 will replace it with user-declared
  types; we coordinate that change with phase 19's plan.
- **Embedding model** — `brain-embed::Embedder` is the existing
  service. 16.3 calls it for entity embeddings (synchronous, low
  rate). No new model.
- **Single-writer-per-shard discipline** — all CRUD goes through one
  `&mut MetadataDb` per shard, mirroring the substrate pattern.
- **`ResolverConfig`** — exposed as a struct, defaults per spec
  §18/01. Phase 20 extractors will override per-extractor; Phase 16
  uses defaults.

## Crate dependency additions

- `brain-core`: no new external crate; `brain-embed` becomes a
  dependency (for `Embedder` access in the resolver).
- `brain-metadata`: no new external crate.
- `brain-index`: existing `hnsw_rs` (instantiated with new params).
- `brain-protocol`: no new external crate.
- `brain-server`: no new external crate.
- `brain-sdk-rust`: no new external crate.

No `proc-macro` crate added (derive macro deferred to phase 19).

## Done-when (phase)

- All 9 sub-task commits land in order.
- Workspace `cargo zigbuild --workspace --tests` clean.
- `knowledge_compat.rs` from 15.5 still passes (substrate
  regression).
- New integration test `knowledge_entities.rs` (16.9) exercises
  create / get / update / rename / merge / unmerge / resolve (tiers
  1, 2, 3) end-to-end.
- Resolver returns correct outcomes for the spec §18/01 test
  matrix.
- Entity HNSW search P50 ≤ 5 ms for 100K entities (deferred to
  Phase 14 quiet-hardware acceptance; in-CI test asserts only that
  search returns sane results).
- Tag `phase-16-complete` after the soak — same pattern as Phase 15.

## Risk register

| Risk | Mitigation |
|---|---|
| Embedding entity names is slow (BGE inference ~5ms each) | Synchronous in 16.3 is fine for low-rate entity creation. If `knowledge_entities.rs` creates 1000+ entities in a single test, batch the embedding calls. |
| Trigram Jaccard scoring at query time scans many entities | Cap the candidate set: scan only entities sharing ≥ 1 trigram with the input. Phase 16 documents the perf envelope (~10K entities = ~5 ms scan); real-workload tuning is Phase 14. |
| Resolver decisions diverge from spec test matrix | 16.5 plan will enumerate the matrix as test cases up front; resolver implementation just satisfies them. |
| Schema-version bump on `EntityMetadata::v1` → `::v2` (alias field) breaks existing serialized data | No existing data — 15.5's regression test confirms knowledge tables are empty on substrate-only deployments. The bump is safe pre-1.0. |
| Phase 19 (schema DSL) wants to remove the seeded `Person` type and clashes | Coordinated in the phase-19 plan — the user's first `SCHEMA_UPLOAD` is responsible for replacing built-in types. Document the convention in `spec/21_schema_dsl/00_purpose.md` follow-up (not part of 16). |
| 16.7 (merge) needs to cascade to statements / relations that don't exist yet | Stub the cascade — merge only updates `entity_mentions` + the `merged_into` flag in 16. Add a TODO referencing phases 17 + 18 for the full cascade. Test what's testable; 16.9 doesn't assert statement/relation redirects. |
| 16.8 SDK ergonomics without a derive macro | Hand-written `Person` wrapper is fine for v1.0 scope. The macro lands in phase 19 alongside the schema DSL, which is the natural source of truth for the macro to consume. |

## Open questions for your approval

1. **Person bootstrap (F-1)** — seed a built-in `Person` row at
   `MetadataDb::open` with `EntityTypeId(1)`? Or require the test
   to insert it manually? **Recommended: seed.** Without it, the
   `entities` table can't accept any row in Phase 16 (CREATE
   validates entity_type_id against the registry).
2. **Sync embedding in 16.3 (F-3)** — synchronous (simpler, slower
   per create) or async worker now? **Recommended: synchronous.**
   Entity create rate is low; embedding worker lands when phase
   21's LLM extractor adds throughput pressure.
3. **`attributes_blob` stays opaque (F-2)** — keep as `Vec<u8>` in
   Phase 16; typed shape lands in phase 19? **Recommended: opaque.**
4. **Derive macro deferred (F-9)** — hand-written `Person`
   wrapper in 16.8 + macro in phase 19? **Recommended: defer.**
   Macros need the schema DSL to consume.
5. **0x36 ENTITY_RESOLVE deferred to phase 20 (F-8)** — agree?
   **Recommended: defer.** Resolver is internal-use only in 16;
   phase 20 is where extractors invoke it.
6. **Plan-file cadence** — per-sub-task plan + approval for each
   of 16.1–16.9 (same as Phase 15)? **Yes, per the established
   convention.**

## Workflow

On your nod on the open questions:

1. I draft `.claude/plans/phase-16-task-01.md` for sub-task 16.1
   (Entity record types + alias promotion + Person bootstrap).
2. You approve → I implement → I commit → I stop.
3. Repeat for 16.2 through 16.9.

After 16.9 lands and CI is green: tag `phase-16-complete`.
