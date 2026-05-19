# 04 — HNSW index

**Audience:** anyone tuning recall quality, debugging a slow
`RECALL`, persisting an index, or thinking about the cost of an
ENCODE.

**Goal:** by the end you should understand why Brain uses HNSW
specifically (not IVF, not flat scan), what the three knobs do,
how `MemoryId`s map onto the graph, what a tombstone means, and
when the index gets rebuilt.

This chapter assumes [03 — Arena and WAL](03-arena-and-wal.md):
the vectors HNSW indexes live in the arena. The index itself is
RAM-only; on every cold start it's reconstructed.

---

## What the index is for

When a `RECALL` arrives with a 384-dim query vector, the shard
needs to find the `k` nearest stored memories. The brute-force
answer is to compute the cosine similarity against every vector
in the arena. For a 1 M-memory shard that's 384 multiplies × 1 M
vectors per query — fast as a single-thread float-op but well
past the request budget once you also have filters, fan-out, and
embedding cost on top.

HNSW ("Hierarchical Navigable Small World") gives us approximate
nearest neighbours in `O(log N)` average-case rather than `O(N)`.
At a 1 M-vector shard, a `k=10` query touches a few hundred
vectors instead of a million. The trade-off is *recall*: HNSW
might miss a true neighbour if the graph isn't traversed
sufficiently. The three knobs all govern that trade-off.

The implementation is `brain-index` (`crates/brain-index/src/lib.rs`),
a closed leaf — vectors in, candidates out, no dependency on
arena or metadata. Cross-crate composition (rebuilding the HNSW
from arena slots + active-memory scans) lives at a layer above.
`#![forbid(unsafe_code)]` again (`crates/brain-index/src/lib.rs:28`);
the underlying `hnsw_rs` crate has its own internals.

---

## The mental model

```
         layer 3      ●─────────●─────────────●          (sparse top)
                      │         │             │
         layer 2   ●──●──●──●───●──●──●──●────●          (medium)
                   │  │  │  │   │  │  │  │    │
         layer 1   ●●●●●●●●●●●●●●●●●●●●●●●●●●●           (dense)
                                                  ─── etc.
         layer 0   ●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●● (all memories,
                                                  every node here)
```

HNSW is a multi-layer graph. Every memory is a node in the
bottom layer. Most nodes only exist there; a random subset is
also promoted into higher layers, and a smaller subset into
higher-higher, and so on. The number of layers a node lives on
follows an exponential distribution at insertion time. The top
few layers are sparse hubs; the bottom layer is dense and complete.

A search starts at the top layer's entry point, greedy-walks
toward the query, drops to the next layer, repeats, and finally
runs a beam search at the bottom layer for the `k` nearest. The
combined cost is roughly `O(log N)` graph traversals — fast,
because each step touches one node and its small fan-out of
edges.

Two implementation knobs control the graph's shape:

- **`M`** — maximum edges per node per non-bottom layer (the
  bottom layer uses `2M` by convention).
- **`ef_construction`** — search beam width *during inserts*. A
  larger beam finds better neighbours to link, producing a
  higher-quality graph at the cost of insert time.

And one knob controls query behaviour:

- **`ef_search`** — search beam width *during queries*. Larger =
  better recall, slower query.

Brain's defaults
(`crates/brain-index/src/params.rs:47`):

```rust
IndexParams {
    m: 16,
    ef_construction: 200,
    ef_search: 64,
    ef_search_max: 500,
}
```

Why these? See the next section.

---

## The three knobs

### `M = 16` — graph density

`M` is the max edges per node on layers above the bottom (the
bottom layer uses `2M = 32`). It's set at index creation and
isn't easily changed afterwards — changing `M` requires
rebuilding the whole graph.

- Higher `M` → more edges per node → better recall, slower build,
  more memory per node.
- Lower `M` → fewer edges → faster build, lower recall.

