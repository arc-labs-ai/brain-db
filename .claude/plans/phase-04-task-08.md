# Phase 4 — Task 4.8: Concurrency wrapper (`SharedHnsw`)

**Classification:** moderate-to-complex. The last Phase 4 sub-task. The architecturally interesting one: how readers and writers coexist on the HNSW. Spec anchor: `spec/06_ann_index/08_concurrency.md`.

## 1. Up-front decision needed

**Spec §06/08 §3 mandates lock-free reads via `ArcSwap<HnswState>`** — the writer mutates a "pending" buffer, periodically rebuilds the published state, and atomically swaps it in. This requires being able to **clone the HNSW graph cheaply**. `hnsw_rs::Hnsw<f32, DistCosine>` doesn't implement `Clone`, and even if it did, cloning a 1M-node graph (~150 MB) every 100 ms would not fit the spec's own timing budget.

The spec's model implicitly assumes a custom HNSW where clone-and-swap is cheap. With `hnsw_rs` as the workspace's mandated HNSW (CLAUDE.md §6), the spec's literal pattern isn't implementable.

**My recommendation:** ship v1 with `Arc<RwLock<HnswIndex<D>>>` (specifically `parking_lot::RwLock`, already a workspace dep). Reads are concurrent (RwLock allows multiple readers); writes serialize against readers. Not lock-free, but acceptable: under hnsw_rs's measured timings (1–3 ms per insert at 1M), the write lock is held briefly enough that read latency dips are tolerable. Log as SD-4.8-1.

If you want the lock-free path, we'd need to either (a) patch hnsw_rs to expose a clone-aware mutation model, or (b) replace `hnsw_rs` with a custom HNSW — both are significant Phase 11+ work that conflict with the "ship Phase 4 quickly" goal.

I'll proceed with the RwLock plan after you confirm.

## 2. Spec quotes that bind the design (independent of the lock choice)

> **§06/08 §1 (single-writer):** "Within a shard, only one task writes to the HNSW: the **writer task**. All inserts and removes go through this task." → 4.8 encodes this at the type level: the writer handle isn't `Clone`.
>
> **§06/08 §2:** "Lock around the entry point (every insert may update it). Lock around each node's edge list. Coordination to ensure consistency. The locking adds overhead and complexity; throughput doesn't scale linearly with writer count anyway." → multi-writer is explicitly out of scope.
>
> **§06/08 §3:** "Reads (searches) are concurrent and lock-free." → with `RwLock` we get **concurrent reads** (multiple readers OK) but **not lock-free** (writers block readers). The spec's lock-free reader requires ArcSwap; see §1 above.
>
> **§06/08 §13:** "Operations on different shards run on different executors (different OS threads, different cores). They're truly parallel." → Per-shard `SharedHnsw` is independent.

## 3. Design (RwLock variant — pending §1 user confirmation)

### 3.1 `SharedHnsw<D>` reader handle (cloneable)

```rust
#[derive(Clone)]
pub struct SharedHnsw<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
}

impl<const D: usize> SharedHnsw<D> {
    /// Reader-only methods. All take `&self`; concurrent calls from
    /// multiple clones of `SharedHnsw` proceed in parallel through
    /// `RwLock::read()`.
    pub fn search<F>(&self, query: &[f32; D], k: usize, ef: Option<usize>, filter: F)
        -> Vec<(MemoryId, f32)>
    where F: Fn(MemoryId) -> bool;
    pub fn search_active(&self, query: &[f32; D], k: usize, ef: Option<usize>)
        -> Vec<(MemoryId, f32)>;
    pub fn contains(&self, memory_id: MemoryId) -> bool;
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn tombstone_count(&self) -> usize;
    pub fn params(&self) -> IndexParams;
}
```

### 3.2 `Writer<D>` writer handle (NOT cloneable)

```rust
pub struct Writer<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
}
// No Clone impl. Mutation methods take &mut self.

impl<const D: usize> Writer<D> {
    pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError>;
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError>;
}
```

The combination of "not cloneable" + `&mut self` on writes encodes **single-writer-per-shard at the type level**. A second `Writer` can only come from `SharedHnsw::take_writer()`, which can only succeed once.

### 3.3 Constructor returns the pair

```rust
impl<const D: usize> SharedHnsw<D> {
    pub fn new(params: IndexParams) -> Result<(Self, Writer<D>), HnswError>;
    pub fn from_index(idx: HnswIndex<D>) -> (Self, Writer<D>);
}
```

Both reader and writer share the same `Arc<RwLock>` internally. The pair pattern means the caller can hand the reader to N threads and the writer to exactly one.

### 3.4 No pending buffer

Spec §06/08 §10's pending-buffer model exists to amortise ArcSwap publication cost. With RwLock, every write commits immediately under the write lock; no buffering needed. Read-after-write is automatic (spec §06/08 §11's `consistency=ReadAfterWrite` hint becomes a no-op).

