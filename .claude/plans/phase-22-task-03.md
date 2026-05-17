# Plan: Phase 22 — Task 03, MemoryTextIndexer worker

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Stand up the post-ENCODE-commit pipeline that keeps
`memory_text.tantivy/` in sync with the substrate
`MEMORIES_TABLE`. Bounded channel + spawn-local drain loop +
group-commit cadence per §27/02 + §26/01 §3. First commit on a
fresh index stamps the brain-side schema payload (§26/01 §2)
that 22.1 reads on subsequent opens.

Concrete deliverables:

1. New module `brain-ops/src/ops/text_indexer/mod.rs` housing:
   - `IndexableMemory` event type (§27/02 §2).
   - `MemoryTextDispatcher` — bounded async-`flume::Sender`; the
     ENCODE post-commit hook calls `dispatcher.dispatch(...)`,
     awaiting on overflow (backpressure §27/02 §1).
   - `spawn_memory_text_indexer(ctx, receiver)` — `glommio::spawn_local`
     drain loop owning the per-shard `IndexWriter`.
2. Group-commit policy:
   - N=256 writes OR T=1 s, whichever first (env-overridable
     via `BRAIN_TANTIVY_COMMIT_N` + `BRAIN_TANTIVY_COMMIT_MS`).
   - `PreparedCommit::set_payload(schema_payload_json())` on
     every commit so the 22.1 schema-version check has a
     payload to read.
3. Hook from `handle_encode` post-WAL-fsync (alongside the
   phase-20 `run_extractor_pipeline` call).
4. FORGET path: `MemoryTextDispatcher::dispatch_forget(memory_id)`
   from `handle_forget` post-commit.
5. Observability: gauges + counters + commit-latency histogram
   per §27/02 §8 (the metrics struct lives in
   `brain-workers::metrics` for reuse with 22.4).

NOT in scope:
- Statement text indexer (22.4 — separate worker, same
  harness).
- Rebuild worker (22.6).
- WAL-replay re-emission on startup (22.7).
- The retriever consumes nothing yet (22.5 owns the read side).

## 2. Spec references

- `spec/27_knowledge_workers/02_text_indexer_workers.md` §1, §2,
  §4, §5 — binding for queue capacity, backpressure-on-overflow,
  retry-once-then-fatal commit policy, post-commit fan-out
  ordering.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §3 — N=256 /
  T=1 s defaults + env vars.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §2 — schema
  version stamping mechanism (`IndexMeta::payload`).
- `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.1 —
  ENCODE p99 = 20 ms; this hook participates in that budget.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `IndexWriter::prepare_commit() -> PreparedCommit` | docs.rs/tantivy/0.26.1 | Yes — `PreparedCommit::set_payload(&str)` then `commit()`. |
| `IndexWriter::delete_term` | docs.rs | Yes — idempotent at replay. |
| `glommio::spawn_local` lifetimes | brain-workers existing usage | Used by the phase-9.11 fan-out task; pattern matches. |
| `flume::Sender::send_async` backpressure | brain-server existing usage | Used for cross-shard event fan-out; matches §27/02 backpressure semantics. |
| ENCODE post-commit hook site | `crates/brain-ops/src/ops/encode.rs:60` | `run_extractor_pipeline(ctx, &memory).await;` — same insertion point. |

## 4. Architecture sketch

```rust
// crates/brain-ops/src/ops/text_indexer/mod.rs

use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::{AgentId, MemoryId, MemoryKind};
use brain_index::{schema_payload_json, BRAIN_SCHEMA_VERSION, IndexHandle};
use flume::{bounded, Receiver, Sender};
use tantivy::{schema::Field, Document, IndexWriter, Term};

pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;
pub const DEFAULT_COMMIT_N: usize = 256;
pub const DEFAULT_COMMIT_MS: u64 = 1000;

pub struct CommitPolicy {
    pub n_writes: usize,
    pub interval: Duration,
}

