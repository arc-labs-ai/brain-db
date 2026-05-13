//! `brain-cli snapshot` integration tests against a mock admin
//! HTTP server. Mirrors the pattern from `tests/cli.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::snapshot;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Shared snapshot store the mock server uses. Each integration
/// test creates its own.
#[derive(Default)]
struct MockState {
    next_id: u64,
    snapshots: Vec<(u64, u64, u64)>, // (id, taken_at, size)
}

fn spawn_mock_admin(state: Arc<Mutex<MockState>>) -> SocketAddr {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            addr_tx.send(addr).expect("send");
            loop {
                let (mut socket, _peer) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let st = state.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first_line = req.lines().next().unwrap_or("");
                    let mut parts = first_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");

                    let (status, body) = handle(&st, method, path);
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

fn handle(state: &Arc<Mutex<MockState>>, method: &str, path: &str) -> (u16, String) {
    // Strip query.
    let path_only = path.split('?').next().unwrap_or(path);

    match (method, path_only) {
        ("POST", "/v1/snapshots") => {
            let mut s = state.lock().unwrap();
            s.next_id += 1;
            let id = s.next_id;
            s.snapshots.push((id, 1_700_000_000_000_000_000, 4096));
            (201, format!("{{\"id\":{id},\"shard\":0}}\n"))
        }
        ("GET", "/v1/snapshots") => {
            let s = state.lock().unwrap();
            let mut body = String::from("[");
            for (i, (id, ts, sz)) in s.snapshots.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&format!(
                    "{{\"shard\":0,\"id\":{id},\"taken_at_unix_nanos\":{ts},\"size_bytes\":{sz}}}"
                ));
            }
            body.push_str("]\n");
            (200, body)
        }
        ("DELETE", p) if p.starts_with("/v1/snapshots/") => {
            let id_str = &p["/v1/snapshots/".len()..];
            let id: u64 = match id_str.parse() {
                Ok(id) => id,
                Err(_) => return (400, "bad id\n".into()),
            };
            let mut s = state.lock().unwrap();
            let len_before = s.snapshots.len();
            s.snapshots.retain(|(sid, _, _)| *sid != id);
            if s.snapshots.len() == len_before {
                (404, "not found\n".into())
            } else {
                (204, String::new())
            }
        }
        _ => (404, "not found\n".into()),
    }
}

#[test]
fn snapshot_create_then_list() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let addr = spawn_mock_admin(state.clone());
    let server = addr.to_string();

    let created = snapshot::create::run(&server, 0, OutputFormat::Json).expect("create");
    let v: serde_json::Value = serde_json::from_str(created.trim()).expect("json");
    assert_eq!(v["id"], 1);
    assert_eq!(v["shard"], 0);

    let listed = snapshot::list::run(&server, OutputFormat::Json).expect("list");
    let v: serde_json::Value = serde_json::from_str(listed.trim()).expect("json");
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], 1);
}

#[test]
fn snapshot_create_then_delete() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let addr = spawn_mock_admin(state.clone());
    let server = addr.to_string();

    let created = snapshot::create::run(&server, 0, OutputFormat::Json).expect("create");
    let v: serde_json::Value = serde_json::from_str(created.trim()).expect("json");
    let id = v["id"].as_u64().expect("id");

    let deleted = snapshot::delete::run(&server, id, 0, OutputFormat::Json).expect("delete");
    let v: serde_json::Value = serde_json::from_str(deleted.trim()).expect("json");
    assert_eq!(v["status"], "deleted");

    let listed = snapshot::list::run(&server, OutputFormat::Json).expect("list");
    let v: serde_json::Value = serde_json::from_str(listed.trim()).expect("json");
    assert_eq!(v.as_array().unwrap().len(), 0);
}

#[test]
fn snapshot_list_empty() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let addr = spawn_mock_admin(state);
    let server = addr.to_string();

    let listed = snapshot::list::run(&server, OutputFormat::Table).expect("list");
    assert!(listed.contains("(no snapshots)"));
}

#[test]
fn snapshot_restore_is_stub() {
    // No network call expected — stub message only.
    let out = snapshot::restore::run("ignored:0", 42, OutputFormat::Table).expect("restore");
    assert!(out.contains("not yet supported"));
    assert!(out.contains("id=42"));
}

#[test]
fn snapshot_delete_unknown_errors() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let addr = spawn_mock_admin(state);
    let server = addr.to_string();

    let result = snapshot::delete::run(&server, 999, 0, OutputFormat::Json);
    assert!(result.is_err());
}
