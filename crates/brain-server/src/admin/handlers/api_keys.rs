//! Admin HTTP routes for scope-bound API keys (W2.5).
//!
//! Three actions:
//!
//! - `POST /v1/api-keys` — mint a key. Body is JSON with `agent_id`,
//!   `namespace`, `permissions` (string array or `u32` bitfield), and
//!   optional `user_id` / `org_id` (both reserved audit tags, default
//!   all-zero). Minting interns the namespace and binds the agent +
//!   permissions. The reply carries the raw secret once — never again.
//! - `GET /v1/api-keys?agent=…` — list keys for the given agent.
//! - `DELETE /v1/api-keys/<hex>` — revoke a key by hex key-hash.
//!
//! All three are admin-only and intended to be reached over the
//! loopback / mTLS-fronted admin listener.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_http::body::ResponseBody;
use brain_metadata::api_keys::bits;
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::auth::hex32;

/// JSON body of `POST /v1/api-keys`.
#[derive(Debug, Deserialize)]
struct MintBody {
    /// 32 hex chars; optional. Reserved audit tag, all-zero when omitted
    /// (the locked design dropped org_id as an identity dimension).
    #[serde(default)]
    org_id_hex: Option<String>,
    /// 32 hex chars; optional. All-zero when omitted.
    #[serde(default)]
    user_id_hex: Option<String>,
    /// 32 hex chars = 16 bytes.
    agent_id_hex: String,
    /// Schema namespace ("acme", "brain", …).
    namespace: String,
    /// Either a list of named permissions or a raw `u32` bitfield.
    permissions: PermissionsSpec,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PermissionsSpec {
    Bits(u32),
    Names(Vec<String>),
}

impl PermissionsSpec {
    fn resolve(&self) -> Result<u32, String> {
        match self {
            Self::Bits(n) => Ok(*n),
            Self::Names(list) => {
                let mut bitset: u32 = 0;
                for name in list {
                    let bit = match name.to_ascii_uppercase().as_str() {
                        "ENCODE" => bits::ENCODE,
                        "RECALL" => bits::RECALL,
                        "FORGET" => bits::FORGET,
                        "LINK" => bits::LINK,
                        "SCHEMA_UPLOAD" => bits::SCHEMA_UPLOAD,
                        "ADMIN" => bits::ADMIN,
                        "STANDARD_AGENT" => bits::STANDARD_AGENT,
                        "READ_ONLY" => bits::READ_ONLY,
                        "READ_WRITE" => bits::READ_WRITE,
                        "FULL" => bits::FULL,
                        other => return Err(format!("unknown permission: {other:?}")),
                    };
                    bitset |= bit;
                }
                Ok(bitset)
            }
        }
    }
}

/// Reply shape for `POST /v1/api-keys`.
#[derive(Debug, Serialize)]
struct MintReply {
    /// `brain_<base64url>` — the secret. Never logged, never echoed
    /// back later. The caller must store this themselves.
    secret: String,
    /// Hex of `BLAKE3(secret)`. Use this to revoke later.
    key_hash_hex: String,
}

/// Single row in `GET /v1/api-keys?agent=…`.
#[derive(Debug, Serialize)]
struct ApiKeyView {
    key_hash_hex: String,
    org_id_hex: String,
    user_id_hex: String,
    agent_id_hex: String,
    namespace: String,
    permissions: u32,
    created_at_unix_nanos: u64,
    last_used_at_unix_nanos: u64,
    revoked: bool,
}

#[derive(Debug, Serialize)]
struct ListReply {
    keys: Vec<ApiKeyView>,
}

/// Dispatch entry: routed at `/v1/api-keys` for POST + GET, and at
/// `/v1/api-keys/` prefix for DELETE.
pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    match req.method().clone() {
        Method::POST => mint(req, state).await,
        Method::GET => list(req, state).await,
        Method::DELETE => revoke(req, state).await,
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        )),
    }
}

