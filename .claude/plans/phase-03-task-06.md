# Phase 3 — Task 3.6: Text blob storage

**Classification:** trivial. One key-value table, no custom value type (redb's built-in `&[u8]` works), no helpers needed at this layer. The interesting parts (UTF-8 validation, size limit, zero-on-hard-forget, same-transaction coupling with `memories`) all live above the storage layer per the spec.

**Spec:** `spec/07_metadata_graph/07_text_storage.md` (full — read end-to-end before writing the plan). Cross-checked the phase-doc's "Optional compression per spec" line — the spec **does not mention compression** anywhere in §07/07. Phase-doc text overshoots; we ignore it.

## 1. Scope

In:

- `crates/brain-metadata/src/tables/text.rs` (new):
  - `TEXTS_TABLE: TableDefinition<'static, [u8; 16], &'static [u8]>` — keyed by `MemoryId::to_be_bytes()`, valued by raw UTF-8 bytes. Uses redb's built-in `&[u8]` `Value` impl — no rkyv wrapper.
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod text;`.

Out (deferred):

- **UTF-8 validation** (spec §5). Wire layer (Phase 4).
- **`max_text_bytes` enforcement** (spec §4, §7). Wire layer.
- **`zero_and_remove` / hard-forget secure-erase** (spec §9). Phase 8 hard-forget worker — the secure-erase story needs `FALLOC_FL_PUNCH_HOLE` on the file underlying redb to actually evict pages, which is below redb's API. Shipping a zero-write helper here would be a misleading half-implementation.
- **Same-transaction coupling with `memories`** (spec §15). Composed in `MetadataDb` (3.10) — both tables opened inside one `begin_write()`.
- **Compression**, **dedup** (spec §6 rejects dedup for v1; compression isn't in the spec at all).
- **Snapshot integration** (spec §10). Snapshot layer is Phase 11+.
- **`store_text = false` mode** (spec §14). Wire-layer + config.
- **Bulk-export iteration helper** (spec §12). Client-side concern; redb's `iter()`/`range()` already supports it.

## 2. Spec quotes that bind the design

> **§1 (the table):**
> ```rust
> table: texts
> key: MemoryId
> value: Vec<u8>  // UTF-8 bytes
> ```
>
> **§5:** "Text is stored as UTF-8 bytes. The wire protocol carries UTF-8; the substrate stores it byte-for-byte." — the **substrate** stores byte-for-byte; only the **wire layer** validates UTF-8.
>
> **§4:** "Substrate enforces a max text size (default 1 MB; configurable). Larger texts are rejected at the wire-validation layer." — explicitly not the storage layer.
>
> **§8 (immutability):** "Once written, text is immutable. Brain doesn't support 'update the text of memory M'." — enforced by the application protocol, not by the table. redb's `insert` will overwrite if called twice; the storage layer doesn't fight this. The ENCODE-only write path in Phase 4 guarantees a given MemoryId is inserted exactly once.
>
> **§15 (atomic coupling):** demonstrates `MEMORIES` and `TEXTS` opened in one write transaction. Composition is `MetadataDb`'s job (3.10), not this sub-task's.

## 3. Design decisions

### 3.1 Use redb's built-in `&[u8]` value type, not a custom rkyv wrapper

Every prior 3.x table wrapped a rkyv struct because the value was structured (`MemoryMetadata`, `EdgeData`, etc.). Here the value is raw bytes — redb already supports `&[u8]` as a `Value`:

```rust
pub const TEXTS_TABLE: TableDefinition<'static, [u8; 16], &'static [u8]> =
    TableDefinition::new("texts");
```

Writing the same `Value` impl wrapper around `Vec<u8>` would (a) add a useless rkyv-encode/decode pass on every read, (b) impose the alignment-copy workaround we needed for `MemoryMetadata`/`EdgeData`/`IdempotencyEntry`, (c) make the table strictly slower for zero reason. Spec §1's `Vec<u8>` is the *logical* type; `&[u8]` is the right *physical* type for redb.

This means **no `request_hash`-style deviation**. The table shape matches §1 exactly.

### 3.2 No type-name versioning on this table

Versioning matters for tables whose `Value` is a struct that might gain fields. Raw bytes have no shape — there's nothing to evolve. The redb built-in `&[u8]` `type_name` is fine.

### 3.3 No public constants in this module

Spec §4's `max_text_bytes = 1_048_576` is a wire-layer concern. Putting it in `text.rs` would imply this layer enforces it; it doesn't. The constant lives wherever Phase 4 wire validation lives, alongside its other size limits.

### 3.4 No helper functions

`redb::Table` already exposes `insert`, `get`, `remove`, `iter`, `range` — every operation spec §07/07 needs. A `pub fn put_text(...)` / `pub fn get_text(...)` wrapper would add zero value and force callers to import named helpers instead of using the table handle directly (the same pattern 3.2/3.3 followed). 3.4's `link`/`unlink` exist because edges need cross-table mirroring; texts have no such cross-table logic at this layer.

### 3.5 Variable-length key encoding sanity check

`[u8; 16]` is fixed-width (redb knows this via the built-in `Value` impl), so the table layout is `fixed_key + variable_value` — redb's standard heterogeneous case. No special handling.

## 4. Files touched

- `crates/brain-metadata/src/tables/text.rs` (new) — ~80–100 LOC including tests.
- `crates/brain-metadata/src/tables/mod.rs` — one new `pub mod text;`.
- `docs/phases/phase-03-metadata.md` — flip 3.6 to ✅ with the actual scope (and note that "Optional compression per spec" was a phase-doc artifact, not a spec requirement). Post-implementation.

No edits to brain-core. No deviations to log (the deliberate not-implementing is per-spec, not against it).

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

1. **`insert_and_get_round_trips`** — write a small UTF-8 string's bytes, read them back, byte-for-byte equal.
2. **`missing_key_returns_none`** — `.get(&unknown_mid).unwrap().is_none()`.
3. **`overwrite_replaces_bytes`** — a second insert at the same key replaces the prior bytes. (We don't enforce immutability at this layer; the test documents that.)
4. **`empty_text_round_trips`** — zero-length `&[]` survives the round-trip. Edge case; redb handles empty values, but worth pinning so a future change doesn't regress it.
5. **`large_text_round_trips`** — 1 MB of bytes (`max_text_bytes` default) round-trips identically.
6. **`utf8_bytes_round_trip_byte_for_byte`** — bytes containing multi-byte UTF-8 sequences (`"héllo 🌍"` etc.) survive unchanged. The substrate doesn't validate or re-encode.
7. **`delete_removes_row`** — `.remove(&key)` returns `Some(...)` and a subsequent `.get` is `None`.
8. **`iterate_all_entries`** — insert three rows, iterate via `iter()`, assert the three are present (any order, since the natural sort is by raw key bytes).

## 6. Verification

Same harness as 3.5 — Linux dev container, since brain-storage's mmap/pwritev2 code precludes native-macOS compile:

```
docker run --rm \
  -v "$(pwd)":/workspaces/brain ... \
  brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-metadata"
```

Expected outcome: 55 brain-metadata tests pass (47 from prior + 8 new).

## 7. Commit

Branch: `feature/brain-metadata` (continuing). One commit per AUTONOMY §5:

```
feat(brain-metadata): text blob storage (sub-task 3.6)
```

Body summarises: `TEXTS_TABLE` using redb's built-in `&[u8]` value, deliberate non-decisions (UTF-8 / size / zero-on-hard-forget / compression all deferred), 8 new tests. **9 of 13 spec'd tables done** after this lands.

## 8. Done when

- [ ] `TEXTS_TABLE` defined, opens cleanly in a fresh redb.
- [ ] Insert / get / delete / overwrite / empty / 1-MB / multi-byte-UTF-8 round-trips tested.
- [ ] Iterate-all test confirms unordered-set semantics.
- [ ] 8 tests green; full brain-metadata suite green in the container.
- [ ] `docs/phases/phase-03-metadata.md` 3.6 flipped to ✅, with a one-line note that "Optional compression per spec" was a phase-doc artifact (not in the spec).

PLAN READY.
