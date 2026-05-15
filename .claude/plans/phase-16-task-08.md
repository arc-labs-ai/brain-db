# Phase 16 · Sub-task 16.8 — Entity SDK helpers (hand-written Person)

Adds `brain-sdk-rust::knowledge::entity` — a typed Rust SDK over the 9 entity wire opcodes that landed in 16.6c and 16.7. Hand-written for the built-in `Person` type; the derive-macro-driven generalisation (`#[derive(BrainEntity)]`) defers to phase 19 alongside the schema DSL.

## Spec references

- [`spec/29_knowledge_sdk/00_purpose.md`](../../spec/29_knowledge_sdk/00_purpose.md) — "Typed entity API" section (lines 32-77). Phase-scope note added in this commit calling out hand-written-Person-only for 16.8.
- [`spec/28_knowledge_wire_protocol/01_entity_frames.md`](../../spec/28_knowledge_wire_protocol/01_entity_frames.md) — wire shapes.
- [`spec/18_entities/00_purpose.md`](../../spec/18_entities/00_purpose.md) — `Entity` value semantics.
- `crates/brain-sdk-rust/src/ops/{encode,recall,forget}.rs` — pattern reference for how ops are structured.
- `crates/brain-protocol/src/knowledge/{entity_req,entity_resp}.rs` — wire structs the SDK wraps.

## Outputs

```
crates/brain-sdk-rust/src/knowledge/
├── mod.rs                  (new)
├── entity.rs               (new — Person type + EntityHandle wrapper)
├── builder.rs              (new — fluent builders for CREATE / UPDATE / RENAME / MERGE)
└── errors.rs               (new — knowledge-specific error mapping over ClientError)
```

Plus extending `Client` with `entity()` / `entity_resolve()` / `entity_list()` / etc. entry points, defined as a new `KnowledgeEntityClient` impl on `Client`.

## Sub-task breakdown

### 16.8.1 — `Person` type + value mappings

Plain Rust struct mirroring spec §29's `#[derive(BrainEntity)] struct Person { email, role, team }` but without the macro. Carries:

