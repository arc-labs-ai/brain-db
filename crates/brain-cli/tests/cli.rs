//! Integration test for sub-task 10.8.
//!
//! Spawns a mock HTTP server bound to `127.0.0.1:0` that returns
//! the canned `/healthz` + `/metrics` responses, then calls the
//! command functions directly.

use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::{health, stats};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a tokio runtime + mock HTTP server in a dedicated
/// thread (so the blocking command functions can call back into
/// it via the std-library TCP path). Returns the bound address.
fn spawn_mock_admin(metrics_body: &'static str, health_body: &'static str) -> SocketAddr {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            addr_tx.send(addr).expect("send");
            loop {
                let (mut socket, _peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    // Read request bytes up to "\r\n\r\n" — all
                    // we care about is the path on the first line.
                    let mut buf = [0u8; 1024];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("");
                    let (status, body) = match path {
                        "/healthz" => (200, health_body),
                        "/metrics" => (200, metrics_body),
                        _ => (404, "not found"),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        len = body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
    });
    addr_rx.recv_timeout(Duration::from_secs(5)).expect("addr")
}

#[test]
fn health_table_output() {
    let addr = spawn_mock_admin("brain_x 1\n", "ok");
    let server = addr.to_string();
    let out = health::run(&server, OutputFormat::Table).expect("health");
    assert!(out.contains("status"));
    assert!(out.contains("healthy"));
    assert!(out.contains(&server));
}

#[test]
fn health_json_output() {
    let addr = spawn_mock_admin("", "ok");
    let server = addr.to_string();
    let out = health::run(&server, OutputFormat::Json).expect("health");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["status"], "healthy");
    assert_eq!(v["probe"], "/healthz");
}

#[test]
fn health_unreachable_reports_status() {
    // Bind + drop to get a closed port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let out = health::run(&addr.to_string(), OutputFormat::Json).expect("health");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert!(v["status"].as_str().unwrap().starts_with("unreachable"));
}

#[test]
fn stats_json_round_trip() {
    let metrics = "# TYPE brain_connections_total counter\n\
                   brain_connections_total 7\n\
                   brain_admin_requests_total{endpoint=\"/healthz\"} 4\n\
                   brain_admin_requests_total{endpoint=\"/metrics\"} 2\n";
    let addr = spawn_mock_admin(metrics, "ok");
    let server = addr.to_string();
    let out = stats::run(&server, OutputFormat::Json).expect("stats");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["brain_connections_total"][0]["value"], 7.0);
    let admin = v["brain_admin_requests_total"].as_array().unwrap();
    assert_eq!(admin.len(), 2);
}

#[test]
fn stats_table_output_includes_metric_lines() {
    let metrics = "brain_x 42\nbrain_y{shard=\"0\"} 1\n";
    let addr = spawn_mock_admin(metrics, "ok");
    let server = addr.to_string();
    let out = stats::run(&server, OutputFormat::Table).expect("stats");
    assert!(out.contains("brain_x"));
    assert!(out.contains("42"));
    assert!(out.contains("brain_y{shard=0}"));
}
