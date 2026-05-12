//! Per-shard Glommio executor scaffold.
//!
//! One OS thread per shard, hosting a `glommio::LocalExecutor` (single-threaded,
//! io_uring-driven). The Tokio connection layer talks to a shard through a
//! `flume::Sender<ShardRequest>`; replies come back through per-call
//! `flume::Sender<...>` carried in the request. Flume's `send_async` /
//! `recv_async` are reactor-agnostic — both ends `.await` natively under
//! whichever runtime drives them.
//!
//! Lifecycle:
//!
//! ```text
//!   spawn_shard() ─▶ (ShardHandle, ShardJoiner)
//!                       │              │
//!                       │              │  (single-ownership;
//!                       │              │   not cloneable)
//!                       ▼              ▼
//!                 clone freely;  used by graceful
//!                 each clone     shutdown to await
//!                 owns a Sender  the thread's exit
//!                       │
//!                       ▼  (drop every clone)
//!                 channel closes ─▶ shard_main_loop exits ─▶ joiner.join() returns
//! ```
//!
//! The two-handle split avoids a deadlock that would arise if `Drop` tried
//! to call `ExecutorJoinHandle::join` while the very sender it needs to
//! drop is still a field of `self` (fields drop *after* the Drop body
//! returns, so the channel would never close, and the join would hang).
//!
//! Spec §01/04 (layers), §01/05 (hardware: io_uring, CPU pinning),
//! §10/02 (single writer per shard). Audit `phase-09-glommio-port.md` §7
//! locks flume as the boundary primitive; §8.2 defers the in-shard
//! `Rc<Cell<bool>>` shutdown flag to 9.7.

#![cfg(target_os = "linux")]

use brain_core::ShardId;
use flume::{Receiver, Sender};
use glommio::{ExecutorJoinHandle, LocalExecutorBuilder, Placement};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Request type — extended by 9.10 with `Frame { req, reply_tx }`.
// ---------------------------------------------------------------------------

pub(crate) enum ShardRequest {
    /// Trivial round-trip. The shard replies with `()`. Used by 9.4
    /// integration tests; the connection layer (9.9+) replaces this
    /// with real wire-frame requests.
    Ping { reply_tx: Sender<()> },
}

// ---------------------------------------------------------------------------
// Spawn config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ShardSpawnConfig {
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
}

impl Default for ShardSpawnConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            pin_cpu: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,

    #[error("failed to launch Glommio executor: {0}")]
    Spawn(String),

    #[error("failed to join shard executor thread: {0}")]
    Join(String),
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Cloneable, `Send + Sync` handle the connection layer (Tokio) holds.
/// Each clone holds a `flume::Sender`. When every clone drops, the
/// shard's request channel closes and the executor's main loop exits.
/// The thread itself is awaited through [`ShardJoiner::join`].
#[derive(Clone)]
pub struct ShardHandle {
    shard_id: ShardId,
    tx: Sender<ShardRequest>,
}

impl ShardHandle {
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Round-trip Ping. Returns once the shard has replied.
    pub async fn ping(&self) -> Result<(), ShardError> {
        let (reply_tx, reply_rx) = flume::bounded::<()>(1);
        self.tx
            .send_async(ShardRequest::Ping { reply_tx })
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| ShardError::ShardDisconnected)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Joiner — single-ownership, used by graceful shutdown
// ---------------------------------------------------------------------------

/// One-shot ownership of the shard's OS thread. Returned alongside
/// [`ShardHandle`] from [`spawn_shard`]. Call [`ShardJoiner::join`] *after*
/// every `ShardHandle` clone has been dropped to wait for the executor's
/// thread to exit cleanly. Forgetting to call `join()` leaks the thread
/// (it becomes a daemon and stops on process exit) — acceptable in tests
/// and prototype code, but production callers (the connection layer in
/// 9.9 / graceful shutdown in 9.14) MUST call it.
pub struct ShardJoiner {
    shard_id: ShardId,
    handle: Option<ExecutorJoinHandle<()>>,
}

impl ShardJoiner {
    /// Block the current thread until the shard's executor exits.
    pub fn join(mut self) -> Result<(), ShardError> {
        let Some(h) = self.handle.take() else {
            return Ok(());
        };
        match h.join() {
            Ok(()) => {
                info!(shard_id = self.shard_id, "shard joined cleanly");
                Ok(())
            }
            Err(e) => Err(ShardError::Join(e.to_string())),
        }
    }
}

impl Drop for ShardJoiner {
    fn drop(&mut self) {
        if self.handle.is_some() {
            warn!(
                shard_id = self.shard_id,
                "ShardJoiner dropped without calling join(); thread will leak"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Public spawn entry point
// ---------------------------------------------------------------------------

/// Launch a shard on a dedicated OS thread + `LocalExecutor`. Returns the
/// cloneable [`ShardHandle`] used to send work and the single-ownership
/// [`ShardJoiner`] used during graceful shutdown.
pub fn spawn_shard(
    shard_id: ShardId,
    cfg: ShardSpawnConfig,
) -> Result<(ShardHandle, ShardJoiner), ShardError> {
    let (tx, rx) = flume::bounded::<ShardRequest>(cfg.channel_capacity);
    let placement = match cfg.pin_cpu {
        Some(cpu) => Placement::Fixed(cpu),
        None => Placement::Unbound,
    };
    let join_handle = LocalExecutorBuilder::new(placement)
        .name(&format!("brain-shard-{shard_id}"))
        .spawn(move || async move {
            shard_main_loop(shard_id, rx).await;
        })
        .map_err(|e| ShardError::Spawn(e.to_string()))?;
    let handle = ShardHandle { shard_id, tx };
    let joiner = ShardJoiner {
        shard_id,
        handle: Some(join_handle),
    };
    Ok((handle, joiner))
}

// ---------------------------------------------------------------------------
// Shard main loop
// ---------------------------------------------------------------------------

async fn shard_main_loop(shard_id: ShardId, rx: Receiver<ShardRequest>) {
    info!(shard_id, "shard executor entering main loop");
    while let Ok(req) = rx.recv_async().await {
        match req {
            ShardRequest::Ping { reply_tx } => {
                if reply_tx.send_async(()).await.is_err() {
                    warn!(shard_id, "Ping reply dropped (caller gone)");
                }
            }
        }
    }
    info!(shard_id, "shard main loop exiting (channel closed)");
}

// ---------------------------------------------------------------------------
// Compile-time invariants
// ---------------------------------------------------------------------------

const _: fn() = || {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<ShardHandle>();
    require_send_sync::<Sender<ShardRequest>>();
    // ShardJoiner is intentionally Send but NOT Sync — it's single-owner.
    fn require_send<T: Send>() {}
    require_send::<ShardJoiner>();
};

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_handle_is_send_sync_compile_check() {
        // Statically asserted above; this test exists so the file's
        // intent is discoverable from `cargo test` output.
    }

    #[test]
    fn shard_spawn_config_default() {
        let cfg = ShardSpawnConfig::default();
        assert_eq!(cfg.channel_capacity, 1024);
        assert_eq!(cfg.pin_cpu, None);
    }

    #[test]
    fn spawn_unbound_and_join() {
        let (handle, joiner) = spawn_shard(0, ShardSpawnConfig::default())
            .expect("Glommio spawn should succeed with Unbound placement");
        assert_eq!(handle.shard_id(), 0);
        drop(handle);
        joiner.join().expect("shard should join cleanly");
    }
}
