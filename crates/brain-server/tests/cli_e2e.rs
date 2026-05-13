//! End-to-end: drive the in-process brain-server harness via
//! `brain_cli` library-level command functions. Sub-task 10.13.
//!
//! These tests don't go through argv parsing (that's covered by
//! brain-cli's own `tests/cli.rs`). They invoke each command's
//! `run()` directly against the harness's admin port, asserting
//! the returned string / JSON shape against real server output.

#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
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

mod support_harness;

use brain_cli::cli::OutputFormat;
use brain_cli::commands;

use support_harness::start;

/// `health` round-trips the admin /healthz endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_health_returns_ok() {
    let server = start(1).await;
    let addr = server.admin_addr.to_string();
    let out = tokio::task::spawn_blocking(move || commands::health::run(&addr, OutputFormat::Json))
        .await
        .expect("join")
        .expect("health");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["status"], "healthy", "health output: {out}");
    server.stop().await;
}

/// `stats` parses the Prometheus /metrics body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_stats_emits_brain_up() {
    let server = start(1).await;
    let addr = server.admin_addr.to_string();
    let out = tokio::task::spawn_blocking(move || commands::stats::run(&addr, OutputFormat::Json))
        .await
        .expect("join")
        .expect("stats");
    assert!(out.contains("brain_up"), "stats output: {out}");
}

/// `shard list` returns one entry per harness shard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_shard_list_matches_topology() {
    let server = start(3).await;
    let addr = server.admin_addr.to_string();
    let out =
        tokio::task::spawn_blocking(move || commands::shard::list::run(&addr, OutputFormat::Json))
            .await
            .expect("join")
            .expect("shard list");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["shards"].as_array().map(Vec::len), Some(3));
    server.stop().await;
}

/// `worker list` includes at least one known worker name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_worker_list_includes_known_worker() {
    let server = start(1).await;
    let addr = server.admin_addr.to_string();
    let out = tokio::task::spawn_blocking(move || {
        commands::worker::list::run(&addr, None, OutputFormat::Json)
    })
    .await
    .expect("join")
    .expect("worker list");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    let workers = v["workers"].as_array().expect("workers array");
    assert!(
        workers.iter().any(|w| w["name"] == "decay"),
        "expected `decay` worker in: {out}"
    );
    server.stop().await;
}

/// `debug-snapshot` returns the 10.12 partial schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_debug_snapshot_partial_schema() {
    let server = start(1).await;
    let addr = server.admin_addr.to_string();
    let out = tokio::task::spawn_blocking(move || {
        commands::diagnostics::debug_snapshot::run(&addr, 0, None, OutputFormat::Json)
    })
    .await
    .expect("join")
    .expect("debug snapshot");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["partial"], true, "snapshot must flag partial=true: {out}");
    let deferred = v["deferred"].as_array().expect("deferred array");
    let names: Vec<&str> = deferred.iter().filter_map(|s| s.as_str()).collect();
    assert!(names.contains(&"active_tasks"));
    assert!(names.contains(&"pending_requests"));
    server.stop().await;
}

/// `snapshot create` then `snapshot list` — the created id appears
/// in the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_snapshot_create_then_list_includes_id() {
    let server = start(1).await;
    let addr = server.admin_addr.to_string();
    let created = tokio::task::spawn_blocking({
        let addr = addr.clone();
        move || commands::snapshot::create::run(&addr, 0, OutputFormat::Json)
    })
    .await
    .expect("join")
    .expect("snapshot create");
    let v: serde_json::Value = serde_json::from_str(created.trim()).expect("json");
    let created_id = v["id"].as_u64().expect("id");

    let list = tokio::task::spawn_blocking(move || {
        commands::snapshot::list::run(&addr, OutputFormat::Json)
    })
    .await
    .expect("join")
    .expect("snapshot list");
    let arr: serde_json::Value = serde_json::from_str(list.trim()).expect("json");
    let ids: Vec<u64> = arr
        .as_array()
        .expect("list array")
        .iter()
        .filter_map(|e| e["id"].as_u64())
        .collect();
    assert!(
        ids.contains(&created_id),
        "created id {created_id} not in {ids:?}"
    );
    server.stop().await;
}
