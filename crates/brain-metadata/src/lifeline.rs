//! Shard lifeline: lets background jobs that read a shard's `MetadataDb`
//! off the shard thread (aggregate verify, analytics export) stop when the
//! shard stops, and lets the shard wait until they have let go of the DB.
//!
//! Without it, such a job can hold the last `Arc<MetadataDb>` after the
//! shard exits. redb's `Database::drop` then runs on that thread later —
//! after a restore may already have replaced the file — or never, if the
//! process exits first (forcing a repair on the next open).
//!
//! Protocol: a job calls [`Lifeline::enter`] before touching the DB and
//! keeps the guard until *after* it has dropped its DB handle; inside long
//! loops it checks [`Lifeline::is_alive`] and stops when false. The shard
//! calls [`LifelineOwner::shutdown`] on exit, which flips the lifeline and
//! waits for every guard to drop.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long dropping an owner without [`LifelineOwner::shutdown`] (e.g. a
/// failed shard spawn) waits for jobs to leave.
const DROP_WAIT: Duration = Duration::from_secs(30);

/// Held by the shard. [`Self::shutdown`] — or dropping it, on any path —
/// ends the lifeline and waits for every job to leave.
#[derive(Debug)]
pub struct LifelineOwner {
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicUsize>,
    done: bool,
}

/// Handed to background jobs.
#[derive(Debug, Clone)]
pub struct Lifeline {
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicUsize>,
}

/// Proof a job is inside the lifeline; drop it after dropping DB handles.
#[derive(Debug)]
pub struct LifelineGuard {
    busy: Arc<AtomicUsize>,
}

impl Drop for LifelineGuard {
    fn drop(&mut self) {
        self.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for LifelineOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl LifelineOwner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            busy: Arc::new(AtomicUsize::new(0)),
            done: false,
        }
    }

    #[must_use]
    pub fn lifeline(&self) -> Lifeline {
        Lifeline {
            alive: self.alive.clone(),
            busy: self.busy.clone(),
        }
    }

    /// End the lifeline and wait up to `timeout` for every job to leave.
    /// Returns `false` on timeout (a job ignored [`Lifeline::is_alive`]).
    #[must_use]
    pub fn shutdown(mut self, timeout: Duration) -> bool {
        self.done = true;
        self.stop_and_wait(timeout)
    }

    fn stop_and_wait(&self, timeout: Duration) -> bool {
        // SeqCst on both sides (this store / `busy` loads, and `enter`'s
        // `busy` increment / `alive` load) rules out the store-buffering
        // outcome where each side misses the other's write.
        self.alive.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        while self.busy.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

impl Drop for LifelineOwner {
    fn drop(&mut self) {
        if !self.done && !self.stop_and_wait(DROP_WAIT) {
            tracing::warn!("background jobs still held the shard DB after {DROP_WAIT:?}");
        }
    }
}

impl Lifeline {
    /// Register a job. `None` once the shard is stopping.
    #[must_use]
    pub fn enter(&self) -> Option<LifelineGuard> {
        // Count first, then check: a shutdown that saw the count at zero
        // stored `alive = false` before that load, so this entry sees it
        // and backs out (its guard drops, restoring the count).
        self.busy.fetch_add(1, Ordering::SeqCst);
        let guard = LifelineGuard {
            busy: self.busy.clone(),
        };
        self.is_alive().then_some(guard)
    }

    /// `false` once the shard is stopping; long loops should stop.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_waits_for_jobs_and_refuses_new_ones() {
        let owner = LifelineOwner::new();
        let line = owner.lifeline();
        let g = line.enter().expect("alive");
        let worker_line = line.clone();
        let t = std::thread::spawn(move || {
            while worker_line.is_alive() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(30));
            drop(g);
        });
        assert!(owner.shutdown(Duration::from_secs(5)));
        t.join().unwrap();
        assert!(line.enter().is_none(), "no entry after shutdown");
        assert!(!line.is_alive());
    }

    #[test]
    fn dropping_the_owner_also_waits_for_jobs() {
        let owner = LifelineOwner::new();
        let line = owner.lifeline();
        let g = line.enter().unwrap();
        let t = std::thread::spawn(move || {
            while line.is_alive() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(30));
            drop(g);
            Instant::now()
        });
        drop(owner); // e.g. a failed spawn
        let left_at = t.join().unwrap();
        assert!(
            Instant::now() >= left_at,
            "drop returned only after the job left"
        );
    }

    #[test]
    fn shutdown_times_out_on_a_stuck_job() {
        let owner = LifelineOwner::new();
        let _stuck = owner.lifeline().enter().unwrap();
        assert!(!owner.shutdown(Duration::from_millis(20)));
    }
}
