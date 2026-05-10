# 05.12 Open Questions

Storage-layer questions unresolved as of this spec version.

---

## OQ-ST-1: Vector compression (PQ, scalar quantization)

**Issue.** Vectors take 1.5 KB each. For very large shards, compression could meaningfully reduce storage and possibly improve cache hit rate (smaller vectors → more in cache).

**Options.**

a) **Stay f32.** Status quo. Simple, fast SIMD, no quality loss.

b) **f16 (half precision).** 2× compression, slight quality loss, requires SIMD support that's spotty.

c) **Scalar quantization (SQ8).** 4× compression. ~5% accuracy loss for retrieval. Used by FAISS and others.

d) **Product quantization (PQ).** 8-32× compression. More accuracy loss, higher implementation complexity.

**Recommendation.** Defer. v1 prioritizes simplicity; if a deployment hits storage walls, revisit. SQ8 is the most natural next step.

---

## OQ-ST-2: Non-blocking checkpoints

**Issue.** Current checkpoint procedure has a brief drain (10–50 ms). For latency-critical deployments, this is a noticeable hiccup.

**Options.**

a) **Status quo.** Brief drain.

b) **Non-blocking via snapshot.** Use the snapshot mechanism: reflink files at a point in time, work with the reflinked copies, no drain. The active files continue accepting writes during the checkpoint.

c) **Online checkpoint.** Use HNSW's online checkpoint protocol (capture state without stopping inserts). Complex; coupling to HNSW internals.

**Recommendation.** Add option (b) as a config flag in v1.1. The reflink-based approach is well-understood and adds complexity in one place.

---

## OQ-ST-3: Per-record sequence number gaps

**Issue.** WAL records have monotonic LSNs. Gaps (e.g., LSN 5 missing while 4 and 6 present) are treated as errors. Some recovery scenarios might benefit from gap-tolerant replay.

**Options.**

a) **Strict.** Status quo. Gap = corruption; refuse to start.

b) **Tolerant on operator request.** A `--allow-wal-gaps` flag for advanced operators who know what they're doing.

c) **Repair via replay from snapshot.** If a snapshot covers the gap, restore from the snapshot.

**Recommendation.** Stay strict (a). Gaps in append-only WAL should not happen; tolerating them silently could mask real bugs. Option (c) is the operator's normal recovery path.

---

## OQ-ST-4: Group-commit window adaptive sizing

**Issue.** The group-commit window is statically configured (default 100 µs). Under varying load, a fixed window may be suboptimal.

**Options.**

a) **Static.** Status quo. Operator tunes for typical load.

b) **Adaptive based on queue depth.** Wider window under low load (more chance for batches), narrower under high load (latency wins).

**Recommendation.** Defer. The static window is good enough for most workloads. Revisit if load profiling reveals scenarios where adaptation helps significantly.

---

## OQ-ST-5: Direct I/O for arena reads

**Issue.** Arena reads go through the page cache. For some workloads (huge arenas, low memory), bypassing the page cache might be better.

**Options.**

a) **mmap (status quo).** Page cache handles working set.

b) **O_DIRECT reads.** Manual buffer management, more deterministic latency, but loses kernel readahead.

c) **Hybrid.** Configurable per shard.

**Recommendation.** Stay with mmap. The page cache works well for our access patterns; manual buffer management would be a significant complexity increase for marginal benefit.

---

## OQ-ST-6: WAL compression

**Issue.** WAL records contain text and vectors that could be compressed. Especially the vector (1.5 KB of f32) may compress well in some cases.

**Options.**

a) **No compression.** Simplest.

b) **Per-record zstd.** Records over a threshold get compressed.

c) **Streaming compression on segment level.** Each segment is gzip/zstd-compressed.

**Recommendation.** Defer. Compression is a meaningful complexity increase; let's see what real workloads look like before committing. Streaming compression at segment level is the most attractive option if we go this way (handles long sequences efficiently).

---

## OQ-ST-7: Multi-shard WAL coalescing

**Issue.** Each shard has its own WAL with its own fsync. A node hosting many shards has many concurrent fsyncs. Could shards on the same node share a WAL device or fsync barrier?

**Options.**

a) **Per-shard WALs.** Status quo. Independent fsyncs.

b) **Shared WAL with shard tagging.** All shards on a node write to one WAL with a shard-id tag per record. Recovery filters per shard.

c) **Coalesced fsync.** Per-shard WALs but a single fsync covers all of them.

**Recommendation.** Stay per-shard (a). Coupling shards' durability is the wrong direction; we want isolation, not coupling. The fsync overhead is bounded by the device's IOPS, which is plenty for our targets.

---

## OQ-ST-8: Snapshot streaming to remote storage

**Issue.** Snapshots produce local files. For backup, the operator typically copies them to S3, GCS, etc. Could the substrate stream snapshots directly?

**Options.**

a) **Local-only.** Status quo. Operators run external backup tools.

b) **Direct S3 / S3-compatible.** The substrate uploads snapshot files to a configured S3 bucket.

c) **Pluggable backend.** A trait for snapshot destinations; operators configure their preferred backend.

**Recommendation.** Stay local-only. External backup tools (rclone, restic, native AWS CLI) are mature; the substrate doesn't need to compete. Document the integration patterns and let operators handle the upload.

---

## OQ-ST-9: WAL fsync coalescing across operations

**Issue.** Each operation that mutates state writes a WAL record. For workloads with many tiny mutations (salience updates dominating), per-record fsync overhead adds up.

**Options.**

a) **Group commit + record coalescing (status quo).** Multiple records → one fsync; salience updates coalesce within a record.

b) **More aggressive coalescing.** Multiple operations from the same agent within a window become a single record.

c) **Delayed fsync for non-critical records.** Salience updates and similar bookkeeping records aren't fsync'd individually; they piggyback on the next fsync.

**Recommendation.** (c) is interesting. Salience updates are not "durable" in the strict sense — losing them in a crash means the substrate loses some salience boost, which is annoying but not catastrophic. Treating them as deferred-durability could improve throughput. But the implementation must not delay them indefinitely; a max-delay is needed. Defer to v1.1.

---

*Continue to [`13_references.md`](13_references.md) for references.*
