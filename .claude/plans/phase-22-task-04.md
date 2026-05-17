# Plan: Phase 22 — Task 04, StatementTextIndexer worker

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Mirror 22.3's harness for `statements.tantivy/`. Same channel +
spawn-local drain + group-commit + retry-once shape; different
event payload, different post-commit hook sites (statement_create
+ supersede + tombstone), and a per-event join against
`ENTITIES_TABLE.canonical_name` + `PREDICATES_TABLE.name` to
build the text representation.

Concrete deliverables:

1. Extend `brain-ops/src/ops/text_indexer/` from 22.3 with:
   - `StatementTextOp` (`Upsert { ... }` + `Delete { id }`).
   - `StatementTextDispatcher` — same shape as
     `MemoryTextDispatcher`, separate channel.
   - `spawn_statement_text_indexer` — same drain loop pattern;
     owns the `statements.tantivy` IndexWriter.
2. Text representation computed at index time:
   `text_repr = subject.canonical_name + " " + predicate.name + " " + object_text`
   (§27/02 §3 binding).
3. Hooks at:
   - `handle_statement_create` post-commit → `Upsert`.
   - Supersede flow (existing in `brain-ops::ops::statement::*`)
     → `Delete` for the superseded id + `Upsert` for the new id.
   - Tombstone flow → `Delete` for the tombstoned id.
4. Resolution of the canonical_name + predicate_name happens in
   the dispatch site (read-side latency is cheaper there than in
   the worker, which is meant to be I/O-bound on tantivy).
5. Reuse `CommitPolicy::from_env()` from 22.3 — same env vars
   drive both indexers.
6. Reuse the `text_indexer::metrics` struct from 22.3 with a
   scope label.

NOT in scope:
- Recovery / WAL-replay of in-flight statement writes (22.7).
- Confidence rebucketing on aggregation update (the existing
  noisy-OR aggregator will re-emit `Upsert` events when it
  rewrites a statement; 22.4 just consumes them).
- Snippet generation (22.5).

## 2. Spec references

- `spec/27_knowledge_workers/02_text_indexer_workers.md` §3 — binding
  for Upsert/Delete semantics, text representation, supersede +
  tombstone branches.
- `spec/27_knowledge_workers/02_text_indexer_workers.md` §5 —
  post-commit ordering puts statement indexer after memory text
  indexer.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §2 — schema
  binding for confidence bucketing (`(confidence * 10).floor()`).
- `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.3 —
  statement-create p99 budgets that the indexer hook must
  respect.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `entities_table::get(id)` returns canonical_name | brain-metadata existing API | Yes. |
| `predicates_table::get(id)` returns `PredicateRecord { name, .. }` | brain-metadata existing API | Yes. |
| StatementId is u128 → 16-byte field | `brain-core::StatementId::to_be_bytes()` | Yes. |
| Supersede + tombstone flow | `crates/brain-ops/src/ops/statement_supersede.rs` etc. | Existing flows expose post-commit hooks; the §17 phase plans referenced them. |

## 4. Architecture sketch

Adding to the module 22.3 sets up:

```rust
// crates/brain-ops/src/ops/text_indexer/mod.rs (extending 22.3)

pub enum StatementTextOp {
    Upsert {
        id: StatementId,
        subject_canonical_name: String,
        predicate_name: String,
        object_text: String,
        kind: StatementKind,
        confidence: f32,
        extracted_at_unix_ms: u64,
    },
    Delete { id: StatementId },
}

pub struct StatementTextDispatcher {
    tx: Sender<StatementTextOp>,
}

impl StatementTextDispatcher {
    pub async fn dispatch(&self, op: StatementTextOp) { /* backpressure-await */ }
}

pub fn spawn_statement_text_indexer(
    handle: IndexHandle,
    rx: Receiver<StatementTextOp>,
    policy: CommitPolicy,
) {
    // Same drain loop as memory indexer; apply_op encodes the
    // confidence bucket and writes the doc.
}

