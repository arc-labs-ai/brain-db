//! Aggregate-counter maintenance worker.
//!
//! Drives the counters in `brain_metadata::tables::agg` on the shard
//! executor, like every other metadata writer here, so its commits are
//! serialized with the shard's own writes and with the metadata file copy a
//! snapshot takes:
//!
//! - **Online build.** After an upgrade the counters are built in id
//!   order, `batch_size` memories per write txn, one batch per tick. An
//!   idle tick is a single read (no commit).
//! - **Verify + repair.** Shortly after start (`first_verify_delay`) and then
//!   every `verify_interval`, recompute the counters from one read snapshot
//!   on a helper thread (read-only, so it can't disturb the shard), then
//!   apply `actual - stored` on the shard executor. Drift should never
//!   happen; a non-zero `drifted_buckets` means a write path bypassed the
//!   counters.
//! - **Change-log pruning.** With analytics on, drops the snapshot change
//!   log entries its latest full snapshot covers, a batch per tick.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_metadata::tables::agg::{
    apply_corrections, build_cursor, build_step, verify_until, MaintenanceStats, VerifyReport,
};
use brain_metadata::tables::snapshot_changes;
use brain_metadata::Lifeline;
use parking_lot::Mutex;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// `[aggregates]` knobs beyond the generic worker config.
#[derive(Debug, Clone)]
pub struct AggCountersKnobs {
    /// Memories counted per build write txn.
    pub build_batch_rows: usize,
    /// Cadence of the full verify; `Duration::ZERO` disables verifying.
    pub verify_interval: Duration,
    /// Delay before the first verify after start, so a restart (or a
    /// rollback that left counters stale) is checked soon, not a full
    /// interval later.
    pub first_verify_delay: Duration,
}

impl Default for AggCountersKnobs {
    fn default() -> Self {
        Self {
            build_batch_rows: 2_000,
            verify_interval: Duration::from_secs(24 * 3600),
            first_verify_delay: Duration::from_secs(300),
        }
    }
}

type VerifyResult = Result<VerifyReport, String>;

/// Retry delay after a verify that could not run or could not repair.
const VERIFY_RETRY: Duration = Duration::from_secs(300);

struct State {
    next_verify: Instant,
    pending: Option<Receiver<VerifyResult>>,
}

pub struct AggCountersWorker {
    config: WorkerConfig,
    knobs: AggCountersKnobs,
    stats: Arc<MaintenanceStats>,
    /// The shard's lifeline: the verify thread holds it while it has the
    /// DB open and stops when the shard does. `None` in tests.
    lifeline: Option<Lifeline>,
    /// Snapshot change-log entries up to this sequence are covered by the
    /// analytics base snapshot and pruned here. `None` without analytics.
    prune_changes_upto: Option<Arc<AtomicU64>>,
    state: Mutex<State>,
}

impl AggCountersWorker {
    #[must_use]
    pub fn new(
        knobs: AggCountersKnobs,
        stats: Arc<MaintenanceStats>,
        lifeline: Option<Lifeline>,
    ) -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::AggCounters);
        config.batch_size = knobs.build_batch_rows.max(1);
        let next_verify = Instant::now() + knobs.first_verify_delay;
        Self {
            config,
            knobs,
            stats,
            lifeline,
            prune_changes_upto: None,
            state: Mutex::new(State {
                next_verify,
                pending: None,
            }),
        }
    }

    /// Prune the analytics change log up to what `upto` says (shared with
    /// the shard's `brain_analytics::Analytics`).
    #[must_use]
    pub fn with_change_log_pruning(mut self, upto: Arc<AtomicU64>) -> Self {
        self.prune_changes_upto = Some(upto);
        self
    }

    fn verify_due(&self, st: &State) -> bool {
        !self.knobs.verify_interval.is_zero()
            && st.pending.is_none()
            && Instant::now() >= st.next_verify
    }

    fn retry_soon(&self, st: &mut State) {
        st.next_verify = Instant::now() + VERIFY_RETRY.min(self.knobs.verify_interval);
    }
}

impl Worker for AggCountersWorker {
    fn name(&self) -> &'static str {
        WorkerKind::AggCounters.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::AggCounters
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(async move { cycle(self, ctx) })
    }
}

fn ops_err(what: &str) -> impl Fn(redb::Error) -> WorkerError + '_ {
    move |e| WorkerError::Ops(format!("agg counters {what}: {e}"))
}

