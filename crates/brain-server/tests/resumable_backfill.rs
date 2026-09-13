//! `/v1/backfill` (resumable worker) — end-to-end integration.
//!
//! Distinct from `tests/extract_backfill.rs`, which drives the one-shot
//! `POST /v1/extract/backfill` re-enqueue. This suite drives the
//! resumable, checkpointed `BackfillWorker` through its HTTP admin
//! surface: `POST` submits a run and returns its id + per-shard
//! progress, `GET` snapshots progress, and `DELETE /v1/backfill/<id>`
//! flags the run for cancellation. It verifies the admin route reaches
//! the per-shard worker handle (through the shard message loop) and
//! renders well-formed JSON — the wire opcodes reject as HTTP-only.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream as StdTcpStream;
use std::time::Duration;

#[allow(dead_code)]
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

mod support_harness;

use support_harness::start_in;
use tempfile::TempDir;

/// Overall budget for an admin response.
const HTTP_READ_DEADLINE: Duration = Duration::from_secs(60);

/// Blocking HTTP request (no body) that runs inside `spawn_blocking`.
/// Returns `(status_code, body_string)`.
fn http_request(admin_addr: &str, method: &str, path: &str) -> (u16, String) {
    let mut stream = StdTcpStream::connect_timeout(
        &admin_addr.parse().expect("admin addr"),
        Duration::from_secs(5),
    )
    .expect("connect admin");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Admin /v1 routes are gated on the operator secret; Config::for_tests
    // sets it to "test-admin-token".
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {admin_addr}\r\nContent-Length: 0\r\n\
         Authorization: Bearer test-admin-token\r\n\
         Connection: close\r\nAccept: */*\r\n\r\n",
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let deadline = std::time::Instant::now() + HTTP_READ_DEADLINE;
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "admin {method} {path} produced no response within {HTTP_READ_DEADLINE:?} \
                     ({} bytes buffered)",
                    raw.len(),
                );
            }
            Err(e) => panic!(
                "admin {method} {path} read failed after {} bytes: {e}",
                raw.len()
            ),
        }
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "admin {method} {path} response has no header/body delimiter ({} bytes): {:?}",
                raw.len(),
                String::from_utf8_lossy(&raw[..raw.len().min(256)]),
            )
        });
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    (status, body)
}

/// Pull the `backfill_id` hex out of a submit response body.
fn extract_backfill_id(body: &str) -> String {
    let key = "\"backfill_id\":\"";
    let start = body.find(key).expect("backfill_id in body") + key.len();
    let rest = &body[start..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_owned()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Submit a run via `POST`, read progress via `GET`, then `DELETE` the
/// run id. Each step must reach the per-shard worker and return
/// well-formed JSON — proving the HTTP surface (not the wire) drives the
/// resumable worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_status_cancel_round_trip() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let admin_addr = server.admin_addr.to_string();

    // ── POST submit ────────────────────────────────────────────────────
    let addr = admin_addr.clone();
    let (status, body) = tokio::task::spawn_blocking(move || {
        http_request(&addr, "POST", "/v1/backfill?all&extractors=1")
    })
    .await
    .expect("join");
    assert_eq!(status, 200, "submit body: {body}");
    assert!(body.contains("\"backfill_id\":\""), "submit body: {body}");
    assert!(body.contains("\"shards\":1"), "submit body: {body}");
    assert!(body.contains("\"progress\":["), "submit body: {body}");
    let id = extract_backfill_id(&body);
    assert_eq!(id.len(), 32, "expected 32-char simple uuid, got {id:?}");

    // ── GET status ─────────────────────────────────────────────────────
    let addr = admin_addr.clone();
    let (status, body) =
        tokio::task::spawn_blocking(move || http_request(&addr, "GET", "/v1/backfill"))
            .await
            .expect("join");
    assert_eq!(status, 200, "status body: {body}");
    assert!(body.contains("\"progress\":["), "status body: {body}");
    assert!(body.contains("\"shard\":0"), "status body: {body}");

    // ── DELETE cancel ──────────────────────────────────────────────────
    let addr = admin_addr.clone();
    let path = format!("/v1/backfill/{id}");
    let (status, body) = tokio::task::spawn_blocking(move || http_request(&addr, "DELETE", &path))
        .await
        .expect("join");
    assert_eq!(status, 200, "cancel body: {body}");
    assert!(body.contains("\"cancelled\":["), "cancel body: {body}");
    assert!(body.contains("\"any_cancelled\":"), "cancel body: {body}");
    assert!(body.contains(&id), "cancel echoes run id: {body}");
}

/// A malformed run id in the `DELETE` path is a `400`, not a `500`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_rejects_bad_id_400() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let admin_addr = server.admin_addr.to_string();

    let (status, _body) = tokio::task::spawn_blocking(move || {
        http_request(&admin_addr, "DELETE", "/v1/backfill/not-a-uuid")
    })
    .await
    .expect("join");
    assert_eq!(status, 400);
}

/// A submit with a missing / empty extractor list is a `400`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_rejects_missing_extractors_400() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let admin_addr = server.admin_addr.to_string();

    let (status, _body) =
        tokio::task::spawn_blocking(move || http_request(&admin_addr, "POST", "/v1/backfill?all"))
            .await
            .expect("join");
    assert_eq!(status, 400);
}
