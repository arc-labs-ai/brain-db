//! Shared mock-admin scaffold for sub-task 10.11 integration
//! tests. Spawns a tokio-driven HTTP server on 127.0.0.1:0 and
//! invokes a per-test responder for each accepted connection.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a mock admin server. `responder` receives (method, path,
/// body) and returns (status, response_body).
pub fn spawn_mock<F>(responder: F) -> SocketAddr
where
    F: Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static,
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
                    let mut buf = vec![0u8; 8192];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    let (status, resp_body) = r(&method, &path, &body);
                    let reason = match status {
                        200 => "OK",
                        201 => "Created",
                        400 => "Bad Request",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        501 => "Not Implemented",
                        _ => "OK",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{resp_body}",
                        len = resp_body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
    });
    addr_rx.recv_timeout(Duration::from_secs(5)).expect("addr")
}

/// Standard 501 body shape emitted by the 10.11 admin endpoints.
pub fn not_implemented_body(deferred_to: &str, detail: &str) -> String {
    format!(
        "{{\"error\":\"not_implemented\",\"deferred_to\":\"{deferred_to}\",\"detail\":\"{detail}\"}}\n"
    )
}
