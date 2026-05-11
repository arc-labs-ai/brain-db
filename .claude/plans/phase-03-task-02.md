# Phase 3 — Task 3.2: Memory metadata table

**Classification:** moderate. First "real" domain table, so this sub-task establishes three patterns that the next 9 tables will follow: (a) rkyv value encoding via `redb::Value`, (b) byte-array key encoding for `MemoryId`, (c) the public-API-vs-storage-representation seam.

**Spec:** `spec/07_metadata_graph/03_memory_table.md` (full — defines row layout, access patterns, lifecycle). Cross-checked: `02_table_layout.md` §5 (rkyv mandate), `02_data_model/02_memory_entity.md` (kind/salience semantics).

## 1. Scope

Deliver:

- `crates/brain-metadata/src/tables/memory.rs` (new) — `MemoryMetadata` struct, `MEMORIES_TABLE` definition, rkyv-via-`redb::Value` impl, ergonomic getter helpers that return brain-core types.
- `crates/brain-metadata/src/tables/mod.rs` (new) — declares `pub mod memory;`.
- `crates/brain-metadata/src/lib.rs` — add `pub mod tables;` (one line).

Out:

- Any helper "table handle" type (e.g., `MemoryTable`). The phase doc says tests cover insert/get/scan/delete; we can do those against the raw `redb::Table` directly. The `MetadataDb` wrapper in 3.10 will own any helper shapes.
- Auxiliary indexes for "list by agent" or "list by context." Spec §3.2 and §3.3 explicitly call those out as `11_open_questions.md` items deferred from v1.
- Edge count maintenance. The `edges_out_count` / `edges_in_count` fields are stored, but the maintenance code lives with 3.4 (edges).
- Tombstone advancement (`forgot_at`/`tombstoned_at`/active-bit clear). 3.7 owns the tombstone path.
- Reclamation. 3.7 + later integration.
- Migration registry. No v1 → v1.x migration exists.

## 2. Spec quotes that bind the design

> **§07/03 §1 (row layout):** the 20-field `MemoryMetadata` struct. ~140 bytes/row.
>
> **§07/03 §2.7 (flags):** bit 0 = Active; bit 1 = HardForgotten; bit 2 = Pinned; bit 3 = Stale (vector pre-model-change); bits 4–31 reserved.
>
> **§07/02 §5 (encoding):** "Values are encoded with **rkyv**."
>
> **§07/03 §10 (lifecycle):** active → maybe consolidated → maybe forgotten → eventually reclaimed.
>
> **§07/03 §2.6:** "Used for cross-model exclusion in queries" — the `embedding_model_fp` field gets filtered in RECALL (Phase 4-ish).
>
> **§07/03 §9 (cross-version compat):** "Adding fields requires bumping the table's schema version." We track that globally via 3.1's `__schema_meta` table (single global version; see SD-3.1).

## 3. Spec ambiguities / decisions to surface

### 3.1 Storing brain-core types vs raw bytes

