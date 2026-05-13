//! `brain-cli shard list` — GET /v1/shards.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{json, table};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardEntry {
    pub index: usize,
    pub shard_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardList {
    pub shards: Vec<ShardEntry>,
}

pub fn run(server: &str, output: OutputFormat) -> anyhow::Result<String> {
    let resp = get(server, "/v1/shards")?;
    if resp.status != 200 {
        anyhow::bail!(
            "GET /v1/shards returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let list: ShardList = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed shard list JSON: {e}; body = {}", resp.body))?;
    render(&list, output)
}

fn render(list: &ShardList, output: OutputFormat) -> anyhow::Result<String> {
    match output {
        OutputFormat::Json => json::render(list),
        OutputFormat::Table => {
            if list.shards.is_empty() {
                return Ok("(no shards)\n".into());
            }
            let rows: Vec<(String, String)> = list
                .shards
                .iter()
                .map(|s| {
                    (
                        format!("index {}", s.index),
                        format!("shard_id={}", s.shard_id),
                    )
                })
                .collect();
            Ok(table::render_kv(&rows))
        }
    }
}
