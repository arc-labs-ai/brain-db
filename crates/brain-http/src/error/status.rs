//! Map [`Error`] variants to HTTP status codes.

use http::StatusCode;

use super::Error;

/// Compute the wire status for a brain-http error. Centralised here so
/// the table is reviewable in one place.
#[must_use]
pub fn status_for_error(err: &Error) -> StatusCode {
    match err {
        Error::BodyTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        Error::HeaderTooLarge { .. } => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        Error::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        Error::Server(s) | Error::Client(s) => *s,
        Error::Upgrade(_) => StatusCode::BAD_REQUEST,
        Error::ConnectionClosed => StatusCode::SERVICE_UNAVAILABLE,
        // I/O, hyper, http: server-side oops by default.
        Error::Io(_) | Error::Hyper(_) | Error::Http(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
