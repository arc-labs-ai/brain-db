# Sub-task 16.5 — Resolver tiers 1 + 2 + 3

> Per-sub-task plan. Plan-first convention. The heaviest sub-task of
> Phase 16 — composes everything 16.1–16.4 set up.

## Goal

Implement the entity resolver algorithm per spec §18/01. After this
sub-task:

- `resolve_entity` exists in brain-core, takes `(candidate, context,
  entity_type_hint, config)`, returns `ResolutionOutcome`.
- Tier 1 (exact name + alias), Tier 2 (trigram + Jaccard), Tier 3
  (embedding HNSW + cosine) all work.
- Tier 4 (LLM) is a stub returning a deterministic "not implemented"
  outcome — it doesn't actually call an LLM. Phase 21 wires the
  real LLM extractor.
- Tier 5 (Created) returns a fresh `EntityId` (UUIDv7) for the
  caller to persist. The resolver does NOT itself write to redb.
- Ambiguity detection: ≥2 high-confidence candidates with close
  scores → `Ambiguous` outcome with an `AuditId` placeholder.

## Strict scope boundaries

What 16.5 does NOT do:

- **Concrete trait impls in brain-metadata / brain-embed /
  brain-index.** The trait surface lives in brain-core; real impls
  land when phase 20 (extractors) wires the resolver into the
  pipeline. For 16.5's tests we use mock impls in the test module.
- **Writing the new entity row** in the Created case. The resolver
  returns the fresh EntityId; the caller calls `entity_put` (16.2)
  to persist.
- **Wire opcode `ENTITY_RESOLVE` (0x36).** Per phase-16 plan F-8,
  deferred to phase 20 where extractors actually invoke the
  resolver.
- **Audit-record persistence.** `Ambiguous { audit_id, ... }`
  carries an `AuditId` returned by the resolver, but the audit row
  isn't written to `entity_resolution_audit` here. Persistence is
  a phase-20+ concern that uses the AuditId the resolver minted.

## Reading list

1. `spec/18_entities/01_resolution.md` — full algorithm + config
   semantics.
2. `crates/brain-core/src/knowledge/resolver.rs` — 16.1 shipped the
   types (`ResolverConfig`, `ResolutionOutcome`, `ResolverTier`,
   `TypeConstraint`); 16.5 adds the algorithm + traits.
3. `crates/brain-metadata/src/trigram_ops.rs` — primitives for
   tier 2.
4. `crates/brain-metadata/src/entity_ops.rs` — primitives for
   tier 1.
5. `crates/brain-index/src/entity_hnsw.rs` — primitives for tier 3.

## Pre-flight findings

### F-1 — brain-core can't depend on brain-metadata / -embed / -index

brain-core is the pure-types leaf. Adding it as a dep on
brain-metadata / brain-embed / brain-index would create cycles
(those crates already depend on brain-core).

**Resolution: trait-based dependency inversion.** brain-core defines
three traits (`ResolverStorage`, `ResolverEmbedder`,
`ResolverIndex`); the concrete implementations live in the crates
that own the corresponding data. The resolver function takes
generic parameters bound by those traits.

### F-2 — Trait surface (minimal for the algorithm)

```rust
pub trait ResolverStorage {
    fn lookup_exact_canonical_name(
        &self,
        type_id: EntityTypeId,
        candidate: &str,
    ) -> Result<Option<EntityId>, ResolverError>;

    fn lookup_exact_aliases(
        &self,
        type_id: EntityTypeId,
        candidate: &str,
    ) -> Result<Vec<EntityId>, ResolverError>;

    fn trigram_candidates(
        &self,
        type_id: EntityTypeId,
        query_normalized: &str,
    ) -> Result<HashSet<EntityId>, ResolverError>;

    fn trigrams_of(
        &self,
        id: EntityId,
    ) -> Result<HashSet<[u8; 3]>, ResolverError>;

    /// Tier-3 type-constraint filter needs each candidate's type.
    fn entity_type_of(
        &self,
        id: EntityId,
    ) -> Result<Option<EntityTypeId>, ResolverError>;
}

pub trait ResolverEmbedder {
    /// Produce a 384-dim BGE-small L2-normalised vector.
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], ResolverError>;
}

pub trait ResolverIndex {
    /// HNSW top-k. Returns `(EntityId, similarity)` descending by
    /// similarity. Tombstoned entries pre-filtered by the impl.
    fn search(
        &self,
        query: &[f32; VECTOR_DIM],
        top_k: usize,
    ) -> Result<Vec<(EntityId, f32)>, ResolverError>;
}
```

