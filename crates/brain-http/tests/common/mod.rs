//! Shared scaffold for server integration tests.
//!
//! Provides [`TestServer`] which binds a port-0 listener, spawns the
//! accept loop, exposes the bound address, and offers a `shutdown()`
//! method that triggers graceful drain. Plus a tiny synchronous
//! HTTP client (`http_request`) that goes straight to TCP so the
//! tests don't pull a full HTTP client library.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use brain_http::router::Router;
use brain_http::server::{HttpServer, ShutdownHandle};
use hyper::body::Incoming;

pub struct TestServer {
    addr: SocketAddr,
    handle: ShutdownHandle,
    join: tokio::task::JoinHandle<brain_http::Result<()>>,
}

impl TestServer {
    pub async fn start(router: Router<Incoming>) -> Self {
        let bound = HttpServer::bind("127.0.0.1:0".parse().unwrap())
            .router(router)
            .listen()
            .await
            .expect("bind");
        let addr = bound.local_addr().expect("local_addr");
        let (handle, run) = bound.into_runner();
        let join = tokio::spawn(run);
        Self { addr, handle, join }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) -> brain_http::Result<()> {
        self.handle.shutdown();
        match tokio::time::timeout(Duration::from_secs(5), self.join).await {
            Ok(r) => r.expect("server task did not panic"),
            Err(_) => panic!("server did not shut down in time"),
        }
    }
}

/// Simple blocking HTTP/1.1 request. Returns `(status, body)`.
pub fn http_request(addr: SocketAddr, raw_request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(raw_request.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut raw = Vec::with_capacity(4096);
    stream.read_to_end(&mut raw).expect("read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("CRLFCRLF");
    let head = std::str::from_utf8(&raw[..split]).expect("utf8 head");
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();
    (status, body)
}

/// Sends `n` GET requests on a single keep-alive connection. Returns
/// the body of each response in order.
pub fn keep_alive_round_trip(addr: SocketAddr, path: &str, n: usize) -> Vec<String> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut bodies = Vec::with_capacity(n);
    for i in 0..n {
        let last = i == n - 1;
        let conn = if last { "close" } else { "keep-alive" };
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: {conn}\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("write");
        stream.flush().expect("flush");
        bodies.push(read_one_response(&mut stream));
    }
    bodies
}

fn read_one_response(stream: &mut TcpStream) -> String {
    // Read headers, find Content-Length, then the body.
    let mut head = Vec::with_capacity(1024);
    loop {
        let mut byte = [0u8; 1];
        let n = stream.read(&mut byte).expect("read");
        if n == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_str = String::from_utf8_lossy(&head);
    let cl = head_str
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_else(|| "0".into());
    let cl: usize = cl.parse().unwrap_or(0);
    let mut body = vec![0u8; cl];
    if cl > 0 {
        stream.read_exact(&mut body).expect("body");
    }
    String::from_utf8_lossy(&body).into_owned()
}
