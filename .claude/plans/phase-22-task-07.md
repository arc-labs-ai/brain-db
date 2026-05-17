# Plan: Phase 22 — Task 07, Recovery on shard startup

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Make the shard spawn path honour `IndexStatus::NeedsRebuild`
from 22.1 by invoking the 22.6 rebuild functions before the
indexer workers start. After this sub-task, a shard that boots
with a corrupt or version-mismatched tantivy directory recovers
without operator intervention.

Concrete deliverables:

1. New module `crates/brain-server/src/shard/tantivy_recovery.rs`
   (a single function `recover_tantivy_on_open`).
2. Signature:
   ```rust
   pub fn recover_tantivy_on_open(
       shard_dir: &Path,
       metadata: &MetadataDb,
       startup: TantivyShardStartup,
   ) -> Result<Arc<TantivyShard>, RecoveryError>;
   ```
3. Behaviour per scope (memory + statement, independent):
   - `IndexStatus::Ready` → no-op, keep the handle.
   - `IndexStatus::NeedsRebuild { reason }` → log the reason at
     `warn`, call `rebuild_memory_text` / `rebuild_statements`
     (22.6), then re-open the index via `TantivyShard::open` to
     pick up the fresh on-disk state.
4. After both scopes are `Ready`, return the (possibly re-opened)
   `Arc<TantivyShard>`. The rest of the shard spawn proceeds as
   before — registering the analyzer, spawning indexer workers
   (22.3 / 22.4), constructing the retriever (22.5).
5. If a rebuild itself fails (rebuild fn returns an error),
   surface the failure to the caller — shard spawn fails. The
   shard supervisor logs + does not start (operators investigate).