### 3.5 No epoch protocol

Spec §06/08 §5 describes crossbeam-epoch for safe data reclamation in the ArcSwap model. With RwLock, the lock's own semantics handle reader-vs-writer races; epoch is unnecessary.

### 3.6 No `parking_lot` API leakage

Use `parking_lot::RwLock` internally but never expose `RwLockReadGuard` or related types in our public API. All public methods drop the guard before returning.

### 3.7 Snapshot operations

`save_snapshot` and `load_snapshot` from sub-task 4.5 stay on `HnswIndex`. The `SharedHnsw` wrapper exposes a `save_snapshot` that acquires the read lock (writes block briefly, readers don't) and delegates. `load_snapshot` is a static constructor that produces a new `(SharedHnsw, Writer)` pair.

### 3.8 SD-4.8-1: RwLock instead of ArcSwap

Logged in `docs/spec-deviations.md`:

- Spec §06/08 §3 mandates lock-free reads via `ArcSwap<HnswState>` and a pending buffer.
- Implementation uses `Arc<parking_lot::RwLock<HnswIndex<D>>>`: concurrent reads, exclusive writes.
- Reason: hnsw_rs's `Hnsw` doesn't expose a clone-aware mutation model; the spec's ArcSwap pattern would require deep-cloning the graph (~150 MB at 1M nodes) every 100 ms, which doesn't fit the spec's own timing budget.
- Reconciliation: a future Phase 11+ effort either (a) patches hnsw_rs, or (b) replaces it with a custom HNSW. Until then, the lock dip on writes is the v1 trade-off.

## 4. Files touched

- `crates/brain-index/src/shared.rs` (new) — `SharedHnsw<D>` + `Writer<D>` + threading tests. ~250 LOC.
- `crates/brain-index/Cargo.toml` — add `parking_lot.workspace = true` (already in workspace).
- `crates/brain-index/src/lib.rs` — `pub mod shared;` + re-export `SharedHnsw, Writer`.
- `docs/spec-deviations.md` — append SD-4.8-1.
- `docs/phases/phase-04-ann-index.md` — flip 4.8 ✅ + close phase exit checklist, tag `phase-4-complete`.

## 5. Tests (gated `#[cfg(test)]`)

In `shared.rs`:

1. **`new_returns_reader_and_writer_pair`** — `new()` returns `(SharedHnsw, Writer)`; reader is `Clone`, writer is not (compile-test by attempting `.clone()` on `Writer` — but since this is a negative compile test, instead document and skip).
2. **`single_threaded_insert_and_search`** — writer.insert(M); reader.search returns M. Baseline.
3. **`reader_clones_share_state`** — clone the reader twice; insert M via writer; both reader clones see the new memory.
4. **`reader_search_during_no_writer_doesnt_block`** — N reader clones search in parallel; no panic.
5. **`writer_during_readers_serialises`** — 1 writer + 8 readers in `std::thread::scope`; writer does 100 inserts, each reader does 1000 searches. No panic, no data race, final `len() == 100`. The big concurrent test the phase doc requires.
6. **`tombstone_visible_after_write`** — writer.mark_tombstoned(M); reader.is_tombstoned(M) == true on the next read. RwLock semantics: writes commit before unlock.
7. **`writer_serialises_among_self_mut_borrows`** — `writer.insert(M1)` then `writer.insert(M2)` from the same Writer (sequential calls, not threads). Both succeed; reader sees both.
8. **`shared_save_snapshot_roundtrip`** — `shared.save_snapshot` writes; `SharedHnsw::load_snapshot` reads; search results match pre-save.

Total: 7 tests (test #1's compile-test character means we skip it as a runtime test). brain-index test count: 66 → ~73.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-index"
```

Expected: brain-index 66 → ~73 tests. Workspace clippy clean.

## 7. Commit + phase-4 close-out

Branch: `feature/brain-index` (continuing). Two commits:

```
feat(brain-index): SharedHnsw concurrency wrapper (sub-task 4.8)
```

then:

```
chore: tag phase-4-complete
```

(Tag commit message body summarises Phase 4: 8 sub-tasks done, ~73 brain-index tests, two SD entries — 4.5-1 directory snapshot, 4.5-2 Box::leak HnswIo, 4.8-1 RwLock-not-ArcSwap. Plus the audit-followups SD-3.11-3.)

## 8. Done when

- [ ] `SharedHnsw<D>` + `Writer<D>` pair exists; reader cloneable, writer not.
- [ ] Snapshot save/load works through `SharedHnsw`.
- [ ] 8-reader / 1-writer concurrent test passes — no panic, no data race.
- [ ] SD-4.8-1 logged.
- [ ] Phase 4 exit checklist all ticked; `phase-4-complete` tag applied.

PLAN READY pending §1 confirmation. Say **go** with the RwLock approach, or pick a different concurrency model.
