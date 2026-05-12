# Sub-task 8.8 — WAL retention worker

**Spec:** `spec/11_background_workers/07_wal_retention.md`
**Phase doc:** `docs/phases/phase-08-workers.md` §8.8
**Done when:** Old WAL segments are deleted only after their records are checkpointed. Invariant: no gap in retained LSN ranges.

---

## 1. Honest scope

Same infrastructure gap as 8.5 (HNSW maintenance): **brain-ops's `RealWriterHandle` doesn't write to a WAL yet** — Phase 9 wires that up. brain-storage already ships the WAL substrate (segments, group commit, checkpoint writer) but has no public `Wal::list_segments()` or `Wal::delete_segment()`, and the writer doesn't hold a `Wal` instance.

So 8.8 ships the same shape as 8.5:

1. **Decision logic** as a pure function — given a checkpoint + segment list + retention buffer, return the ids to delete.
2. **`WalRetentionSource` trait** — pluggable seam where Phase 9 wires `brain_storage::Wal`. Default = `DisabledWalRetentionSource` returning `Disabled`.
3. **`WalRetentionWorker`** — 1 min cadence, asks the source for `current_checkpoint() + list_segments()`, runs `decide_deletions`, calls `delete_segment(id)` for each candidate.

Out of scope:
- Real `brain_storage::Wal` integration (no public list/delete API yet) → Phase 9.
- `ADMIN_WAL_PRUNE` manual trigger → Phase 9.
- Audit-log emission (spec §8, §15) → Phase 9 observability.
- Crypto hashing of segments before deletion (spec §15) → future.
- Disk-full load shedding (spec §14) → Phase 9 admission control.

---

## 2. Pure decision logic

```rust
// crates/brain-workers/src/wal_retention.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDesc {
    pub segment_id: u64,
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointDesc {
    pub durable_lsn: u64,
}

/// Spec §11/07 §3 — return ids of segments fully covered by the
/// checkpoint, minus the retention buffer. Pure; unit-testable.
///
/// A segment is deletable iff `last_lsn < (durable_lsn - retention_extra)`.
/// The cutoff saturates at 0 (very early life of a shard).
#[must_use]
pub fn decide_deletions(
    segments: &[SegmentDesc],
    checkpoint: CheckpointDesc,
    retention_extra_lsns: u64,
) -> Vec<u64> {
    let safe_cutoff = checkpoint.durable_lsn.saturating_sub(retention_extra_lsns);
    segments
        .iter()
        .filter(|s| s.last_lsn < safe_cutoff)
        .map(|s| s.segment_id)
        .collect()
}
```

Default retention extra: **0 LSNs**. Spec §3 + §7 give the conceptual default in bytes ("256 MiB ≈ one segment's worth"); we'll let the source supply a concrete LSN count via `with_retention_extra_lsns()`. v1 leaves it at 0 because there's nothing to retain yet.

---

## 3. `WalRetentionSource` trait

```rust
#[derive(Debug, thiserror::Error)]
pub enum WalRetentionSourceError {
    /// No WAL hookup yet (v1 default).
    #[error("WAL retention source disabled")]
    Disabled,
    /// Spec §9 safety check failed.
    #[error("WAL retention source rejected operation: {0}")]
    Rejected(String),
    /// Underlying I/O / WAL error.
    #[error("WAL retention source failed: {0}")]
    Failed(String),
}

pub trait WalRetentionSource: Send + Sync + 'static {
    fn current_checkpoint(&self)
        -> Pin<Box<dyn Future<Output = Result<CheckpointDesc, WalRetentionSourceError>> + Send + '_>>;

    fn list_segments(&self)
        -> Pin<Box<dyn Future<Output = Result<Vec<SegmentDesc>, WalRetentionSourceError>> + Send + '_>>;

    fn delete_segment(&self, segment_id: u64)
        -> Pin<Box<dyn Future<Output = Result<(), WalRetentionSourceError>> + Send + '_>>;
}

pub struct DisabledWalRetentionSource;
```

Same `Pin<Box<Future>>` pattern as `Summarizer` / `RebuildSource`. To keep the `type_complexity` clippy lint happy we'll define type aliases:

```rust
pub type CheckpointFuture<'a> = Pin<Box<dyn Future<Output = Result<CheckpointDesc, WalRetentionSourceError>> + Send + 'a>>;
pub type SegmentListFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<SegmentDesc>, WalRetentionSourceError>> + Send + 'a>>;
pub type DeleteFuture<'a> = Pin<Box<dyn Future<Output = Result<(), WalRetentionSourceError>> + Send + 'a>>;
```

---

## 4. The worker

```rust
pub struct WalRetentionWorker {
    config: WorkerConfig,
    retention_extra_lsns: u64,
    source: Arc<dyn WalRetentionSource>,
}

impl WalRetentionWorker {
    pub fn new(source: Arc<dyn WalRetentionSource>) -> Self;
    pub fn with_config(self, cfg: WorkerConfig) -> Self;
    pub fn with_retention_extra_lsns(self, n: u64) -> Self;
}
```

