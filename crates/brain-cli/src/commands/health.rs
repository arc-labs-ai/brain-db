//! `brain-cli health` — probes the admin server's `/healthz`.
//!
//! brain-server's /healthz returns `200 OK\n\nok` on liveness;
//! any non-2xx is treated as `unhealthy`. Spec §14/06 §3.

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{json, table};

#[derive(Debug, Serialize)]
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
    render(&report, output)
}

fn render(r: &HealthReport, output: OutputFormat) -> anyhow::Result<String> {
    match output {
        OutputFormat::Json => json::render(r),
        OutputFormat::Table => {
            let rows = vec![
                ("status".into(), r.status.clone()),
                ("admin_endpoint".into(), r.admin_endpoint.clone()),
                ("probe".into(), r.probe.into()),
            ];
            Ok(table::render_kv(&rows))
        }
    }
}