- `pub canonical_name: String`
- `pub aliases: Vec<String>`
- `pub attributes: PersonAttributes` (typed accessor; phase 19's macro replaces this)

`PersonAttributes`:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonAttributes {
    pub email: Option<String>,
    pub role: Option<String>,
    pub team: Option<String>,
}
```

Encode / decode to the wire's opaque `attributes_blob` (`Vec<u8>`) via rkyv. The blob format is a fixed `PersonAttributesWire` struct local to this module; phase 19's schema DSL replaces the bespoke encoding with attribute-bag rkyv decoded against the schema.

`EntityHandle<Person>`: returned by `create` / `get` / `update` / `rename` / `merge` / `unmerge`. Carries:

- `pub id: EntityId`
- `pub canonical_name: String`
- `pub normalized_name: String`
- `pub aliases: Vec<String>`
- `pub attributes: PersonAttributes` (decoded from `EntityView.attributes_blob`)
- `pub mention_count: u32`
- `pub created_at`, `updated_at` (chrono / unix nanos — pick one consistent with existing SDK)
- `pub merged_into: Option<EntityId>`
- `pub flags: u32` + helpers `is_tombstoned()`, `is_merged()`

### 16.8.2 — Fluent builders

`PersonBuilder`:

```rust
client.entity::<Person>()
    .canonical_name("Alice")
    .alias("A.")
    .with_email("alice@example.com")
    .with_role("Engineer")
    .create()
    .await?
```

Internally constructs `EntityCreateRequest`, sends, awaits response, decodes into `EntityHandle<Person>`.

Other builders:

- `client.entity::<Person>().get(id).await?` → `Option<EntityHandle<Person>>` (None on `EntityNotFound`).
- `client.entity::<Person>().update(id).rename("Alice Cooper").with_role("Senior").commit().await?`.
- `client.entity::<Person>().rename(id, "Alice Cooper").await?` (shortcut).
- `client.entity::<Person>().merge(survivor_id, merged_id).with_confidence(0.92).with_reason("dup").execute().await?`.
- `client.entity::<Person>().unmerge(merged_id).execute().await?`.
- `client.entity::<Person>().resolve("Alice").with_type_hint().execute().await?` → `ResolutionOutcome`.
- `client.entity::<Person>().list().with_prefix("ali").limit(50).fetch().await?` → `Vec<EntityHandle<Person>>` (single-page; 16.7 cap = 1000).
- `client.entity::<Person>().tombstone(id, "reason").await?`.

### 16.8.3 — `ResolutionOutcome` SDK type

Mirror `brain_core::knowledge::ResolutionOutcome` but use SDK-level types (`EntityHandle<Person>` not `EntityId`). Surface from `resolve()`:

```rust
pub enum ResolutionOutcome<T> {
    Resolved { entity: EntityHandle<T>, confidence: f32, tier: u8 },
    Created { entity: EntityHandle<T> },
    Ambiguous { candidates: Vec<EntityId>, audit_id: [u8; 16] },
    NotFound,
}
```

Phase 16.8 `resolve` returns `Resolved | Ambiguous | NotFound`; `Created` lights up when phase 17+ wires auto-create through extractors.

### 16.8.4 — Error mapping

`brain-sdk-rust::error::ClientError` already exists. Extend with knowledge-error mapping:

- Substrate `NotFound` carrying `what="entity"` → SDK `BrainError::EntityNotFound(EntityId)`.
- Substrate `Conflict` from merge / duplicate → `BrainError::EntityConflict { message }`.
- Substrate `InvalidArgument` from type mismatch → `BrainError::EntityTypeMismatch`.

Map at the SDK layer in `knowledge/errors.rs`. Internal helper used by every builder's `await`.

### 16.8.5 — Integration tests

`crates/brain-sdk-rust/tests/knowledge_entity.rs` (Linux-only, in-process server):

- create_person_via_builder.
- get_persists_attributes.
- update_changes_canonical_name_and_attributes.
- rename_moves_old_to_alias.
- merge_and_unmerge_via_builders.
- resolve_exact_match.
- list_with_prefix.
- tombstone_then_get_shows_flag.
- error_paths: EntityNotFound, EntityTypeMismatch (unknown type id), low-confidence merge.

Uses the `support_harness` shared with brain-server tests (re-export or duplicate).

### 16.8.6 — Re-exports + docs

`brain-sdk-rust/src/lib.rs` re-exports `knowledge::*` at the crate root so callers write `use brain_sdk::Person;`. Cargo.toml stays — no new deps.

## Out of scope (16.8)

- Derive macros (`#[derive(BrainEntity)]`) — phase 19.
- Schema builder (`SchemaBuilder::new(...)`) — phase 19.
- Statement / relation / query builders — phases 17 / 18 / 22-23.
- Subscribe extensions for knowledge events — phase 17+ (once statements emit them).
- Cursor pagination on `list()` — phase 23.

## Risks

- **Type signature for `client.entity::<Person>()`.** The spec example uses turbofish. Rust generic methods + type-state-style builders are tricky. Likely landing: define `EntityClient<T: BrainEntityType>` trait + impl for `Person`. The `BrainEntityType` trait carries the wire `entity_type_id` constant and the `decode/encode` round-trip for attributes. Phase 19's derive macro auto-impls this trait for any user type.

- **`attributes_blob` round-trip.** Phase 16.6c carried the blob as opaque `Vec<u8>`. 16.8 introduces an SDK-side rkyv encode/decode for `PersonAttributes`. The wire layer remains untouched; the SDK introduces a stable `PersonAttributesWire` archive type.

- **No statement / relation API yet.** Some §29 examples cross-reference statements (`client.fact()...`). The SDK shouldn't expose those methods at all in 16.8 — they'd return runtime errors. Better to gate them behind compile-time `feature = "statements"` or simply not implement.

- **Linux-only integration tests.** Mirror the `knowledge_entity_*_wire.rs` pattern — `#![cfg(target_os = "linux")]` + the shared `support_harness`.

## Suggested commits

1. `feat(sdk): Person type + EntityHandle + attribute round-trip (16.8.1)`.
2. `feat(sdk): fluent entity builders (CREATE/GET/UPDATE/RENAME) (16.8.2a)`.
3. `feat(sdk): merge/unmerge/resolve/list/tombstone builders (16.8.2b)`.
4. `feat(sdk): knowledge error mapping (16.8.4)`.
5. `test(sdk): knowledge_entity integration tests (16.8.5)`.

Five commits, each compiles + tests independently.

## Verification gate

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- `cargo test -p brain-sdk-rust` host-runnable subset (the SDK doesn't pull glommio).
- `cargo test -p brain-protocol -p brain-core` — sanity for upstream deps.
- Clippy `-D warnings` clean.

## After 16.8

16.9 — phase-final integration tests + perf check (entity HNSW P50 ≤ 5ms target). Then `phase-16-complete` tag.

## Conventions

- No Co-Authored-By Claude trailer in commits (memory).
- Folder-per-concern under SDK `src/` (memory) — `knowledge/` directory, one file per concern.
- No `unwrap()` outside tests; `expect("invariant: …")` where unreachable (CLAUDE.md §7).
- Branch: `feature/phase-16-entity-layer` (current).
