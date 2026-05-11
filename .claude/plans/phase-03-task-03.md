# Phase 3 — Task 3.3: Agents and contexts tables

**Classification:** moderate. Four of the 13 tables (`agents`, `contexts`, `context_names`, `agent_contexts`). Introduces the first composite keys in Phase 3, which sets the pattern for `edges_out`/`edges_in` in 3.4.

**Spec:** `spec/07_metadata_graph/05_context_table.md` (full — three context tables), `spec/07_metadata_graph/02_table_layout.md` §12 (agents table shape), `spec/02_data_model/04_context.md` (model).

## 1. Scope

In:

- `crates/brain-metadata/src/tables/agent.rs` (new):
  - `AGENTS_TABLE: TableDefinition<[u8; 16], AgentMetadata>`.
  - `AgentMetadata` (rkyv-derived).
  - `redb::Value` impl, typed getter for `agent_id()`.
- `crates/brain-metadata/src/tables/context.rs` (new) — three tables co-located because their access patterns are interlocked (spec §07/05 §3 calls them "three small tables for three access patterns"):
  - `CONTEXTS_TABLE: TableDefinition<u64, ContextMetadata>`.
  - `CONTEXT_NAMES_TABLE: TableDefinition<(&[u8;16], &str), u64>` — the name→ID index.
  - `AGENT_CONTEXTS_TABLE: TableDefinition<([u8;16], u64), ()>` — the agent→[context_ids] index.
  - `ContextMetadata` (rkyv-derived).
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod agent;` + `pub mod context;`.

Out:

- "Lazy-creation on ENCODE" logic (spec §07/05 §5). That's writer-task logic (Phase 9-ish); 3.3 delivers the tables only.
- ContextId allocation policy (spec §07/05 §4) — that's writer-side too.
- Soft/hard limit enforcement (spec §07/05 §7). Phase 9.
- Rename/delete admin paths (§07/05 §8–§9). Future admin opcodes.
- Periodic `memory_count` reconciliation (§07/05 §10). Phase 8 worker.

## 2. Spec quotes that bind the design

> **§07/05 §2.1 (contexts table value):**
> ```rust
> struct ContextMetadata {
>     context_id: ContextId,
>     agent_id: AgentId,
>     name: String,
>     created_at: u64,
>     last_active_at: u64,
>     memory_count: u32,
>     description: Option<String>,
>     tags: Vec<String>,
> }
> ```
>
> **§07/05 §2.2** (`context_names`): "Lookup: in agent A, what's the ContextId of context named 'foo'? → range query."
>
> **§07/05 §2.3** (`agent_contexts`): "the membership index. Lists all contexts an agent has."
>
> **§07/02 §12 (agents table shape):** "AgentId. Display name (optional). Created at. Stats (memory count, contexts count). Configuration overrides."
>
> **§07/05 §6 (implicit context):** ""_default"" context per agent, created on first memory. Leading underscore reserved.

## 3. Design decisions

### 3.1 `AgentMetadata` minimal shape

Spec §07/02 §12 lists five concept areas; v1 stores only the load-bearing ones:

```rust
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct AgentMetadata {
    pub agent_id_bytes: [u8; 16],
    pub display_name: Option<String>,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    pub memory_count: u64,         // denormalized
    pub context_count: u32,        // denormalized
}
```

"Configuration overrides" (spec §07/02 §12's last bullet) is deferred to a future field addition — typical workloads don't need it, and adding an `Option<rkyv_compatible_config>` later is the field-addition path covered by §02/09 §2.

### 3.2 Composite keys

For `context_names` and `agent_contexts` we lean on redb v4's tuple Key. The encoded form is concatenation of the components' bytes; since `[u8; 16]` is fixed-width and comes first, range scans by agent (prefix `(agent, ...)`) work correctly.

```rust
pub const CONTEXT_NAMES_TABLE: TableDefinition<(&'static [u8; 16], &'static str), u64> =
    TableDefinition::new("context_names");