fn cycle(w: &AggCountersWorker, ctx: &WorkerContext) -> Result<usize, WorkerError> {
    let db = &ctx.ops.executor.metadata;

    // 1. Online build: one batch per tick, only while a cursor exists.
    let building = {
        let rtxn = db
            .read_txn()
            .map_err(|e| WorkerError::Ops(format!("agg counters rtxn: {e}")))?;
        build_cursor(&rtxn).map_err(ops_err("cursor"))?.is_some()
    };
    if building {
        let wtxn = db
            .write_txn()
            .map_err(|e| WorkerError::Ops(format!("agg counters wtxn: {e}")))?;
        let step = build_step(&wtxn, w.config.batch_size).map_err(ops_err("build"))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Ops(format!("agg counters commit: {e}")))?;
        w.stats.record_build(step);
        if let brain_metadata::tables::agg::BuildStep::Finished { .. } = step {
            tracing::info!(
                rows = w
                    .stats
                    .build_rows
                    .load(std::sync::atomic::Ordering::Relaxed),
                "aggregate counters built"
            );
        }
        return Ok(match step {
            brain_metadata::tables::agg::BuildStep::Progress { rows }
            | brain_metadata::tables::agg::BuildStep::Finished { rows } => rows,
            brain_metadata::tables::agg::BuildStep::Idle => 0,
        });
    }
    w.stats
        .record_build(brain_metadata::tables::agg::BuildStep::Idle);

    // Drop change-log entries the analytics base snapshot already covers
    // (a read-only check first, so an idle tick commits nothing).
    if let Some(upto) = w
        .prune_changes_upto
        .as_ref()
        .map(|u| u.load(AtomicOrdering::Relaxed))
    {
        let due = {
            let rtxn = db
                .read_txn()
                .map_err(|e| WorkerError::Ops(format!("agg counters rtxn: {e}")))?;
            snapshot_changes::prune_due(&rtxn, upto).map_err(ops_err("prune check"))?
        };
        if due {
            let wtxn = db
                .write_txn()
                .map_err(|e| WorkerError::Ops(format!("agg counters wtxn: {e}")))?;
            let n = snapshot_changes::prune(&wtxn, upto, w.config.batch_size)
                .map_err(ops_err("prune"))?;
            wtxn.commit()
                .map_err(|e| WorkerError::Ops(format!("agg counters commit: {e}")))?;
            return Ok(n);
        }
    }

    let mut st = w.state.lock();

    // 2. Apply a finished verify's repair on this (the shard) thread.
    let finished = match st.pending.as_ref().map(Receiver::try_recv) {
        Some(Ok(res)) => Some(res),
        Some(Err(TryRecvError::Disconnected)) => Some(Err("verify thread exited".to_owned())),
        Some(Err(TryRecvError::Empty)) | None => None,
    };
    let mut repaired = 0;
    if let Some(res) = finished {
        st.pending = None;
        match res {
            Ok(report) => {
                if !report.is_clean() {
                    tracing::error!(
                        buckets = report.drift.len(),
                        first = ?report.drift.first(),
                        "aggregate counters drifted from memories; repairing \
                         (a write path bypassed put_memory)"
                    );
                    let repair = || -> Result<(), WorkerError> {
                        let wtxn = db
                            .write_txn()
                            .map_err(|e| WorkerError::Ops(format!("agg counters wtxn: {e}")))?;
                        apply_corrections(&wtxn, &report.drift).map_err(ops_err("repair"))?;
                        wtxn.commit()
                            .map_err(|e| WorkerError::Ops(format!("agg counters commit: {e}")))
                    };
                    if let Err(e) = repair() {
                        // Not repaired: re-verify soon rather than in a day.
                        w.retry_soon(&mut st);
                        return Err(e);
                    }
                    repaired = report.drift.len();
                }
                w.stats.record_verify(&report);
            }
            Err(e) => {
                tracing::warn!(error = %e, "aggregate counter verify failed");
                w.retry_soon(&mut st);
            }
        }
    }

    // 3. Start a verify when due. The scan is read-only and long, so it
    //    runs off the shard thread; only its repair (step 2) writes.
    if w.verify_due(&st) {
        st.next_verify = Instant::now() + w.knobs.verify_interval;
        // Enter the lifeline here, on the shard thread, before the DB
        // handle is cloned: the scan thread must never hold the DB
        // without being counted. A stopping shard starts no verify.
        let guard = match w.lifeline.as_ref().map(Lifeline::enter) {
            Some(None) => return Ok(repaired),
            g => g.flatten(),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let db = db.clone();
        let line = w.lifeline.clone();
        let spawned = std::thread::Builder::new()
            .name("brain-agg-verify".into())
            .spawn(move || {
                // Bound guard-then-DB, so the DB handle drops before the
                // guard on every path.
                let _guard = guard;
                let db = db;
                let stop = || line.as_ref().is_some_and(|l| !l.is_alive());
                let res = db
                    .read_txn()
                    .map_err(|e| e.to_string())
                    .and_then(|rtxn| {
                        let now = brain_metadata::salience::now_unix_nanos();
                        verify_until(&rtxn, now, &stop).map_err(|e| e.to_string())
                    })
                    .and_then(|r| r.ok_or_else(|| "shard stopping".to_owned()));
                drop(db);
                let _ = tx.send(res);
            });
        match spawned {
            Ok(_) => st.pending = Some(rx),
            Err(e) => {
                // The unrun closure (guard + DB) dropped here, on this thread.
                tracing::warn!(error = %e, "could not start aggregate counter verify");
                w.retry_soon(&mut st);
            }
        }
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(interval: Duration, first: Duration) -> AggCountersWorker {
        AggCountersWorker::new(
            AggCountersKnobs {
                build_batch_rows: 10,
                verify_interval: interval,
                first_verify_delay: first,
            },
            Arc::default(),
            None,
        )
    }

    #[test]
    fn verify_schedule_first_delay_interval_and_retry() {
        let w = worker(Duration::from_secs(3600), Duration::ZERO);
        let mut st = w.state.lock();
        assert!(w.verify_due(&st), "due right after the zero first delay");
        st.next_verify = Instant::now() + w.knobs.verify_interval;
        assert!(!w.verify_due(&st), "then waits a full interval");
        w.retry_soon(&mut st);
        let wait = st.next_verify - Instant::now();
        assert!(
            wait <= VERIFY_RETRY,
            "a failure retries within {VERIFY_RETRY:?}"
        );
        drop(st);

        let off = worker(Duration::ZERO, Duration::ZERO);
        assert!(
            !off.verify_due(&off.state.lock()),
            "zero interval disables verify"
        );
    }
}
