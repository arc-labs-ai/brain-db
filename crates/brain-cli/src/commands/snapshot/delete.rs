//! `brain-cli snapshot delete <id> [--shard N]` — DELETE
//! /v1/snapshots/<id>.

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::output::{json, table};

#[derive(Debug, Clone, Serialize)]
pub struct DeleteReport {
    pub id: u64,
    pub shard: usize,
    pub status: String,
}

pub fn run(server: &str, id: u64, shard: usize, output: OutputFormat) -> anyhow::Result<String> {
    delete_no_body(server, &format!("/v1/snapshots/{id}?shard={shard}"))?;
    let report = DeleteReport {
        id,
        shard,
        status: "deleted".into(),
    };
    match output {
        OutputFormat::Json => json::render(&report),
        OutputFormat::Table => Ok(table::render_kv(&[
            ("id".into(), report.id.to_string()),
            ("shard".into(), report.shard.to_string()),
            ("status".into(), report.status),
        ])),
    }
}

fn delete_no_body(endpoint: &str, path: &str) -> anyhow::Result<()> {
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
        "DELETE {path} HTTP/1.1\r\n\
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
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
        anyhow::bail!("DELETE {path} returned HTTP {status}: {}", body.trim());
    }
    Ok(())
}
