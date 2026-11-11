//! `GET /v1/config[?key=...]` — read the loaded config (or a subtree).
//!
//! Spec uses dotted paths like `workers.decay.interval`. We serialize
//! the config to a `serde_json::Value`, walk by segment, and return
//! whatever is at that subtree (object/scalar/array).
//!
//! Secrets (`admin.token`, `llm.credentials.kek`, and anything under a key
//! named `api_key` / `token` / `secret` / `password` / `kek`, or ending in
//! `_key`) are redacted before any response is built, so an admin-token
//! holder can confirm whether a credential is *set* without ever reading its
//! value — and no `?key=` subtree access can reach around the redaction,
//! because it runs on the full tree first.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use serde_json::Value;
use tracing::warn;

use crate::admin::handlers::config::walk;
use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

/// Object keys whose values are secrets and must never be serialized to a
/// client. Matched case-insensitively, at any depth.
///
/// Any key ending in `_key` is also treated as a secret — see
/// [`is_secret_key`]. The list and the suffix rule are both needed: a name
/// like `kek` carries key material without matching the suffix, and a name
/// like `summarizer_api_key` matches the suffix without being on the list.
///
/// `kek` is the sealing key for every stored tenant provider credential, so
/// serving it is equivalent to serving all of them at once.
const SECRET_KEYS: &[&str] = &["api_key", "token", "secret", "password", "kek"];

/// Suffix that marks a key-material field whatever it is prefixed with, so a
/// newly added `*_key` secret is redacted by default rather than on the day
/// someone remembers to extend [`SECRET_KEYS`].
///
/// Deliberately a SUFFIX with the underscore, not a substring: `tls.key` is a
/// path to a key file on disk, not key material, and operators need to read it
/// to diagnose a bad path. `key` alone therefore does not match.
const SECRET_KEY_SUFFIX: &str = "_key";

/// Whether an object key names a secret.
fn is_secret_key(k: &str) -> bool {
    let lower = k.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| lower == *s) || lower.ends_with(SECRET_KEY_SUFFIX)
}

/// Marker substituted for a set secret. A distinct sentinel (not `null`)
/// so operators can tell "configured" from "unset".
const REDACTED: &str = "***redacted***";

/// Recursively replace the value of any secret-named key. A `null` value is
/// left as `null` (unset stays visibly unset); any present value — string or
/// otherwise — becomes [`REDACTED`].
fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_secret_key(k) {
                    if !v.is_null() {
                        *v = Value::String(REDACTED.to_string());
                    }
                } else {
                    redact_secrets(v);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_secrets(item);
            }
        }
        _ => {}
    }
}

