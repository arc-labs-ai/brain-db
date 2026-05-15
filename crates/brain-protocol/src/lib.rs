//! # brain-protocol
//!
//! Brain's wire protocol: a custom binary protocol over TCP (with optional
//! TLS). Frames have a fixed 32-byte header, a magic of `b"BRN0"`, header
//! and payload CRC32C, and a 24-bit payload length cap (16 MiB).
//!
//! See `spec/03_wire_protocol/` for the authoritative format.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod convert;
pub mod crc;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod header;
pub mod knowledge;
pub mod opcode;
pub mod request;
pub mod requests;
pub mod response;
pub mod responses;
mod rkyv_codec;

pub use error::{ErrorCategory, ErrorCode, ProtocolError};
pub use frame::Frame;
pub use header::{Header, VERSION};
pub use opcode::Opcode;
pub use request::RequestBody;
pub use response::ResponseBody;

/// Frame magic bytes. Identifies a Brain frame on the wire.
pub const MAGIC: [u8; 4] = *b"BRN0";

/// Fixed frame header size in bytes.
pub const HEADER_SIZE: usize = 32;

/// Maximum payload size (16 MiB - 1), enforced by the 24-bit length field.
pub const MAX_PAYLOAD_BYTES: usize = (1 << 24) - 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_brn0() {
        assert_eq!(&MAGIC, b"BRN0");
    }

    #[test]
    fn header_size_is_32() {
        assert_eq!(HEADER_SIZE, 32);
    }

    #[test]
    fn max_payload_fits_in_24_bits() {
        assert_eq!(MAX_PAYLOAD_BYTES, 16 * 1024 * 1024 - 1);
    }
}
