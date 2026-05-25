# Phase 27 — Bottleneck fixes (post phase-26 audit)

> **Status:** in progress. Spawned from the post-phase-26 bottleneck audit. Each workstream below is an independent worktree-isolated agent run; the parent rebases them in the recommended order at the end.

## Audit findings being addressed

| # | Finding | Workstream |
|---|---|---|
| 1 | `Arc<Mutex<MetadataDb>>` serializes reads against reads (parking_lot mutex defeats redb's MVCC concurrency) | A |
| 2 | Sequential retriever fan-out in `brain-planner/src/hybrid/executor.rs:189` — additive latency where parallel would give `max()` | B |
| 3 | Per-phase WAL channel hop in `brain-ops/src/writer/submit.rs:577-675` — 8 await round-trips for a 6-phase encode | C |
| 4 | Triple-commit fsync amplification on schemaless `STATEMENT_CREATE` / `RELATION_CREATE` / `SCHEMA_REPLACE` | D |
| 7 | Hot-path allocs in `submit.rs` (acks.clone, cached.clone) and `encode.rs:115` (req.text.clone) | folded into C |

Skipped on purpose:
- #5 cross-encoder rerank on shard executor thread — deliberate design choice per CLAUDE.md anti-pattern "Don't introduce a thread pool for parallel work."
- #6 no embedding batching for ENCODE — needs new wire opcode (ENCODE_BATCH); separate phase.

## Workstream A — Mutex split

**Branch:** `feature/metadata-reader-split`
**Worktree:** `.claude/worktrees/metadata-readers`
**Blast radius:** workspace-wide (~71 lock sites in brain-ops, plus brain-planner, brain-workers, brain-extractors, brain-server).

### Goal

Replace `SharedMetadataDb = Arc<Mutex<MetadataDb>>` with a split type:
- Readers hold `Arc<MetadataDb>` (or a thin reader-only wrapper) and call `read_txn(&self)` directly — no lock.
- The writer task owns `MetadataDb` directly (single owner ⇒ `&mut self` for `write_txn` without locking).

The current doc comment at `crates/brain-planner/src/executor/context.rs:25-29` already claims "redb's MVCC means read txns don't block subsequent writes once the lock is released" — but the parking_lot guard is held for the entire duration of any `ReadTransaction` (because the txn borrows from the DB). The split makes the doc match reality.

### Design notes for the agent

- `redb::Database` is already `Send + Sync`. The wrap-in-Mutex was defensive, not required.
- Two viable shapes:
  1. **Owned-by-writer + Arc-readers**: writer task owns the `MetadataDb`, exposes a `reader_handle() -> Arc<DatabaseReaderView>` that holds `Arc<redb::Database>` and exposes only `read_txn(&self)`. Writer keeps mutable methods to itself.
  2. **RwLock**: swap `Mutex` for `RwLock`. Simpler but introduces reader-starvation risk under sustained write load.
- Recommended: shape (1), matching the existing single-writer-per-shard discipline.
- The `MetadataDb` wrapper also holds `schema_version` etc — those are immutable post-open. Make them accessible via `Arc<MetadataDb>` directly.
- `txn_store` is a separate `Arc<TxnStore>` and not affected.

### Affected files

- `crates/brain-metadata/src/db.rs` — split `MetadataDb` into reader/writer halves.
- `crates/brain-planner/src/executor/context.rs` — replace `SharedMetadataDb` type alias.
- `crates/brain-ops/src/context.rs` — `OpsContext` field shape.
- `crates/brain-ops/src/handlers/*.rs` — every `metadata.lock()` read site drops the lock.
- `crates/brain-ops/src/writer/submit.rs` — writer still gets exclusive access (just not via Mutex).
- `crates/brain-ops/src/index/{semantic_retriever,graph_retriever}/*.rs` — retrievers read.
- `crates/brain-server/src/shard/mod.rs` — wires the reader handle + writer.
- `crates/brain-workers/src/**` — workers reading metadata.
- `crates/brain-extractors/src/**` — extractor backfill reading.

### Constraints

- Public SDK verbs stay the same (no API breakage).
- WAL-before-ack, single-writer-per-shard, all 7 invariants in CLAUDE.md §5 hold.
- Tests stay green: brain-ops lib + integration + brain-planner lib + brain-storage + brain-metadata.
- No `pub use X as Y` aliases (saved memory feedback).
- No `// Spec §X/Y` ref comments (saved memory feedback).

### Done when

1. `Arc<Mutex<MetadataDb>>` does not appear anywhere in the workspace (except possibly test fixtures that need it for legacy reasons — document the exception).
2. RECALL throughput on a single shard scales with concurrent in-flight queries instead of plateauing at 1.
3. `cargo check --workspace --all-targets` clean in devcontainer.
4. `cargo test -p brain-ops --lib` + `-p brain-planner --lib` + `-p brain-storage --lib` + `-p brain-metadata --lib` all green.

---

## Workstream B — parallel retriever fan-out

**Branch:** `feature/parallel-retriever-fanout`
**Worktree:** `.claude/worktrees/retriever-fanout`
**Blast radius:** brain-planner + retriever traits.

### Goal

The for-loop at `crates/brain-planner/src/hybrid/executor.rs:189` runs the three retrievers sequentially. Make them concurrent via `futures::join_all` or `glommio`'s join primitives.

### Design notes for the agent

- Retriever traits in `brain-ops/src/index/{semantic_retriever,lexical_retriever,graph_retriever}/` currently return `Result<...>`, not `Future`. Three options:
  1. **Change trait signatures to async** — `fn search(...) -> impl Future<Output = Result<...>>`. Cleanest. Touches every retriever impl + the executor.
  2. **`spawn_local` + join** — wrap sync retriever calls in Glommio tasks; join the handles. Less invasive.
  3. **Manual `poll` interleaving** — bespoke executor logic. Avoid.
- The "semantic feeds graph" dependency (`needs_semantic_first` at line 153) must be preserved: when `GraphAnchorMode::MemoryFromSemantic`, run semantic first eagerly, then run lexical || graph in parallel.
- For pure CPU-bound retrievers (HNSW search), concurrent on a single-thread Glommio executor gives **interleaving** not true parallelism — but I/O-bound retrievers (Tantivy mmap reads on cold cache, redb reads) yield to io_uring and produce real overlap. Document this expectation honestly.

### Affected files

- `crates/brain-planner/src/hybrid/executor.rs` — fan-out loop.
- `crates/brain-planner/src/hybrid/retriever.rs` — trait signatures if option (1).
- `crates/brain-ops/src/index/{semantic_retriever,lexical_retriever,graph_retriever}/*.rs` — impl signatures.

### Done when

1. The three retrievers (minus the semantic-feeds-graph dependency) run concurrently.
2. Cold-cache RECALL p99 drops below the sum of individual retriever p99s.
3. `cargo test -p brain-planner --lib` green; `cargo test -p brain-ops --test recall` green.

---

## Workstream C — batched WAL append + hot-path alloc cleanups

**Branch:** `feature/wal-append-many`
**Worktree:** `.claude/worktrees/wal-batched`
**Blast radius:** brain-ops writer + brain-storage WAL sink boundary.

### Goal A — batched append

Replace the per-phase `sink.append(record).await` loop in `wal_append_for_write` (`crates/brain-ops/src/writer/submit.rs:577-675`) with a single `sink.append_many(records).await` that returns the LSN range or the first LSN.

Right now an Encode-with-5-edges issues **8 sink.append calls** = 8 channel-hop + 8 oneshot-allocation + 8 wakeups. One batched call gives 1.

### Goal B — alloc cleanups

- `submit.rs:346` `acks.clone()` — use `mem::take` + reconstruct, or restructure so the durable ack doesn't need a clone.
- `submit.rs:273` `(*cached).clone()` — already wrapped in Arc; clients can hold the Arc.
- `encode.rs:115` `req.text.clone()` — `req` is consumed downstream; use `mem::take(&mut req.text)`.

### Design notes for the agent

- `WalSink::append` returns `Pin<Box<dyn Future<...>>>`. Add `WalSink::append_many` with the same shape but `&[WalRecord]` input and `Vec<Lsn>` (or `LsnRange { first, last }`) output.
- The drain task in `brain-server` (or wherever it lives — check) should handle the batched message by appending all records to the segment **in one call to `segment.append_many`** if the segment supports it, otherwise loop internally but still issue one fdatasync. Group commit on the segment side already does this implicitly; verify the batched path doesn't break it.
- Backpressure semantics preserved: bounded channel still applies — if the drain task is N records behind, the writer awaits.

### Affected files

- `crates/brain-ops/src/writer/wal_sink.rs` — trait + ChannelWalSink impl + Noop/Recording/Failing test impls.
- `crates/brain-ops/src/writer/submit.rs` — `wal_append_for_write` + the cleanups above.
- `crates/brain-ops/src/handlers/encode.rs` — `req.text.clone()` cleanup.
- `crates/brain-server/src/shard/wal_drain.rs` (or equivalent) — drain task receives the batched message variant.
- `crates/brain-storage/src/wal/segment.rs` — possibly add `append_many` to amortize the segment's internal bookkeeping.

### Done when

1. A single Encode-with-N-edges issues exactly one `sink.append_many` call (verified by a unit test on `RecordingWalSink`).
2. All durability tests pass: `cargo test -p brain-storage --lib` + `cargo test -p brain-ops --lib writer::`.
3. No regressions in `cargo test -p brain-ops --test encode --test link --test forget`.

---

## Workstream D — fold vocab intern into main wtxn (kill fsync amplification)

**Branch:** `feature/vocab-intern-single-wtxn`
**Worktree:** `.claude/worktrees/vocab-fsync`
**Blast radius:** brain-ops handlers/{statement, relation, schema_replace} + write/phase + apply/{statement, relation}.

### Goal

Eliminate the "micro-wtxn" pattern that does an extra `commit()` per schemaless STATEMENT_CREATE / RELATION_CREATE.

Current state (schemaless STATEMENT_CREATE):
1. Micro-wtxn #1: intern PredicateId → commit (fsync)
2. Main `submit()` → WAL fsync + redb wtxn → commit (fsync)
3. Micro-wtxn #2: stamp `implicit_predicate` flag → commit (fsync)

Target: **1 fsync total**.

### Design notes for the agent

- Two viable shapes:
  1. **Apply does the intern**: extend `apply_upsert_statement` to intern the predicate on-the-fly if a sentinel `PredicateId::Implicit { namespace, name }` is passed in the Phase. The Phase carries the qname; apply resolves to a real PredicateId inside the wtxn.
  2. **Pre-mint PredicateId, no separate commit**: the handler computes a deterministic PredicateId from the qname hash, builds the Phase with both the row data and an intern-this-predicate hint. Apply inserts the predicate row in the same wtxn.
- (2) keeps the handler's "pre-allocate IDs, apply runs cleanly" invariant. Recommended.
- For the implicit-flag stamp: instead of a post-commit micro-wtxn, make it a side effect of the same `apply_upsert_statement` when the intern path fires. The flag is just a byte; setting it inside the same wtxn is free.
- Similarly for RELATION_CREATE: pre-mint RelationTypeId or carry the qname hint, intern inside `apply_upsert_relation`.
- For SCHEMA_REPLACE: the existing pre-flight micro-wtxn validates `force_drop_existing` + checks current state. Either fold the check into the main `apply_upsert_schema` (it's idempotent enough) or accept the extra commit as a one-time admin op cost (low-frequency).

### Affected files

- `crates/brain-ops/src/handlers/statement.rs` — kill micro-wtxn #1 + #2.
- `crates/brain-ops/src/handlers/relation.rs` — kill micro-wtxn.
- `crates/brain-ops/src/handlers/schema_replace.rs` — decide & document.
- `crates/brain-ops/src/write/phase.rs` — extend Phase variants if needed (e.g., `predicate_qname: Option<(String, String)>` field on `UpsertStatement`).
- `crates/brain-ops/src/apply/statement.rs` — handle the intern path inside the wtxn.
- `crates/brain-ops/src/apply/relation.rs` — handle the intern path inside the wtxn.

### Constraints

- Idempotency by RequestId still works — replay of a schemaless statement must reuse the original PredicateId (don't re-intern with a different ID).
- Strict-mode path (predicate already declared) unchanged.
- No new wire fields exposed to clients.

### Done when

1. `STATEMENT_CREATE` schemaless: 1 fsync per call (verified by counting fdatasyncs in a test or by reading the WAL log).
2. `RELATION_CREATE` schemaless: 1 fsync per call.
3. All existing tests pass: `cargo test -p brain-ops --test '*'` green (modulo the pre-existing PQ-codebook failures documented in commit `951ceb9`).
4. Idempotency replay tests still pass for schemaless writes.

---

## Integration order (parent rebase)

User chose "all four parallel from main" — accepting rebase risk. Recommended landing order to minimize conflict surface:

1. **D first** — handler-level changes, smallest blast.
2. **C second** — writer-side; touches `submit.rs` which D doesn't.
3. **B third** — planner + retriever traits; touches files C didn't.
4. **A last** — wraps everything; conflict-resolve against the new shape from D/C/B.

Each landed via fast-forward or merge commit; cleanup worktrees + branches as we go.

## Verification (post-integration)

```
just verify
cargo test -p brain-ops --lib
cargo test -p brain-ops --tests
cargo test -p brain-planner --lib
cargo test -p brain-storage --lib
cargo test -p brain-metadata --lib
```

All green (modulo the documented pre-existing PQ-codebook + NopDispatcher failures from `951ceb9`).
