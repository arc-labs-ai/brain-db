# Phase 3 — Task 3.4: Edge storage

**Classification:** moderate. Two more tables (bringing the count to 7 of 13), plus the first sub-task with cross-table consistency logic (LINK touches both `edges_out` and `edges_in`, and symmetric edges touch them again in the reverse direction). The composite-key pattern is the same one 3.3 used; the new wrinkle is the symmetric-edge mirroring.

**Spec:** `spec/07_metadata_graph/04_edge_storage.md` (full — both tables, value layout, LINK/UNLINK protocol, queries, limits, symmetric handling). Cross-checked `spec/02_data_model/06_edges.md` (edge kind catalog + symmetric flag).

## 1. Scope

In:

- `crates/brain-metadata/src/tables/edge.rs` (new):
  - `EDGES_OUT_TABLE: TableDefinition<([u8;16], u8, [u8;16]), EdgeData>`.
  - `EDGES_IN_TABLE: TableDefinition<([u8;16], u8, [u8;16]), EdgeData>`.
  - `EdgeData` (rkyv-derived: weight, origin, derived_by, created_at, annotation).
  - `link(out, in_, source, kind, target, data) -> Result<(), redb::StorageError>` — handles symmetric mirroring per spec §02/06.
  - `unlink(out, in_, source, kind, target) -> Result<bool, redb::StorageError>` — same.
  - `list_edges_from(out, source, kind: Option<EdgeKind>) -> Result<Vec<(MemoryId, EdgeData)>, _>` — range scan; `None` returns all kinds.
  - `list_edges_to(in_, target, kind: Option<EdgeKind>) -> Result<Vec<(MemoryId, EdgeData)>, _>` — same.
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod edge;`.

Out:

- **Edge count maintenance** on the `memories` table (spec §07/04 §5–§6: "update_count(... edges_out_count, +1)"). That's cross-table logic; the `MetadataDb` wrapper in 3.10 will compose `link()` with the count update. 3.4 owns the edge tables only.
- **Reclamation cascade** (§07/04 §13: when a memory is reclaimed, delete all its edges, batched if >10K). 3.7 / 3.10 / Phase 8 worker territory.
- **Soft/hard edge limits** (§07/04 §11). Phase 9 enforcement.
- **Auto-derived edge maintenance** (§07/04 §14). Workers (Phase 8).
- **Multi-hop graph queries** (§07/04 §16). Query planner (Phase 6).

## 2. Spec quotes that bind the design

> **§07/04 §2:** "Same data, two indexes. Forward queries use `edges_out`; reverse queries use `edges_in`."
>
> **§07/04 §3:** keys are `(source, kind, target)` for `edges_out` and `(target, kind, source)` for `edges_in`. Both encoded as little-endian concatenation.
>
> **§07/04 §4 (EdgeData):**
> ```rust
> struct EdgeData {
>     weight: f32,
>     origin: u8,           // Explicit / AutoDerived
>     derived_by: u8,       // which worker created it
>     created_at: u64,
>     annotation: Option<String>,
> }
> ```
>
> **§07/04 §12 (no multi-edges):** "A second LINK with the same key updates the existing edge's data rather than creating a duplicate." → redb's `insert` already does this.
>
> **§02/06 §2 (symmetric kinds):** `SimilarTo` and `Contradicts` are symmetric — brain-core's `EdgeKind::is_symmetric()` returns `true` for these two. "The substrate stores all edges directionally; symmetric kinds are stored both ways."

## 3. Design decisions

### 3.1 Composite key encoding

3-tuple `([u8; 16], u8, [u8; 16])` for both tables. redb v4's tuple `Key` impl should accept this (3.3 succeeded with 2-tuples and a mix of fixed-array + u64). If it doesn't, fall back to a flat `[u8; 33]` key with manual concatenation — same lexicographic order, slightly less ergonomic.

### 3.2 Symmetric edge mirroring

For symmetric kinds (`SimilarTo`, `Contradicts`), one logical edge from A to B requires **four** physical rows:

| Table | Key | Why |
|---|---|---|
| `edges_out` | `(A, kind, B)` | direct |
| `edges_in` | `(B, kind, A)` | reverse-index of direct |
| `edges_out` | `(B, kind, A)` | mirror for symmetric semantics |
| `edges_in` | `(A, kind, B)` | reverse-index of mirror |

For asymmetric kinds (the other 6), two rows: direct in `edges_out` + reverse-index in `edges_in`.

**Self-edge guard:** if `source == target`, skip the mirror — `(A, K, A)` is its own reverse. Without the guard, we'd insert `(A, K, A)` into both tables once, then try to insert it again (idempotent at the table level, but conceptually clearer to guard).

### 3.3 `EdgeData::origin` ↔ `brain-core::EdgeOrigin`

Mirrors the Phase 2 mapping from `brain_storage::wal::payload`:
- `0 = Explicit`
- `1 = AutoDerived`

Local `edge_origin_to_u8` / `edge_origin_from_u8` helpers. Duplicates noted (third occurrence of this mapping; first was Phase 2.2's WAL payload, second was the implicit "if a third caller appears, promote to brain-core" note from 3.2). **Third caller appearing → escalate to a brain-core helper in a follow-up.**

Likewise for `EdgeKind` ↔ `u8`. brain-core's `EdgeKind` already has `#[repr(u8)]` with explicit discriminants `0..=7`. Cast via `kind as u8`. Reverse via a local match.

