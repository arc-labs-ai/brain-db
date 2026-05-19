//! `brain-cli extract --backfill ...` — POST `/v1/extract/backfill`.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::commands::extract::BackfillKind;
use crate::output::{json, table};

/// JSON shape returned by `POST /v1/extract/backfill`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillReport {
    pub enqueued: u64,
    pub skipped: u64,
    pub shards: usize,
}

pub fn run(server: &str, kind: BackfillKind, output: OutputFormat) -> anyhow::Result<String> {
    let query = match kind {
        BackfillKind::Memory(id) => format!("memory={id}"),
        BackfillKind::Since(ts) => format!("since={ts}"),
        BackfillKind::All => "all".into(),
    };
    let body = post_no_body(server, &format!("/v1/extract/backfill?{query}"))?;
    let report: BackfillReport = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed backfill JSON: {e}; body = {body}"))?;
    render(&report, kind, output)
}

fn render(r: &BackfillReport, kind: BackfillKind, output: OutputFormat) -> anyhow::Result<String> {
    match output {
        OutputFormat::Json => json::render(r),
        OutputFormat::Table => {
            let selector = match kind {
                BackfillKind::Memory(id) => format!("memory={id}"),
                BackfillKind::Since(ts) => format!("since={ts}"),
                BackfillKind::All => "all".into(),
            };
            let rows = vec![
                ("selector".into(), selector),
                ("shards".into(), r.shards.to_string()),
                ("enqueued".into(), r.enqueued.to_string()),
                ("skipped".into(), r.skipped.to_string()),
            ];
            Ok(table::render_kv(&rows))
        }
    }
}

/// Minimal blocking HTTP/1.1 POST with an empty body. Mirrors the
/// helper inlined in `commands::snapshot::create` and
/// `commands::rebuild`; not lifted into a shared module yet because
/// each command's response shape is its own concern.
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
    // Backfill walks the full memories table on `--all`; allow plenty
    // of headroom for the synchronous reply.
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