pub async fn get(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let key = query::config_key(&query_str).map(|s| s.to_owned());
    let cfg_json = match serde_json::to_value(state.config.as_ref()) {
        Ok(mut v) => {
            // Redact on the full tree BEFORE any subtree walk, so
            // `?key=llm.api_key` cannot bypass it.
            redact_secrets(&mut v);
            v
        }
        Err(e) => {
            warn!(error = %e, "config serialize failed");
            return Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config serialize failed\n",
            ));
        }
    };
    let value = match key.as_deref() {
        None => cfg_json,
        Some(path) => match walk(&cfg_json, path) {
            Some(v) => v.clone(),
            None => {
                return Ok(text_response(
                    StatusCode::NOT_FOUND,
                    &format!("unknown config key `{path}`\n"),
                ));
            }
        },
    };
    let body = match serde_json::to_string(&value) {
        Ok(s) => s + "\n",
        Err(_) => return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "encode\n")),
    };
    Ok(json_response(StatusCode::OK, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_set_secrets_at_any_depth() {
        let mut v = json!({
            "llm": { "api_key": "sk-live-abc", "model": "gpt-4o-mini" },
            "admin": { "token": "operator-secret" },
            "server": { "listen_addr": "0.0.0.0:8080" },
        });
        redact_secrets(&mut v);
        assert_eq!(v["llm"]["api_key"], REDACTED);
        assert_eq!(v["admin"]["token"], REDACTED);
        // Non-secret fields are untouched.
        assert_eq!(v["llm"]["model"], "gpt-4o-mini");
        assert_eq!(v["server"]["listen_addr"], "0.0.0.0:8080");
    }

    /// A secret is redacted by the shape of its name, not by appearing on an
    /// exact-match list. An unlisted `*_key` / `*_kek` served in plaintext
    /// would defeat this module's whole premise, which is that an admin-token
    /// holder can confirm a secret is SET without reading it.
    #[test]
    fn redacts_a_secret_whose_name_merely_ends_in_a_key_suffix() {
        let mut v = json!({
            "llm": {
                "model": "gpt-4o-mini",
                "credentials": { "kek": "0f1e2d3c", "kek_file": "/var/lib/brain/kek" }
            }
        });
        redact_secrets(&mut v);
        assert_eq!(
            v["llm"]["credentials"]["kek"], REDACTED,
            "a `*_kek` secret must be redacted without being listed by name"
        );
        // Non-secret siblings stay readable: an operator diagnosing a wrong
        // path needs to see the path, and the model is not a secret.
        assert_eq!(v["llm"]["credentials"]["kek_file"], "/var/lib/brain/kek");
        assert_eq!(v["llm"]["model"], "gpt-4o-mini");
    }

    /// Every secret the config type redacts from its own `Debug` output must
    /// also be redacted by this endpoint. Two independent redaction surfaces
    /// drift apart silently: `llm.credentials.kek` was redacted in `Debug`
    /// from the day it was added and served in plaintext here, because `kek`
    /// matches neither the exact-match list nor the `_key` suffix.
    ///
    /// Driven off the real config rather than a hand-written JSON blob, so a
    /// newly added secret field is covered without this test being edited.
    #[test]
    fn no_secret_the_config_redacts_from_debug_survives_serialization() {
        const SENTINEL: &str = "SENTINEL-NEVER-SERVE-THIS";

        let mut cfg = crate::config::Config::for_tests();
        cfg.llm.credentials.kek = Some(SENTINEL.to_string());
        cfg.admin.token = Some(SENTINEL.to_string());

        // The config's own `Debug` is the reference for what counts as a
        // secret; if it hides a value, this endpoint must too.
        let debugged = format!("{cfg:?}");
        assert!(
            !debugged.contains(SENTINEL),
            "the config's Debug impl must redact these fields for this test to \
             mean anything; it leaked one instead"
        );

        let mut v = serde_json::to_value(&cfg).expect("config serializes");
        redact_secrets(&mut v);
        let rendered = v.to_string();
        assert!(
            !rendered.contains(SENTINEL),
            "a secret the config redacts from Debug reached the response body: {rendered}"
        );
        assert_eq!(v["llm"]["credentials"]["kek"], REDACTED);
        assert_eq!(v["admin"]["token"], REDACTED);
    }

    /// `tls.key` is a PATH to a key file, not key material. Operators read it
    /// to diagnose a wrong path, so the suffix rule must not swallow it.
    #[test]
    fn leaves_a_bare_key_path_readable() {
        let mut v = json!({ "tls": { "cert": "/etc/c.pem", "key": "/etc/k.pem" } });
        redact_secrets(&mut v);
        assert_eq!(v["tls"]["key"], "/etc/k.pem");
        assert_eq!(v["tls"]["cert"], "/etc/c.pem");
    }

    #[test]
    fn leaves_unset_secrets_as_null() {
        // An unset credential (serialized as null) stays visibly unset, so
        // operators can distinguish "not configured" from "redacted".
        let mut v = json!({ "llm": { "api_key": Value::Null } });
        redact_secrets(&mut v);
        assert!(v["llm"]["api_key"].is_null());
    }

    #[test]
    fn redaction_is_case_insensitive_and_covers_arrays() {
        let mut v = json!({
            "items": [{ "Token": "t1" }, { "PASSWORD": "p1" }],
        });
        redact_secrets(&mut v);
        assert_eq!(v["items"][0]["Token"], REDACTED);
        assert_eq!(v["items"][1]["PASSWORD"], REDACTED);
    }
}
