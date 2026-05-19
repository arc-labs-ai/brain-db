//! `brain-cli worker list [--shard N]` — GET /v1/workers.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{dispatch_to_string, render::worker_status::WorkerStatusRendered};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEntry {
    pub shard: usize,
    pub name: String,
    pub cycles: u64,
    pub processed: u64,
    pub errors: u64,
    pub last_run_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerList {
    pub workers: Vec<WorkerEntry>,
}

pub fn run(server: &str, shard: Option<usize>, output: OutputFormat) -> anyhow::Result<String> {
    let path = match shard {
        Some(n) => format!("/v1/workers?shard={n}"),
        None => "/v1/workers".to_string(),
    };
    let resp = get(server, &path)?;
    if resp.status != 200 {
        anyhow::bail!(
            "GET {path} returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let list: WorkerList = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed worker list JSON: {e}; body = {}", resp.body))?;
    dispatch_to_string(&WorkerStatusRendered(list), output)
}
