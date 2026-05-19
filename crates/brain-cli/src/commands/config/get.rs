//! `brain-cli config get [--key dotted.path]` — GET /v1/config.

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{dispatch_to_string, render::config_dump::ConfigDumpRendered};

pub fn run(server: &str, key: Option<&str>, output: OutputFormat) -> anyhow::Result<String> {
    let path = match key {
        Some(k) => format!("/v1/config?key={k}"),
        None => "/v1/config".to_string(),
    };
    let resp = get(server, &path)?;
    if resp.status != 200 {
        anyhow::bail!(
            "GET {path} returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let value: serde_json::Value = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed config JSON: {e}; body = {}", resp.body))?;
    dispatch_to_string(&ConfigDumpRendered(value), output)
}
