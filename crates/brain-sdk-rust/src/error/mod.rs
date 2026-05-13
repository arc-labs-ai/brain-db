//! `ClientError` — the SDK's unified error type.
//!
//! Mirrors the spec §13/02 §13 enum sketch. Variants present in
//! 10.1 cover the failure surface a connecting client can hit;
//! later sub-tasks may add `Overloaded`, `Timeout`, etc. without
//! breaking callers (the enum is `#[non_exhaustive]`).

use std::io;

use brain_protocol::error::ProtocolError;

/// Failures returned by `Client::connect`, `Client::bye`, and the
/// per-op methods landing in 10.5+.
///
/// Variants are stable; new ones may be added in future minor
/// releases (`#[non_exhaustive]`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// TCP connect failed (refused, unreachable, DNS, …).
    #[error("connect failed: {0}")]
    Connect(#[source] io::Error),

    /// Handshake / authentication failure. The server closed the
    /// connection during HELLO → WELCOME → AUTH → AUTH_OK.
    #[error("handshake failed: {0}")]
    Handshake(String),

    /// The server rejected our AUTH frame.
    #[error("authentication rejected: {0}")]
    Auth(String),

    /// Wire-protocol failure: frame decode error, CRC mismatch,
    /// unexpected opcode, truncated read.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// Socket I/O error after the handshake completed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// The connection was closed (by us or by the peer).
    #[error("connection closed")]
    Closed,

    /// Server returned an ERROR frame. The string carries the
    /// server's diagnostic message; consult [`code`] to dispatch
    /// programmatically.
    ///
    /// [`code`]: ClientError::code
    #[error("server error ({code}): {message}")]
    Server {
        /// Wire error code from spec §03/10.
        code: u16,
        /// Human-readable detail from the server.
        message: String,
    },
}

impl ClientError {
    /// Return the spec §03/10 error code for variants that carry
    /// one. Returns `None` for client-side failures (Connect, Io,
    /// Closed) that don't map to a wire code.
    #[must_use]
    pub fn code(&self) -> Option<u16> {
        match self {
            Self::Server { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Whether retrying the same request is safe.
    ///
    /// Per spec §13/04 §3, retryable failures are: `Overloaded`,
    /// transient network errors. 10.1 returns conservatively
    /// `false`; 10.3 refines this with the full mapping.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Io(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_extracts_server_code() {
        let e = ClientError::Server {
            code: 0x0801,
            message: "x".into(),
        };
        assert_eq!(e.code(), Some(0x0801));
    }

    #[test]
    fn code_none_for_client_side() {
        let e = ClientError::Connect(io::Error::new(io::ErrorKind::ConnectionRefused, "x"));
        assert_eq!(e.code(), None);
    }

    #[test]
    fn is_retryable_for_io_and_connect() {
        let connect = ClientError::Connect(io::Error::new(io::ErrorKind::ConnectionRefused, "x"));
        assert!(connect.is_retryable());

        let io_err = ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "x"));
        assert!(io_err.is_retryable());

        let closed = ClientError::Closed;
        assert!(!closed.is_retryable());
    }
}
