//! Shared in-process server harness for integration tests.
//!
//! Brings up N shards + the data-plane connection listener + the
//! admin HTTP server on ephemeral 127.0.0.1 ports, returns a
//! [`Server`] with the bound addresses. The tests in `e2e.rs`,
//! the wire integration tests share this scaffold.
//!
//! ## Why each test file still has `#[path]` mounts
//!
//! The brain-server source files use `crate::config`,
//! `crate::connection`, etc. to reach internals. For each
//! integration-test binary, the test *file* must mount those
//! modules at its own crate root so the imports resolve. This
//! file refers to them via `crate::*` and assumes the test file
//! has declared the canonical set (see `MOUNTS` doc-comment
//! below).
//!
//! Each test file MUST mount these at root:
//!
//! ```text
//! #[allow(dead_code)] #[path = "../src/admin/mod.rs"]      mod admin;
//! #[allow(dead_code)] #[path = "../src/config/mod.rs"]     mod config;
//! #[allow(dead_code)] #[path = "../src/network/connection.rs"] mod connection;
//! #[path = "../src/network/dispatch.rs"]                   mod dispatch;
//! #[path = "../src/metrics/mod.rs"]                       mod metrics;
//! #[allow(dead_code)] #[path = "../src/network/routing.rs"] mod routing;
//! #[allow(dead_code)] #[path = "../src/shard/mod.rs"]      mod shard;
//! #[path = "../src/network/subscribe.rs"]                  mod subscribe;
//! #[allow(dead_code)] #[path = "../src/bootstrap/tls.rs"]  mod tls;
//! ```

#![cfg(target_os = "linux")]
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_protocol::connection::handshake::{AuthMethod, ServerCapabilities};
use tempfile::TempDir;

use crate::admin::{AdminServer, AdminState};
use crate::config;
use crate::connection::{
    ConnectionLimits, ConnectionListener, ConnectionMetrics, ShutdownSignal, ShutdownTrigger,
    Topology,
};
use crate::routing::RoutingTable;
use crate::shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

/// Integration-test stub dispatcher. The harness never exercises
/// embedding quality and we don't want to load a real BGE model per
/// test binary.
struct TestStubDispatcher;
impl Dispatcher for TestStubDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

pub fn stub_dispatcher() -> Arc<dyn Dispatcher> {
    Arc::new(TestStubDispatcher)
}

pub struct Server {
    pub data_plane_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub trigger: ShutdownTrigger,
    pub listener_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub admin_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub handles: Vec<ShardHandle>,
    pub joiners: Vec<Option<ShardJoiner>>,
    /// `Some` when [`start`] owns the data dir (auto-cleanup on `stop`);
    /// `None` when [`start_in`] was used and the caller holds the
    /// `TempDir` (so the data dir survives `stop` for inspection).
    pub _data_dir: Option<TempDir>,
}

impl Server {
    pub async fn stop(mut self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.listener_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.admin_handle).await;
        drop(self.handles);
        for joiner in self.joiners.iter_mut().filter_map(|j| j.take()) {
            let _ = tokio::task::spawn_blocking(move || joiner.join())
                .await
                .map_err(|_| ());
        }
    }
}

pub async fn start(n_shards: usize) -> Server {
    let data_dir = TempDir::new().expect("tmp");
    let mut server = start_in(data_dir.path(), n_shards).await;
    server._data_dir = Some(data_dir);
    server
}

/// Same as [`start`] but uses a caller-supplied data directory. The
/// caller owns the directory's lifetime — useful when a test wants to
/// inspect on-disk state after `Server::stop()` returns.
pub async fn start_in(data_dir: &Path, n_shards: usize) -> Server {
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = ShardSpawnConfig::new(data_dir, stub_dispatcher());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }
    let shards: Arc<Vec<ShardHandle>> = Arc::new(handles.clone());
    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let request_metrics = Arc::new(crate::metrics::request::RequestMetrics::new());

    let auth_store_path = data_dir.join("api_keys.redb");
    let auth_store =
        Arc::new(crate::auth::AuthStore::open(&auth_store_path, false).expect("open auth store"));
    let topology = Topology {
        shards: shards.clone(),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/e2e",
            vec![AuthMethod::None],
        )),
        request_metrics: request_metrics.clone(),
        auth_store: auth_store.clone(),
    };

    let connections = Arc::new(ConnectionMetrics::default());
    let (trigger, signal) = ShutdownSignal::channel();

    let listener = ConnectionListener::new(
        "127.0.0.1:0".parse().unwrap(),
        None,
        topology,
        connections.clone(),
        ConnectionLimits::default(),
        signal.clone(),
    );
    let bound = listener.bind().expect("bind listener");
    let data_plane_addr = bound.local_addr();
    let listener_handle = tokio::spawn(async move { bound.serve().await });

    let admin_state = Arc::new(AdminState::new(
        shards,
        connections,
        Arc::new(config::Config::for_tests()),
        request_metrics,
        auth_store,
    ));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), admin_state, signal);
    let bound_admin = admin.bind().await.expect("bind admin");
    let admin_addr = bound_admin.local_addr();
    let admin_handle = tokio::spawn(async move { bound_admin.serve().await });

    Server {
        data_plane_addr,
        admin_addr,
        trigger,
        listener_handle,
        admin_handle,
        handles,
        joiners,
        _data_dir: None,
    }
}
