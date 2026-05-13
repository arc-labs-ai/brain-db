//! Shared in-process server harness for integration tests.
//!
//! Brings up N shards + the data-plane connection listener + the
//! admin HTTP server on ephemeral 127.0.0.1 ports, returns a
//! [`Server`] with the bound addresses. The tests in `e2e.rs`,
//! `sdk_e2e.rs`, and `cli_e2e.rs` all share this scaffold.
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
//! #[allow(dead_code)] #[path = "../src/network/routing.rs"] mod routing;
//! #[allow(dead_code)] #[path = "../src/shard/mod.rs"]      mod shard;
//! #[path = "../src/network/subscribe.rs"]                  mod subscribe;
//! #[allow(dead_code)] #[path = "../src/bootstrap/tls.rs"]  mod tls;
//! ```

#![cfg(target_os = "linux")]
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::handshake::{AuthMethod, ServerCapabilities};
use tempfile::TempDir;

use crate::admin::{AdminServer, AdminState};
use crate::config;
use crate::connection::{
    ConnectionLimits, ConnectionListener, ConnectionMetrics, ShutdownSignal, ShutdownTrigger,
    Topology,
};
use crate::routing::RoutingTable;
use crate::shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

pub struct Server {
    pub data_plane_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub trigger: ShutdownTrigger,
    pub listener_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub admin_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    pub handles: Vec<ShardHandle>,
    pub joiners: Vec<Option<ShardJoiner>>,
    pub _data_dir: TempDir,
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
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = ShardSpawnConfig::new(data_dir.path());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }
    let shards: Arc<Vec<ShardHandle>> = Arc::new(handles.clone());
    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let topology = Topology {
        shards: shards.clone(),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/e2e",
            vec![AuthMethod::None],
        )),
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
    ));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), admin_state, signal);
    let bound_admin = admin.bind().expect("bind admin");
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
        _data_dir: data_dir,
    }
}
