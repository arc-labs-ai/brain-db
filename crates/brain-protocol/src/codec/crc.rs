//! CRC32C wrappers for the wire protocol.
//!
//! Brain uses **CRC32C** (Castagnoli polynomial `0x1EDC6F41`, the iSCSI
//! variant — *not* the Ethernet CRC32). Hardware-accelerated on x86 (SSE
//! 4.2) and ARM (CRC32 extension).
//!
//! Two CRCs are computed independently per frame:
//!
//! - [`header_crc`] over the header bytes that don't include the
//!   `header_crc32c` field itself (bytes 0..8 followed by bytes 12..32 of
//!   a 32-byte header).
//! - [`payload_crc`] over the payload bytes that follow the header.
//!
//! Both are stored on the wire as **big-endian** `u32`.

/// CRC32C of the header bytes excluding the `header_crc32c` field.
///
/// Per, the CRC is computed over the header's bytes
/// `0..8` followed by `12..32` — i.e., the entire 32-byte header *minus*
/// the 4-byte CRC slot at offsets 8..12. Callers must supply the
/// concatenated 28 bytes; this function does not handle splicing.
///
/// Returns the raw CRC32C as a host `u32`. Convert with [`u32::to_be_bytes`]
/// when writing into the header's `header_crc32c` field.
#[inline]
#[must_use]
pub fn header_crc(header_bytes_excl_crc: &[u8]) -> u32 {
    crc32c::crc32c(header_bytes_excl_crc)
}

/// CRC32C of a payload.
///
/// Per, computed over all payload bytes (after the
/// 32-byte header). If the frame has no payload (`payload_len == 0`),
/// callers MUST store `0` in the header's `payload_crc32c` field rather
/// than calling this function on an empty slice.
///
/// Returns the raw CRC32C as a host `u32`. Convert with [`u32::to_be_bytes`]
/// when writing into the header's `payload_crc32c` field.
#[inline]
#[must_use]
pub fn payload_crc(payload: &[u8]) -> u32 {
    crc32c::crc32c(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard CRC32C test vector: `crc32c("123456789") == 0xE3069283`.
    /// See <https://datatracker.ietf.org/doc/html/rfc3720#appendix-B.4>.
    #[test]
    fn payload_crc_known_vector_check_bytes() {
        assert_eq!(payload_crc(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn payload_crc_empty_input() {
        assert_eq!(payload_crc(&[]), 0);
    }

    #[test]
    fn payload_crc_single_byte() {
        // crc32c(0x00) is a stable, hand-pinned value (zero seed, single
        // byte through the Castagnoli table).
        assert_eq!(payload_crc(&[0x00]), 0x527D_5351);
    }

    #[test]
    fn header_crc_matches_payload_crc_for_same_input() {
        // The two functions are pure CRC32C wrappers — given identical
        // input bytes they MUST yield identical output. This pins the
        // contract that they share an implementation.
        let bytes = b"the brain frame header bytes...";
        assert_eq!(header_crc(bytes), payload_crc(bytes));
    }

    /// Sealing a `Header` with [`crate::codec::header::Header::new`] and then
    /// recomputing the header CRC over bytes `0..8 ++ 12..32` of the
    /// resulting on-wire bytes MUST yield the value stored at offsets
    /// `8..12`. This pins the.6 "computed minus this field" rule.
    #[test]
    fn header_crc_excludes_self() {
        let h = crate::codec::header::Header::new(0x21, 0x0000, 7, 64);
        let bytes: [u8; 32] = bytemuck::cast(h);
        let stored = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let mut spliced = [0u8; 28];
        spliced[..8].copy_from_slice(&bytes[..8]);
        spliced[8..].copy_from_slice(&bytes[12..32]);
        assert_eq!(header_crc(&spliced), stored);
    }

    /// Hand-pinned vector for the exact 28-byte input that
    /// `Header::new(0x10, 0, 0, 0)` produces (i.e. bytes 0..8 ++ 12..32 of
    /// a freshly built PING-shaped header). If the layout or constants
    /// change, this test fails — that is the intent.
    #[test]
    fn header_crc_known_vector_for_minimal_header() {
        let mut input = [0u8; 28];
        // bytes 0..4 — magic
        input[0..4].copy_from_slice(b"BRN0");
        // byte 4 — version = 1
        input[4] = 1;
        // byte 5 — opcode = 0x10
        input[5] = 0x10;
        // bytes 6..7 — flags = 0
        // bytes 8..28 correspond to offsets 12..32 of the header:
        //   stream_id (4) + payload_len (3) + reserved (1) +
        //   payload_crc32c (4) + reserved (8) — all zero.

        // Hand-pinned (computed by the reference `crc32c` crate). If this
        // changes, the wire-format constants changed and call sites must
        // be reviewed.
        const EXPECTED: u32 = 0x2982_4C64;
        assert_eq!(header_crc(&input), EXPECTED);
    }
}
