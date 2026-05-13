//! `brain-cli config reload` — POST /v1/config/reload. Returns the
//! structured 501 today (no live-reload pathway).

use std::time::Duration;

use crate::http::post;

pub fn run(server: &str) -> anyhow::Result<String> {
    let path = "/v1/config/reload";
    let resp = post(server, path, "", Duration::from_secs(10))?;
    super::surface_501(&resp, path)
}