### 3.4 `derived_by: u8` — what values?

Spec §07/04 §4 says "which worker created it; e.g., consolidation" but doesn't enumerate. Store as `u8`; document the v1 assignment:

```rust
pub mod derived_by {
    /// Client-asserted edge (origin = Explicit).
    pub const CLIENT: u8 = 0;
    /// Consolidation worker (spec §11/...).
    pub const CONSOLIDATION_WORKER: u8 = 1;
    /// Similarity worker (auto-derived SIMILAR_TO).
    pub const SIMILARITY_WORKER: u8 = 2;
    // 3..=255 reserved for future workers.
}
```

For symmetric mirror entries: `derived_by` is the same as the original (the symmetric mirror is bookkeeping, not a separate creator).

### 3.5 `EdgeData::annotation` — `Option<String>`

rkyv 0.7 supports both. Round-trip tested.

### 3.6 Helper API: pre-opened tables vs `&mut WriteTransaction`

Take pre-opened `&mut Table<...>` handles. redb's `open_table` returns an error if the table is already open in the same txn, so taking pre-opened handles avoids the conflict; the caller decides the open order.

Type signatures for `Table` and `ReadOnlyTable` in redb v4 are `Table<'a, K, V>` and `ReadOnlyTable<K, V>` (with the lifetime on the writable variant only). The helpers spell them out:

```rust
type OutKey = ([u8; 16], u8, [u8; 16]);
type InKey  = ([u8; 16], u8, [u8; 16]);

pub fn link<'a>(
    edges_out: &mut redb::Table<'a, OutKey, EdgeData>,
    edges_in: &mut redb::Table<'a, InKey, EdgeData>,
    source: MemoryId,
    kind: EdgeKind,
    target: MemoryId,
    data: &EdgeData,
) -> Result<(), redb::StorageError>;

pub fn unlink<'a>(
    edges_out: &mut redb::Table<'a, OutKey, EdgeData>,
    edges_in: &mut redb::Table<'a, InKey, EdgeData>,
    source: MemoryId,
    kind: EdgeKind,
    target: MemoryId,
) -> Result<bool, redb::StorageError>;  // true if at least one row was removed
```

Reads:

```rust
pub fn list_edges_from(
    edges_out: &redb::ReadOnlyTable<OutKey, EdgeData>,
    source: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(MemoryId, EdgeData)>, redb::StorageError>;

pub fn list_edges_to(
    edges_in: &redb::ReadOnlyTable<InKey, EdgeData>,
    target: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(MemoryId, EdgeData)>, redb::StorageError>;
```

The return type collects into `Vec` for ergonomics; spec §07/04 §10 estimates 8 edges/memory typical, so a vec allocation per call is fine. A streaming iterator variant is a follow-up if profiling shows a need.

### 3.7 Range bounds for "all kinds of a source"

For `kind: None`, range `(source, 0, [0; 16])..=(source, u8::MAX, [0xFF; 16])`. Since the key sorts lexicographically as `source bytes → kind byte → target bytes`, this captures everything with the given source.

For `kind: Some(k)`, range `(source, k, [0; 16])..=(source, k, [0xFF; 16])`.

