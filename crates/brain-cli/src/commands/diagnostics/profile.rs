//! `brain-cli profile --shard N [--duration-secs D]` — POST
//! /v1/diagnostics/profile. Returns the structured 501 today
//! (no in-process Glommio profiler yet; operators use `perf` against
//! the server PID).

use std::time::Duration;

use crate::http::post;

pub fn run(
    server: &str,
    shard: usize,
    duration_secs: u32,
    _output_path: Option<&str>,
) -> anyhow::Result<String> {
    let path = format!("/v1/diagnostics/profile?shard={shard}&duration_secs={duration_secs}");
    // The eventual real profiler will block for `duration_secs`;
    // pick a generous read timeout so v2 doesn't have to retune
    // the CLI.
    let read_timeout = Duration::from_secs(u64::from(duration_secs).saturating_add(30));
    let resp = post(server, &path, "", read_timeout)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
