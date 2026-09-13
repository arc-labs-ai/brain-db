//! Span constructors — OTel HTTP server semantic-convention compliant.
//!
//! Attribute names follow `opentelemetry.io/docs/specs/semconv/http/`.
//! picks the server-side subset; field names here mirror
//! those exactly so a Jaeger / Tempo backend can join correctly.

use std::net::SocketAddr;

use http::Request;
use tracing::Span;

/// Construct a span describing one inbound request.
///
/// `http.response.status_code` starts empty and is recorded onto the
/// span by [`record_status`] after the handler returns. The server
/// enters this span around the request body of
/// `connection::handle_request`.
#[must_use]
pub fn request_span<B>(req: &Request<B>) -> Span {
    tracing::info_span!(
        "http.request",
        http.method               = %req.method(),
        http.path                 = %req.uri().path(),
        http.version              = ?req.version(),
        http.response.status_code = tracing::field::Empty,
        otel.kind                 = "server",
    )
}

/// Record `http.response.status_code` on a request span after the
/// handler returns. The status is a `u16` rather than
/// `http::StatusCode` so this helper is usable from anywhere on the
/// response path (including the timeout fallback that builds the 504
/// directly).
pub fn record_status(span: &Span, status: u16) {
    span.record("http.response.status_code", status);
}

/// Construct a span describing one accepted TCP connection. The server
/// enters this span on each accept; child request spans descend
/// from it.
#[must_use]
pub fn connection_span(peer: SocketAddr) -> Span {
    tracing::info_span!(
        "http.connection",
        net.peer.ip   = %peer.ip(),
        net.peer.port = peer.port(),
        otel.kind     = "server",
    )
}
