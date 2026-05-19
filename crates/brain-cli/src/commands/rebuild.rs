//! `brain-cli rebuild-ann [--shard N]` — POST /v1/rebuild-ann.
//!
//! v1 is synchronous: the HTTP request blocks until the rebuild
//! completes on the named shard. An async dispatch + status follow-up
//! lands once we have at least one other long-running op to share
//! job-id infrastructure with.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::output::{dispatch_to_string, render::shard_health::RebuildRendered};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildReport {
    pub entries: u64,
    pub elapsed_ms: u64,
    pub shard: usize,
}

pub fn run(server: &str, shard: usize, output: OutputFormat) -> anyhow::Result<String> {
    let body = post_no_body(server, &format!("/v1/rebuild-ann?shard={shard}"))?;
    let report: RebuildReport = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed RebuildReport JSON: {e}; body = {body}"))?;
    dispatch_to_string(&RebuildRendered(report), output)
}

/// Minimal blocking HTTP/1.1 POST with an empty body. Returns the
/// response body on 2xx; errors on non-2xx or transport failure.
///
/// Duplicated from `commands::snapshot::create` for the same
/// reason: each command's payload shape is its own concern.
fn post_no_body(endpoint: &str, path: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr = endpoint
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("resolve {endpoint}: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("resolve {endpoint}: no addresses"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("connect {addr}: {e}"))?;
    // Rebuild can take a while on large shards; allow up to 5 minutes
    // for the HTTP read. A future async version will swap to a status poll.
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {endpoint}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         Accept: */*\r\n\r\n",
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut raw = Vec::with_capacity(1024);
    stream.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed response"))?;
    let head = std::str::from_utf8(&raw[..split])?;
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad status line: {status_line:?}"))?;
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    if !(200..300).contains(&status) {
        anyhow::bail!("POST {path} returned HTTP {status}: {}", body.trim());
    }
    Ok(body)
}
