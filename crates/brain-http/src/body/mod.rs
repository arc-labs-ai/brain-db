//! Body types and helpers.
//!
//! brain-http does not define a new body trait — [`http_body::Body`]
//! is the version-neutral abstraction. This module re-exports the
//! combinators most handlers want and adds two Brain-specific helpers:
//!
//! - [`empty`] / [`full`] — produce a [`ResponseBody`] for the common
//!   "no body" and "fixed-size body" cases.
//! - [`read_to_bytes`] — bounded body reader; the safe way to collect
//!   a request body without OOM-ing on adversarial input.

pub use http_body::{Body, Frame, SizeHint};
pub use http_body_util::combinators::BoxBody;
pub use http_body_util::{BodyExt, Empty, Full, StreamBody};

mod limits;
pub use limits::{read_to_bytes, MAX_BODY_BYTES};

use bytes::Bytes;

/// Body alias used by Brain handlers that return a fixed-size response
/// (JSON, plain text). Falls under [`ResponseBody`] when boxed.
pub type StaticBody = Full<Bytes>;

/// Boxed body for handlers that may return any of: `Empty`, `Full`,
/// `Stream`, or an error-mapped variant. Routers store handlers
/// returning this type to keep the dispatch table monomorphic.
pub type ResponseBody = BoxBody<Bytes, crate::Error>;

/// Construct an empty response body.
#[must_use]
pub fn empty() -> ResponseBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// Construct a response body from a `Bytes`-convertible value.
#[must_use]
pub fn full(bytes: impl Into<Bytes>) -> ResponseBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn empty_collects_to_zero_bytes() {
        let body = empty();
        let collected = body.collect().await.unwrap().to_bytes();
        assert!(collected.is_empty());
    }

    #[tokio::test]
    async fn full_round_trips_payload() {
        let body = full("hello");
        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(collected.as_ref(), b"hello");
    }
}
