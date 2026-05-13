//! `brain-cli snapshot create [--shard N]` — POST /v1/snapshots.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::output::{json, table};

/// JSON shape returned by `POST /v1/snapshots`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReport {
    pub id: u64,
    pub shard: usize,
}

pub fn run(server: &str, shard: usize, output: OutputFormat) -> anyhow::Result<String> {
    // The shared `http::get` doesn't speak POST; for this path we
    // hand-roll a minimal one inline. Keeps the http module slim
    // and avoids over-abstracting before more verbs land.
    let body = post_no_body(server, &format!("/v1/snapshots?shard={shard}"))?;
    let report: CreateReport = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed CreateReport JSON: {e}; body = {body}"))?;
    render(&report, output)
}

fn render(r: &CreateReport, output: OutputFormat) -> anyhow::Result<String> {
    match output {
        OutputFormat::Json => json::render(r),
        OutputFormat::Table => {
            let rows = vec![
                ("id".into(), r.id.to_string()),
                ("shard".into(), r.shard.to_string()),
            ];
            Ok(table::render_kv(&rows))
        }
    }
}

/// Minimal blocking HTTP/1.1 POST with an empty body. Returns the
/// response body on 2xx; errors on non-2xx or transport failure.
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
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
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
    let mut raw = Vec::with_capacity(4096);
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
