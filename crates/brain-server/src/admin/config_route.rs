//! Admin HTTP handlers for `config` (spec §14/06 §7; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/config[?key=a.b.c]` → 200 + JSON (whole config or
//!   subtree).
//! - `POST /v1/config/reload` → 501 (no live-reload pathway yet).
//! - `POST /v1/config?key=…` → 501 (no editable in-memory store).
//!
//! ## Key walk
//!
//! Spec uses dotted paths like `workers.decay.interval`. We
//! serialize the config to a `serde_json::Value`, walk by segment,
//! and return whatever is at that subtree (object/scalar/array).

use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;
use tracing::warn;

use super::{write_not_implemented, write_response, AdminState};

const HDR_JSON: &str = "application/json; charset=utf-8";
const HDR_TEXT: &str = "text/plain; charset=utf-8";

pub async fn dispatch<W>(
    stream: &mut W,
    method: &str,
    path: &str,
    query: &str,
    state: &Arc<AdminState>,
) -> Option<io::Result<()>>
where
    W: AsyncWrite + Unpin,
{
    match (method, path) {
        ("GET", "/v1/config") => Some(handle_get(stream, query, state).await),
        ("POST", "/v1/config/reload") => Some(
            write_not_implemented(
                stream,
                "phase-11/live-config-reload",
                "live config reload from disk",
            )
            .await,
        ),
        ("POST", "/v1/config") => Some(
            write_not_implemented(
                stream,
                "phase-11/runtime-config-set",
                "runtime mutation of config keys",
            )
            .await,
        ),
        _ => None,
    }
}

async fn handle_get<W>(stream: &mut W, query: &str, state: &Arc<AdminState>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let key = parse_key(query);
    let cfg_json = match serde_json::to_value(state.config.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "config serialize failed");
            return write_response(
                stream,
                500,
                "Internal Server Error",
                HDR_TEXT,
                "config serialize failed\n",
            )
            .await;
        }
    };
    let value = match key {
        None => cfg_json,
        Some(path) => match walk(&cfg_json, path) {
            Some(v) => v.clone(),
            None => {
                return write_response(
                    stream,
                    404,
                    "Not Found",
                    HDR_TEXT,
                    &format!("unknown config key `{path}`\n"),
                )
                .await;
            }
        },
    };
    let body = match serde_json::to_string(&value) {
        Ok(s) => s + "\n",
        Err(_) => {
            return write_response(stream, 500, "Internal Server Error", HDR_TEXT, "encode\n").await
        }
    };
    write_response(stream, 200, "OK", HDR_JSON, &body).await
}

fn parse_key(query: &str) -> Option<&str> {
    if query.is_empty() {
        return None;
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("key=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}

fn walk<'a>(root: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut cursor = root;
    for segment in dotted.split('.') {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_extracts_dotted() {
        assert_eq!(parse_key(""), None);
        assert_eq!(
            parse_key("key=workers.decay.interval"),
            Some("workers.decay.interval")
        );
        assert_eq!(parse_key("other=1"), None);
        assert_eq!(parse_key("key="), None);
    }

    #[test]
    fn walk_steps_segments() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":{"b":{"c":42}},"x":1}"#).unwrap();
        assert_eq!(walk(&v, "a.b.c").unwrap(), &serde_json::json!(42));
        assert_eq!(walk(&v, "x").unwrap(), &serde_json::json!(1));
        assert!(walk(&v, "missing.path").is_none());
    }
}