NOT in scope:
- Partial WAL replay (re-indexing redb rows whose timestamp
  exceeds the indexer's commit cursor). Explicit v1 scope cut
  — startup either accepts the index as-is or does a full
  rebuild; loss bound is ≤ N-1 writes per indexer (§26/01 §3).
  Phase 22+ revisits with cursor tracking.
- On-demand admin rebuild (`ADMIN_TANTIVY_REBUILD` wire op).
- Async / streaming rebuild progress to clients during startup
  — fully synchronous in v1.

## 2. Spec references

- `spec/26_knowledge_storage/01_tantivy_layout.md` §6 —
  recovery on startup. Binding.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §5 —
  rebuild algorithm consumed via 22.6.
- 22.1 plan (`phase-22-task-01.md`) — `TantivyShard::open` +
  `IndexStatus` contract that 22.7 acts on.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `MetadataDb::read_txn` availability at shard spawn | existing `brain-server::shard::spawn` — already used for the extractor registry materialization (phase 20.7) | Yes — the metadata DB is opened **before** the Glommio executor spawns. |
| `TantivyShard::open` is idempotent on a freshly-rebuilt dir | 22.1 tests | Yes — fresh dirs go through the `create_fresh` branch + `Ready` status. |
| Rebuild + re-open ordering | §26/01 §5 + §6 | Yes — atomic swap means the second open sees the new dir state immediately. |

## 4. Architecture sketch

```rust
// crates/brain-server/src/shard/tantivy_recovery.rs

use std::path::Path;
use std::sync::Arc;

use brain_index::{IndexStatus, LexicalScope, TantivyShard, TantivyShardStartup};
use brain_metadata::MetadataDb;
use brain_ops::ops::text_indexer::rebuild::{
    rebuild_memory_text, rebuild_statements, RebuildError, RebuildReport,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("tantivy open: {0}")]
    Open(#[from] brain_index::TantivyShardError),
    #[error("memory_text rebuild: {0}")]
    MemoryRebuild(#[source] RebuildError),
    #[error("statements rebuild: {0}")]
    StatementsRebuild(#[source] RebuildError),
}

/// Walk the [`TantivyShardStartup`] and run any rebuilds needed
/// per §26/01 §6. Returns a fresh `Arc<TantivyShard>` whose
/// indexes are guaranteed `Ready`.
pub fn recover_tantivy_on_open(
    shard_dir: &Path,
    metadata: &MetadataDb,
    startup: TantivyShardStartup,
) -> Result<Arc<TantivyShard>, RecoveryError> {
    let TantivyShardStartup {
        shard,
        memory_status,
        statements_status,
    } = startup;

    let memory_needs_rebuild = matches!(memory_status, IndexStatus::NeedsRebuild { .. });
    let statements_needs_rebuild =
        matches!(statements_status, IndexStatus::NeedsRebuild { .. });

    if !memory_needs_rebuild && !statements_needs_rebuild {
        return Ok(shard);
    }

    // Drop the existing Arc before rebuild — we'll re-open after.
    // Any other holders (none at this point in spawn) would block
    // the directory rename.
    drop(shard);

    if memory_needs_rebuild {
        log_reason(LexicalScope::MemoryText, &memory_status);
        let report = rebuild_memory_text(shard_dir, metadata)
            .map_err(RecoveryError::MemoryRebuild)?;
        log_report(&report);
    }
    if statements_needs_rebuild {
        log_reason(LexicalScope::StatementText, &statements_status);
        let report = rebuild_statements(shard_dir, metadata)
            .map_err(RecoveryError::StatementsRebuild)?;
        log_report(&report);
    }

    // Re-open. Both scopes must now be Ready.
    let fresh = TantivyShard::open(shard_dir)?;
    debug_assert!(matches!(fresh.memory_status, IndexStatus::Ready));
    debug_assert!(matches!(fresh.statements_status, IndexStatus::Ready));
    Ok(fresh.shard)
}

fn log_reason(scope: LexicalScope, status: &IndexStatus) {
    if let IndexStatus::NeedsRebuild { reason } = status {
        tracing::warn!(
            target: "brain_server::shard",
            ?scope,
            ?reason,
            "tantivy rebuild scheduled at startup",
        );
    }
}

fn log_report(report: &RebuildReport) {
    tracing::info!(
        target: "brain_server::shard",
        scope = ?report.scope,
        rows = report.rows_processed,
        duration_ms = report.duration.as_millis() as u64,
        "tantivy rebuild complete",
    );
}
```

Shard spawn integration replaces the existing
`TantivyShard::open` warn-and-continue block (added in 22.1):

```rust
// Before (22.1):
let tantivy_for_ops = match brain_index::TantivyShard::open(&shard_dir_for_executor) {
    Ok(startup) => { /* warn on NeedsRebuild, continue */ Some(startup.shard) }
    Err(err) => { /* log error */ None }
};

// After (22.7):
let tantivy_for_ops = match brain_index::TantivyShard::open(&shard_dir_for_executor) {
    Ok(startup) => match crate::shard::tantivy_recovery::recover_tantivy_on_open(
        &shard_dir_for_executor,
        &metadata_for_recovery,  // a clone of the MetadataDb arc-mutex acquired pre-Glommio
        startup,
    ) {
        Ok(shard) => Some(shard),
        Err(err) => {
            tracing::error!(error = %err, "tantivy recovery failed; aborting shard spawn");
            return Err(ShardError::Recovery(err));
        }
    },
    Err(err) => {
        tracing::error!(error = %err, "TantivyShard::open failed");
        None
    }
};
```

The `metadata_for_recovery` is the same `Arc<Mutex<MetadataDb>>`
used by everything else; we acquire the lock briefly for the
rebuild's read-txn-based iteration. Since this happens before
the Glommio executor starts, lock contention is zero.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Synchronous startup rebuild (this plan) | Simple; deterministic boot state; matches §26/01 §6 | Long startup for large indexes | ✓ |
| Background async rebuild + serve "IndexUnavailable" | Fast boot | Adds state machine; client error during rebuild; complicates 22.5 retriever | rejected — v1 has no need |
| Partial WAL replay (no full rebuild) | Faster recovery | Requires cursor tracking + redb row scanning; cursor lives in tantivy meta payload (schema-version field expands) | rejected — explicit v1 scope cut |
| Recovery in `brain-index` | Co-locates with `TantivyShard` | brain-index would need to depend on brain-ops (for rebuild fns) — backwards layering | rejected |
| Recovery in `brain-ops` | One less crate boundary | brain-server's spawn already orchestrates startup; the rebuild is a server-side concern | rejected — cleaner in brain-server |
| Recovery fn returns `Result<()>` and mutates a passed-in slot | One call site | More plumbing; the `Arc<TantivyShard>` return is more ergonomic | rejected |

## 6. Risks / open questions

- **Risk:** Recovery acquires the metadata lock for the duration of the rebuild. **Mitigation:** rebuild runs before the executor spawns, so no other shard work contends. Reads from redb happen via the read-txn that the rebuild fn opens; no write-txn needed.
- **Risk:** If rebuild fails, shard spawn aborts. **Mitigation:** that's the desired behaviour — a shard whose tantivy is unrecoverable shouldn't accept ENCODE/RECALL until the operator investigates. The error surfaces via the shard supervisor.
- **Risk:** A successful rebuild followed by a failing re-open suggests rebuild produced a broken index. **Mitigation:** debug_assert on `Ready` post-rebuild; if it ever trips, that's a 22.6 bug to fix. In release builds the assert is removed but the function still returns a `RecoveryError` on failure (rebuild returning `Ok` then open failing → `Open(TantivyShardError)`).
- **Open question:** Should we also rebuild on `IndexStatus::Ready` if a redb row exists whose `created_at_unix_ms` exceeds the indexer's last commit cursor? **Resolution:** no — v1 scope cut. The bound (≤ N-1 writes lost) is documented; partial replay lands post-v1.

## 7. Test plan

Unit tests in `crates/brain-server/src/shard/tantivy_recovery_tests.rs`:

- `recover_with_ready_indexes_is_noop` — open a fresh shard, recovery returns the same Arc (compared by `Arc::ptr_eq` if possible, or by re-checking status).
- `recover_with_corrupt_memory_index_rebuilds` — write a memory via redb (no indexer), corrupt the on-disk `memory_text.tantivy/meta.json`, run `TantivyShard::open` → `NeedsRebuild`, run recovery, assert status is `Ready` and the redb-stored memory is now searchable.
- `recover_with_corrupt_statements_index_rebuilds` — symmetric for statements with an entity + predicate join.
- `recover_with_both_corrupt_rebuilds_both` — both scopes corrupt; recovery handles both; ordering doesn't matter (memory first).
- `recovery_propagates_rebuild_error` — inject a rebuild failure (e.g. set the rebuild dir read-only mid-iteration) and assert `RecoveryError::MemoryRebuild`.
- `version_mismatch_triggers_rebuild` — write meta.json with `brain_schema_version: 99`, recovery rebuilds, post-recovery status is `Ready` with version `1`.

End-to-end smoke (extend `spawn_creates_knowledge_directories`):
- After spawn on a fresh shard, assert `meta.json` exists for both indexes. (Already covered by 22.1; no regression expected.)
- Corrupt one of the indexes between two spawn calls; second spawn boots cleanly because recovery rebuilds.

## 8. Commit shape

```
feat(server,ops): 22.7 — tantivy recovery on shard startup

- crates/brain-server/src/shard/tantivy_recovery.rs (new):
  recover_tantivy_on_open + RecoveryError. Honours
  IndexStatus::NeedsRebuild by calling the 22.6 rebuild
  functions and re-opening the shard.
- crates/brain-server/src/shard/mod.rs: spawn replaces the
  warn-and-continue block from 22.1 with the recovery call.
  Spawn aborts if recovery fails — a shard with unrecoverable
  lexical indexes shouldn't accept reads.
- crates/brain-server/src/shard/tantivy_recovery_tests.rs:
  6 unit tests + 1 end-to-end spawn test (corrupt-then-respawn).
```

## 9. Confirmation

Please confirm:

1. **Synchronous startup rebuild** (vs background async + IndexUnavailable). Long boots on large indexes accepted as the v1 cost.
2. **Spawn aborts on recovery failure** (vs degrading to `lexical_retriever: None`). Operator visibility wins over availability for this class of error.
3. **No partial WAL replay** — explicit v1 scope cut; the loss bound ≤ N-1 writes per indexer is acceptable.
4. **Recovery module lives in `brain-server`** (vs `brain-ops` or `brain-index`).