async fn mint(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let body = match collect_body(req).await {
        Ok(b) => b,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
    };
    let parsed: MintBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON body: {e}\n"),
            ))
        }
    };
    let org_id = match parsed.org_id_hex.as_deref() {
        Some(s) => match parse_16(s) {
            Ok(b) => b,
            Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
        },
        None => [0u8; 16],
    };
    let user_id = match parsed.user_id_hex.as_deref() {
        Some(s) => match parse_16(s) {
            Ok(b) => b,
            Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
        },
        None => [0u8; 16],
    };
    let agent_id = match parse_16(&parsed.agent_id_hex) {
        Ok(b) => b,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
    };
    let permissions = match parsed.permissions.resolve() {
        Ok(b) => b,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let minted = match state.auth_store.mint(
        org_id,
        user_id,
        parsed.namespace,
        agent_id,
        permissions,
        now,
    ) {
        Ok(m) => m,
        Err(e) => {
            return Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("mint failed: {e}\n"),
            ))
        }
    };
    let reply = MintReply {
        secret: minted.formatted(),
        key_hash_hex: hex32(&minted.key_hash),
    };
    let body = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into());
    Ok(json_response(StatusCode::CREATED, body))
}

async fn list(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("");
    let agent_hex = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("agent="))
        .unwrap_or("");
    if agent_hex.is_empty() {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing ?agent=<32-hex-agent-id>\n",
        ));
    }
    let agent_id = match parse_16(agent_hex) {
        Ok(b) => b,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
    };
    let rows = match state.auth_store.list_for_agent(&agent_id) {
        Ok(r) => r,
        Err(e) => {
            return Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("list failed: {e}\n"),
            ))
        }
    };
    let keys = rows
        .into_iter()
        .map(|r| ApiKeyView {
            key_hash_hex: hex32(&r.key_hash),
            org_id_hex: hex16(&r.org_id),
            user_id_hex: hex16(&r.user_id),
            agent_id_hex: hex16(&r.agent_id),
            namespace: r.namespace,
            permissions: r.permissions,
            created_at_unix_nanos: r.created_at_unix_nanos,
            last_used_at_unix_nanos: r.last_used_at_unix_nanos,
            revoked: r.revoked,
        })
        .collect();
    let body = serde_json::to_string(&ListReply { keys }).unwrap_or_else(|_| "{}".into());
    Ok(json_response(StatusCode::OK, body))
}

async fn revoke(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let path = req.uri().path();
    let key_hash_hex = path.trim_start_matches("/v1/api-keys/");
    if key_hash_hex.is_empty() {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing key hash in path\n",
        ));
    }
    let key_hash = match parse_32(key_hash_hex) {
        Ok(b) => b,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
    };
    match state.auth_store.revoke(&key_hash) {
        Ok(true) => Ok(text_response(StatusCode::NO_CONTENT, "")),
        Ok(false) => Ok(text_response(StatusCode::NOT_FOUND, "key not found\n")),
        Err(e) => Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("revoke failed: {e}\n"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn collect_body(req: Request<Incoming>) -> Result<Bytes, String> {
    req.into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("body read failed: {e}\n"))
}

fn parse_16(s: &str) -> Result<[u8; 16], String> {
    decode_hex_bytes::<16>(s).map_err(|msg| msg + "\n")
}

fn parse_32(s: &str) -> Result<[u8; 32], String> {
    decode_hex_bytes::<32>(s).map_err(|msg| msg + "\n")
}

fn decode_hex_bytes<const N: usize>(s: &str) -> Result<[u8; N], String> {
    if s.len() != N * 2 {
        return Err(format!("expected {} hex chars, got {}", N * 2, s.len()));
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(s.as_bytes()[i * 2])?;
        let lo = hex_nibble(s.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(format!("invalid hex character: {:?}", other as char)),
    }
}

fn hex16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
