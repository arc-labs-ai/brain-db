//! Fixed 32-byte frame header.
//!
//! All multi-byte header fields are big-endian.
//!
//! Multi-byte fields are stored as raw big-endian byte arrays rather than
//! native integers so the struct is trivially `bytemuck::Pod` and matches
//! the on-wire layout byte-for-byte without endian conversion at cast
//! boundaries.
//!
//! ## Opcode + flags split
//!
//! The opcode field is a `u16` (bytes 5-6, big-endian). The high byte is a
//! **namespace**:
//! - `0x00xx` — cognitive primitives + connection mgmt + admin,
//! - `0x01xx` — typed-graph ops (schema / entities / statements / relations /
//!   queries / extractors), active once a schema is declared,
//! - `0x02xx`–`0xFFxx` — reserved for future namespaces.
//!
//! Within a namespace the low byte's high bit selects direction (request vs
//! response). The `0x2N → 0xAN` (encode→encode_resp) convention is
//! preserved as `0x002N → 0x00AN`; typed-graph follows the same convention
//! (e.g. `0x0130 ENTITY_CREATE` ↔ `0x01B0 ENTITY_CREATE_RESP`).
//!
//! Flags are a `u8` (byte 7) — three bits are defined (EOS / MPL / CMP) and
//! the low five are reserved and rejected by [`Header::validate`].

use bytemuck::{Pod, Zeroable};

use crate::codec::crc::header_crc;
use crate::error::ProtocolError;
use crate::{MAGIC, MAX_PAYLOAD_BYTES};

/// Wire protocol version.
///
/// Brain is pre-release (v0.1.0). The wire protocol is still in flux;
/// the VERSION byte will lock at `1` when v1.0 ships. Until then,
/// breaking wire changes are made in place without a version bump.
pub const VERSION: u8 = 1;

/// 32-byte frame header. Layout matches exactly.
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
    /// Bytes 5–6: big-endian `u16` opcode.
    pub opcode: [u8; 2],
    /// Byte 7: flags. Three bits are defined (EOS=0x80, MPL=0x40,
    /// CMP=0x20); the remaining five are reserved and MUST be zero.
    pub flags: u8,
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

/// Bits in [`Header::flags`] that MUST be zero.
///
/// Defined flag bits (after the u16→u8 shrink, see module doc):
/// - `0x80` — `EOS` (end of stream)
/// - `0x40` — `MPL` (multi-payload)
/// - `0x20` — `CMP` (compressed; reserved, not used in v1)
///
/// Bits `0x1F` (the low five) are reserved and rejected by [`Header::validate`].
pub const FLAGS_RESERVED_MASK: u8 = 0b0001_1111;

