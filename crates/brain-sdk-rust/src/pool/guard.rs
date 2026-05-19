//! `PoolGuard` — RAII checkout of a pool slot.
//!
//! Hands the caller `&mut Connection` for the duration of the
//! guard; releases the slot back to the pool on drop. Each release
//! wakes one waiter via the pool's `Notify`.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Instant;

use super::Connection;
use super::Pool;

/// A checked-out connection. Deref to `&mut Connection`; release
/// happens on drop.
///
/// If the caller observes a fatal error on the connection (any
/// [`crate::error::ClientError::Io`], `Closed`, or `Protocol`
/// during a send/recv), it MUST call [`Self::mark_failed`] before
/// the guard drops. On drop, a failed guard discards the
/// connection (slot → `Closed`) so the next acquire opens a fresh
/// socket. Without this, the broken connection sits in the pool's
/// Idle list and every subsequent op repeats the failure until
/// retry-exhaustion. See plan-mode write-up "Bug B" in
/// `/Users/dodo/.claude/plans/i-want-proper-doc-indexed-brook.md`
/// for the full diagnosis.
#[derive(Debug)]
pub struct PoolGuard {
    /// `Some` while the guard is live; taken on drop to release
    /// the slot back into the pool.
    inner: Option<GuardInner>,
    /// Set by [`Self::mark_failed`]. On drop the connection is
    /// discarded (slot → `Closed`) instead of returned to Idle.
    failed: bool,
}

#[derive(Debug)]
pub(super) struct GuardInner {
    pub(super) pool: Arc<Pool>,
    pub(super) slot_index: usize,
    pub(super) connection: Connection,
}

impl PoolGuard {
    pub(super) fn new(pool: Arc<Pool>, slot_index: usize, connection: Connection) -> Self {
        Self {
            inner: Some(GuardInner {
                pool,
                slot_index,
                connection,
            }),
            failed: false,
        }
    }

    /// Mark this guard as failed. On drop the slot transitions to
    /// `Closed` instead of `Idle` and the broken connection is
    /// discarded. Callers should invoke this whenever they observe
    /// a fatal error on the underlying connection (Io / Closed /
    /// Protocol). Idempotent — multiple calls are safe.
    pub fn mark_failed(&mut self) {
        self.failed = true;
    }
}

impl Deref for PoolGuard {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self
            .inner
            .as_ref()
            .expect("PoolGuard accessed after release")
            .connection
    }
}

impl DerefMut for PoolGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .inner
            .as_mut()
            .expect("PoolGuard accessed after release")
            .connection
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        let Some(GuardInner {
            pool,
            slot_index,
            connection,
        }) = self.inner.take()
        else {
            return;
        };
        if self.failed {
            // Broken connection — slot → Closed, drop the socket.
            // Don't re-pool a dead fd; the next acquire opens
            // fresh.
            pool.discard(slot_index);
            drop(connection);
        } else {
            pool.release(slot_index, connection, Instant::now());
        }
    }
}
