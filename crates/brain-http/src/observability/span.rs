//! Span constructors — OTel HTTP server semantic-convention compliant.
//!
//! Attribute names follow `opentelemetry.io/docs/specs/semconv/http/`.
//! Spec §14/03 picks the server-side subset; field names here mirror
//! those exactly so a Jaeger / Tempo backend can join correctly.

use std::net::SocketAddr;

use http::Request;
use tracing::Span;

/// Construct a span describing one inbound request. M8 enters this
/// span around the entire request lifecycle.
#[must_use]
pub fn request_span<B>(req: &Request<B>) -> Span {
    tracing::info_span!(
        "http.request",
        http.method   = %req.method(),
        http.path     = %req.uri().path(),
        http.version  = ?req.version(),
        net.peer.ip   = tracing::field::Empty,
        otel.kind     = "server",
    )
}

/// Construct a span describing one accepted TCP connection. M2
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
