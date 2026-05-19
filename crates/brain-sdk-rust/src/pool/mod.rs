//! `Pool` — per-server connection pool.
//!
//! Backs `Client`. The pool keeps `min..=max` connections to one
//! brain-server, hands them out via [`PoolGuard`] (RAII checkout),
//! reaps idle ones past `idle_timeout`, and rejects new acquires
//! with `ClientError::Overloaded` once `acquire_timeout` fires at
//! the `max` cap.

pub mod config;
pub mod connection;
mod guard;

pub use config::PoolConfig;
pub use connection::{Connection, IdleConnection};
pub use guard::PoolGuard;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::AgentId;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::config::ClientConfig;
use crate::error::ClientError;

/// Per-slot lifecycle state.
#[derive(Debug)]
enum SlotState {
    /// Slot owns an [`IdleConnection`] (background task running, auto-
    /// pongs SERVER_PING). Nobody's using it for an op.
    Idle {
        connection: IdleConnection,
        last_used: Instant,
    },
    /// Slot is checked out via a `PoolGuard`. The connection
    /// itself lives in the guard; we keep the marker here so the
    /// pool's vec doesn't shift indices on release.
    InUse,
    /// Slot was closed (reaper, error, or shutdown). The reaper
    /// compacts these on its next pass.
    Closed,
}

/// Connection pool. Construct via [`Pool::new`] (lazy — no
/// connections opened until [`Pool::warm_up`] or [`Pool::acquire`]
/// is called).
pub struct Pool {
    addr: SocketAddr,
    agent_id: AgentId,
    config: ClientConfig,
    slots: Mutex<Vec<SlotState>>,
    /// Woken on every release so waiters can re-try acquire.
    release_notify: Notify,
    /// `true` once [`Pool::close`] runs; further `acquire` calls
    /// short-circuit with `PoolClosed`.
    closed: AtomicBool,
    /// Signal the reaper task to exit. Sent on close.
    shutdown_notify: Notify,
}