`MemoryId::MIN` and `MemoryId::MAX` mentioned in the spec aren't pre-existing constants in brain-core; we use `[0; 16]` and `[0xFF; 16]` directly.

## 4. Architecture

### 4.1 Files

```
crates/brain-metadata/src/tables/
├── agent.rs
├── context.rs
├── edge.rs    (NEW)
├── memory.rs
└── mod.rs     (modify: pub mod edge;)
```

### 4.2 Public surface

```rust
// crates/brain-metadata/src/tables/edge.rs

pub type EdgeKey = ([u8; 16], u8, [u8; 16]);

pub const EDGES_OUT_TABLE: TableDefinition<'static, EdgeKey, EdgeData> =
    TableDefinition::new("edges_out");
pub const EDGES_IN_TABLE: TableDefinition<'static, EdgeKey, EdgeData> =
    TableDefinition::new("edges_in");

pub mod origin {
    pub const EXPLICIT: u8 = 0;
    pub const AUTO_DERIVED: u8 = 1;
}

pub mod derived_by {
    pub const CLIENT: u8 = 0;
    pub const CONSOLIDATION_WORKER: u8 = 1;
    pub const SIMILARITY_WORKER: u8 = 2;
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EdgeData {
    pub weight: f32,
    pub origin: u8,
    pub derived_by: u8,
    pub created_at_unix_nanos: u64,
    pub annotation: Option<String>,
}

impl EdgeData { pub fn new(...) -> Self; }

impl redb::Value for EdgeData { /* rkyv-backed, same pattern */ }

pub fn link(...) -> Result<(), redb::StorageError>;
pub fn unlink(...) -> Result<bool, redb::StorageError>;
pub fn list_edges_from(...) -> Result<Vec<(MemoryId, EdgeData)>, redb::StorageError>;
pub fn list_edges_to(...) -> Result<Vec<(MemoryId, EdgeData)>, redb::StorageError>;
```

### 4.3 `link` flow

```rust
pub fn link<'a>(
    edges_out: &mut Table<'a, EdgeKey, EdgeData>,
    edges_in: &mut Table<'a, EdgeKey, EdgeData>,
    source: MemoryId,
    kind: EdgeKind,
    target: MemoryId,
    data: &EdgeData,
) -> Result<(), redb::StorageError> {
    let kind_byte = kind as u8;
    let s_bytes = source.to_be_bytes();
    let t_bytes = target.to_be_bytes();

    let key_out = (s_bytes, kind_byte, t_bytes);
    let key_in  = (t_bytes, kind_byte, s_bytes);
    edges_out.insert(&key_out, data)?;
    edges_in.insert(&key_in, data)?;

    // Symmetric mirror — skip if self-edge.
    if kind.is_symmetric() && source != target {
        let key_out_rev = (t_bytes, kind_byte, s_bytes);
        let key_in_rev  = (s_bytes, kind_byte, t_bytes);
        edges_out.insert(&key_out_rev, data)?;
        edges_in.insert(&key_in_rev, data)?;
    }
    Ok(())
}
```

`MemoryId: PartialEq` already (Phase 1).

### 4.4 `unlink` flow

Mirror of `link`. Returns `true` if the direct edge was removed; partial removal (only the direct half exists, not the mirror) doesn't matter for the return — `true` means "the canonical (source, kind, target) row was found and removed."

### 4.5 Range-scan helpers

```rust
pub fn list_edges_from(
    edges_out: &ReadOnlyTable<EdgeKey, EdgeData>,
    source: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(MemoryId, EdgeData)>, redb::StorageError> {
    let s = source.to_be_bytes();
    let (kind_lo, kind_hi) = match kind {
        Some(k) => (k as u8, k as u8),
        None => (0u8, u8::MAX),
    };
    let lo = (s, kind_lo, [0u8; 16]);
    let hi = (s, kind_hi, [0xFFu8; 16]);
    let mut out = Vec::new();
    for entry in edges_out.range(lo..=hi)? {
        let (k, v) = entry?;
        let (_, _, t_bytes) = k.value();
        out.push((MemoryId::from_be_bytes(t_bytes), v.value()));
    }
    Ok(out)
}
```

`list_edges_to` is the same against `edges_in`, returning the *source* of each edge (since `edges_in` is keyed by `(target, kind, source)`).

## 5. Trade-offs

