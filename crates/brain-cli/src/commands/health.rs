//! `brain-cli health` — probes the admin server's `/healthz`.
//!
//! brain-server's `/healthz` returns `200 OK\n\nok` on liveness; any non-2xx
//! is treated as `unhealthy`. The response shape is intentionally simple —
//! status + endpoint + probe path — so scripts can grep for "healthy".

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{dispatch_to_string, render::shard_health::ShardHealthRendered};

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub status: String,
    pub admin_endpoint: String,
    pub probe: &'static str,
}

pub fn run(server: &str, output: OutputFormat) -> anyhow::Result<String> {
    let report = match get(server, "/healthz") {
        Ok(resp) if resp.status == 200 => HealthReport {
            status: "healthy".into(),
            admin_endpoint: server.into(),
            probe: "/healthz",
        },
        Ok(resp) => HealthReport {
            status: format!("unhealthy: HTTP {}", resp.status),
            admin_endpoint: server.into(),
            probe: "/healthz",
        },
        Err(e) => HealthReport {
            status: format!("unreachable: {e}"),
            admin_endpoint: server.into(),
            probe: "/healthz",
        },
    };
    dispatch_to_string(&ShardHealthRendered(report), output)
}