impl Pool {
    /// Construct an empty pool. No connections are opened until
    /// `warm_up()` or `acquire()`. Spawns the idle-reaper task on
    /// the current Tokio runtime.
    #[must_use]
    pub fn new(addr: SocketAddr, agent_id: AgentId, config: ClientConfig) -> Arc<Self> {
        let pool = Arc::new(Self {
            addr,
            agent_id,
            config,
            slots: Mutex::new(Vec::new()),
            release_notify: Notify::new(),
            closed: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        });
        // Spawn the reaper. The Weak ref means a dropped Pool
        // doesn't keep the task alive.
        let weak = Arc::downgrade(&pool);
        let interval = (pool.config.pool.idle_timeout / 4).max(Duration::from_millis(100));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = async {
                        // Capture the shutdown_notify reference for the
                        // duration of one wait — once the strong count
                        // drops to zero (the user dropped the only Pool
                        // handle), the upgrade below fails and we exit.
                        if let Some(p) = weak.upgrade() {
                            p.shutdown_notify.notified().await;
                        }
                    } => break,
                }
                let Some(p) = weak.upgrade() else { break };
                p.reap_idle();
                if p.closed.load(Ordering::Acquire) {
                    break;
                }
            }
        });
        pool
    }

    /// Pre-establish `min_connections` connections in parallel.
    /// Errors if any handshake fails — the pool is left empty so
    /// the caller can retry.
    pub async fn warm_up(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ClientError::PoolClosed);
        }
        let min = self.config.pool.min_connections as usize;
        let mut handles = Vec::with_capacity(min);
        for _ in 0..min {
            let addr = self.addr;
            let agent_id = self.agent_id;
            let config = self.config.clone();
            handles.push(tokio::spawn(async move {
                Connection::open(addr, agent_id, &config).await
            }));
        }
        let mut opened: Vec<Connection> = Vec::with_capacity(min);
        for h in handles {
            match h.await {
                Ok(Ok(c)) => opened.push(c),
                Ok(Err(e)) => return Err(e),
                Err(join_err) => return Err(ClientError::Internal(join_err.to_string())),
            }
        }
        let now = Instant::now();
        let mut slots = self.slots.lock();
        for c in opened {
            slots.push(SlotState::Idle {
                connection: IdleConnection::from_active(c),
                last_used: now,
            });
        }
        Ok(())
    }

    /// Acquire a free connection, opening a new one if there's
    /// headroom and no idle slot. Waits up to
    /// `config.pool.acquire_timeout` for a release before
    /// returning `ClientError::Overloaded`.
    pub async fn acquire(self: &Arc<Self>) -> Result<PoolGuard, ClientError> {
        let deadline = Instant::now() + self.config.pool.acquire_timeout;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(ClientError::PoolClosed);
            }
            // Try to grab an idle slot first. May fail if the
            // background pong task died — slot gets marked Closed,
            // we retry (loop will then try_open_new).
            match self.try_take_idle().await {
                Some(Ok(guard)) => return Ok(guard),
                Some(Err(_e)) => continue,
                None => {}
            }
            // Or open a fresh one if we're under cap.
            if let Some(guard) = self.try_open_new().await? {
                return Ok(guard);
            }
            // Wait for a release or the deadline.
            let now = Instant::now();
            if now >= deadline {
                return Err(ClientError::Overloaded {
                    detail: format!(
                        "no free pool slot within {:?} (cap = {})",
                        self.config.pool.acquire_timeout, self.config.pool.max_connections
                    ),
                });
            }
            let remaining = deadline - now;
            let notified = self.release_notify.notified();
            match tokio::time::timeout(remaining, notified).await {
                Ok(()) => { /* re-try; loop */ }
                Err(_) => {
                    return Err(ClientError::Overloaded {
                        detail: format!(
                            "no free pool slot within {:?} (cap = {})",
                            self.config.pool.acquire_timeout, self.config.pool.max_connections
                        ),
                    })
                }
            }
        }
    }

    /// Close the pool: prevents further `acquire`, signals the
    /// reaper, and drops every idle connection (best-effort —
    /// connections currently checked out keep working until their
    /// guard is dropped, at which point `release` notices the
    /// `closed` flag and discards them).
    pub fn close(self: &Arc<Self>) {
        self.closed.store(true, Ordering::Release);
        self.shutdown_notify.notify_one();
        let mut slots = self.slots.lock();
        for idx in 0..slots.len() {
            if matches!(slots[idx], SlotState::Idle { .. }) {
                slots[idx] = SlotState::Closed;
            }
        }
        // Wake any pending acquire so it can observe PoolClosed.
        self.release_notify.notify_waiters();
    }

    /// Number of currently-tracked slots (idle + in-use). Useful
    /// for tests; not a stable public surface.
    #[must_use]
    pub fn live_slots(&self) -> usize {
        let slots = self.slots.lock();
        slots
            .iter()
            .filter(|s| !matches!(s, SlotState::Closed))
            .count()
    }

    /// Pull one `Idle` slot out, reactivate it (cancel the
    /// background pong task, recover the stream), return a guard.
    ///
    /// Three outcomes:
    ///   - `None` — no Idle slot in the pool; caller should
    ///     `try_open_new` or wait.
    ///   - `Some(Ok(guard))` — reactivated successfully.
    ///   - `Some(Err(e))` — slot was Idle but reactivation failed
    ///     (the background pong task exited on a fatal I/O or
    ///     protocol error before we asked for the stream back). The
    ///     slot is marked `Closed` and the caller retries — the
    ///     next iteration of `acquire`'s loop will try_open_new and
    ///     get a fresh connection.
    async fn try_take_idle(
        self: &Arc<Self>,
    ) -> Option<Result<PoolGuard, ClientError>> {
        // Step 1: synchronously pull the IdleConnection out and
        // mark the slot InUse. Release the lock immediately so the
        // (potentially-slow) reactivate await doesn't block other
        // pool operations.
        let (idx, idle) = {
            let mut slots = self.slots.lock();
            let mut found: Option<(usize, IdleConnection)> = None;
            for i in 0..slots.len() {
                if matches!(slots[i], SlotState::Idle { .. }) {
                    let prev = std::mem::replace(&mut slots[i], SlotState::InUse);
                    let SlotState::Idle { connection, .. } = prev else {
                        unreachable!()
                    };
                    found = Some((i, connection));
                    break;
                }
            }
            found?
        };

        // Step 2: cancel the background pong task and recover the
        // stream. If the bg task already died, mark the slot Closed.
        match idle.into_active().await {
            Ok(conn) => Some(Ok(PoolGuard::new(self.clone(), idx, conn))),
            Err(e) => {
                {
                    let mut slots = self.slots.lock();
                    slots[idx] = SlotState::Closed;
                }
                self.release_notify.notify_one();
                Some(Err(e))
            }
        }
    }

    /// Open a fresh connection if we're under the cap. Returns
    /// `Ok(None)` if we're at the cap (caller should wait).
    async fn try_open_new(self: &Arc<Self>) -> Result<Option<PoolGuard>, ClientError> {
        let max = self.config.pool.max_connections as usize;
        // Reserve a slot index before doing any network I/O so
        // concurrent acquires don't double-open.
        let reservation = {
            let mut slots = self.slots.lock();
            let live = slots
                .iter()
                .filter(|s| !matches!(s, SlotState::Closed))
                .count();
            if live >= max {
                return Ok(None);
            }
            // Re-use a `Closed` slot if there's one in the vec; else push.
            if let Some(i) = slots.iter().position(|s| matches!(s, SlotState::Closed)) {
                slots[i] = SlotState::InUse;
                i
            } else {
                slots.push(SlotState::InUse);
                slots.len() - 1
            }
        };
        // Open the connection (drops the mutex first).
        let res = Connection::open(self.addr, self.agent_id, &self.config).await;
        match res {
            Ok(conn) => Ok(Some(PoolGuard::new(self.clone(), reservation, conn))),
            Err(e) => {
                // Undo the reservation.
                let mut slots = self.slots.lock();
                slots[reservation] = SlotState::Closed;
                self.release_notify.notify_one();
                Err(e)
            }
        }
    }

    /// Called by `PoolGuard::drop`. Returns the connection to the
    /// slot (or discards if the pool is closing) and wakes one
    /// waiter.
    pub(super) fn release(&self, slot_index: usize, connection: Connection, last_used: Instant) {
        {
            let mut slots = self.slots.lock();
            // The slot index is stable because we only ever swap
            // `Idle ↔ InUse` and replace `Closed` slots in place.
            if self.closed.load(Ordering::Acquire) {
                slots[slot_index] = SlotState::Closed;
            } else {
                // Wrap the recovered connection in an IdleConnection
                // that spawns the background pong task. Without this,
                // the slot would sit silently and the server would
                // close it after `idle_timeout + ping_timeout`
                // (spec §03/02 §6.1).
                slots[slot_index] = SlotState::Idle {
                    connection: IdleConnection::from_active(connection),
                    last_used,
                };
            }
        }
        self.release_notify.notify_one();
    }

    /// Called by [`super::PoolGuard::drop`] when the op observed a
    /// fatal error on the connection. Slot → `Closed`, broken
    /// socket dropped by caller; next acquire opens a fresh
    /// connection in this slot's place. Wakes one waiter so a
    /// retry can proceed without waiting for the (impossible)
    /// release of this stale slot.
    pub(super) fn discard(&self, slot_index: usize) {
        {
            let mut slots = self.slots.lock();
            slots[slot_index] = SlotState::Closed;
        }
        self.release_notify.notify_one();
    }

    /// Reaper pass — drop `Idle` slots whose `last_used` is past
    /// `idle_timeout`. Respects `min_connections`.
    fn reap_idle(&self) {
        let now = Instant::now();
        let mut slots = self.slots.lock();
        let live: usize = slots
            .iter()
            .filter(|s| !matches!(s, SlotState::Closed))
            .count();
        if live as u32 <= self.config.pool.min_connections {
            return;
        }
        let mut budget = live - self.config.pool.min_connections as usize;
        for idx in 0..slots.len() {
            if budget == 0 {
                break;
            }
            let stale = matches!(&slots[idx], SlotState::Idle { last_used, .. } if now.duration_since(*last_used) >= self.config.pool.idle_timeout);
            if stale {
                slots[idx] = SlotState::Closed;
                budget -= 1;
            }
        }
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("addr", &self.addr)
            .field("agent_id", &self.agent_id)
            .field("live_slots", &self.live_slots())
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}
