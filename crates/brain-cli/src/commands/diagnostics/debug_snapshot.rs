//! `brain-cli debug-snapshot --shard N [--value PATH]` —
//! GET /v1/diagnostics/debug-snapshot. v1 returns the partial schema:
//! workers populated; other spec'd fields listed under `deferred[]`.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{dispatch_to_string, render::shard_health::DebugSnapshotRendered};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub name: String,
    pub cycles: u64,
    pub processed: u64,
    pub errors: u64,
    pub last_run_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugSnapshot {
    pub shard: usize,
    pub captured_at_unix: u64,
    pub partial: bool,
    pub deferred: Vec<String>,
    pub workers: Vec<WorkerStatus>,
}

pub fn run(
    server: &str,
    shard: usize,
    output_path: Option<&str>,
    output: OutputFormat,
) -> anyhow::Result<String> {
    let path = format!("/v1/diagnostics/debug-snapshot?shard={shard}");
    let resp = get(server, &path)?;
    if resp.status != 200 {
        anyhow::bail!(
            "GET {path} returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let snap: DebugSnapshot = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed debug-snapshot JSON: {e}; body = {}", resp.body))?;
    if let Some(p) = output_path {
        fs::write(Path::new(p), &resp.body).map_err(|e| anyhow::anyhow!("write {p}: {e}"))?;
    }
    dispatch_to_string(&DebugSnapshotRendered(snap), output)
}