For 384-dim vectors, the original HNSW paper sweeps `M = 12..48`;
`16` is in the middle. At `M = 16` the steady-state memory is
roughly `16 edges × 4 bytes × log₁₆(N) layers` per node, plus
`32 × 4 = 128 bytes` for the bottom layer. A 1 M-node shard
costs ~150 MB of edge storage; 10 M costs ~1.5 GB.

The crate validates `M` against `4..=64`
(`crates/brain-index/src/params.rs:60`).

### `ef_construction = 200` — insert beam width

When inserting a new node, HNSW looks for the existing nodes
nearest to it (so it knows which ones to attach to). `ef_construction`
is how wide that search beam is. Wider = better neighbours found
= higher-quality graph, at the cost of insert time.

- `ef_construction = 100` → ~30 µs/insert, slightly lower-quality
  graph.
- `ef_construction = 200` → ~80 µs/insert, **good-quality graph**.
- `ef_construction = 500` → ~250 µs/insert, marginal quality
  gains.

200 is the standard recommendation and what we ship. Range
`50..=500`
(`crates/brain-index/src/params.rs:63`).

### `ef_search = 64` — query beam width

The most interesting knob, because it's the only one a client
can override per query. Larger beam = better recall, slower
query. On a 1 M-vector index, the trade-off looks like:

| `ef_search` | recall@10 | typical latency |
|---|---|---|
| 16 | ~85 % | ~0.5 ms |
| 32 | ~92 % | ~1 ms |
| 64 | ~96 % | ~2 ms |
| 128 | ~98 % | ~4 ms |
| 256 | ~99 % | ~8 ms |

(Numbers are for a 1 M-vector shard; smaller indexes give higher
recall at the same `ef_search`.)

The relationship to `k`: HNSW can only return `min(k, ef_search)`
results. The query path therefore clamps `ef_search` to `max(k,
default_ef_search)` so a request for `k = 100` doesn't silently
get capped to 64.

Range `10..=500`
(`crates/brain-index/src/params.rs:68`). The per-query override
is itself clamped to `[k, ef_search_max]` — see the search
implementation below.

### `ef_search_max = 500` — the hard ceiling

A per-query `ef_search` override is bounded by `ef_search_max`.
This is what prevents an agent from accidentally (or maliciously)
asking for `ef_search = 100 000` and tying up an executor. The
clamp is in `IndexParams::validate`
(`crates/brain-index/src/params.rs:71`): `ef_search_max` must be
≥ `ef_search`.

### `MAX_LAYER = 16`

The graph's maximum layer count is set at construction
(`crates/brain-index/src/params.rs:17`). HNSW theory says `max_layer
≥ log_M(N)`; at `M = 16` and the spec's 10 M per-shard ceiling,
`log₁₆(10 M) ≈ 5.6`, so `16` is a comfortable upper bound. The
underlying `hnsw_rs` crate uses the same default.

---

## `MemoryId ↔ internal_id` mapping

`hnsw_rs` indexes nodes by a `usize`. Brain hands out `MemoryId`s
(16 bytes). The boundary is `IdMap`
(`crates/brain-index/src/idmap.rs:30`):

```rust
pub struct IdMap {
    forward: HashMap<[u8; 16], u32>,
    reverse: HashMap<u32, [u8; 16]>,
    next_id: u32,
}
```

Two design notes:

- **Internal id is `u32`, not `usize`.** Saves ~80 MB at the
  spec's 10 M-memory-per-shard ceiling
  (`crates/brain-index/src/idmap.rs:9`). The `u32` is cast to
  `usize` at the `hnsw_rs` API boundary; overflow at `u32::MAX`
  returns `IdMapError::Exhausted` and the insert fails.
- **No atomic counter.** Inserts are single-writer-per-shard,
  enforced by `&mut self` (`crates/brain-index/src/idmap.rs:89`).
  No need for `AtomicU32::fetch_add` — that's a multi-writer
  pattern Brain doesn't have.