pub const AGENT_CONTEXTS_TABLE: TableDefinition<([u8; 16], u64), ()> =
    TableDefinition::new("agent_contexts");
```

`&'static` lifetimes in the const are the standard redb pattern; at use sites the borrow checker uses the Key trait's `SelfType<'a>` to accept any-lifetime references.

If redb v4's tuple Key impl doesn't cover these forms exactly (e.g., requires `&[u8]` rather than `&[u8; 16]`), fall back to manual byte-concatenation keys (`TableDefinition<&[u8], u64>` with a hand-rolled encoder). Iterate at compile time.

### 3.3 `ContextMetadata` and the `context_id_bytes` redundancy

`ContextId` is `u64`, not 16 bytes. Spec §07/05 §2.1 stores both `context_id: ContextId` (8 B) and `agent_id: AgentId` (16 B) in the value. We mirror:

```rust
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ContextMetadata {
    pub context_id: u64,              // matches the table key for convenience
    pub agent_id_bytes: [u8; 16],
    pub name: String,
    pub created_at_unix_nanos: u64,
    pub last_active_at_unix_nanos: u64,
    pub memory_count: u32,
    pub description: Option<String>,
    pub tags: Vec<String>,
}
```

Self-keyed: `CONTEXTS_TABLE: TableDefinition<u64, ContextMetadata>`. Caller passes `&context_id`.

### 3.4 Variable-length values: `String`, `Option<String>`, `Vec<String>`

rkyv 0.7 supports all three out of the box. Round-trip tests will verify, and `Vec<String>` for `tags` requires no special handling — rkyv's `Archive` derive auto-handles vec-of-archive types.

### 3.5 `type_name()` strings

Following 3.2's pattern: `"brain_metadata::AgentMetadata::v1"` and `"brain_metadata::ContextMetadata::v1"`. Index tables (`context_names`, `agent_contexts`) use `u64` and `()` values respectively, which have redb's built-in `Value` impls — no custom `type_name`.

### 3.6 No table-handle helpers in 3.3

Following 3.2's pattern, tests use raw redb tables. The ergonomic wrapper (e.g., `ContextStore::insert_full(agent, name, ...)` that atomically updates all three tables) lives in 3.10's `MetadataDb`.

## 4. Architecture

### 4.1 Files

```
crates/brain-metadata/src/tables/
├── agent.rs    (NEW)
├── context.rs  (NEW)
├── memory.rs   (existing — 3.2)
└── mod.rs      (modify: pub mod agent; pub mod context;)
```

### 4.2 Public surface (`agent.rs`)

```rust
pub const AGENTS_TABLE: TableDefinition<[u8; 16], AgentMetadata> =
    TableDefinition::new("agents");

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct AgentMetadata { /* §3.1 */ }

impl AgentMetadata {
    pub fn new(
        agent_id: AgentId,
        display_name: Option<String>,
        created_at_unix_nanos: u64,
    ) -> Self;
    pub fn agent_id(&self) -> AgentId;
}

impl redb::Value for AgentMetadata { /* rkyv-backed, deserialize-on-read */ }
```

### 4.3 Public surface (`context.rs`)

```rust
pub const CONTEXTS_TABLE: TableDefinition<u64, ContextMetadata> =
    TableDefinition::new("contexts");
pub const CONTEXT_NAMES_TABLE: TableDefinition<(&'static [u8; 16], &'static str), u64> =
    TableDefinition::new("context_names");
pub const AGENT_CONTEXTS_TABLE: TableDefinition<([u8; 16], u64), ()> =
    TableDefinition::new("agent_contexts");

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ContextMetadata { /* §3.3 */ }

impl ContextMetadata {
    pub fn new(
        context_id: ContextId,
        agent_id: AgentId,
        name: String,
        created_at_unix_nanos: u64,
    ) -> Self;
    pub fn context_id(&self) -> ContextId;
    pub fn agent_id(&self) -> AgentId;
}

impl redb::Value for ContextMetadata { /* rkyv-backed */ }
```

