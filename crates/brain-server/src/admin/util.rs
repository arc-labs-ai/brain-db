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

/// Uniform `501 Not Implemented` body shape used by routes whose CLI
/// surface is wired (Phase 10) but whose server-side primitive lands
/// in a later phase.
///
/// Shape: `{"error":"not_implemented","deferred_to":<slug>,"detail":<text>}`.
pub fn not_implemented(deferred_to: &str, detail: &str) -> Response<ResponseBody> {
    let body = format!(
        "{{\"error\":\"not_implemented\",\"deferred_to\":\"{deferred_to}\",\"detail\":\"{detail}\"}}\n"
    );
    json_response(StatusCode::NOT_IMPLEMENTED, body)
}
