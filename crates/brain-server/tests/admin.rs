//! Integration tests for the admin HTTP server.
//!
//! Each test brings up an `AdminServer` (and where needed, a
//! `ConnectionListener` + shards) on `127.0.0.1:0`, makes a single
//! HTTP/1.1 GET, and asserts the response.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::connection::handshake::{AuthMethod, ServerCapabilities};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/network/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[path = "../src/metrics/mod.rs"]
mod metrics;
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
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use connection::{
    ConnectionLimits, ConnectionListener, ConnectionMetrics, ShutdownSignal, ShutdownTrigger,
    Topology,
};
use routing::RoutingTable;
use shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

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
fn stub_dispatcher() -> Arc<dyn Dispatcher> {
    Arc::new(TestStubDispatcher)
}

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
    let auth_store = {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("api_keys.redb");
        let store = Arc::new(crate::auth::AuthStore::open(&p).expect("open auth store"));
        std::mem::forget(tmp);
        store
    };
    let state = Arc::new(AdminState::new(
        Arc::new(Vec::new()),
        connections,
        Arc::new(config::Config::for_tests()),
        Arc::new(metrics::request::RequestMetrics::new()),
        auth_store,
    ));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), state, signal);
    let bound = admin.bind().await.expect("bind admin");
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
        let cfg = ShardSpawnConfig::new(data_dir.path(), stub_dispatcher());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }
    let shards: Arc<Vec<ShardHandle>> = Arc::new(handles.clone());
    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let request_metrics = Arc::new(metrics::request::RequestMetrics::new());
    let __auth_store = {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("api_keys.redb");
        let store = std::sync::Arc::new(crate::auth::AuthStore::open(&p).expect("open auth store"));
        std::mem::forget(tmp);
        store
    };
    let topology = Topology {
        shards: shards.clone(),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/test",
            vec![AuthMethod::Token],
        )),
        request_metrics: request_metrics.clone(),
        auth_store: __auth_store.clone(),
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

    let state = Arc::new(AdminState::new(
        shards,
        connections,
        Arc::new(config::Config::for_tests()),
        request_metrics,
        __auth_store.clone(),
    ));
    let admin = AdminServer::new("127.0.0.1:0".parse().unwrap(), state, signal);
    let bound_admin = admin.bind().await.expect("bind admin");
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

/// Admin bearer token configured by `config::Config::for_tests`. Every
/// `/v1/*` route is gated on it.
const ADMIN_TOKEN: &str = "test-admin-token";

/// Authed GET against a gated `/v1/*` route. Returns (status_code, body).
async fn http_get_authed(addr: SocketAddr, path: &str) -> (u16, String) {
    http_send(addr, "GET", path).await
}

/// Authed POST with an empty body against a gated `/v1/*` route.
async fn http_post_authed(addr: SocketAddr, path: &str) -> (u16, String) {
    http_send(addr, "POST", path).await
}