| Question | Choice | Why |
|---|---|---|
| 3-tuple vs flat `[u8; 33]` key | 3-tuple | Cleaner; redb v4 supports it (3.3 evidence). Fallback ready if needed. |
| Pre-opened tables vs `&mut WriteTransaction` | Pre-opened | Avoids the "already-open" conflict; caller controls order. |
| Vec return vs streaming iterator | Vec | 8 edges/memory typical; alloc is cheap. Streaming is a follow-up. |
| `EdgeOrigin` / `EdgeKind` mapping location | Local helpers | Third duplicate noted; promote to brain-core in a follow-up. |
| Self-edge symmetric handling | Skip the mirror | `(A, K, A)` is its own reverse; mirror would be a redundant insert. |
| `derived_by` enumeration | u8 constants in a module | Not in spec; document the v1 assignment for future workers. |
| `MemoryId::MIN`/`MAX` constants | Use `[0; 16]` and `[0xFF; 16]` directly | brain-core doesn't expose them; add later if needed. |

## 6. Risks

- **redb v4 tuple Key for 3-tuples.** Should work (3.3's 2-tuples did). If it doesn't compile, fall back to flat `[u8; 33]`.
- **`Table` lifetime ergonomics.** redb v4's `Table<'a, K, V>` lifetime can be fiddly. If the helper signatures don't elide cleanly, we'll inline (`pub fn link` takes the txn and opens tables internally), accepting the "can't have other writers open" constraint.
- **`Vec<(MemoryId, EdgeData)>` allocates two strings worth of clone per result** (EdgeData has `Option<String>`). For 8-result returns this is negligible; for any future bulk scan we'd switch to a streaming iterator.
- **f32 NaN in `weight`.** Possible but not tested. Spec §07/04 §15 says weight is in [0,1] or [-1,1] for negative kinds; the storage layer accepts any f32. No special handling.

## 7. Test plan

All tests in `edge.rs`'s `#[cfg(all(test, not(miri)))] mod tests`.

### EdgeData round-trip (2)

1. **Insert + get with non-empty annotation.** Annotation = `Some("...")`.
2. **Annotation `None` round-trip.**

### Asymmetric link/unlink (2)

3. **Caused edge: link writes both indexes.** After `link(A, Caused, B, data)`, both `edges_out` has `(A, Caused, B)` and `edges_in` has `(B, Caused, A)`.
4. **Unlink removes both.** After `unlink`, neither table has the row.

### Symmetric link/unlink (3)

5. **SimilarTo: link writes 4 rows.** `(A, SimilarTo, B)` and `(B, SimilarTo, A)` in both tables → 4 rows total.
6. **Unlink symmetric removes 4 rows.**
7. **Self-edge symmetric: only 2 rows** (not 4). `link(A, SimilarTo, A)` → `(A, SimilarTo, A)` in both tables, no mirror.

### Range queries (4)

8. **`list_edges_from(A, None)` returns all kinds for source A**, sorted by kind then target.
9. **`list_edges_from(A, Some(Caused))` returns only Caused edges from A.**
10. **`list_edges_to(B, None)` returns all kinds for target B.**
11. **`list_edges_to(B, Some(SimilarTo))` returns SimilarTo edges to B**, including the mirrored ones.

### Update-via-relink (1)

12. **Second link with same key overwrites EdgeData.** Spec §07/04 §12 idempotency.

**Total: 12 tests.**

## 8. Estimated commit shape

One commit on `feature/brain-metadata`:

> `feat(brain-metadata): edge storage with symmetric mirroring (sub-task 3.4)`

Body:
- Two tables (`edges_out`, `edges_in`) with 3-tuple composite keys.
- `EdgeData` (rkyv, with `Option<String>` annotation).
- `link` / `unlink` / `list_edges_from` / `list_edges_to` helpers.
- Symmetric edge handling (`is_symmetric()` from brain-core's EdgeKind).
- `origin` and `derived_by` byte-mapping constants.
- Test count.

Files touched:
- `crates/brain-metadata/src/tables/edge.rs` (new, ~440 lines incl. tests)
- `crates/brain-metadata/src/tables/mod.rs` (add `pub mod edge;`)

Verify gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p brain-metadata`, `./scripts/check-skills.sh`.

---

PLAN READY: see `.claude/plans/phase-03-task-04.md` — confirm to proceed.
