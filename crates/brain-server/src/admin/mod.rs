//! Admin / observability HTTP server (sub-task 9.13).
//!
//! Spec §14/01. Binds a separate listener on `cfg.server.metrics_addr`
//! (default `127.0.0.1:9091`) and serves:
//!
//! - `GET /healthz` → `200 OK\nok`
//! - `GET /metrics` → Prometheus text-format exposition
//! - everything else → `400 Bad Request`
//!
//! ## Why hand-rolled HTTP?
//!
//! The endpoint surface is two static paths. Pulling in `hyper` /
//! `axum` would add ~50 deps for two `GET` handlers. The spec's
//! Prometheus text format is a few `format!`s away; a registry crate
//! (`prometheus`, `metrics-exporter-prometheus`) doesn't save code
//! at the v1 metric count.
//!
//! ## Scope
//!
//! v1 emits only metrics that are *already counted* somewhere
//! first-party (workers, connections, build info). Spec §14/01 lists
//! ~50 metric families; the per-op latency histograms, storage
//! gauges, HNSW health gauges, and embedder metrics need
//! instrumentation that lands in later sub-tasks. See
//! `.claude/plans/phase-09-task-13.md` §1 for the survey.

#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tracing::{debug, info, warn};

use crate::connection::{ConnectionMetrics, ShutdownSignal};
use crate::shard::ShardHandle;

const HTTP_REQUEST_LINE_MAX: usize = 8 * 1024;
const HTTP_HEADER_BLOCK_MAX: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// AdminState
// ---------------------------------------------------------------------------

/// Static build information, populated from `env!` at compile time.
#[derive(Clone, Copy, Debug)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_commit: &'static str,
}

impl BuildInfo {
    pub fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            // No vergen wired up yet; surface the literal in v1 and
            // upgrade to `env!("VERGEN_GIT_SHA")` when the build script
            // is added.
            git_commit: "unknown",
        }
    }
}

/// Shared, read-only state the admin server consults per scrape.
/// All atomics live behind `Arc` so the connection layer and the
/// admin server can publish / read concurrently.
pub struct AdminState {
    pub started_at: Instant,
    pub started_at_unix_secs: u64,
    pub build_info: BuildInfo,
    pub shards: Arc<Vec<ShardHandle>>,
    pub connections: Arc<ConnectionMetrics>,
}

impl AdminState {
    pub fn new(shards: Arc<Vec<ShardHandle>>, connections: Arc<ConnectionMetrics>) -> Self {
        let started_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            started_at: Instant::now(),
            started_at_unix_secs,
            build_info: BuildInfo::from_env(),
            shards,
            connections,
        }
    }
}

// ---------------------------------------------------------------------------
// AdminServer
// ---------------------------------------------------------------------------

pub struct AdminServer {
    listen_addr: SocketAddr,
    state: Arc<AdminState>,
    shutdown: ShutdownSignal,
}

pub struct BoundAdminServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    state: Arc<AdminState>,
    shutdown: ShutdownSignal,
}

impl AdminServer {
    pub fn new(listen_addr: SocketAddr, state: Arc<AdminState>, shutdown: ShutdownSignal) -> Self {
        Self {
            listen_addr,
            state,
            shutdown,
        }
    }

    /// Bind the admin HTTP socket. Mirrors `ConnectionListener::bind`
    /// for ergonomic ephemeral binding in tests.
    pub fn bind(self) -> io::Result<BoundAdminServer> {
        let listener = bind_listener(self.listen_addr)?;
        let local_addr = listener.local_addr()?;
        info!(addr = %local_addr, "admin server bound");
        Ok(BoundAdminServer {
            listener,
            local_addr,
            state: self.state,
            shutdown: self.shutdown,
        })
    }
}

impl BoundAdminServer {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn serve(mut self) -> io::Result<SocketAddr> {
        let local_addr = self.local_addr;
        info!(addr = %local_addr, "admin server accepting");
        loop {
            tokio::select! {
                biased;
                () = self.shutdown.recv() => {
                    info!(addr = %local_addr, "admin server shutdown signalled");
                    return Ok(local_addr);
                }
                accepted = self.listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "admin accept failed");
                            continue;
                        }
                    };
                    let state = self.state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_request(stream, state).await {
                            debug!(peer = %peer, error = %e, "admin request ended");
                        }
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 request handler
// ---------------------------------------------------------------------------

async fn serve_request(stream: TcpStream, state: Arc<AdminState>) -> io::Result<()> {
    let mut reader = BufReader::new(stream);

    // Request line: `METHOD SP PATH SP VERSION CRLF`.
    let mut request_line = String::new();
    let n = read_line(&mut reader, &mut request_line, HTTP_REQUEST_LINE_MAX).await?;
    if n == 0 {
        return Ok(());
    }

    // Header block (we don't care about most headers; just drain until
    // CRLFCRLF or the bound).
    let mut header_bytes = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        let read = read_line(&mut reader, &mut line, HTTP_HEADER_BLOCK_MAX - header_bytes).await?;
        if read == 0 {
            break;
        }
        header_bytes += read;
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
    }

