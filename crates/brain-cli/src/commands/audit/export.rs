//! `brain-cli audit export [--value <path>]` — GET /v1/audit/export.
//! Returns the structured 501 today.

use crate::http::get;

pub fn run(server: &str, _output_path: Option<&str>) -> anyhow::Result<String> {
    let path = "/v1/audit/export";
    let resp = get(server, path)?;
    crate::commands::worker::common::surface_status(&resp, path)
}
