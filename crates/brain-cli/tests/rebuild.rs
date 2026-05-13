//! `brain-cli rebuild-ann` integration tests against a mock
//! admin HTTP server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::rebuild;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a tokio-driven mock admin server bound to 127.0.0.1:0.
/// `responder` runs for every accepted connection.
fn spawn_mock<F>(responder: F) -> SocketAddr
where
    F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
{
    let responder = Arc::new(responder);
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
                let r = responder.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    let (status, body) = r(method, path);
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
fn rebuild_ann_json_round_trip() {
    let addr = spawn_mock(|method, path| {
        assert_eq!(method, "POST");
        assert!(path.starts_with("/v1/rebuild-ann"));
        (201, "{\"entries\":42,\"elapsed_ms\":5,\"shard\":0}".into())
    });
    let out = rebuild::run(&addr.to_string(), 0, OutputFormat::Json).expect("rebuild");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
    assert_eq!(v["entries"], 42);
    assert_eq!(v["elapsed_ms"], 5);
    assert_eq!(v["shard"], 0);
}

#[test]
fn rebuild_ann_table_output() {
    let addr =
        spawn_mock(|_method, _path| (201, "{\"entries\":7,\"elapsed_ms\":100,\"shard\":2}".into()));
    let out = rebuild::run(&addr.to_string(), 2, OutputFormat::Table).expect("rebuild");
    assert!(out.contains("shard"));
    assert!(out.contains("entries"));
    assert!(out.contains("100"));
}

#[test]
fn rebuild_ann_passes_shard_query() {
    let seen_path: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let seen_path_clone = seen_path.clone();
    let addr = spawn_mock(move |_m, path| {
        *seen_path_clone.lock().unwrap() = Some(path.to_string());
        (201, "{\"entries\":0,\"elapsed_ms\":0,\"shard\":3}".into())
    });
    let _ = rebuild::run(&addr.to_string(), 3, OutputFormat::Json).expect("rebuild");
    let captured = seen_path.lock().unwrap().clone();
    assert_eq!(captured.as_deref(), Some("/v1/rebuild-ann?shard=3"));
}

#[test]
fn rebuild_ann_5xx_reports_error() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();
    let addr = spawn_mock(move |_m, _p| {
        calls_clone.fetch_add(1, Ordering::Relaxed);
        (500, "rebuild source: Disabled".into())
    });
    let err = rebuild::run(&addr.to_string(), 0, OutputFormat::Json).expect_err("err");
    assert!(err.to_string().contains("HTTP 500"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
