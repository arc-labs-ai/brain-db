# Phase 4 — Task 4.5: Persistence (snapshot / load)

**Classification:** moderate. Largest sub-task in Phase 4 by LOC. Three interlocking pieces: a wrapper file format, hnsw_rs's two-file serialization, and the integration that ties them together. Spec anchor: `spec/06_ann_index/06_persistence.md` §5.

## 1. Scope

In:

- `crates/brain-index/src/persistence.rs` (new) — `Snapshot` module: write/parse the BHN0-wrapper file, header validation, BLAKE3 footer, format-version handling. ~300 LOC + tests.
- `crates/brain-index/src/hnsw.rs` — public `HnswIndex::save_snapshot` and `HnswIndex::load_snapshot`. ~60 LOC.
- `crates/brain-index/Cargo.toml` — add `blake3.workspace = true`, `crc32c.workspace = true` deps (already workspace-declared, just enable).
- `crates/brain-index/src/lib.rs` — `pub mod persistence;` + re-exports.
- `docs/spec-deviations.md` — log SD-4.5-1 (snapshot is a directory of three files, not the single file spec §06/06 §5.1 describes).

Out (deferred):

- **Snapshot-vs-rebuild orchestration** (spec §06/06 §8 — "if snapshot is corrupted, fall back to rebuild") — recovery driver concern; Phase 8 worker composes `load_snapshot` with a try/catch and falls back to `rebuild` from 4.6.
- **mmap-based load** (hnsw_rs's `ReloadOptions::set_mmap`) — v1 4.5 uses the in-memory load path. Spec §06/06 doesn't require mmap.
- **Snapshot retention / pruning** — operator concern; one snapshot at a time per shard for now.
- **Cross-version migration** (spec §06/06 §10) — when this crate bumps `FORMAT_VERSION`, decide load behaviour. v1 ships with version 1; older versions don't exist yet.

## 2. Spec quotes that bind the design

> **§06/06 §1:** "The HNSW index is **not persisted** as a primary on-disk structure. It's rebuilt at startup from the arena and metadata." → Snapshot is an *optional* fast-restart artifact, not the source of truth.
>
> **§06/06 §5.1 (the format):**
> ```
> [header: 64 bytes]
>   magic: "BHN0"  (Brain HNSW v0)
>   format_version: u32
>   shard_uuid: [u8; 16]
>   taken_at_lsn: u64
>   graph_size: u64
>   parameters: { M, ef_construction }
>   header_crc32c: u32
> [graph data: serialized via hnsw_rs's built-in serialization]
> [id_map data: serialized HashMaps]
> [footer: 8 bytes — full-file BLAKE3 hash truncated to u64]
> ```
>
> **§06/06 §5.3:** "If the snapshot is corrupted (CRC fails, deserialize errors), the substrate falls back to full rebuild." → 4.5 returns typed errors; the recovery driver decides.
>
> **§06/06 §5.3:** "If the snapshot is older than the metadata (some checkpointing failure), the substrate detects this via LSN comparison and rebuilds rather than using a stale snapshot." → 4.5 exposes `taken_at_lsn` to the caller; the LSN-staleness comparison happens upstream.

## 3. Design decisions

### 3.1 SD-4.5-1: snapshot is a **directory** of three files, not a single file

Spec §06/06 §5.1 describes the snapshot as a single file with embedded sections. **hnsw_rs's `Hnsw::file_dump(path, basename)` writes two separate files** (`<basename>.hnsw.graph` and `<basename>.hnsw.data`) and provides no Cursor/Write interface for in-memory serialization. To honour the spec literally we'd have to dump to a temp dir, read both files into memory, and concatenate into one wrapper file — extra I/O, extra disk, complicated atomic write.

**v1 decision:** the snapshot is a directory containing three files at the same `basename`:

```
<dir>/<basename>.hnsw.graph     <- hnsw_rs graph (written by Hnsw::file_dump)
<dir>/<basename>.hnsw.data      <- hnsw_rs data
<dir>/<basename>.brain          <- our wrapper: header + id_map + tombstones + footer
```

The `.brain` file carries the BHN0 magic + header CRC + BLAKE3 footer. The two `.hnsw.*` files are treated as opaque blobs from our perspective; their integrity is checked transitively via the BLAKE3 footer **over the .brain file only** — hnsw_rs's own format has internal validation.

SD-4.5-1 logged. Reconciliation: raise a spec PR amending §06/06 §5.1 to describe the directory layout.

### 3.2 `.brain` file layout

```
offset  size  field
------  ----  ------
   0    4     magic "BHN0"
   4    4     format_version: u32 LE  (= 1)
   8    16    shard_uuid: [u8; 16]
  24    8     taken_at_lsn: u64 LE
  32    8     graph_node_count: u64 LE  (= IdMap.len at save time)
  40    4     m: u32 LE
  44    4     ef_construction: u32 LE
  48    4     ef_search: u32 LE
  52    4     ef_search_max: u32 LE
  56    4     vector_dim: u32 LE  (= D — pinned at load time to prevent loading a mismatched-dim snapshot)
  60    4     header_crc32c: u32 LE  (CRC32C over bytes [0..60])
  ────────────────  64-byte header ends ────────────────
  64    4     id_map_count: u32 LE
  68    n×20 id_map entries: [u8; 16] memory_id + u32 internal_id
  ...   4     next_internal_id: u32 LE
  ...   8     tombstone_word_count: u64 LE
  ...   m×8  tombstone bitmap: u64 words
  ...   4     tombstone_set_count: u32 LE (the running counter from TombstoneBitmap)
  ...   8     footer: BLAKE3 hash of everything before, truncated to u64 LE
```

Key choices:

- **Forward id_map only, reverse rebuilt at load.** Saves ~20 bytes/entry; reverse is computable in O(N) at load time.
- **`vector_dim` (D) in the header.** Prevents loading a 384-dim snapshot into a `HnswIndex<128>` — catches a misuse before hnsw_rs's deserialiser hits it.
- **Tombstone count stored explicitly** so we can verify the loaded bitmap's running counter without re-summing all bits.
- **Header CRC covers bytes [0..60]** (everything before the CRC field itself). Same pattern as brain-storage's arena header.

### 3.3 Atomic write via per-file rename

For each of the three files, write to `<file>.tmp`, fsync, rename. `fsync` on the directory at the end so the directory entries are durable. Standard pattern; matches brain-storage's segment write.

Partial-snapshot tolerance: spec §06/06 §5.3 says corruption falls back to rebuild. We don't need atomic-all-three (which would require renaming a whole directory, more complex). If `.brain` is present and validates, but one of the `.hnsw.*` files is missing or corrupt, hnsw_rs's loader fails → `HnswError::HnswLoadFailed` → caller falls back to rebuild. The contract: the `.brain` file is the canonical "snapshot is intended to exist" marker. Write it **last** of the three.

### 3.4 BLAKE3 footer scope: `.brain` file only

The footer hashes the `.brain` file's bytes from offset 0 through the last byte before the footer (so everything except the 8-byte footer itself). This proves the wrapper is intact; the `.hnsw.*` files are validated by hnsw_rs's own internal format.

For end-to-end integrity at v2 we could checksum the `.hnsw.*` blobs into the `.brain` header. v1 keeps it simpler.

### 3.5 `HnswError` extensions

```rust
#[error("I/O error during snapshot: {0}")]
SnapshotIo(#[from] std::io::Error),

#[error("snapshot magic mismatch: expected BHN0, got {0:?}")]
SnapshotBadMagic([u8; 4]),

#[error("snapshot format_version {0} not supported (this binary supports {})", FORMAT_VERSION)]
SnapshotUnsupportedVersion(u32),

#[error("snapshot shard_uuid mismatch: expected {expected:?}, got {got:?}")]
SnapshotShardMismatch { expected: [u8; 16], got: [u8; 16] },

#[error("snapshot vector dim mismatch: expected {expected}, got {got}")]
SnapshotDimMismatch { expected: usize, got: u32 },

#[error("snapshot header CRC mismatch: expected {expected:08x}, got {got:08x}")]
SnapshotBadHeaderCrc { expected: u32, got: u32 },

#[error("snapshot BLAKE3 footer mismatch: file corrupted")]
SnapshotBadFooter,

#[error("snapshot truncated: expected at least {expected} bytes, got {got}")]
SnapshotTruncated { expected: usize, got: usize },

#[error("hnsw_rs load failed: {0}")]
HnswLoadFailed(String),
```

Eight new variants. All map cleanly to spec §06/06 §5.3's "falls back to rebuild" branch — the caller distinguishes "snapshot bad → rebuild" from "transient I/O → retry" by matching the variant.

### 3.6 Public API on `HnswIndex`

```rust
impl<const D: usize> HnswIndex<D> {
    /// Save this index as a snapshot in `dir` under `basename`. Writes
    /// three files: `<basename>.hnsw.graph`, `<basename>.hnsw.data`,
    /// `<basename>.brain`. `taken_at_lsn` is opaque to brain-index;
    /// the caller (Phase 8 worker) reads it back at load and compares
    /// against the metadata store's durable_lsn.
    pub fn save_snapshot(
        &self,
        dir: &Path,
        basename: &str,
        taken_at_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<(), HnswError>;

    /// Load an index from a snapshot. Verifies magic, version,
    /// shard_uuid match, header CRC, BLAKE3 footer; rebuilds the id_map
    /// reverse direction from the forward direction.
    ///
    /// Returns the `taken_at_lsn` recorded in the header alongside the
    /// loaded index — callers compare against the metadata store's
    /// durable_lsn to detect staleness (spec §06/06 §5.3).
    pub fn load_snapshot(
        dir: &Path,
        basename: &str,
        expected_shard_uuid: [u8; 16],
    ) -> Result<(Self, u64), HnswError>;
}
```

Two-tuple return for `load_snapshot` keeps the LSN obvious at the call site rather than burying it in an accessor.

### 3.7 Validate-then-construct

Load reads the entire `.brain` file into a Vec<u8>, validates the BLAKE3 footer, then parses. If validation fails, we never construct an `HnswIndex` — the type system guarantees a loaded index is well-formed. Same discipline as brain-metadata's `open_or_init_schema`.

### 3.8 No mmap, no streaming

The `.brain` file is small (~2.5 MB at 10M memories: id_map 20 bytes × 10M = 200 MB — wait, that's not small). Let me recompute. id_map at 10M entries × 20 bytes = 200 MB. Tombstone bitmap = 1.25 MB. Total `.brain` ≈ 200 MB. That's larger than I thought but still acceptable to read into memory at load time (the hnsw_rs graph is ~1.5 GB at 10M, so 200 MB is a small fraction of total RAM).

For v1 stick with full-read; spec §06/06 §11 mentions mmap-based "warm" rebuild as future work. Phase 11+.

## 4. Files touched

- `crates/brain-index/src/persistence.rs` (new) — ~300 LOC + tests.
- `crates/brain-index/src/hnsw.rs` — add `save_snapshot` / `load_snapshot` + 8 error variants. ~80 LOC.
- `crates/brain-index/src/lib.rs` — `pub mod persistence;`.
- `crates/brain-index/Cargo.toml` — `blake3.workspace = true`, `crc32c.workspace = true`.
- `docs/spec-deviations.md` — append SD-4.5-1.

## 5. Tests (gated `#[cfg(test)]`)

### persistence.rs (4 tests — pure format)

1. **`header_round_trip`** — encode a known header, decode back, fields match.
2. **`header_rejects_bad_magic`** — corrupt first 4 bytes; decoder returns `SnapshotBadMagic`.
3. **`header_rejects_bad_crc`** — flip a header byte; decoder returns `SnapshotBadHeaderCrc`.
4. **`footer_validates_full_blob`** — write file, flip a byte in the body, attempt verify, returns `SnapshotBadFooter`.

### hnsw.rs (10 integration tests)

5. **`round_trip_empty_index`** — save empty index, load, `len() == 0`.
6. **`round_trip_with_memories`** — insert 3, save, load, search returns same MemoryIds in same order.
7. **`round_trip_with_tombstones`** — insert 5, mark 2 tombstoned, save, load: tombstones survive (`is_tombstoned(mid) == true` for the two).
8. **`round_trip_preserves_next_id`** — insert mid(1)..mid(5), save, load, insert mid(6) → succeeds (next_id wasn't reset to 0).
9. **`load_returns_taken_at_lsn`** — save with `taken_at_lsn = 12345`, load returns `(_, 12345)`.
10. **`load_rejects_wrong_shard_uuid`** — save with uuid=A, load with expected=B → `SnapshotShardMismatch`.
11. **`load_rejects_wrong_dim`** — save `HnswIndex<4>`, load as `HnswIndex<8>` → `SnapshotDimMismatch`.
12. **`load_rejects_corrupted_brain_footer`** — flip a byte near the end of `.brain`, load fails with `SnapshotBadFooter`.
13. **`load_rejects_missing_hnsw_files`** — write `.brain` only (delete `.hnsw.graph`/`.hnsw.data` after save), load fails with `HnswLoadFailed`.
14. **`load_rejects_unsupported_version`** — write a snapshot, manually bump the format_version byte, load fails with `SnapshotUnsupportedVersion`.

Total: 14 tests. brain-index 41 → 55.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index 41 → 55 tests. Workspace clippy clean.

## 7. Commit

Branch: `feature/brain-index` (continuing). AUTONOMY §5:

```
feat(brain-index): snapshot persistence (sub-task 4.5)
```

Body summarises: directory-layout snapshot (SD-4.5-1: hnsw_rs's two-file output + our `.brain` wrapper), BHN0 header with CRC32C + format version + shard_uuid + LSN + vector_dim, BLAKE3 footer over `.brain`, 8 new error variants, atomic per-file rename writes, 14 new tests covering format + integration + integrity failures.

## 8. Done when

- [ ] `save_snapshot` writes three files atomically (per-file tempfile + rename).
- [ ] `load_snapshot` validates all eight failure paths (magic, version, shard, dim, header CRC, footer, hnsw load, truncated).
- [ ] Round-trip preserves: len, search results, tombstones, next_id.
- [ ] 14 tests green; clippy clean.
- [ ] SD-4.5-1 logged in `docs/spec-deviations.md`.

PLAN READY.