impl CommitPolicy {
    pub fn from_env() -> Self {
        let n = std::env::var("BRAIN_TANTIVY_COMMIT_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_COMMIT_N);
        let ms = std::env::var("BRAIN_TANTIVY_COMMIT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_COMMIT_MS);
        Self { n_writes: n, interval: Duration::from_millis(ms) }
    }
}

pub enum MemoryTextOp {
    Upsert {
        id: MemoryId,
        text: String,
        agent_id: AgentId,
        kind: MemoryKind,
        created_at_unix_ms: u64,
    },
    Forget { id: MemoryId },
}

pub struct MemoryTextDispatcher {
    tx: Sender<MemoryTextOp>,
}

impl MemoryTextDispatcher {
    pub async fn dispatch(&self, op: MemoryTextOp) {
        // Backpressure: await on send_async if queue is full.
        if self.tx.send_async(op).await.is_err() {
            tracing::error!(
                target: "brain_ops::text_indexer",
                "memory text indexer receiver dropped; shard is shutting down",
            );
        }
    }
}

pub fn spawn_memory_text_indexer(
    handle: IndexHandle,
    rx: Receiver<MemoryTextOp>,
    policy: CommitPolicy,
) {
    glommio::spawn_local(async move {
        let mut writer = handle.index.writer_with_num_threads(1, 50_000_000)
            .expect("invariant: IndexWriter creation cannot fail on a healthy index");
        let mut batch = 0usize;
        let mut last_commit = Instant::now();
        let memory_id_field = handle.index.schema().get_field("memory_id").unwrap();
        // ...other fields cached similarly.

        loop {
            let next = match rx.recv_async().await {
                Ok(op) => op,
                Err(_) => {
                    // Sender dropped: drain + final commit + exit.
                    let _ = flush_commit(&mut writer);
                    return;
                }
            };
            apply_op(&mut writer, &next, &fields);
            batch += 1;

            if batch >= policy.n_writes || last_commit.elapsed() >= policy.interval {
                flush_commit(&mut writer);
                batch = 0;
                last_commit = Instant::now();
            }
        }
    }).detach();
}

fn flush_commit(writer: &mut IndexWriter) -> tantivy::Result<()> {
    let prepared = writer.prepare_commit()?;
    let _ = prepared.set_payload(&schema_payload_json())?;
    // retry-once
    match prepared.commit() {
        Ok(_) => Ok(()),
        Err(first) => {
            tracing::warn!(error = %first, "memory text indexer commit failed; retry");
            // backoff 10 ms via glommio timer
            // Note: PreparedCommit is consumed; for retry we need to call commit() again
            // on a fresh prepare — re-prepare on the writer.
            let retry = writer.prepare_commit()?;
            let _ = retry.set_payload(&schema_payload_json())?;
            retry.commit().map(|_| ()).map_err(|e| {
                tracing::error!(error = %e, "memory text indexer commit failed twice; shard fatal");
                e
            })
        }
    }
}
```

(The retry-once shape needs care because `PreparedCommit` is
single-use. The implementation re-prepares on retry — adds + deletes
since the failed commit remain in the IndexWriter buffer.)

Wire into `OpsContext` via a new field:

```rust
pub struct OpsContext {
    // ...
    pub memory_text_dispatcher: Option<Arc<MemoryTextDispatcher>>,
}
```

ENCODE hook (in `crates/brain-ops/src/ops/encode.rs`):

```rust
// after run_extractor_pipeline:
if let Some(dispatcher) = ctx.memory_text_dispatcher.as_ref() {
    if let Some(text) = memory.text.as_ref() {
        dispatcher.dispatch(MemoryTextOp::Upsert {
            id: memory.id,
            text: text.clone(),
            agent_id: memory.agent,
            kind: memory.kind,
            created_at_unix_ms: memory.created_at_unix_ms,
        }).await;
    }
}
```

FORGET hook: same pattern, `Op::Forget`.

Server-side spawn (in `brain-server::shard::spawn`):

```rust
let (memory_text_tx, memory_text_rx) = bounded(DEFAULT_QUEUE_CAPACITY);
let dispatcher = Arc::new(MemoryTextDispatcher { tx: memory_text_tx });
let policy = CommitPolicy::from_env();
spawn_memory_text_indexer(tantivy_for_ops.as_ref().unwrap().memory_text.clone(), memory_text_rx, policy);
let ops = OpsContext::new(executor_ctx)
    // ...existing builders...
    .with_memory_text_dispatcher(Some(dispatcher));
```

(Cloning `IndexHandle` needs a `Clone` impl — added in 22.3 since `tantivy::Index` is `Clone`.)

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Bounded channel + spawn-local drain (this plan) | Matches §27/02 §1 backpressure; minimal locking; matches phase-9 cross-shard fan-out pattern | One extra task per shard | ✓ |
| Synchronous in-handler write | One less moving part | Holds ENCODE on `add_document` + commit cadence; loses batching | rejected — defeats §26/01 §3 group commit |
| Worker via `brain-workers::scheduler` | Reuses existing worker registration | Overkill for a single-purpose drain loop; scheduler optimises for periodic workers | rejected |
| Drop on overflow (existing knowledge-worker default) | No ENCODE latency variance | Index drift = lexical recall correctness gap (§27/02 §1 explicit) | rejected |
| Commit on every write (no batching) | Maximum durability | Tantivy commit is ~5 ms; ENCODE p99 = 20 ms — batching is necessary | rejected |

## 6. Risks / open questions

- **Risk:** `PreparedCommit::set_payload` returns `Result`; failure isn't documented. **Mitigation:** treat as commit failure → retry-once → shard-fatal.
- **Risk:** Glommio's spawn_local task lifetime + IndexWriter Send-ness. `tantivy::IndexWriter` is `Send + Sync`; the spawn_local task is per-shard and never crosses cores, so this is fine.
- **Risk:** Backpressure on ENCODE blocks the user-facing op. **Mitigation:** queue capacity 4096 + commit cadence 1 s means the worker drains ~4 K items/sec at worst; production load (~100 enc/s/shard per §16) leaves 40× headroom.
- **Open question:** What about ENCODE-non-commit paths (txn path)? phase 20 already skips extractor dispatch for txn ENCODE; 22.3 mirrors that. **Resolution:** dispatch only from the non-txn path; txn-path text indexing happens on COMMIT in phase 22+ (declared scope cut).

## 7. Test plan

Integration tests in `crates/brain-ops/tests/text_indexer.rs`:

- `dispatch_on_encode` — spin up an in-process shard (using existing test harness), ENCODE a memory with text, assert the tantivy `memory_text.tantivy` index returns it via `Searcher::search` for a stemmed term.
- `dispatch_forget_removes` — ENCODE, then FORGET; the term query returns no hits.
- `commit_cadence_by_count` — env override `BRAIN_TANTIVY_COMMIT_N=4`, ENCODE 4 memories; assert the index is committed (`Searcher::search` returns hits within the timeout) without waiting the 1-s default.
- `commit_cadence_by_time` — ENCODE 1 memory, wait > 1 s, assert committed.
- `backpressure_under_overflow` — queue capacity 4; fire 8 ENCODEs in a tight loop without giving the drain task time; assert the 5th ENCODE awaited on send (timing-sensitive — use a slow-receiver mock).
- `first_commit_stamps_payload` — ENCODE one memory, commit, drop shard, re-open via `TantivyShard::open`; assert status is `Ready` (the stamped payload satisfies the version check).
- `schema_version_mismatch_after_payload_change` — write a memory through the dispatcher, manually edit `meta.json` payload to version 99, re-open; assert `NeedsRebuild { SchemaVersionMismatch { found: 99, expected: 1 } }`. This complements 22.1's tests by exercising the writer path.

Unit tests for the commit-cadence math live in
`crates/brain-ops/src/ops/text_indexer/policy_tests.rs`.

## 8. Commit shape

Single commit:

```
feat(ops,server,index): 22.3 — MemoryTextIndexer worker

- crates/brain-ops/src/ops/text_indexer/mod.rs (new):
  MemoryTextOp, MemoryTextDispatcher, CommitPolicy,
  spawn_memory_text_indexer with retry-once-then-fatal commit.
- crates/brain-ops/src/context.rs: OpsContext.memory_text_dispatcher +
  with_memory_text_dispatcher builder.
- crates/brain-ops/src/ops/encode.rs: post-commit hook dispatches
  Upsert.
- crates/brain-ops/src/ops/forget.rs: post-commit hook dispatches
  Forget.
- crates/brain-index/src/tantivy_shard/mod.rs: `impl Clone for
  IndexHandle` so the worker can take ownership.
- crates/brain-server/src/shard/mod.rs: spawn the drain task at
  shard spawn; thread the dispatcher into OpsContext.
- 7 integration tests + a handful of unit tests on commit-policy
  math.
```

## 9. Confirmation

Please confirm:

1. **Module lives in `brain-ops/src/ops/text_indexer/`** (vs. `brain-workers/`) — it's tightly coupled to ENCODE / FORGET dispatch.
2. **Bounded `flume` channel + `glommio::spawn_local` drain** (vs. scheduler-managed worker).
3. **Retry-once-then-shard-fatal commit policy** per §27/02 §4.
4. **Payload stamped on every commit** (not just first) — keeps the on-disk version always current with the running binary.
5. **Worker owns the `IndexWriter`** (handle cloned out of `TantivyShard`); future 22.5 retrieval uses `Index::reader()` independently.

After approval: implement + tests + commit.
