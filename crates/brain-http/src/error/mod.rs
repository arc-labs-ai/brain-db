//! Brain-HTTP error taxonomy, built on `thiserror`.
//!
//! Variants are deliberately Brain-flavoured — they correspond to the
//! shapes the admin / client surfaces care about. Hyper's own
//! [`hyper::Error`] is collapsed under [`Error::Hyper`] for now; finer
//! mapping can come later if it's worth the surface.

use http::StatusCode;

mod status;
pub use status::status_for_error;

/// All errors brain-http exposes. `#[non_exhaustive]` because we
/// expect to grow the variant set over time.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying I/O failure (typically socket-level).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Hyper produced an error. Collapsed wrapper for now; a future
    /// revision may pattern-match on `hyper::Error` to surface specific
    /// Brain variants.
    #[error("hyper: {0}")]
    Hyper(#[from] hyper::Error),

    /// `http` crate-level errors (header construction, URI parsing).
    #[error("http: {0}")]
    Http(#[from] http::Error),

    /// Inbound body exceeded the configured byte limit. Returned by
    /// [`crate::body::read_to_bytes`] without buffering the rest of
    /// the body (mitigates a trivial-DoS pattern).
    #[error("body too large: {actual} > {limit} bytes")]
    BodyTooLarge {
        /// Bytes seen on the wire or declared in `Content-Length`.
        actual: u64,
        /// Configured ceiling.
        limit: u64,
    },

    /// Request header block exceeded the configured byte limit.
    #[error("header too large: {actual} > {limit} bytes")]
    HeaderTooLarge {
        /// Bytes consumed by the header block.
        actual: usize,
        /// Configured ceiling.
        limit: usize,
    },

    /// Per-request wall-clock timeout fired.
    #[error("request timeout after {0:?}")]
    Timeout(std::time::Duration),

    /// Connection closed before the response was fully received /
    /// sent.
    #[error("connection closed")]
    ConnectionClosed,

    /// HTTP/1.1 Upgrade handshake (e.g. WebSocket) failed.
    #[error("upgrade failed: {0}")]
    Upgrade(String),

    /// Server-side error worth surfacing with a specific status
    /// code (5xx).
    #[error("server error: {0}")]
    Server(StatusCode),

    /// Client-side request error (4xx).
    #[error("client error: {0}")]
    Client(StatusCode),
}

/// Crate-wide `Result` alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The HTTP status code a server-side response builder should
    /// emit if this error reaches the response path. Useful in
    /// handler error branches.
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        status_for_error(self)
    }
}

/// `Infallible` from `http_body_util::{Empty, Full}`. Lets bounded
/// helpers in [`crate::body`] accept those body types directly.
impl From<std::convert::Infallible> for Error {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}
