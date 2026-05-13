//! `brain-cli audit query [--since X --until Y --agent A]` — GET
//! /v1/audit?... Returns the structured 501 today.

use crate::http::get;

pub fn run(
    server: &str,
    since: Option<&str>,
    until: Option<&str>,
    agent: Option<&str>,
) -> anyhow::Result<String> {
    let mut qs = String::new();
    let mut sep = '?';
    for (k, v) in [("since", since), ("until", until), ("agent", agent)] {
        if let Some(val) = v {
            qs.push(sep);
            sep = '&';
            qs.push_str(k);
            qs.push('=');
            qs.push_str(val);
        }
    }
    let path = format!("/v1/audit{qs}");
    let resp = get(server, &path)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
