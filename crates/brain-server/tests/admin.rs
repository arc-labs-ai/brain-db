//! Integration tests for the sub-task 9.13 admin HTTP server.
//!
//! Each test brings up an `AdminServer` (and where needed, a
//! `ConnectionListener` + shards) on `127.0.0.1:0`, makes a single
//! HTTP/1.1 GET, and asserts the response.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::handshake::{AuthMethod, ServerCapabilities};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[allow(dead_code)]
#[path = "../src/network/routing.rs"]
mod routing;
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/network/subscribe.rs"]
mod subscribe;
#[allow(dead_code)]
#[path = "../src/bootstrap/tls.rs"]
mod tls;

use admin::{AdminServer, AdminState};
use connection::{
    ConnectionLimits, ConnectionListener, ConnectionMetrics, ShutdownSignal, ShutdownTrigger,
    Topology,
};
use routing::RoutingTable;
use shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

// ---------------------------------------------------------------------------
// Scaffold
// ---------------------------------------------------------------------------

struct Bringup {
    admin_addr: SocketAddr,
    conn_addr: Option<SocketAddr>,
    trigger: ShutdownTrigger,
    admin_handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    listener_handle: Option<tokio::task::JoinHandle<std::io::Result<SocketAddr>>>,
    handles: Vec<ShardHandle>,
    joiners: Vec<Option<ShardJoiner>>,
    _data_dir: Option<TempDir>,
}

impl Bringup {
    async fn stop(mut self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.admin_handle).await;
        if let Some(h) = self.listener_handle.as_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
        drop(self.handles);
        for joiner in self.joiners.iter_mut().filter_map(|j| j.take()) {
            let _ = tokio::task::spawn_blocking(move || joiner.join())
                .await
                .map_err(|_| ());
        }
    }
}

/// Admin-only bringup. No shards spawned; the worker-counter test
/// uses `start_admin_with_shards` instead.
async fn start_admin_only() -> Bringup {
    let (trigger, signal) = ShutdownSignal::channel();
    let connections = Arc::new(ConnectionMetrics::default());
    let state = Arc::new(AdminState::new(Arc::new(Vec::new()), connections));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), state, signal);
    let bound = admin.bind().expect("bind admin");
    let admin_addr = bound.local_addr();
    let admin_handle = tokio::spawn(async move { bound.serve().await });

    Bringup {
        admin_addr,
        conn_addr: None,
        trigger,
        admin_handle,
        listener_handle: None,
        handles: Vec::new(),
        joiners: Vec::new(),
        _data_dir: None,
    }
}

/// Bring up shards + connection listener + admin server. Shares
/// connection metrics so a TCP connect on the connection listener
/// shows up in `/metrics`.
async fn start_admin_with_shards(n_shards: usize) -> Bringup {
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
            "brain-server/test",
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
    let bound_listener = listener.bind().expect("bind listener");
    let conn_addr = bound_listener.local_addr();
    let listener_handle = tokio::spawn(async move { bound_listener.serve().await });

    let state = Arc::new(AdminState::new(shards, connections));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), state, signal);
    let bound_admin = admin.bind().expect("bind admin");
    let admin_addr = bound_admin.local_addr();
    let admin_handle = tokio::spawn(async move { bound_admin.serve().await });

    Bringup {
        admin_addr,
        conn_addr: Some(conn_addr),
        trigger,
        admin_handle,
        listener_handle: Some(listener_handle),
        handles,
        joiners,
        _data_dir: Some(data_dir),
    }
}

/// Single-shot GET. Returns (status_code, body).
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();
    // Parse status code from the first line.
    let first_line = response.lines().next().unwrap_or("");
    let code = first_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    // Split off the body after the first blank line.
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_returns_ok() {
    let server = start_admin_only().await;
    let (code, body) = http_get(server.admin_addr, "/healthz").await;
    assert_eq!(code, 200);
    assert_eq!(body.trim(), "ok");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_emits_build_info_and_up() {
    let server = start_admin_only().await;
    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("brain_build_info{"),
        "missing brain_build_info; body:\n{body}"
    );
    assert!(
        body.contains("brain_up 1"),
        "missing brain_up; body:\n{body}"
    );
    assert!(
        body.contains("brain_shards_total 0"),
        "expected zero shards in admin-only mode; body:\n{body}"
    );
    assert!(
        body.contains("process_uptime_seconds"),
        "missing process_uptime_seconds"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_increments_connections_total_on_accept() {
    let server = start_admin_with_shards(1).await;
    let conn_addr = server.conn_addr.expect("conn_addr");

    // Open + close two TCP connections (no handshake; just the
    // accept counter).
    for _ in 0..2 {
        let s = TcpStream::connect(conn_addr).await.expect("connect");
        drop(s);
    }
    // Allow the accept loop + ConnectionGuard to update the atomic.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    // The accepted count may also include a third bookkeeping
    // connection from the prior tests' tokio scheduler; assert
    // >=2 to keep the test flake-free.
    let line = body
        .lines()
        .find(|l| l.starts_with("brain_connections_total "))
        .expect("brain_connections_total line missing");
    let value: u64 = line
        .split_whitespace()
        .last()
        .and_then(|v| v.parse().ok())
        .expect("parse counter");
    assert!(
        value >= 2,
        "expected ≥2 accepted connections, got {value}; body:\n{body}"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_emits_worker_counters() {
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    // brain-workers ships 12 Phase-8 workers per shard (sub-task 9.7b).
    // We assert presence of at least the headline cycle counters for
    // a couple of well-known names; counts are 0 (workers sleep).
    for worker in ["decay", "consolidation", "hnsw_maintenance"] {
        let needle = format!("brain_worker_cycles_total{{shard=\"0\",worker=\"{worker}\"}}");
        assert!(body.contains(&needle), "missing {needle}; body:\n{body}");
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_path_returns_400() {
    let server = start_admin_only().await;
    let (code, _body) = http_get(server.admin_addr, "/unknown").await;
    assert_eq!(code, 400);
    server.stop().await;
}
