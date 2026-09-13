//! The `Worker` trait every background worker implements + the
//! `drive_batch` helper that codifies 's
//! "bounded cycle with periodic yields" pattern.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;

/// Yield to the runtime every N processed units.
/// At 50 units the scheduler stays responsive even when batch_size is
/// in the thousands.
const YIELD_EVERY: usize = 50;

/// each worker has a `run_cycle` the scheduler calls
/// on its interval, plus stable name + config accessors so the
/// registry can index it.
///
/// **Deviation from phase doc:** the doc sketches `run_cycle(&mut self)`.
/// We use `&self` with interior mutability where needed; the writer
/// trait in `brain-planner` follows the same convention, and it avoids
/// having the scheduler own an exclusive lock on every worker.
pub trait Worker: 'static {
    /// Stable display name (matches `WorkerKind::name`).
    fn name(&self) -> &'static str;

    /// The kind this worker implements.
    fn kind(&self) -> WorkerKind;

    /// Configuration knobs (interval, batch_size, max_runtime).
    fn config(&self) -> WorkerConfig;

    /// When `true`, the scheduler sleeps one `interval` *before* this
    /// worker's first `run_cycle` instead of ticking immediately on
    /// register. Defaults to `false` — most workers tick promptly so
    /// any pending state from a previous run drains right away. The
    /// Snapshot worker overrides this to `true`: an immediate tick at
    /// shard spawn would write a CHECKPOINT_BEGIN/END pair to the WAL
    /// before any user work, shifting LSN positions in ways that
    /// surprise recovery tooling and tests.
    fn skip_first_tick(&self) -> bool {
        false
    }

    /// Execute one bounded cycle. Returns the number of units
    /// processed — the scheduler adds it to `processed_total`.
    ///
    /// Implementations typically delegate to [`drive_batch`], which
    /// honours the batch / runtime bounds and the yield discipline.
    /// Workers with monolithic cycles (e.g., HNSW rebuild) may implement
    /// their own bounded body.
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>>;
}

/// drive a stream of work-units, bounded by batch
/// size, wall-clock time, and shutdown. Yields every
/// `YIELD_EVERY` units.
///
/// `unit` returns `Ok(true)` when work was done and more *may* exist,
/// `Ok(false)` when there's nothing else to do (cycle ends early),
/// or `Err` to abort the cycle.
pub async fn drive_batch<F, Fut>(
    cfg: &WorkerConfig,
    ctx: &WorkerContext,
    mut unit: F,
) -> Result<usize, WorkerError>
where
    F: FnMut(&WorkerContext) -> Fut,
    Fut: Future<Output = Result<bool, WorkerError>>,
{
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let start = Instant::now();
    let mut processed = 0usize;
    loop {
        if processed >= cfg.batch_size {
            break;
        }
        if start.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        match unit(ctx).await? {
            true => processed += 1,
            false => break,
        }
        if processed.is_multiple_of(YIELD_EVERY) {
            glommio::executor().yield_if_needed().await;
        }
    }
    Ok(processed)
}
