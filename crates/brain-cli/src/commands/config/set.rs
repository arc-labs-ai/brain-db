//! `brain-cli config set --key <k> --value <v>` — POST /v1/config.
//! Returns the structured 501 today.

use std::time::Duration;

use crate::http::post;

pub fn run(server: &str, key: &str, value: &str) -> anyhow::Result<String> {
    let path = format!("/v1/config?key={key}");
    let body = format!("{{\"value\":\"{value}\"}}");
    let resp = post(server, &path, &body, Duration::from_secs(10))?;
    super::surface_501(&resp, &path)
}
