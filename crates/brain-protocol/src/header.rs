//! Fixed 32-byte frame header.
//!
//! Implements `spec/03_wire_protocol/03_frame_header.md`. All multi-byte
//! header fields are big-endian (spec §03/03 §1, §8).
//!
//! Multi-byte fields are stored as raw big-endian byte arrays rather than
//! native integers so the struct is trivially `bytemuck::Pod` and matches
//! the on-wire layout byte-for-byte without endian conversion at cast
//! boundaries.

use bytemuck::{Pod, Zeroable};

use crate::crc::header_crc;
use crate::error::ProtocolError;
use crate::{MAGIC, MAX_PAYLOAD_BYTES};

/// Wire protocol version. See spec §03/03 §3.2.
pub const VERSION: u8 = 1;

/// 32-byte frame header. Layout matches spec §03/03 §1 exactly.
///
/// `Eq`/`PartialEq` are implemented by hand below — deriving them on a
/// `repr(C, packed)` struct fails because the derive macro takes field
/// references, which packed layout disallows.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Header {
    /// Bytes 0–3: `b"BRN0"`.
    pub magic: [u8; 4],
    /// Byte 4: protocol version.
    pub version: u8,
    /// Byte 5: opcode (see spec §03/05).
    pub opcode: u8,
    /// Bytes 6–7: big-endian `u16` flags.
    pub flags: [u8; 2],
    /// Bytes 8–11: big-endian CRC32C of the rest of the header
    /// (this field is treated as zero during computation).
    pub header_crc32c: [u8; 4],
    /// Bytes 12–15: big-endian `u32` stream id.
    pub stream_id: [u8; 4],
    /// Bytes 16–18: big-endian `u24` payload length.
    pub payload_len: [u8; 3],
    /// Byte 19: reserved; must be zero.
    pub reserved_a: u8,
    /// Bytes 20–23: big-endian CRC32C of the payload.
    pub payload_crc32c: [u8; 4],
    /// Bytes 24–31: reserved; must be zero.
    pub reserved_b: [u8; 8],
}

// Compile-time guarantees about the on-wire footprint.
const _: () = {
    assert!(core::mem::size_of::<Header>() == 32);
    assert!(core::mem::align_of::<Header>() == 1);
};

impl PartialEq for Header {
    fn eq(&self, other: &Self) -> bool {
        let a: &[u8; 32] = bytemuck::cast_ref(self);
        let b: &[u8; 32] = bytemuck::cast_ref(other);
        a == b
    }
}

impl Eq for Header {}

impl Header {
    /// Build a new header for the given opcode/flags/stream/payload, computing
    /// and storing the header CRC32C.
    ///
    /// # Panics
    ///
    /// Panics if `payload_len` exceeds [`MAX_PAYLOAD_BYTES`]. The 24-bit field
    /// physically cannot represent more; callers must split via multi-payload
    /// framing (spec §03/03 §6).
    pub fn new(opcode: u8, flags: u16, stream_id: u32, payload_len: u32) -> Self {
        assert!(
            (payload_len as usize) <= MAX_PAYLOAD_BYTES,
            "payload_len {payload_len} exceeds 24-bit max"
        );
        let len_be = payload_len.to_be_bytes();
        let mut h = Self {
            magic: MAGIC,
            version: VERSION,
            opcode,
            flags: flags.to_be_bytes(),
            header_crc32c: [0; 4],
            stream_id: stream_id.to_be_bytes(),
            payload_len: [len_be[1], len_be[2], len_be[3]],
            reserved_a: 0,
            payload_crc32c: [0; 4],
            reserved_b: [0; 8],
        };
        h.seal();
        h
    }

    /// Recompute the header CRC32C and store it in `header_crc32c`.
    /// Call this after mutating any header field other than `header_crc32c`
    /// itself — e.g. after the frame encoder writes `payload_len` and
    /// `payload_crc32c`.
    pub fn seal(&mut self) {
        self.header_crc32c = [0; 4];
        self.header_crc32c = compute_header_crc(self).to_be_bytes();
    }

