//! `brain-cli worker start --name <w> [--shard N]` — POST
//! /v1/workers/{name}/start. Returns the structured 501 today.

use std::time::Duration;

use crate::cli::OutputFormat;
use crate::http::post;

pub fn run(
    server: &str,
    name: &str,
    _shard: usize,
    _output: OutputFormat,
) -> anyhow::Result<String> {
    let path = format!("/v1/workers/{name}/start");
    let resp = post(server, &path, "", Duration::from_secs(10))?;
    super::common::surface_status(&resp, &path)
}