/// Single-shot authenticated request with an empty body.
async fn http_send(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {ADMIN_TOKEN}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();
    let first_line = response.lines().next().unwrap_or("");
    let code = first_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
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
    // config_info + process resource metrics.
    assert!(
        body.contains("brain_config_info{"),
        "missing brain_config_info; body:\n{body}"
    );
    assert!(
        body.contains("process_cpu_seconds_total"),
        "missing process_cpu_seconds_total"
    );
    assert!(
        body.contains("process_open_fds "),
        "missing process_open_fds"
    );
    assert!(
        body.contains("process_memory_resident_bytes "),
        "missing process_memory_resident_bytes"
    );
    // Connection-extended family.
    assert!(
        body.contains("brain_connections_closed_total{reason=\"bye\"}"),
        "missing brain_connections_closed_total{{reason=\"bye\"}}"
    );
    assert!(
        body.contains("brain_frame_send_total"),
        "missing brain_frame_send_total"
    );
    assert!(
        body.contains("brain_frame_recv_total"),
        "missing brain_frame_recv_total"
    );
    // Frame-size histogram lines should appear once exposition
    // walks ConnectionMetrics. Empty histogram is fine — count=0
    // still emits the bucket + _sum + _count lines.
    assert!(
        body.contains("brain_frame_size_bytes_bucket{direction=\"send\""),
        "missing brain_frame_size_bytes send buckets"
    );
    assert!(
        body.contains("brain_frame_size_bytes_bucket{direction=\"recv\""),
        "missing brain_frame_size_bytes recv buckets"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_emits_hnsw_counts() {
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("brain_hnsw_node_count{shard=\"0\"}"),
        "missing brain_hnsw_node_count; body:\n{body}"
    );
    assert!(
        body.contains("brain_hnsw_tombstone_count{shard=\"0\"}"),
        "missing brain_hnsw_tombstone_count"
    );
    assert!(
        body.contains("brain_hnsw_tombstone_ratio{shard=\"0\"}"),
        "missing brain_hnsw_tombstone_ratio"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_emits_retriever_and_query_series() {
    // The retriever_* / query_* families are always wired (recall runs
    // on every shard), so their series are present at startup with zero
    // values even before any recall is served — the v1 acceptance gate
    // requires them emitted and consistent.
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);

    for retriever in ["semantic", "lexical", "graph"] {
        let needle =
            format!("brain_retriever_invocations_total{{shard=\"0\",retriever=\"{retriever}\"}}");
        assert!(body.contains(&needle), "missing {needle}; body:\n{body}");
        let needle =
            format!("brain_retriever_candidates_total{{shard=\"0\",retriever=\"{retriever}\"}}");
        assert!(body.contains(&needle), "missing {needle}");
    }
    assert!(
        body.contains("brain_retriever_latency_ms_bucket{shard=\"0\",retriever=\"semantic\","),
        "missing brain_retriever_latency_ms histogram; body:\n{body}"
    );

    assert!(
        body.contains("brain_query_total{shard=\"0\"}"),
        "missing brain_query_total; body:\n{body}"
    );
    assert!(
        body.contains("brain_query_rerank_invoked_total{shard=\"0\"}"),
        "missing brain_query_rerank_invoked_total"
    );
    for outcome in ["single", "many", "none"] {
        let needle = format!("brain_query_outcome_total{{shard=\"0\",outcome=\"{outcome}\"}}");
        assert!(body.contains(&needle), "missing {needle}");
    }
    assert!(
        body.contains("brain_query_latency_ms_bucket{shard=\"0\","),
        "missing brain_query_latency_ms histogram"
    );
    assert!(
        body.contains("brain_query_fusion_k_bucket{shard=\"0\","),
        "missing brain_query_fusion_k histogram"
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
    // brain-workers ships 12 background workers per shard.
    // We assert presence of at least the headline cycle counters for
    // a couple of well-known names; counts are 0 (workers sleep).
    for worker in ["decay", "consolidation", "hnsw_maintenance"] {
        let needle = format!("brain_worker_cycles_total{{shard=\"0\",worker=\"{worker}\"}}");
        assert!(body.contains(&needle), "missing {needle}; body:\n{body}");
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_emits_worker_health_series() {
    // O7: panics_total / pending_work / cycle_duration_ms must be
    // rendered on /metrics, not just cycles/processed/errors/last_run.
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    // HELP lines for the newly-rendered families.
    for help in [
        "# HELP brain_worker_panics_total",
        "# HELP brain_worker_pending_work",
        "# HELP brain_worker_cycle_duration_ms",
    ] {
        assert!(
            body.contains(help),
            "missing HELP line {help}; body:\n{body}"
        );
    }
    // Per-worker series for a well-known worker.
    for needle in [
        "brain_worker_panics_total{shard=\"0\",worker=\"decay\"}",
        "brain_worker_pending_work{shard=\"0\",worker=\"decay\"}",
        "brain_worker_cycle_duration_ms{shard=\"0\",worker=\"decay\"}",
    ] {
        assert!(body.contains(needle), "missing {needle}; body:\n{body}");
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_list_reports_paused_after_stop() {
    // O6: after POST /v1/workers/<w>/stop the LIST must report
    // paused=true for that worker.
    let server = start_admin_with_shards(1).await;

    let (code, body) = http_get_authed(server.admin_addr, "/v1/workers").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("\"name\":\"decay\"") && body.contains("\"paused\":false"),
        "decay should start un-paused; body:\n{body}"
    );

    let (code, body) = http_post_authed(server.admin_addr, "/v1/workers/decay/stop").await;
    assert_eq!(code, 200, "stop should succeed; body:\n{body}");

    let (code, body) = http_get_authed(server.admin_addr, "/v1/workers").await;
    assert_eq!(code, 200);
    // The decay object must now carry paused=true. Parse it out to avoid
    // matching another worker's paused flag.
    let decay_obj = body
        .split("{\"shard\"")
        .find(|chunk| chunk.contains("\"name\":\"decay\""))
        .expect("decay object present");
    assert!(
        decay_obj.contains("\"paused\":true"),
        "decay must report paused=true after stop; obj:\n{decay_obj}"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_control_rejects_c0_and_unknown() {
    // O4: a C0 always-on worker is rejected (403, C0 reason); an
    // unregistered name still 400s; a registered C2 worker is accepted.
    let server = start_admin_with_shards(1).await;

    // C0 worker → 403 with the C0 reason.
    let (code, body) = http_post_authed(server.admin_addr, "/v1/workers/extractor/stop").await;
    assert_eq!(code, 403, "extractor is C0; body:\n{body}");
    assert!(
        body.contains("C0 always-on"),
        "rejection must state the C0 reason; body:\n{body}"
    );

    // Unregistered name → 400.
    let (code, body) = http_post_authed(server.admin_addr, "/v1/workers/nonesuch/stop").await;
    assert_eq!(code, 400, "unknown worker must 400; body:\n{body}");
    assert!(body.contains("unknown worker"), "body:\n{body}");

    // Registered C2 worker → 200.
    let (code, body) = http_post_authed(server.admin_addr, "/v1/workers/decay/stop").await;
    assert_eq!(code, 200, "decay is controllable C2; body:\n{body}");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_path_returns_404() {
    // An earlier hand-rolled admin server returned 400 for unknown
    // paths; brain-http's Router returns 404 (correct per RFC 9110
    // §15.5.5). External scrapers and admin clients are unaffected —
    // they don't hit unknown paths.
    let server = start_admin_only().await;
    let (code, _body) = http_get(server.admin_addr, "/unknown").await;
    assert_eq!(code, 404);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_ann_alias_still_works() {
    // Back-compat: POST /v1/rebuild-ann rebuilds the memory HNSW and
    // returns 201 with an entries/elapsed body.
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_post_authed(server.admin_addr, "/v1/rebuild-ann").await;
    assert_eq!(code, 201, "rebuild-ann should succeed; body:\n{body}");
    assert!(body.contains("\"entries\""), "body:\n{body}");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_index_each_target_returns_201() {
    let server = start_admin_with_shards(1).await;
    for target in [
        "memory_hnsw",
        "entity_hnsw",
        "hype_hnsw",
        "statement_question_hnsw",
        "all",
    ] {
        let path = format!("/v1/rebuild?index={target}");
        let (code, body) = http_post_authed(server.admin_addr, &path).await;
        assert_eq!(code, 201, "rebuild {target} should 201; body:\n{body}");
        assert!(
            body.contains("\"entries\""),
            "target {target} body:\n{body}"
        );
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_index_unknown_returns_400() {
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_post_authed(server.admin_addr, "/v1/rebuild?index=bogus").await;
    assert_eq!(code, 400, "unknown index must 400; body:\n{body}");
    assert!(body.contains("unknown index"), "body:\n{body}");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_index_missing_param_returns_400() {
    let server = start_admin_with_shards(1).await;
    let (code, body) = http_post_authed(server.admin_addr, "/v1/rebuild").await;
    assert_eq!(code, 400, "missing ?index= must 400; body:\n{body}");
    server.stop().await;
}