Cycle:
```rust
async fn do_cycle(&self, ctx) -> Result<usize, WorkerError> {
    let cfg = self.config();
    if cfg.batch_size == 0 { return Ok(0); }

    let checkpoint = match self.source.current_checkpoint().await {
        Ok(c) => c,
        Err(WalRetentionSourceError::Disabled) => return Ok(0),
        Err(WalRetentionSourceError::Rejected(_)) => return Ok(0),
        Err(WalRetentionSourceError::Failed(e)) => return Err(WorkerError::Ops(format!("checkpoint: {e}"))),
    };
    let segments = match self.source.list_segments().await {
        Ok(s) => s,
        Err(WalRetentionSourceError::Disabled) => return Ok(0),
        Err(WalRetentionSourceError::Failed(e)) => return Err(WorkerError::Ops(format!("list: {e}"))),
        Err(WalRetentionSourceError::Rejected(_)) => return Ok(0),
    };

    let candidates = decide_deletions(&segments, checkpoint, self.retention_extra_lsns);
    let started = Instant::now();
    let mut deleted = 0;
    for id in candidates.into_iter().take(cfg.batch_size) {
        if started.elapsed() >= cfg.max_runtime { break; }
        if ctx.is_shutdown() { break; }
        match self.source.delete_segment(id).await {
            Ok(()) => deleted += 1,
            Err(WalRetentionSourceError::Rejected(_)) => continue,   // safety check denied, try next cycle
            Err(WalRetentionSourceError::Disabled) => break,
            Err(WalRetentionSourceError::Failed(e)) => return Err(WorkerError::Ops(format!("delete: {e}"))),
        }
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}
```

`Rejected` returns from the source aren't a worker error — they're spec §9's safety net ("if any check fails, the deletion is skipped"). The worker just moves on.

---

## 5. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-workers/src/wal_retention.rs` | NEW | `SegmentDesc`, `CheckpointDesc`, `decide_deletions`, source trait, worker |
| `crates/brain-workers/src/lib.rs` | Edit | Re-export |
| `crates/brain-workers/tests/wal_retention.rs` | NEW | ~13 tests |

No spec, wire, brain-storage, or other crate changes.

---

## 6. Tests (`tests/wal_retention.rs`)

### `decide_deletions` (6)
1. `empty_segment_list_returns_empty`.
2. `all_segments_above_cutoff_returns_empty` — segments [1000..2000], checkpoint=500 → none.
3. `segments_below_cutoff_returned` — segments {last_lsn=500, 800, 1500}, checkpoint=1000, buffer=0 → {500, 800}.
4. `retention_buffer_pushes_cutoff_back` — segments {500, 800}, checkpoint=1000, buffer=300 → only 500 (cutoff becomes 700).
5. `buffer_larger_than_checkpoint_keeps_everything` — checkpoint=100, buffer=500 → cutoff=0 → empty.
6. `last_lsn_equal_to_cutoff_is_kept` — checkpoint=1000, buffer=0, segment last_lsn=999 deleted, last_lsn=1000 kept (strict less-than).

### Source surface (3)
7. `disabled_source_returns_disabled` (each method).
8. `stub_source_returns_provided_data` — wrap fixed checkpoint + segments + track deletions.
9. `rejecting_source_makes_worker_skip_deletion` — returns `Rejected` on delete; cycle returns 0; segment list unchanged.

### Cycle (3)
10. `cycle_with_disabled_source_returns_zero`.
11. `stub_source_with_eligible_segments_deletes_and_reports_count` — 3 eligible + 2 kept → cycle returns 3; the stub's tracker shows the right ids deleted.
12. `failed_source_propagates_as_worker_error` — source returns `Failed(...)` on `list_segments` → cycle returns `Err(WorkerError::Ops)`.

### Worker integration (2)
13. `worker_registers_with_correct_kind_and_default_cadence` — 1m interval.
14. `disabled_worker_via_config_does_not_run`.

Total: 14 tests.

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Brittle: a future brain-storage `Wal::list_segments` API could deviate from `SegmentDesc` shape | The trait owns the descriptor types; Phase 9's `WalRetentionSource` impl owns the conversion |
| Retention buffer in LSNs vs bytes — spec §7 talks in bytes | Bytes/LSN ratio depends on record sizes; v1 takes LSNs for purity. Phase 9's impl converts from `wal.segment_size` |
| Worker thrashes on `Rejected` | Each cycle re-queries the source; spec §9 says "try again next cycle" |
| `Disabled` returned mid-loop (delete succeeded for some, fails on later one) | We treat `Disabled` as terminal `break`; processed count is what completed |

---

## 8. Done criteria

- [ ] `decide_deletions` pure-fn + 6 unit-style tests.
- [ ] `WalRetentionSource` + `DisabledWalRetentionSource` shipped.
- [ ] `WalRetentionWorker` implements `Worker`; default cadence 1m.
- [ ] 14 tests pass first run.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-workers): WAL retention worker (sub-task 8.8)`.

~350 LOC impl + ~450 LOC tests, single commit. No brain-storage or other-crate changes.