fn confidence_bucket(c: f32) -> u64 {
    ((c.clamp(0.0, 1.0) * 10.0).floor() as u64).min(9)
}
```

`OpsContext` gains a sibling field:

```rust
pub statement_text_dispatcher: Option<Arc<StatementTextDispatcher>>,
```

Post-commit hook in `handle_statement_create`:

```rust
if let Some(d) = ctx.statement_text_dispatcher.as_ref() {
    let canonical_name = entities_table::get(&rtxn, statement.subject_entity_id)?
        .map(|e| e.canonical_name).unwrap_or_default();
    let predicate_name = predicates_table::get(&rtxn, statement.predicate_id)?
        .map(|p| p.name).unwrap_or_default();
    d.dispatch(StatementTextOp::Upsert {
        id: statement.id,
        subject_canonical_name: canonical_name,
        predicate_name,
        object_text: statement.object_text.clone(),
        kind: statement.kind,
        confidence: statement.confidence,
        extracted_at_unix_ms: statement.extracted_at_unix_ms,
    }).await;
}
```

Supersede flow: emit two ops — `Delete { superseded_id }` then
`Upsert { new_id, ... }`.

Tombstone flow: emit `Delete { id }`.

Server-side spawn (in `brain-server::shard::spawn`, alongside
22.3's wiring):

```rust
let (stmt_text_tx, stmt_text_rx) = bounded(DEFAULT_QUEUE_CAPACITY);
let stmt_dispatcher = Arc::new(StatementTextDispatcher { tx: stmt_text_tx });
spawn_statement_text_indexer(
    tantivy_for_ops.as_ref().unwrap().statements.clone(),
    stmt_text_rx,
    CommitPolicy::from_env(),
);
let ops = OpsContext::new(executor_ctx)
    // ...
    .with_memory_text_dispatcher(Some(memory_dispatcher))
    .with_statement_text_dispatcher(Some(stmt_dispatcher));
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Compute text repr in dispatch site (this plan) | Worker stays I/O-bound; no metadata read from the worker | Two read-txn round-trips per statement on the foreground | ✓ — read latency is ~50 µs vs. tantivy add ~50 µs |
| Compute text repr in worker | Foreground cheaper | Worker has to open a redb read txn per op; couples worker to metadata crate | rejected |
| Cache canonical_name + predicate_name on the statement record | Single source | Spec §17/03 binds the storage shape; expanding it is post-v1 | rejected |
| Single shared dispatcher for both indexers | One channel | Different event types; multiplexing adds enum overhead | rejected |
| Aggregate supersede into a single Upsert+Delete event | Atomic in the worker view | Two separate writes is the natural shape; tantivy commit barriers cover atomicity at the index level | rejected |

## 6. Risks / open questions

- **Risk:** statement_create may run under a wtxn that hasn't committed yet at hook time. **Mitigation:** the hook fires after the wtxn commit (same discipline as 22.3); the read in the dispatch site uses a fresh read txn that observes the just-committed state.
- **Risk:** Predicate / entity reads in the dispatch site can fail (corrupt redb). **Mitigation:** on failure, log + skip the dispatch (the audit log already records the failure; lexical drift is acceptable on a corrupt-metadata path because hybrid query will return whatever the semantic retriever sees).
- **Open question:** Should aggregator-driven statement rewrites (noisy-OR confidence updates) emit `Upsert` events? **Resolution:** yes — confidence bucket bucketing depends on the live value; the aggregator emit site is the right hook (deferred wiring detail; reviewed inline at code time).
- **Open question:** What about cross-shard statement references? **Resolution:** out of scope — per-shard indexer per §27/02 §1; cross-shard handled by the router (§24).

## 7. Test plan

Integration tests in `crates/brain-ops/tests/statement_text_indexer.rs`:

- `dispatch_on_create_indexes` — statement_create with a fresh statement; verify the index returns a hit for the predicate name (exact-match STRING field) and a stemmed subject term.
- `confidence_bucket_round_trip` — confidence 0.27 → bucket 2; 0.99 → bucket 9; 1.0 → bucket 9 (clamp); 0.05 → 0.
- `supersede_flow` — create A, supersede A with B → query returns B's text but not A's; A's id deleted.
- `tombstone_removes` — create + tombstone → query returns nothing.
- `aggregator_rewrite_emits_upsert` — drive the noisy-OR aggregator with a known evidence sequence; assert one Upsert event per statement rewrite. (May be deferred to a focused unit test on the aggregator emit path if integration test is flaky.)
- `cross_index_isolation` — write a memory + a statement; query memory_text for the predicate name (should miss); query statements for the memory text (should miss). Confirms the two indexers don't leak into each other.

Unit tests for `confidence_bucket()` in
`crates/brain-ops/src/ops/text_indexer/policy_tests.rs`.

## 8. Commit shape

Single commit:

```
feat(ops,server): 22.4 — StatementTextIndexer worker

- crates/brain-ops/src/ops/text_indexer/mod.rs: add
  StatementTextOp, StatementTextDispatcher,
  spawn_statement_text_indexer, confidence_bucket().
- crates/brain-ops/src/context.rs: with_statement_text_dispatcher
  + statement_text_dispatcher field.
- crates/brain-ops/src/ops/statement_create.rs (or wherever
  handle_statement_create lives): post-commit Upsert dispatch.
- crates/brain-ops/src/ops/statement_supersede.rs: Delete + Upsert
  dispatch.
- crates/brain-ops/src/ops/statement_tombstone.rs: Delete dispatch.
- crates/brain-server/src/shard/mod.rs: spawn the drain task,
  wire dispatcher.
- 6 integration tests + 1 unit test (confidence_bucket math).
```

## 9. Confirmation

Please confirm:

1. **Text repr computed in the dispatch site** (vs. in the worker).
2. **Two separate dispatchers + channels** (memory + statement),
   not a single multiplexed channel.
3. **Aggregator-driven confidence rewrites emit `Upsert`** — i.e. the indexer follows whatever the aggregator decides, no separate bucket-cache.
4. **Supersede = Delete-then-Upsert** as two separate events (atomicity covered at tantivy commit barriers, not at event granularity).
5. **`confidence_bucket(c)` clamps to `[0..=9]`** — values outside `[0.0, 1.0]` are clamped (defensive against bad evidence inputs).

After approval: implement + tests + commit. 22.4 lands directly after 22.3 since they share the harness.