Inserting the same `MemoryId` twice is a caller bug — the index
returns `HnswError::DuplicateMemoryId` without burning an
internal id
(`crates/brain-index/src/idmap.rs:89`). This is one of the
reasons `MemoryId`s have a `slot_version`: even after `FORGET +
RECLAIM` + a fresh `ENCODE` reuses the same slot, the
`MemoryId.version()` differs, and the id map treats them as
distinct.

Removal from the id map isn't part of `mark_tombstoned` — see the
tombstone discussion below.

---

## Insert

The insert path is one method
(`crates/brain-index/src/hnsw.rs:192`):

```rust
pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D])
        -> Result<(), HnswError>
{
    let internal_id = self.id_map.insert(memory_id)?;
    self.inner.insert_slice((vector.as_slice(), internal_id as usize));
    Ok(())
}
```

Three things to notice:

- **`&mut self`.** The borrow checker enforces single-writer per
  shard. `hnsw_rs` itself only requires `&self` (it has its own
  internal locking for a multi-writer mode we don't use), but
  the wrapper tightens this at the type level — same pattern as
  `MetadataDb::write_txn` in [chapter 05](05-redb-metadata.md).
- **`&[f32; D]`, not `&[f32]`.** The const-generic `D` pins the
  vector dimension at compile time. Production pins `D = 384`
  (`crates/brain-index/src/params.rs:10`), matching BGE-small's
  output.
- **The vector must already be L2-normalised.** The distance
  metric is `DistCosine`
  (`crates/brain-index/src/hnsw.rs:38`); on normalised vectors,
  cosine similarity reduces to a dot product, which we want for
  SIMD performance. The embedder guarantees normalisation; the
  HNSW layer doesn't re-check it on the hot path.

An insert at 1 M-vector scale costs roughly **1–3 ms**
(walking the graph at `ef_construction = 200`, allocating edge
buckets). This is the dominant cost in a substrate ENCODE after
the embedding (chapter 06).

---

## Search

The search method
(`crates/brain-index/src/hnsw.rs:224`):

```rust
pub fn search<F>(
    &self,
    query: &[f32; D],
    k: usize,
    ef: Option<usize>,
    filter: F,
) -> Vec<(MemoryId, f32)>
    where F: Fn(MemoryId) -> bool
```

Returns `(MemoryId, similarity)` pairs, **sorted descending by
similarity** (best match first). Similarity, not distance — the
crate flips `1.0 - distance` so callers see numbers in the
intuitive direction.

Three properties worth knowing:

### Tombstone filter is implicit

Tombstoned memories are *always* excluded
(`crates/brain-index/src/hnsw.rs:257`):

```rust
// Implicit tombstone filter (spec §06/05 §2).
if self.tombstones.is_set(internal_id) { continue; }
```

…regardless of what the caller's `filter` returns. There is no
way to ask "give me the nearest neighbours including the
forgotten ones." If a caller needs that, they need a different
path entirely — and the design isn't sure why they would.

### `ef` is clamped

The `ef` argument lets a caller override the per-query search
width. The clamp is `[k, params.ef_search_max]`:

- `None` → uses `params.ef_search` (default 64).
- `Some(v)` → clamped to at least `k` (so HNSW can return `k`
  results) and at most `ef_search_max` (so a wild value doesn't
  tie up the executor).

### Over-fetch + bailout retry

When the caller passes a `filter` (e.g. "agent X only"), HNSW
might return `k * OVER_FACTOR` candidates and have most of them
filtered out. The implementation does an over-fetch + bailout
loop
(`crates/brain-index/src/hnsw.rs:50`):

```
initial_fetch = k * OVER_FACTOR    (default 2)
loop:
    candidates = hnsw_rs.search(query, initial_fetch, ef)
    survivors  = candidates.filter(tombstones + caller_filter)
    if survivors.len() >= k: return top-k
    if initial_fetch < OVER_FACTOR_CAP: initial_fetch *= 2
    elif ef < ef_search_max: ef *= 2
    else: return whatever survived (fewer than k)
```

This is what handles a filter that rejects 90 % of candidates
without forcing the caller to know that. For unfiltered queries
(`filter` returns `true` for everything), the first iteration is
always enough and the retry never fires.

### Search latency budget

At `ef_search = 64`, a `RECALL` of `k = 10` is `≈ 1-2 ms` on a
1 M-vector shard — well under the embedding cost (chapter 06).
Filter-heavy queries with bailout retries take longer; the worst
case is bounded by `ef_search_max` (default 500) and the index
size.

---

## Tombstones

`FORGET` doesn't remove a node from the HNSW graph. Doing so
would require restructuring the graph's edges around the hole,
which is exactly the kind of thing HNSW is bad at. Instead, the
node is **tombstoned** — marked dead and skipped on every
subsequent search.

The tombstone state is a `Vec<u64>` bitmap indexed by
`internal_id`
(`crates/brain-index/src/tombstones.rs`). `mark_tombstoned`
(`crates/brain-index/src/hnsw.rs:333`) sets one bit. Cost: ~1 ns
per tombstone.

What this means in practice:

- A tombstoned node still uses its arena slot and its place in
  the graph. The graph's recall quality is unaffected for a
  while.
- The graph slowly degrades as the tombstoned fraction grows —
  searches walk through dead nodes and reject them, costing
  walks without contributing results. The over-fetch retry above
  compensates for this up to a point.
- Once the tombstoned ratio crosses a threshold (default ~20 %),
  the **HNSW maintenance worker** triggers a full rebuild that
  drops the tombstones (chapter 07).

`tombstone_count()` is O(1)
(`crates/brain-index/src/hnsw.rs:357`); the worker reads it to
decide when to rebuild.

`is_tombstoned()` is fail-soft — an unknown `MemoryId` returns
`false`, not an error. Query paths shouldn't fault on stale ids
(`crates/brain-index/src/hnsw.rs:347`).

---

## Persistence

The HNSW graph isn't durable by itself; the WAL is what makes the
*operations* durable, and the arena is what makes the *vectors*
durable. The HNSW index is reconstructible from those two — so
why persist it at all?

To avoid an expensive rebuild on every boot. A 1 M-vector index
takes ~30 s to rebuild from scratch
(see [Rebuild](#rebuild) below); persisting it means a warm boot
is sub-second.

A snapshot is a directory with three files at one basename
(`crates/brain-index/src/persistence.rs:6`):

```
<basename>.hnsw.graph   — hnsw_rs graph dump
<basename>.hnsw.data    — hnsw_rs data dump (vector store)
<basename>.brain        — our wrapper (id map + tombstones + header)
```

`.brain` is written **last**. Its presence is the marker for
"this snapshot is complete" — a partially-written snapshot is
detected by the wrapper file's absence and ignored on load.

The `.brain` header layout
(`crates/brain-index/src/persistence.rs:17`):

```
offset  size   field
  0      4    magic = "BHN0"
  4      4    format_version (u32 LE)
  8     16    shard_uuid
 24      8    taken_at_lsn
 32      8    graph_node_count
 40      4    m
 44      4    ef_construction
 48      4    ef_search
 52      4    ef_search_max
 56      4    vector_dim
 60      4    header_crc32c (CRC32C over bytes 0..60)
```

…followed by the id map (forward entries + next-id counter), the
tombstone bitmap, and an 8-byte BLAKE3-truncated footer covering
the whole file. Three integrity checks: magic, header CRC, full
file BLAKE3.

Two important header fields:

- **`shard_uuid`** — must match the arena's `shard_uuid` from
  [chapter 03](03-arena-and-wal.md). A snapshot from one shard
  must not load into another.
- **`taken_at_lsn`** — the WAL LSN at the moment the snapshot
  was taken. After loading, the shard replays WAL records past
  this LSN so the in-RAM HNSW reflects every committed write.

`HnswIndex::save_snapshot`
(`crates/brain-index/src/hnsw.rs:425`) writes all three files in
order. `HnswIndex::load_snapshot`
(`crates/brain-index/src/hnsw.rs:504`) parses the `.brain` file,
hands the two `hnsw_rs` files to its load API, and rebuilds the
id map + tombstone bitmap from the parsed body.

The snapshot worker (chapter 07) drives this on a timer. Its
output is what the `taken_at_lsn` recovery path uses.

---

## Concurrency: `SharedHnsw`

A shard's request handlers all want to *read* the same HNSW
index; one writer task wants to *modify* it. The `SharedHnsw`
wrapper makes that safe
(`crates/brain-index/src/shared.rs:43`):

```rust
#[derive(Clone)]
pub struct SharedHnsw<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
}

pub struct Writer<const D: usize> {
    inner: Arc<RwLock<HnswIndex<D>>>,
}
```

The split is by type, not just convention:

- **`SharedHnsw`** is `Clone`; every method takes `&self`.
  Readers clone it freely. Multiple concurrent searches just
  acquire the read side of the lock.
- **`Writer`** is **not** `Clone`. Mutation methods take
  `&mut self`. Constructed exactly once via `SharedHnsw::new`
  (`crates/brain-index/src/shared.rs:59`). The type system
  enforces single-writer per shard.

This is the same pattern as `MetadataDb::write_txn(&mut self)`
in [chapter 05](05-redb-metadata.md). The discipline is
*structural* — no mutex hygiene, no runtime check.

### Why `RwLock`, not `ArcSwap`

You'd expect the truly lock-free pattern: `ArcSwap<HnswState>`,
build a new state, swap it in, readers see the change atomically.
That's what we considered first
(`crates/brain-index/src/shared.rs:16`).

The blocker is `hnsw_rs::Hnsw` doesn't implement `Clone`. To
`ArcSwap` you'd need to deep-clone the graph on every insert
window, which at 1 M nodes is ~150 MB and several seconds — far
past any reasonable flush cadence. So v1 ships with
`parking_lot::RwLock` instead: concurrent readers, exclusive
writes. Inserts briefly block readers (~1–3 ms at 1 M); for a
shard running at hundreds of requests/sec, that's an
imperceptible bump.

The deviation is tracked. A future replacement either patches
`hnsw_rs` to expose a clone-aware mutation model or replaces it
with a custom HNSW altogether.

### The atomic swap

When the maintenance worker rebuilds the index (next section),
it doesn't insert into the live one — it builds a fresh
`HnswIndex` off to the side, then calls `SharedHnsw::swap()`
(`crates/brain-index/src/shared.rs:184`):

```rust
pub fn swap(&self, new: HnswIndex<D>) {
    let mut guard = self.inner.write();
    *guard = new;
}
```

In-flight readers finish on the old index (because they hold an
`Arc` to the read guard, conceptually). New reads see the new
index. The write-lock acquisition is microsecond-scale and only
happens once per rebuild.

---

## Rebuild

The HNSW index degrades over time. New nodes accumulate, deleted
nodes accumulate tombstones, the topology drifts. Periodic
rebuilds reset the structure.

`HnswIndex::rebuild`
(`crates/brain-index/src/rebuild.rs:59`) is the entry:

```rust
pub fn rebuild_impl<const D: usize, I>(
    params: IndexParams,
    source: I,
) -> Result<(HnswIndex<D>, RebuildReport), HnswError>
where
    I: IntoIterator<Item = (MemoryId, [f32; D])>
```

Three properties:

- **Caller owns the filter.** The iterator yields only *active,
  valid* memories — tombstoned and corrupt-vector entries are
  filtered upstream
  (`crates/brain-index/src/rebuild.rs:10`). `rebuild` just
  iterates and inserts.
- **Sequential.** v1 inserts one at a time. A 1 M-node shard
  rebuilds in ~30 s on commodity hardware. Parallel insertion
  (`parallel_insert_slice` is in `hnsw_rs`) is a future
  optimisation
  (`crates/brain-index/src/rebuild.rs:18`).
- **Tombstones start empty.** The rebuilt graph contains only
  active memories — the "compaction" property. Subsequent
  forgets re-populate the tombstone bitmap.

The maintenance worker (chapter 07) is what drives this. The
flow is: read active memories from the arena, feed them to
`rebuild`, then `SharedHnsw::swap()` the new index in. Between
"start rebuild" and "swap in" the old index keeps serving reads;
writes that happen during that window go to the old index *and*
need to be replayed onto the new one. The catch-up phase isn't
in `brain-index` (it requires arena access); it's the worker's
job.

The returned `RebuildReport` has the memory count and wall-clock
duration
(`crates/brain-index/src/rebuild.rs:37`), which the worker
exposes as metrics.

---

## Cold start

There's no HNSW persistence on the write path. So how does a
fresh shard come up?

Two paths, picked by what's on disk:

- **Snapshot present.** The shard's startup
  (`spawn_shard`, [chapter 01](01-system-architecture.md))
  finds the `.brain` file plus its two `hnsw_rs` siblings,
  validates the header (magic, format version, shard UUID,
  CRC), calls `load_snapshot`, and then replays WAL records
  since `taken_at_lsn` so the in-RAM HNSW reflects every
  durable write. Fast — sub-second for typical snapshots.
- **No snapshot.** The shard reads all active memories from the
  arena and feeds them through `rebuild`. ~30 s for 1 M
  memories, scaling linearly. This is the path on a brand-new
  deployment, or after a snapshot is deliberately discarded.

In both paths, every shard rebuilds *in parallel* with the
others — each shard's HNSW is owned by its own Glommio executor,
so eight shards rebuild on eight threads with no coordination.

---

## Knowledge-layer indexes

Two additional HNSW indexes live alongside the substrate one when
the knowledge layer is active
(`crates/brain-index/src/entity_hnsw.rs`,
`crates/brain-index/src/statement_hnsw.rs`):

- **Entity HNSW** — embeddings of entity canonical names + alias
  pool. Used for entity resolution.
- **Statement HNSW** — embeddings of statement text. Used by the
  semantic retriever in the hybrid query path
  ([chapter 11](11-hybrid-retrieval-rrf.md)).

Both follow the same construction pattern as the substrate
index — `IndexParams`, const-generic dim, `IdMap`, tombstone
bitmap, `SharedHnsw` wrapper. They live in the same
`metadata.redb`'s containing directory and persist as
`entity.hnsw` / `statement.hnsw` files
(`crates/brain-storage/src/layout.rs:61`).

A substrate-only deployment never builds these. They're empty
files-on-disk unless a schema is declared. See
[09 — knowledge layer](09-knowledge-layer.md).

---

## Failure modes

**Snapshot magic mismatch.** Loading a `.brain` whose first 4
bytes aren't `BHN0`. `HnswError::SnapshotBadMagic`
(`crates/brain-index/src/hnsw.rs:101`). Shard falls back to a
full rebuild from the arena.

**Snapshot format version unsupported.** Either too new (binary
is old) or too old (post-format-bump). Same fallback — refuse
to load and rebuild.

**Snapshot shard UUID mismatch.** The snapshot was taken on a
different shard's data dir. Hard refusal; the snapshot is
worthless. Rebuild from arena.

**Snapshot header CRC mismatch.** Bit rot in the header. Refuse,
rebuild.

**Snapshot footer BLAKE3 mismatch.** Bit rot anywhere in the
body. Refuse, rebuild.

**Snapshot truncated.** `.brain` shorter than the header. Refuse,
rebuild.

**hnsw_rs load fails.** The two sibling files (`.hnsw.graph` /
`.hnsw.data`) are corrupt. `HnswError::HnswLoadFailed`. Rebuild.

**`u32::MAX` internal ids exhausted.** Defensive check; the spec
ceiling is 10 M per shard, so this is unreachable in practice.
ENCODE returns `HnswError::IdMapExhausted`
(`crates/brain-index/src/idmap.rs:96`).

**Duplicate `MemoryId` on insert.** Caller bug. Returns
`HnswError::DuplicateMemoryId` *without* burning an internal id.
A shard hitting this in production points at a bug in the
allocator or the recovery driver — same `MemoryId` minted twice.

**Search returns fewer than `k` results.** Expected when the
filter is restrictive or the index is small. Not an error; the
caller gets what's available
(`crates/brain-index/src/hnsw.rs:217`).

**Tombstone ratio creeps up.** Recall quality degrades slowly.
The maintenance worker rebuilds before this matters
([chapter 07](07-background-workers.md)).

---

## Configuration & tuning

Mostly `[ann]` in TOML. Defaults from `config/dev.toml`:

| Field | Default | Notes |
|---|---|---|
| `ann.m` | 16 | Set at index creation. Changing it requires a full rebuild. |
| `ann.ef_construction` | 200 | Set at index creation. Changing requires a full rebuild. |
| `ann.ef_search` | 64 | Default beam; per-query overridable. |
| `ann.ef_search_max` | 500 | Hard ceiling on per-query overrides. |

Operational rules of thumb:

- **Tune `ef_search` per query, not globally.** A search-quality
  knob belongs in the caller's hands — a UI showing a "more
  precise" toggle can flip it from 64 to 128 without affecting
  every other agent on the shard.
- **Don't change `M` lightly.** It's the foundational graph
  density. Operators changing it should plan for a full rebuild
  per shard, and the per-shard memory budget scales linearly.
- **`ef_construction = 200` is rarely worth changing.** Larger
  is a small quality gain at a real insert-latency cost. Smaller
  saves insert time but degrades the graph permanently.
- **Watch the tombstone ratio.** The maintenance worker handles
  it automatically, but if you've paused that worker, recall
  quality degrades.
- **Snapshots are operational hygiene.** A regular snapshot
  cadence keeps cold-boot time short. Without snapshots, a
  process restart costs a full rebuild per shard.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Crate root, exports | `crates/brain-index/src/lib.rs` |
| Parameters + validation | `crates/brain-index/src/params.rs` |
| `HnswIndex<D>` — insert, search, save/load | `crates/brain-index/src/hnsw.rs` |
| `IdMap` | `crates/brain-index/src/idmap.rs` |
| Tombstone bitmap | `crates/brain-index/src/tombstones.rs` |
| Snapshot file format | `crates/brain-index/src/persistence.rs` |
| Rebuild | `crates/brain-index/src/rebuild.rs` |
| `SharedHnsw` + `Writer` | `crates/brain-index/src/shared.rs` |
| Entity HNSW | `crates/brain-index/src/entity_hnsw.rs` |
| Statement HNSW | `crates/brain-index/src/statement_hnsw.rs` |
| Snapshot file paths (`entity.hnsw`, `statement.hnsw`) | `crates/brain-storage/src/layout.rs` |

---

## Further reading

- [03 — Arena and WAL](03-arena-and-wal.md) for what feeds the
  index on cold start.
- [05 — redb metadata](05-redb-metadata.md) for how the
  maintenance worker finds active memories to rebuild from
  (range scans over `MEMORIES_TABLE`).
- [06 — Embedding pipeline](06-embedding-pipeline.md) for where
  the vectors come from.
- [07 — Background workers](07-background-workers.md) for the
  HNSW maintenance worker and the snapshot worker.
- [11 — Hybrid retrieval (RRF)](11-hybrid-retrieval-rrf.md) for
  how the entity and statement HNSWs feed the knowledge-layer
  semantic retriever.
