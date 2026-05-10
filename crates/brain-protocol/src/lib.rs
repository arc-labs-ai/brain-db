//! # brain-protocol
//!
//! Brain's wire protocol: a custom binary protocol over TCP (with optional
//! TLS). Frames have a fixed 32-byte header, a magic of `b"BRN0"`, header
//! and payload CRC32C, and a 24-bit payload length cap (16 MiB).
//!
//! See `spec/03_wire_protocol/` for the authoritative format. The stub below
//! pins constants and the `Opcode` enum; full encode/decode lands in Phase 1.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

/// Frame magic bytes. Identifies a Brain frame on the wire.
pub const MAGIC: [u8; 4] = *b"BRN0";

/// Fixed frame header size in bytes.
pub const HEADER_SIZE: usize = 32;

/// Maximum payload size (16 MiB), enforced by the 24-bit length field.
pub const MAX_PAYLOAD_BYTES: usize = (1 << 24) - 1;

/// Wire-protocol opcodes. Numeric values are stable per
/// `spec/03_wire_protocol/05_opcodes.md`. Do NOT renumber.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    // Cognitive primitives.
    Encode = 0x01,
    Recall = 0x02,
    Plan = 0x03,
    Reason = 0x04,
    Forget = 0x05,
    // Graph operations.
    Link = 0x10,
    Unlink = 0x11,
    // Transactions.
    TxnBegin = 0x20,
    TxnCommit = 0x21,
    TxnAbort = 0x22,
    // Streaming.
    Subscribe = 0x30,
    Next = 0x31,
    Complete = 0x32,
    // Admin.
    AdminStats = 0x40,
    AdminHealth = 0x41,
    // Errors / system.
    Error = 0xF0,
}

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