Plus a single helper constant or doc:

```rust
/// Names starting with `_` are reserved (spec §07/05 §6); the writer
/// task (Phase 9) enforces this on client input. Storage doesn't validate.
pub const RESERVED_NAME_PREFIX: &str = "_";

/// The implicit default context's name (spec §07/05 §6).
pub const DEFAULT_CONTEXT_NAME: &str = "_default";
```

## 5. Trade-offs

| Question | Choice | Why |
|---|---|---|
| Co-locate the 3 context tables or split | Co-locate in `context.rs` | They're interlocked (every context-create touches all 3); having them in one file keeps the consistency model visible. |
| Drop `description`/`tags` from MVP | No, include | Spec §07/05 §2.1 lists them explicitly; small storage cost. |
| Tuple keys via redb vs manual byte-concat | Tuple keys | Cleaner; redb handles ordering. Manual byte-concat is the fallback if the tuple impls don't cover our shape. |
| `context_id_bytes` in `ContextMetadata` | No — store `u64` directly | ContextId is already u64; no need for byte conversion (unlike MemoryId/AgentId which are 16 bytes). |
| Helper API on tables | Defer to 3.10 | Consistent with 3.2. |

## 6. Risks

- **redb v4 tuple Key with `(&[u8; 16], &str)`.** redb's blanket Key impl for tuples may or may not accept a mix of fixed-array-ref + str-ref. If compile fails, fall back to flat `&[u8]` key with manual concatenation. Iterate at first compile.
- **Range scans by agent on `agent_contexts`.** With key `([u8; 16], u64)`, range `(agent, 0)..(agent_next, 0)` returns all contexts of `agent`. `agent_next` is the lex-next 16-byte value — `[0xFF; 16]` won't work generically; we'd compute next via "increment with carry" or use `u128::from_be_bytes(agent) + 1` then back to bytes. Document this idiom in the file; range scans are a follow-up in 3.10 anyway.
- **`String`/`Vec<String>` rkyv round-trip.** rkyv 0.7 supports them, but the archived form is `ArchivedString` / `ArchivedVec<ArchivedString>` — only matters if we go zero-copy (we don't, yet).

## 7. Test plan

Tests in `agent.rs` (`#[cfg(all(test, not(miri)))] mod tests`) and `context.rs` (same gating). Both follow 3.2's structure.

### Agent table (4)

1. **Insert + get round-trip.**
2. **Update (re-insert) overwrites** — bump `memory_count`/`last_active_at`.
3. **Delete.**
4. **Brain-core type round-trip** — construct via `new(AgentId, ...)`, get back, compare via `agent_id()` getter.

### Context tables (5)

5. **`contexts` insert + get** via `ContextId`.
6. **`context_names` lookup** by `(agent, name)` returns the right `ContextId`.
7. **`agent_contexts` range scan** for an agent returns its context IDs in order. (Tests the fixed-prefix range query pattern.)
8. **Cross-agent isolation** — two agents each have a context named `"personal"` (spec §07/05 §13); `context_names` returns distinct IDs.
9. **Tags + description round-trip** — `Vec<String>` + `Option<String>` survive the rkyv round-trip.

**Total: 9 tests.**

## 8. Estimated commit shape

One commit on `feature/brain-metadata`:

> `feat(brain-metadata): agents + contexts tables (sub-task 3.3)`

Body:
- The 4 tables and their value types.
- Composite key approach (tuple Key from redb v4).
- Reserved name prefix and default context constants.
- Test count.

Files touched:
- `crates/brain-metadata/src/tables/agent.rs` (new, ~220 lines incl. tests)
- `crates/brain-metadata/src/tables/context.rs` (new, ~360 lines incl. tests — three tables share fixtures)
- `crates/brain-metadata/src/tables/mod.rs` (add two `pub mod`)

Verify gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p brain-metadata`, `./scripts/check-skills.sh`.

---

PLAN READY: see `.claude/plans/phase-03-task-03.md` — confirm to proceed.
