//! `brain-cli snapshot list` — GET /v1/snapshots.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{json, table};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntry {
    pub shard: usize,
    pub id: u64,
    pub taken_at_unix_nanos: u64,
    pub size_bytes: u64,
}

pub fn run(server: &str, output: OutputFormat) -> anyhow::Result<String> {
    let resp = get(server, "/v1/snapshots")?;
    if resp.status != 200 {
        anyhow::bail!(
            "/v1/snapshots returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let entries: Vec<ListEntry> = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed list JSON: {e}; body = {}", resp.body))?;
    render(&entries, output)
}

fn render(entries: &[ListEntry], output: OutputFormat) -> anyhow::Result<String> {
    match output {
        OutputFormat::Json => json::render(&entries),
        OutputFormat::Table => {
            if entries.is_empty() {
                return Ok("(no snapshots)\n".into());
            }
            let mut rows = Vec::with_capacity(entries.len());
            for e in entries {
                rows.push((
                    format!("shard {} / snapshot {}", e.shard, e.id),
                    format!(
                        "{} bytes, taken_at_unix_nanos={}",
                        e.size_bytes, e.taken_at_unix_nanos
                    ),
                ));
            }
            Ok(table::render_kv(&rows))
        }
    }
}