    /// Validate per spec §03/03 §4.1: magic, version, reserved zeroness,
    /// payload-length bound, and header CRC.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.magic != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        if self.version != VERSION {
            return Err(ProtocolError::BadVersion {
                got: self.version,
                expected: VERSION,
            });
        }
        if self.reserved_a != 0 || self.reserved_b != [0u8; 8] {
            return Err(ProtocolError::ReservedFieldNonZero);
        }
        let len = self.payload_len_u32();
        if (len as usize) > MAX_PAYLOAD_BYTES {
            return Err(ProtocolError::OversizePayload {
                len,
                max: MAX_PAYLOAD_BYTES as u32,
            });
        }
        if u32::from_be_bytes(self.header_crc32c) != compute_header_crc(self) {
            return Err(ProtocolError::BadHeaderCrc);
        }
        Ok(())
    }

    /// Decoded 24-bit payload length.
    #[inline]
    #[must_use]
    pub fn payload_len_u32(&self) -> u32 {
        let b = self.payload_len;
        u32::from_be_bytes([0, b[0], b[1], b[2]])
    }

    /// Decoded stream id.
    #[inline]
    #[must_use]
    pub fn stream_id_u32(&self) -> u32 {
        u32::from_be_bytes(self.stream_id)
    }

    /// Decoded flags.
    #[inline]
    #[must_use]
    pub fn flags_u16(&self) -> u16 {
        u16::from_be_bytes(self.flags)
    }
}

/// CRC32C over the header excluding the `header_crc32c` field
/// (bytes 0–7 followed by bytes 12–31), per spec §03/03 §3.6.
fn compute_header_crc(h: &Header) -> u32 {
    let bytes: &[u8; 32] = bytemuck::cast_ref(h);
    let mut buf = [0u8; 28];
    buf[..8].copy_from_slice(&bytes[..8]);
    buf[8..].copy_from_slice(&bytes[12..32]);
    header_crc(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_has_correct_size() {
        assert_eq!(core::mem::size_of::<Header>(), 32);
    }

    #[test]
    fn header_has_correct_alignment() {
        assert_eq!(core::mem::align_of::<Header>(), 1);
    }

    #[test]
    fn magic_bytes_match() {
        assert_eq!(&MAGIC, b"BRN0");
    }

    #[test]
    fn new_then_validate_passes() {
        let h = Header::new(0x10, 0x0000, 0, 0);
        h.validate().expect("freshly built header validates");
    }

    #[test]
    fn payload_length_round_trips() {
        let h = Header::new(0x21, 0x0000, 7, 12_345);
        assert_eq!(h.payload_len_u32(), 12_345);
        assert_eq!(h.stream_id_u32(), 7);
        h.validate().unwrap();
    }

    #[test]
    fn payload_length_at_24bit_max_round_trips() {
        let max = MAX_PAYLOAD_BYTES as u32;
        let h = Header::new(0x01, 0, 1, max);
        assert_eq!(h.payload_len_u32(), max);
        h.validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_magic() {
        let mut h = Header::new(0x10, 0, 0, 0);
        h.magic = *b"XXXX";
        assert!(matches!(h.validate(), Err(ProtocolError::BadMagic)));
    }

    #[test]
    fn validate_rejects_bad_version() {
        let mut h = Header::new(0x10, 0, 0, 0);
        h.version = 99;
        // Recompute CRC so the version check fires before the CRC check.
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::BadVersion {
                got: 99,
                expected: 1
            })
        ));
    }

    #[test]
    fn validate_rejects_corrupted_crc() {
        let mut h = Header::new(0x10, 0, 0, 0);
        h.header_crc32c[0] ^= 0xFF;
        assert!(matches!(h.validate(), Err(ProtocolError::BadHeaderCrc)));
    }

    #[test]
    fn validate_rejects_nonzero_reserved_a() {
        let mut h = Header::new(0x10, 0, 0, 0);
        h.reserved_a = 1;
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::ReservedFieldNonZero)
        ));
    }

    #[test]
    fn validate_rejects_nonzero_reserved_b() {
        let mut h = Header::new(0x10, 0, 0, 0);
        h.reserved_b[3] = 0xAB;
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::ReservedFieldNonZero)
        ));
    }

    #[test]
    fn pod_roundtrip_via_byte_cast() {
        let h = Header::new(0xA1, 0x8000, 7, 64);
        let bytes: [u8; 32] = bytemuck::cast(h);
        let h2: Header = bytemuck::cast(bytes);
        h2.validate().unwrap();
        assert_eq!(h2.flags_u16(), 0x8000);
        assert_eq!(h2.stream_id_u32(), 7);
        assert_eq!(h2.payload_len_u32(), 64);
    }
}
