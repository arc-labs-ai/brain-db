//! Shared helper for the 10.11 worker / config / audit / agent /
//! shard families: render a uniform error when the admin server
//! returns a structured 501 body. The CLI exits non-zero so scripts
//! can detect deferred actions; the rendered message tells operators
//! which future phase will back the command.

use crate::http::{parse_not_implemented, HttpResponse};

pub fn surface_status(resp: &HttpResponse, path: &str) -> anyhow::Result<String> {
    if (200..300).contains(&resp.status) {
        // 2xx: caller handles its own decoding; just hand back the
        // body so callers that don't need parsing can print it.
        return Ok(if resp.body.ends_with('\n') {
            resp.body.clone()
        } else {
            format!("{}\n", resp.body)
        });
    }
    if resp.status == 501 {
        if let Some(ni) = parse_not_implemented(&resp.body) {
            anyhow::bail!(
                "Not yet implemented.\nDeferred to: {}\nDetail:      {}",
                ni.deferred_to,
                ni.detail
            );
        }
    }
    anyhow::bail!("{path} returned HTTP {}: {}", resp.status, resp.body.trim())
}