    let (method, path) = parse_request_line(&request_line);
    let mut stream = reader.into_inner();
    if method != "GET" {
        return write_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            "bad method\n",
        )
        .await;
    }
    match path {
        "/healthz" => {
            write_response(&mut stream, 200, "OK", "text/plain; charset=utf-8", "ok\n").await
        }
        "/metrics" => {
            let body = format_metrics(&state).await;
            write_response(
                &mut stream,
                200,
                "OK",
                "text/plain; version=0.0.4; charset=utf-8",
                &body,
            )
            .await
        }
        _ => {
            write_response(
                &mut stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                "unknown path\n",
            )
            .await
        }
    }
}

fn parse_request_line(line: &str) -> (&str, &str) {
    // `GET /metrics HTTP/1.1\r\n` → ("GET", "/metrics")
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    (method, path)
}

async fn read_line<R>(reader: &mut R, buf: &mut String, max: usize) -> io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    if max == 0 {
        return Ok(0);
    }
    let mut bytes_read = 0usize;
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        bytes_read += 1;
        if bytes_read > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header line too long",
            ));
        }
        buf.push(byte[0] as char);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(bytes_read)
}

async fn write_response<W>(
    stream: &mut W,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Metrics body builder
// ---------------------------------------------------------------------------

async fn format_metrics(state: &AdminState) -> String {
    let mut s = String::with_capacity(2048);

    // Static + scalars.
    let uptime_secs = state.started_at.elapsed().as_secs();
    writeln!(&mut s, "# HELP brain_build_info Build information.").unwrap();
    writeln!(&mut s, "# TYPE brain_build_info gauge").unwrap();
    writeln!(
        &mut s,
        "brain_build_info{{version=\"{v}\",git_commit=\"{g}\"}} 1",
        v = state.build_info.version,
        g = state.build_info.git_commit,
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP brain_up Server liveness; 1 if accepting requests."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_up gauge").unwrap();
    writeln!(&mut s, "brain_up 1").unwrap();

    writeln!(
        &mut s,
        "# HELP brain_shards_total Number of configured shards."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_shards_total gauge").unwrap();
    writeln!(&mut s, "brain_shards_total {}", state.shards.len()).unwrap();

    writeln!(
        &mut s,
        "# HELP brain_connections_active Currently in-flight client connections."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_connections_active gauge").unwrap();
    writeln!(
        &mut s,
        "brain_connections_active {}",
        state.connections.active.load(Ordering::Relaxed),
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP brain_connections_total Total accepted client connections since startup."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_connections_total counter").unwrap();
    writeln!(
        &mut s,
        "brain_connections_total {}",
        state.connections.total.load(Ordering::Relaxed),
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP process_uptime_seconds Process uptime since admin server start."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE process_uptime_seconds counter").unwrap();
    writeln!(&mut s, "process_uptime_seconds {uptime_secs}").unwrap();

    writeln!(
        &mut s,
        "# HELP process_start_time_seconds Unix timestamp of process start (seconds)."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE process_start_time_seconds gauge").unwrap();
    writeln!(
        &mut s,
        "process_start_time_seconds {}",
        state.started_at_unix_secs
    )
    .unwrap();

    // Per-worker counters from each shard's scheduler.
    writeln!(
        &mut s,
        "# HELP brain_worker_cycles_total Worker cycles completed."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_cycles_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_processed_total Items processed by the worker."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_processed_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_errors_total Worker cycle errors."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_errors_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_last_run_unixtime Unix-time of the worker's last cycle."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_last_run_unixtime gauge").unwrap();

    for shard in state.shards.iter() {
        let shard_id = shard.shard_id();
        match shard.scheduler_snapshot().await {
            Ok(snapshot) => {
                let mut workers = snapshot;
                workers.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in workers {
                    writeln!(
                        &mut s,
                        "brain_worker_cycles_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.cycles_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_processed_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.processed_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_errors_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.errors_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_last_run_unixtime{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.last_run_unix_secs
                    )
                    .unwrap();
                }
            }
            Err(e) => {
                warn!(shard_id, error = %e, "scheduler_snapshot failed");
            }
        }
    }

    s
}

// ---------------------------------------------------------------------------
// Socket setup
// ---------------------------------------------------------------------------

fn bind_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(128)
}
