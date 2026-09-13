//! Load test — 10k concurrent persistent connections, 1 GET per
//! connection per second, for 5 minutes.
//!
//! **Operator-invoked only.** Marked `#[ignore]` so `cargo test`
//! skips it by default. Run with:
//!
//! ```bash
//! ulimit -n 20000
//! cargo test -p brain-http --test load -- --ignored load_10k --nocapture
//! ```
//!
//! Linux only — defensible TCP behaviour at scale + `ulimit`
//! semantics aren't portable to macOS for this size.
//!
//! ## What "passing" means
//!
//! - All 10k client tasks complete the test window without errors.
//! - Server reports no `connection task ended with error` warns.
//! - `top` / `ps` shows stable RSS over the 5-minute window (no
//!   leak).
//!
//! Last run results live in the commit message; update when
//! re-run.

#![cfg(target_os = "linux")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_http::body::{full, ResponseBody};
use brain_http::router::Router;
use bytes::Bytes;
use common::TestServer;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

const N_CONNECTIONS: usize = 10_000;
const DURATION: Duration = Duration::from_secs(300);
const REQUEST_PERIOD: Duration = Duration::from_secs(1);
const SETUP_TEARDOWN_SLACK: Duration = Duration::from_secs(60);

async fn ok_handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .header("content-length", "2")
        .body(full(Bytes::from_static(b"ok")))
        .unwrap())
}

#[ignore = "long-running 5 min load test; run manually with --ignored load_10k"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_10k_concurrent_connections() {
    // Wrap everything in a hard outer timeout. If the test hangs
    // past `DURATION + SETUP_TEARDOWN_SLACK`, we want a deterministic
    // panic rather than waiting forever for cargo-test's own kill.
    let total_budget = DURATION + SETUP_TEARDOWN_SLACK;
    tokio::time::timeout(total_budget, load_10k_inner())
        .await
        .expect("load test exceeded total budget");
}

async fn load_10k_inner() {
    let router = Router::new().get("/healthz", ok_handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();
    println!("load_10k: bound on {addr}; opening {N_CONNECTIONS} clients ...");

    let stop = Arc::new(Notify::new());
    let errors = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(AtomicU64::new(0));

    let mut clients = Vec::with_capacity(N_CONNECTIONS);
    let setup_started = Instant::now();
    for _ in 0..N_CONNECTIONS {
        let stop = stop.clone();
        let errors = errors.clone();
        let requests = requests.clone();
        clients.push(tokio::spawn(async move {
            client_task(addr, stop, errors, requests).await;
        }));
    }
    let setup_elapsed = setup_started.elapsed();
    println!("load_10k: {N_CONNECTIONS} clients spawned in {setup_elapsed:?}");

    // Let the test run for the configured DURATION, then signal stop.
    tokio::time::sleep(DURATION).await;
    stop.notify_waiters();
    println!("load_10k: stop signalled after {DURATION:?}; waiting for clients to drain ...");

    // Drain clients with a 30-second cap.
    let drain_started = Instant::now();
    for c in clients {
        let _ = tokio::time::timeout(Duration::from_secs(30), c).await;
    }
    let drain_elapsed = drain_started.elapsed();

    let total_errors = errors.load(Ordering::SeqCst);
    let total_requests = requests.load(Ordering::SeqCst);
    println!(
        "load_10k: drained in {drain_elapsed:?}; requests={total_requests}, errors={total_errors}"
    );

    server.shutdown().await.expect("shutdown");

    assert_eq!(
        total_errors, 0,
        "load test reported {total_errors} errors over {total_requests} requests"
    );
    // Sanity: each client should have issued ~DURATION/REQUEST_PERIOD
    // requests. Allow a wide tolerance because tokio scheduling
    // varies under 10k concurrent tasks.
    let expected_min = (N_CONNECTIONS as u64) * (DURATION.as_secs() / 2);
    assert!(
        total_requests >= expected_min,
        "load test issued only {total_requests} requests; expected ≥{expected_min}"
    );
}

async fn client_task(
    addr: std::net::SocketAddr,
    stop: Arc<Notify>,
    errors: Arc<AtomicU64>,
    requests: Arc<AtomicU64>,
) {
    // Persistent TCP connection. If the first connect fails, give up
    // — the test environment is broken, not the server.
    let mut s = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => {
            errors.fetch_add(1, Ordering::SeqCst);
            return;
        }
    };
    let _ = s.set_nodelay(true);

    let mut ticker = tokio::time::interval(REQUEST_PERIOD);
    // First tick fires immediately; skip it so all clients don't
    // hammer the server at the same instant.
    ticker.tick().await;

    let mut buf = vec![0u8; 256];
    loop {
        tokio::select! {
            biased;
            _ = stop.notified() => return,
            _ = ticker.tick() => {
                if s.write_all(
                    b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
                )
                .await
                .is_err()
                {
                    errors.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                // Drain a single response. Quick-and-dirty: read until
                // we see CRLF CRLF + 2 bytes (the "ok" body).
                let mut seen = 0usize;
                let mut delim_at: Option<usize> = None;
                loop {
                    let n = match s.read(&mut buf[seen..]).await {
                        Ok(0) | Err(_) => {
                            errors.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        Ok(n) => n,
                    };
                    seen += n;
                    if delim_at.is_none() {
                        if let Some(pos) = buf[..seen]
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                        {
                            delim_at = Some(pos + 4);
                        }
                    }
                    if let Some(d) = delim_at {
                        if seen >= d + 2 {
                            break;
                        }
                    }
                    if seen == buf.len() {
                        errors.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                }
                requests.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}
