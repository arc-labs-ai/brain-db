//! Brain-flavoured response helpers used by every admin handler.
//!
//! All three helpers return [`Response<ResponseBody>`], suitable for
//! direct use from a handler that satisfies brain-http's `Service`
//! shape. They centralise the `Response::builder()` + content-type
//! boilerplate so individual handlers stay focused on the business
//! logic.

use brain_http::body::{full, ResponseBody};
use bytes::Bytes;
use http::{Response, StatusCode};

const HDR_JSON: &str = "application/json; charset=utf-8";
const HDR_TEXT: &str = "text/plain; charset=utf-8";

/// Wrap a JSON body string in a response with the given status.
pub fn json_response(status: StatusCode, body: String) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("content-type", HDR_JSON)
        .body(full(Bytes::from(body)))
        .expect("static response always builds")
}

/// Wrap a plain-text body in a response with the given status.
pub fn text_response(status: StatusCode, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("content-type", HDR_TEXT)
        .body(full(Bytes::copy_from_slice(body.as_bytes())))
        .expect("static response always builds")
}

/// Uniform `401 Unauthorized` response for admin requests that fail the
/// operator admin-secret check.
pub fn unauthorized() -> Response<ResponseBody> {
    json_response(
        StatusCode::UNAUTHORIZED,
        "{\"error\":\"unauthorized\",\"detail\":\"admin requests require \
         Authorization: Bearer <admin token>\"}\n"
            .to_string(),
    )
}

/// Constant-time byte-slice equality. Returns `false` immediately on a
/// length mismatch (the only timing signal it leaks is the secret length,
/// which is not sensitive for a bearer token).
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Uniform `501 Not Implemented` body shape used by routes whose CLI
/// surface is wired but whose server-side primitive lands later.
///
/// Shape: `{"error":"not_implemented","deferred_to":<slug>,"detail":<text>}`.
pub fn not_implemented(deferred_to: &str, detail: &str) -> Response<ResponseBody> {
    let body = format!(
        "{{\"error\":\"not_implemented\",\"deferred_to\":\"{deferred_to}\",\"detail\":\"{detail}\"}}\n"
    );
    json_response(StatusCode::NOT_IMPLEMENTED, body)
}
