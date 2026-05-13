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
#[derive(Debug)]
pub struct PoolGuard {
    /// `Some` while the guard is live; taken on drop to release
    /// the slot back into the pool.
    inner: Option<GuardInner>,
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
        }
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
        pool.release(slot_index, connection, Instant::now());
    }
}