`VECTOR_DIM = 384` is `brain-core`-known (constant, not a generic
parameter — keeps trait calls simple). Phase 21's reverse-cache
optimization can specialize later.

### F-3 — `ResolverError` shape

brain-core can't reference `redb::Error` or `hnsw_rs::Error`
directly without taking those deps. Use a small enum with
domain-tagged String wrappers:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ResolverError {
    #[error("storage: {0}")]
    Storage(String),
    #[error("embedder: {0}")]
    Embedder(String),
    #[error("index: {0}")]
    Index(String),
}
```

Concrete impls in brain-metadata / -embed / -index convert their
native errors via `.to_string()`. Imperfect but cheap; the
alternative (associated `type Error;`) leaks types through every
function signature.

### F-4 — Ambiguity detection rule

Spec §18/01:

> Check for ambiguity: multiple high-confidence candidates from any
> tier? `if all_candidates.len() >= 2 && top_two_close(&all_candidates)`

Defined as: top-2 candidates are within `δ = 0.05` of each other,
both above the relevant tier threshold. For 16.5 we hardcode
`δ = 0.05` (configurable in `ResolverConfig` later — for now spec
default is fine).

`top_two_close(scores) := |scores[0] - scores[1]| < 0.05`

### F-5 — AuditId allocation in Ambiguous

The `Ambiguous` variant carries an `AuditId`. For 16.5, the
resolver mints a fresh `AuditId::new()` for each ambiguous
outcome. The caller is responsible for persisting an audit row if
desired (phase 20 does this). No persistence inside the resolver.

### F-6 — Tier-1 candidate aggregation when multiple exact hits

Spec §18/01:

```rust
match exact_hits.len() {
    1 => return Resolved { entity, confidence: 1.0, tier: Exact },
    0 => { proceed },
    _ => { multiple hits; fall through to tier 2 with this candidate set }
}
```

Multiple exact hits at tier 1 happen via the **alias** index
(`entity_aliases` is multi-value). When `lookup_exact_aliases`
returns N entities, all N are kept as tier-1 candidates and fed
into the tier-2/3 candidate pool. If tiers 2 and 3 don't narrow
to one, ambiguity surfaces.

### F-7 — Tier-3 type-constraint filtering

`ResolverIndex::search` returns top-k regardless of entity type
(the HNSW doesn't know types). The resolver post-filters per
`TypeConstraint`:

- `Strict`: drop candidates whose `entity_type_of(id) != hint`.
- `Hint`: keep all; downstream scoring can still prefer hint-typed
  candidates (we don't bias here — spec leaves this open).
- `None`: keep all.

`hint == None` makes `Strict` behave like `None` (no constraint
to enforce).

### F-8 — Tier-4 stub semantics

Per phase-16 plan F-9: tier 4 is a stub in 16.5. When
`config.enable_llm` is true, the resolver does **not** error —
it skips tier 4 and falls through to ambiguity / creation.
Add a `tracing::warn!("LLM resolver tier enabled but not
implemented in v0.x; skipping")` for visibility.

### F-9 — Tier-5 Create returns a fresh UUIDv7

`EntityId::new()` mints UUIDv7 (already in 16.1). The resolver
returns `Created { entity: fresh_id }`; the caller writes the
entity to redb via `entity_put` if it wants persistence.

The returned EntityId is **always** fresh — even when the
resolver chooses Created over Ambiguous because every other tier
produced sub-threshold candidates.

### F-10 — Mock impls for tests

Tests use in-process mock structs that implement the three
traits. The mocks hold `HashMap`s and `Vec`s — no redb / no
hnsw_rs / no candle. Tests cover the algorithm; integration tests
with real impls land later.

## Design decisions

### D1 — File layout in brain-core

The 16.1 module shape already provides `resolver.rs` with the
types. Two options:

- Add the algorithm + traits to the same file.
- Split: `resolver.rs` (existing) holds types; new
  `resolver_algo.rs` holds the algorithm. Re-export from
  `knowledge::mod.rs`.

**Recommended: keep one file.** The resolver concept is one
unit; splitting feels artificial. `resolver.rs` grows from ~150
lines (16.1) to ~600 lines (16.5).

### D2 — `ResolverError` (D1 of F-3)

Add to `resolver.rs`. brain-core's prelude grows by one error
type.

### D3 — Three traits in brain-core::knowledge::resolver

Per F-2. All three are `?Sized` (work via `&dyn Trait` if needed)
but the resolver function takes generic parameters for
zero-cost dispatch.

### D4 — Algorithm signature

```rust
pub fn resolve_entity<S, E, I>(
    storage: &S,
    embedder: &E,
    index: &I,
    candidate: &str,
    context: &str,
    entity_type_hint: Option<EntityTypeId>,
    config: &ResolverConfig,
) -> Result<ResolutionOutcome, ResolverError>
where
    S: ResolverStorage + ?Sized,
    E: ResolverEmbedder + ?Sized,
    I: ResolverIndex + ?Sized,
{ /* ... */ }
```

`?Sized` lets callers pass `&dyn ResolverStorage` at the cost of
one indirection. Phase 20 may want this for runtime-selected
storage (in-memory mock during testing, real DB in prod).

### D5 — Embedding input

For tier 3 the candidate embedding is computed from
`candidate + " " + context[..min(100, context.len())]`. Spec
§18/01 — context width is bounded at 100 chars to avoid
embedding-cost blow-up on long memory texts.

### D6 — `δ = 0.05` constant for ambiguity

Hardcoded in resolver.rs as `const AMBIGUITY_DELTA: f32 = 0.05;`.
Add to `ResolverConfig` as a configurable later (phase 21+);
spec default works for 16.5.

### D7 — Tests with mock impls

Mock `ResolverStorage`/`ResolverEmbedder`/`ResolverIndex` impls
in `#[cfg(test)] mod tests`. Test matrix from phase doc:

| Case | Expected |
|---|---|
| Exact canonical_name hit | `Resolved { tier: Exact, confidence: 1.0 }` |
| Single exact alias hit | `Resolved { tier: Exact, confidence: 1.0 }` |
| Two exact alias hits (multi-value index) | falls through to tier 2/3; possibly Ambiguous |
| Single fuzzy hit above threshold | `Resolved { tier: Fuzzy }` |
| Fuzzy hits but all below threshold | falls through to tier 3 |
| Two fuzzy hits with very close scores | Ambiguous |
| Single embedding hit above threshold | `Resolved { tier: Embedding }` |
| Embedding hits but all below threshold | falls through |
| All tiers empty | `Created { entity }` |
| `Strict` constraint filters out matches | falls through |
| `enable_exact = false` skips tier 1 | tier 2/3 run as if tier 1 missed |
| `enable_llm = true` but stubbed | falls through (warn log) |

Plus normalization edge cases the lower layers handle.

### D8 — Behavior when all tiers disabled

`config.enable_exact = false` AND `config.enable_fuzzy = false`
AND `config.enable_embedding = false`. The resolver creates a
new entity (Tier 5). Tests cover this.

## File plan

- `crates/brain-core/src/knowledge/resolver.rs` — extend (was
  ~150 lines in 16.1; becomes ~600 lines with algorithm + traits +
  ResolverError + mock-driven tests).
- `crates/brain-core/src/lib.rs` — re-export `ResolverError`,
  `ResolverStorage`, `ResolverEmbedder`, `ResolverIndex`,
  `resolve_entity`.

No new dependencies. No changes to brain-metadata / brain-embed /
brain-index in 16.5 — they get their trait impls in phase 20.

## Done-when

- `cargo test -p brain-core knowledge::resolver` — green on
  macOS native (brain-core is platform-agnostic).
- Workspace `cargo zigbuild --target x86_64-unknown-linux-gnu
  --workspace --tests` — clean.
- All tests in the matrix above (~15 cases) pass.
- 15.5's `knowledge_compat` substrate-only regression still
  passes — brain-core changes don't touch substrate hot path.
- One commit: `feat(core): 16.5 — entity resolver tiers 1+2+3`.

## Risk register

| Risk | Mitigation |
|---|---|
| Generic params explode at call sites (3 trait bounds = verbose) | `?Sized` lets phase-20 wrappers use `&dyn Trait`; concrete generics give zero-cost dispatch where it matters. Both supported by the same signature. |
| Mock impls drift from real impls' semantics, masking bugs | Phase 20 integration tests catch this. Each mock impl documents its mapping to the spec'd primitive (e.g. `MockStorage::trigrams_of` mirrors `trigram_ops::trigrams_of_components`). |
| `ResolverError` string-wraps lose typed error info | Acceptable cost vs. trait-cycle complexity. Phase 14 may revisit. |
| `δ = 0.05` ambiguity threshold is hardcoded; spec hints at configurable | Tracked as a follow-up; spec default works for 16.5. |
| Tier 3 type-constraint filter is O(K) storage lookups for top-K candidates | K ≤ 5 in default config; 5 lookups per resolve is ~5 µs. Acceptable. |
| Tier-4 LLM stub is silently skipped — extractors enabling it expect a result | `tracing::warn!` ensures it's loud in logs. Phase 21 replaces the stub with real LLM. |
| Embedding context truncation at 100 chars may slice mid-codepoint (Unicode) | Use `context.char_indices()` to find the largest safe byte index ≤ 100. |

## Open questions for your approval

1. **Three-trait dependency inversion (F-1, F-2, D3)** —
   `ResolverStorage`/`Embedder`/`Index` traits in brain-core; real
   impls live in their owning crates? **Recommended: yes.** The
   only architecturally clean way to keep the algorithm in
   brain-core.
2. **No real-impl wiring in 16.5 (scope)** — `impl ResolverStorage
   for ...` is deferred to phase 20 when extractors invoke the
   resolver? **Recommended: yes.** Keeps the diff focused; mock
   impls drive tests.
3. **`ResolverError` with String-wrapped variants (F-3)** —
   accept the typed-error info loss to avoid cross-crate type
   coupling? **Recommended: yes.**
4. **Algorithm signature: generic + `?Sized` (D4)** — supports
   both monomorphic and `&dyn`-style callers? **Recommended: yes.**
5. **Hardcoded `δ = 0.05` (D6)** — vs. adding to `ResolverConfig`?
   **Recommended: hardcoded.** Spec default works; configurable
   knob lands when an operator actually wants to tune it.
6. **Tier-5 Created mints fresh UUIDv7; caller persists (F-9)** —
   resolver is read-only? **Recommended: yes.** Decoupling write
   from decide keeps the resolver pure and testable; phase 20's
   pipeline composes them.

## Workflow

On your nod: implement, run `cargo test -p brain-core knowledge::resolver`
+ `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace
--tests`, commit as `feat(core): 16.5 — entity resolver tiers 1+2+3`,
then stop and draft 16.6's plan (wire opcodes 0x30-0x33).
