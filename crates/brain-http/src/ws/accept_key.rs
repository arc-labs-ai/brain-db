//! `Sec-WebSocket-Accept` derivation per RFC 6455 §4.2.2.
//!
//! ```text
//! accept = base64(SHA-1(client_key || GUID))
//! ```
//!
//! The GUID is the constant `258EAFA5-E914-47DA-95CA-C5AB0DC85B11`
//! defined by the spec. Together with the client's randomly-generated
//! `Sec-WebSocket-Key`, this proves the server actually understood
//! the WebSocket upgrade rather than echoing a value back unmodified.

use base64::Engine as _;
use sha1::{Digest, Sha1};

/// RFC 6455 §4.2.2.
const WS_MAGIC_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Compute the `Sec-WebSocket-Accept` value the server returns in
/// the `101 Switching Protocols` response.
#[must_use]
pub fn derive(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_MAGIC_GUID);
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6455 §1.3 worked example.
    #[test]
    fn rfc_6455_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let expected = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert_eq!(derive(key), expected);
    }

    #[test]
    fn different_keys_produce_different_accepts() {
        let a = derive("aaa==");
        let b = derive("bbb==");
        assert_ne!(a, b);
    }
}