/// Bits in [`Header::flags`] that are allowed to be set (the complement of
/// [`FLAGS_RESERVED_MASK`]). Convenience constant for callers that want to
/// validate flags before constructing a header.
pub const FLAGS_VALID_MASK: u8 = !FLAGS_RESERVED_MASK;

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
    /// framing.
    pub fn new(opcode: u16, flags: u8, stream_id: u32, payload_len: u32) -> Self {
        assert!(
            (payload_len as usize) <= MAX_PAYLOAD_BYTES,
            "payload_len {payload_len} exceeds 24-bit max"
        );
        let len_be = payload_len.to_be_bytes();
        let mut h = Self {
            magic: MAGIC,
            version: VERSION,
            opcode: opcode.to_be_bytes(),
            flags,
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

    /// Validate: magic, version, reserved zeroness,
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
        if self.flags & FLAGS_RESERVED_MASK != 0 {
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

    /// Decoded opcode (u16 big-endian on the wire).
    #[inline]
    #[must_use]
    pub fn opcode_u16(&self) -> u16 {
        u16::from_be_bytes(self.opcode)
    }

    /// Decoded flags (u8 on the wire).
    #[inline]
    #[must_use]
    pub fn flags_u8(&self) -> u8 {
        self.flags
    }
}

/// CRC32C over the header excluding the `header_crc32c` field
/// (bytes 0–7 followed by bytes 12–31).
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
        let h = Header::new(0x0010, 0, 0, 0);
        h.validate().expect("freshly built header validates");
    }

    #[test]
    fn payload_length_round_trips() {
        let h = Header::new(0x0021, 0, 7, 12_345);
        assert_eq!(h.payload_len_u32(), 12_345);
        assert_eq!(h.stream_id_u32(), 7);
        assert_eq!(h.opcode_u16(), 0x0021);
        h.validate().unwrap();
    }

    #[test]
    fn payload_length_at_24bit_max_round_trips() {
        let max = MAX_PAYLOAD_BYTES as u32;
        let h = Header::new(0x0001, 0, 1, max);
        assert_eq!(h.payload_len_u32(), max);
        h.validate().unwrap();
    }

    #[test]
    fn opcode_u16_round_trips_both_namespaces() {
        // 0x00xx namespace: ENCODE_REQ.
        let h = Header::new(0x0020, 0, 1, 0);
        assert_eq!(h.opcode_u16(), 0x0020);
        // 0x01xx (typed-graph) namespace: ENTITY_CREATE.
        let h = Header::new(0x0130, 0, 1, 0);
        assert_eq!(h.opcode_u16(), 0x0130);
        h.validate().unwrap();
    }

    #[test]
    fn flags_byte_round_trips() {
        // EOS (0x80) is the only bit set in most final frames.
        let h = Header::new(0x00A1, 0x80, 7, 0);
        assert_eq!(h.flags_u8(), 0x80);
        h.validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_magic() {
        let mut h = Header::new(0x0010, 0, 0, 0);
        h.magic = *b"XXXX";
        assert!(matches!(h.validate(), Err(ProtocolError::BadMagic)));
    }

    #[test]
    fn validate_rejects_bad_version() {
        let mut h = Header::new(0x0010, 0, 0, 0);
        h.version = 99;
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
        let mut h = Header::new(0x0010, 0, 0, 0);
        h.header_crc32c[0] ^= 0xFF;
        assert!(matches!(h.validate(), Err(ProtocolError::BadHeaderCrc)));
    }

    #[test]
    fn validate_rejects_nonzero_reserved_a() {
        let mut h = Header::new(0x0010, 0, 0, 0);
        h.reserved_a = 1;
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::ReservedFieldNonZero)
        ));
    }

    #[test]
    fn validate_rejects_nonzero_reserved_b() {
        let mut h = Header::new(0x0010, 0, 0, 0);
        h.reserved_b[3] = 0xAB;
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::ReservedFieldNonZero)
        ));
    }

    #[test]
    fn validate_rejects_reserved_flag_bits() {
        // The low five bits (0x1F) are reserved in the u8 flags layout.
        let mut h = Header::new(0x0010, 0b0000_0001, 0, 0);
        h.header_crc32c = compute_header_crc(&h).to_be_bytes();
        assert!(matches!(
            h.validate(),
            Err(ProtocolError::ReservedFieldNonZero)
        ));
    }

    #[test]
    fn validate_accepts_defined_flag_bits() {
        // EOS (0x80), MPL (0x40), CMP (0x20) are valid; the all-defined
        // combination 0xE0 must validate.
        let h = Header::new(0x0010, 0xE0, 0, 0);
        h.validate().expect("defined flag combination accepted");
    }

    #[test]
    fn pod_roundtrip_via_byte_cast() {
        let h = Header::new(0x00A1, 0x80, 7, 64);
        let bytes: [u8; 32] = bytemuck::cast(h);
        let h2: Header = bytemuck::cast(bytes);
        h2.validate().unwrap();
        assert_eq!(h2.flags_u8(), 0x80);
        assert_eq!(h2.stream_id_u32(), 7);
        assert_eq!(h2.payload_len_u32(), 64);
        assert_eq!(h2.opcode_u16(), 0x00A1);
    }

    #[test]
    fn on_wire_byte_layout_matches_spec() {
        // bytes 5-6 = opcode (BE u16), byte 7 = flags (u8).
        let h = Header::new(0x0130, 0x80, 0, 0);
        let bytes: [u8; 32] = bytemuck::cast(h);
        assert_eq!(bytes[4], VERSION, "version");
        assert_eq!(bytes[5], 0x01, "opcode high byte (typed-graph namespace)");
        assert_eq!(bytes[6], 0x30, "opcode low byte (ENTITY_CREATE)");
        assert_eq!(bytes[7], 0x80, "flags = EOS");
    }
}