`MemoryMetadata` contains `MemoryId`, `AgentId`, `ContextId`, `MemoryKind` — all brain-core types. None derive rkyv (and shouldn't — brain-core is the data-model layer; rkyv is a redb-/protocol-specific encoding concern).

**Approach:** store byte representations in the rkyv struct's fields, expose brain-core types through getter methods. The conversion is one line each (`MemoryId::from_be_bytes(self.memory_id_bytes)` etc.). Public constructors take brain-core types; storage internals are bytes. Same seam decided in the Phase 3 plan §3.1.

Concrete:

```rust
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MemoryMetadata {
    memory_id_bytes: [u8; 16],
    agent_id_bytes: [u8; 16],
    context_id: u64,
    slot_id: u64,
    slot_version: u32,
    kind: u8,                       // 0=Episodic, 1=Semantic, 2=Consolidated (per 2.2's mapping)
    text_size: u32,
    created_at_unix_nanos: u64,
    last_accessed_at_unix_nanos: u64,
    forgot_at_unix_nanos: Option<u64>,
    tombstoned_at_unix_nanos: Option<u64>,
    consolidated_at_unix_nanos: Option<u64>,
    salience: f32,
    salience_initial: f32,
    access_count: u32,
    embedding_model_fp: [u8; 16],
    flags: u32,
    edges_out_count: u32,
    edges_in_count: u32,
}

impl MemoryMetadata {
    pub fn memory_id(&self) -> MemoryId { MemoryId::from_be_bytes(self.memory_id_bytes) }
    pub fn agent_id(&self) -> AgentId { AgentId::from(self.agent_id_bytes) }
    pub fn context_id(&self) -> ContextId { ContextId(self.context_id) }
    pub fn kind(&self) -> Result<MemoryKind, BadMemoryKind> { memory_kind_from_u8(self.kind) }
    // ... and setters via pub field access where ergonomic
}
```

### 3.2 redb::Value impl: deserialize-on-read, not zero-copy

rkyv 0.7 supports both:
- **Deserialize-on-read** — `rkyv::from_bytes::<MemoryMetadata>(bytes)` returns owned `MemoryMetadata`. Allocates.
- **Zero-copy** — `rkyv::access::<ArchivedMemoryMetadata>(bytes)` returns a `&ArchivedMemoryMetadata` view into the bytes. No allocation; field reads go through the archived type's accessors.

Spec §07/02 §5 says "Zero-copy deserialization: read a value, get a typed reference into the redb-mmap'd page." That argues for the archived-view approach.

**Picking deserialize-on-read for 3.2.** Reasons:
1. redb's `Value::SelfType<'a>` would need to be `&'a ArchivedMemoryMetadata` for zero-copy, which means the trait's `from_bytes` would have to handle bytecheck failures (panic vs return). Panic-on-corrupt is what we want for now; defer the lifetime gymnastics until we measure a hot read path that needs it.
2. The archive type has different field types (`Archived<f32>`, `Archived<Option<u64>>`, etc.); the getters would need to wrap them. Surface area expands.
3. Owned reads are simpler to test.

Zero-copy is a follow-up if/when profiling shows the dezerialization is hot. Document the trade-off in the module doc.

### 3.3 redb::Value::fixed_width()

Returning `None` is correct — rkyv-encoded bytes have variable size due to internal alignment padding. Returning `Some(N)` for a fixed `N` would let redb pack more tightly, but rkyv 0.7 doesn't guarantee a stable encoded size across changes.

### 3.4 redb::Value::type_name()

The spec's schema-evolution guidance (§07/03 §9) implies we should embed version info in the type name so redb detects type-confused mismatches early. Use `"brain_metadata::MemoryMetadata::v1"`. When we bump to v1.1, change to `"::v2"` (or whatever the spec calls).

### 3.5 Where does `MemoryKind` → `u8` mapping live?

Phase 2 already defined this mapping in `brain-storage::wal::payload::memory_kind_to_u8` (private). Duplicating is fine for now; later we could promote it to brain-core (cross-cutting "byte representation" helpers). 3.2 keeps a local copy with a `// duplicated from brain-storage; promote to brain-core if a third caller appears` comment.

### 3.6 What flags constants does 3.2 own?

The flags layout (§07/03 §2.7) is data-model-level. Define constants in `tables/memory.rs`:

```rust
pub mod flags {
    pub const ACTIVE: u32 = 1 << 0;
    pub const HARD_FORGOTTEN: u32 = 1 << 1;
    pub const PINNED: u32 = 1 << 2;
    pub const STALE: u32 = 1 << 3;
    pub const RESERVED_MASK: u32 = !(ACTIVE | HARD_FORGOTTEN | PINNED | STALE);
}
```

Mirrors Phase 2's pattern in `arena/slot.rs::flags`.

## 4. Architecture

### 4.1 Module placement

```
crates/brain-metadata/src/
├── lib.rs                 (add: pub mod tables;)
├── schema.rs
└── tables/
    ├── mod.rs             (NEW: pub mod memory;)
    └── memory.rs          (NEW: this task)
```

### 4.2 Public surface

```rust
// crates/brain-metadata/src/tables/memory.rs

pub const MEMORIES_TABLE: TableDefinition<'static, &'static [u8; 16], MemoryMetadata> =
    TableDefinition::new("memories");

pub mod flags { /* ACTIVE / HARD_FORGOTTEN / PINNED / STALE / RESERVED_MASK */ }

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MemoryMetadata { /* fields per §3.1 above */ }

#[derive(thiserror::Error, Debug)]
pub enum BadMemoryKind {
    #[error("MemoryKind byte {0} is not in {{0, 1, 2}}")]
    Invalid(u8),
}

#[derive(thiserror::Error, Debug)]
pub enum MemoryEncodingError {
    #[error("rkyv serialize error: {0}")]
    Serialize(String),
    #[error("rkyv access error: {0}")]
    Access(String),
}

impl MemoryMetadata {
    pub fn new_active(
        memory_id: MemoryId,
        agent_id: AgentId,
        context_id: ContextId,
        slot_id: u64,
        slot_version: u32,
        kind: MemoryKind,
        embedding_model_fp: [u8; 16],
        salience_initial: f32,
        text_size: u32,
        created_at_unix_nanos: u64,
    ) -> Self;

    pub fn memory_id(&self) -> MemoryId;
    pub fn agent_id(&self) -> AgentId;
    pub fn context_id(&self) -> ContextId;
    pub fn kind(&self) -> Result<MemoryKind, BadMemoryKind>;

    pub fn is_active(&self) -> bool;
    pub fn is_tombstoned(&self) -> bool;
    pub fn is_pinned(&self) -> bool;
    pub fn is_hard_forgotten(&self) -> bool;
    pub fn is_stale(&self) -> bool;

    pub fn set_flag(&mut self, mask: u32, on: bool);

    // Fields like salience/access_count are plain pub for direct manipulation
    // via read-modify-write in transactions (per §07/03 §4.1).
}

impl redb::Value for MemoryMetadata {
    type SelfType<'a> = MemoryMetadata;
    type AsBytes<'a> = Vec<u8>;
    fn fixed_width() -> Option<usize> { None }
    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a> where Self: 'a;
    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>;
    fn type_name() -> redb::TypeName { redb::TypeName::new("brain_metadata::MemoryMetadata::v1") }
}
```

### 4.3 redb::Value impl (the from_bytes / as_bytes pair)

```rust
fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a> where Self: 'a {
    // Validation enabled via `#[archive(check_bytes)]`.
    rkyv::from_bytes::<MemoryMetadata>(data)
        .expect("MemoryMetadata bytes failed rkyv validation; corrupt redb file")
}

fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a> {
    rkyv::to_bytes::<_, 256>(value)
        .expect("MemoryMetadata is rkyv-serializable")
        .into_vec()
}
```

The `expect` on `from_bytes` is appropriate for now — corrupt bytes in redb mean the file is broken (a much bigger problem than a single row). The `expect` on `as_bytes` is for an "always succeeds" path because the struct is fixed-shape; rkyv serialization of `Option<u64>` + `[u8; N]` + integers cannot fail.

If profiling later shows the panic is reachable in any other way, we'll switch to `redb::Value`'s upcoming fallible variant (proposed in redb's tracking issues).

## 5. Trade-offs

| Question | Choice | Why |
|---|---|---|
| Storage repr: brain-core types vs bytes | Bytes (with brain-core getter methods) | Avoids derive-on-foreign-type orphan rule; clean separation between data-model layer and encoding layer. |
| Deserialize-on-read vs zero-copy | Owned read | Simpler now; revisit if profiling shows hot path. |
| `rkyv::to_bytes::<_, 256>` scratch buffer size | 256 bytes | Rough fit for a 140-byte struct; rkyv grows if needed. Document. |
| `type_name` string format | `"brain_metadata::MemoryMetadata::v1"` | Versioning in the type name catches type-confused mismatches early. |
| Field visibility | Mostly `pub` (direct field access in callers) | Spec §07/03 §4.1 shows read-modify-write patterns; getters/setters would be churn for no benefit. The brain-core typed accessors are wrappers (`memory_id()`, etc.). |
| Helper `MemoryTable` newtype | Defer to 3.10 (`MetadataDb`) | Tests use raw redb in 3.2; rich API gets centralized later. |

## 6. Risks

- **rkyv 0.7's `from_bytes` signature** — needs `T: Archive + ...` plus `T::Archived: bytecheck::CheckBytes<...>`. The `#[archive(check_bytes)]` derive produces this. Confirm at first compile; if friction, fall back to manual `rkyv::access` + `rkyv::deserialize`.
- **redb v4's `Value` trait lifetimes.** v4 may have refined the trait shape vs v2. Implementation may need adjustment; concrete details discovered at first compile.
- **`AsBytes = Vec<u8>` requires an alloc per write.** Acceptable for the typical write rate (10K/sec target per shard). Zero-copy writes would need `AsBytes<'a> = &'a [u8]`, only possible if we keep a serialized buffer alive across the `as_bytes` call. Defer.
- **`MemoryKind` u8 mapping is duplicated** from `brain-storage::wal::payload`. Document. If a third caller appears (likely in Phase 4's wire protocol), promote to brain-core.

## 7. Test plan

All tests in `tables/memory.rs`'s `#[cfg(all(test, not(miri)))]` mod (consistent with 3.1; redb uses mmap).

### Round-trip (4 — phase-doc done-when)

1. **Insert + get.** Create `MemoryMetadata` via `new_active`, insert into redb under its `memory_id` key, get it back, assert field-for-field equality.
2. **Get non-existent key returns `None`.**
3. **Update (insert with same key) overwrites.** Inserting a modified version retrieves the new value.
4. **Delete.** Insert, delete, then get returns `None`.

### Scan (1)

5. **Scan-by-(agent, context) via filter.** Insert 5 memories with mixed agents/contexts. Use `range` over all keys, filter in code, count matches per (agent, context) pair. This validates the spec §07/03 §3.2's "filter in code, no index in v1" path.

### Field round-trips (3)

6. **`Option<u64>` fields survive `None` and `Some(_)` round-trips.** All three temporal options.
7. **Flag bit manipulation.** `set_flag(flags::ACTIVE, true)` then `is_active()`.
8. **Brain-core type round-trip.** Construct via `new_active(MemoryId, AgentId, ContextId, ...)`, read back via getter methods, compare to inputs.

### Encoding stability (1)

9. **Same input → same bytes.** Encode the same `MemoryMetadata` twice via `as_bytes`; assert byte-for-byte equality. Catches accidental nondeterminism in serialization. (rkyv encoding is deterministic by design but this is cheap insurance.)

**Total: 9 tests.**

## 8. Estimated commit shape

One commit on `feature/brain-metadata`:

> `feat(brain-metadata): memory metadata table (sub-task 3.2)`

Body:
- The 20-field `MemoryMetadata` struct (rkyv-derived).
- `MEMORIES_TABLE` definition.
- `redb::Value` impl via rkyv with `check_bytes` validation.
- Brain-core typed getters; bytes-stored representation.
- The deserialize-on-read vs zero-copy decision.
- The duplicated `memory_kind_from_u8` mapping note.
- Test count.

Files touched:
- `crates/brain-metadata/src/lib.rs` (add `pub mod tables;`)
- `crates/brain-metadata/src/tables/mod.rs` (new)
- `crates/brain-metadata/src/tables/memory.rs` (new, ~360 lines incl. tests)

Verify gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p brain-metadata`, `./scripts/check-skills.sh`.

---

PLAN READY: see `.claude/plans/phase-03-task-02.md` — confirm to proceed.
