//! Frame envelope: the 32-byte [`Header`] together with its raw payload bytes.
//!
//! This module gives the wire-protocol codec for a single frame. It does NOT
//! interpret the payload — that's the job of higher layers (rkyv decoders for
//! structured data, `bytemuck` views over raw vector bytes per spec §03/04).
//! Here, the payload is simply `Vec<u8>`.
//!
//! [`Frame::encode`] is the canonical sealing point: it recomputes
//! `payload_len`, `payload_crc32c`, and `header_crc32c` so callers don't have
//! to keep them in sync manually. [`Frame::decode`] parses one frame off the
//! front of a byte stream and returns the unconsumed tail so callers can run
//! it in a loop.

use crate::crc::payload_crc;
use crate::error::ProtocolError;
use crate::header::Header;
use crate::{HEADER_SIZE, MAX_PAYLOAD_BYTES};

/// One wire frame: a header plus its payload bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Build a `Frame` for the given opcode/flags/stream/payload, with all
    /// length and CRC fields filled in. The returned frame is wire-ready;
    /// `encode` will produce a byte sequence that re-validates.
    ///
    /// # Panics
    ///
    /// Panics if `payload.len()` exceeds [`MAX_PAYLOAD_BYTES`]. Callers must
    /// split into multi-payload frames (spec §03/03 §6) before that point.
    #[must_use]
    pub fn new(opcode: u8, flags: u16, stream_id: u32, payload: Vec<u8>) -> Self {
        assert!(
            payload.len() <= MAX_PAYLOAD_BYTES,
            "payload length {} exceeds 24-bit max",
            payload.len()
        );
        #[allow(clippy::cast_possible_truncation)]
        let mut header = Header::new(opcode, flags, stream_id, payload.len() as u32);
        header.payload_crc32c = payload_crc(&payload).to_be_bytes();
        header.seal();
        Self { header, payload }
    }

    /// Serialize the frame to a wire-ready byte vector.
    ///
    /// `payload_len`, `payload_crc32c`, and `header_crc32c` are recomputed
    /// from `self.payload` — callers may freely mutate `self.header.opcode`,
    /// `flags`, or `stream_id` without resealing first.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut h = self.header;
        #[allow(clippy::cast_possible_truncation)]
        let len_be = (self.payload.len() as u32).to_be_bytes();
        h.payload_len = [len_be[1], len_be[2], len_be[3]];
        h.payload_crc32c = payload_crc(&self.payload).to_be_bytes();
        h.seal();

        let mut out = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        out.extend_from_slice(bytemuck::bytes_of(&h));
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a single frame off the front of `bytes`, returning the frame
    /// and the unconsumed tail.
    ///
    /// Equivalent to [`Frame::decode_with_max`] with the spec's hard 24-bit
    /// payload bound. Higher layers that have negotiated a smaller maximum
    /// should call [`Frame::decode_with_max`] directly.
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        #[allow(clippy::cast_possible_truncation)]
        Self::decode_with_max(bytes, MAX_PAYLOAD_BYTES as u32)
    }

    /// Decode one frame, rejecting payloads larger than `max_payload_bytes`.
    ///
    /// `max_payload_bytes` is checked *before* the payload is read, so an
    /// attacker can't force a large allocation by claiming a long payload.
    pub fn decode_with_max(
        bytes: &[u8],
        max_payload_bytes: u32,
    ) -> Result<(Self, &[u8]), ProtocolError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ProtocolError::Truncated {
                have: bytes.len(),
                need: HEADER_SIZE,
            });
        }
        let header_bytes: [u8; HEADER_SIZE] = bytes[..HEADER_SIZE]
            .try_into()
            .expect("invariant: slice is exactly HEADER_SIZE bytes after the length check above");
        let header: Header = bytemuck::cast(header_bytes);
        header.validate()?;

        let payload_len = header.payload_len_u32();
        if payload_len > max_payload_bytes {
            return Err(ProtocolError::OversizePayload {
                len: payload_len,
                max: max_payload_bytes,
            });
        }
        let need = HEADER_SIZE + payload_len as usize;
        if bytes.len() < need {
            return Err(ProtocolError::Truncated {
                have: bytes.len(),
                need,
            });
        }
        let payload_slice = &bytes[HEADER_SIZE..need];

        let stored = u32::from_be_bytes(header.payload_crc32c);
        let actual = payload_crc(payload_slice);
        if stored != actual {
            return Err(ProtocolError::BadPayloadCrc);
        }

        let frame = Self {
            header,
            payload: payload_slice.to_vec(),
        };
        Ok((frame, &bytes[need..]))
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::MAGIC;

    fn sample_frame() -> Frame {
        Frame::new(0x21, 0x0000, 7, b"hello brain".to_vec())
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let original = sample_frame();
        let bytes = original.encode();
        assert_eq!(bytes.len(), HEADER_SIZE + original.payload.len());

        let (decoded, rest) = Frame::decode(&bytes).expect("decode round-trip");
        assert!(rest.is_empty(), "decoder consumed exactly one frame");
        assert_eq!(decoded.payload, original.payload);
        assert_eq!(decoded.header.opcode, original.header.opcode);
        assert_eq!(decoded.header.stream_id_u32(), 7);
        assert_eq!(
            decoded.header.payload_len_u32() as usize,
            original.payload.len()
        );
    }

    #[test]
    fn encode_then_decode_empty_payload() {
        let frame = Frame::new(0x10, 0x0000, 0, Vec::new());
        let bytes = frame.encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        let (decoded, rest) = Frame::decode(&bytes).expect("decode empty");
        assert!(rest.is_empty());
        assert!(decoded.payload.is_empty());
        // Spec §03/03 §3.7: payload_crc32c MUST be zero when payload_len is zero.
        assert_eq!(decoded.header.payload_crc32c, [0; 4]);
    }

    #[test]
    fn decode_returns_unconsumed_tail() {
        let mut bytes = sample_frame().encode();
        bytes.extend_from_slice(b"AFTERBYTES");
        let (_frame, rest) = Frame::decode(&bytes).expect("decode with trailing bytes");
        assert_eq!(rest, b"AFTERBYTES");
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = sample_frame().encode();
        bytes[0] = b'X';
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadMagic)
        ));
        // Sanity: the unmodified bytes started with MAGIC.
        assert_ne!(MAGIC[0], b'X');
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut bytes = sample_frame().encode();
        bytes[4] = 99;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadVersion {
                got: 99,
                expected: 1
            })
        ));
    }

    #[test]
    fn decode_rejects_bad_header_crc() {
        let mut bytes = sample_frame().encode();
        // Flip a bit inside the header CRC field (offsets 8..12).
        bytes[8] ^= 0xFF;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadHeaderCrc)
        ));
    }

    #[test]
    fn decode_rejects_bad_payload_crc() {
        let mut bytes = sample_frame().encode();
        // Flip a payload byte (after the 32-byte header) without touching
        // any header field; the header CRC remains valid.
        bytes[HEADER_SIZE] ^= 0xFF;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(ProtocolError::BadPayloadCrc)
        ));
    }

    #[test]
    fn decode_rejects_truncated_header() {
        let bytes = sample_frame().encode();
        let truncated = &bytes[..10];
        assert!(matches!(
            Frame::decode(truncated),
            Err(ProtocolError::Truncated {
                have: 10,
                need: HEADER_SIZE
            })
        ));
    }

    #[test]
    fn decode_rejects_truncated_payload() {
        let bytes = sample_frame().encode();
        // Drop the last 5 bytes of the payload — header passes, payload
        // bound check fails.
        let truncated = &bytes[..bytes.len() - 5];
        let result = Frame::decode(truncated);
        assert!(matches!(result, Err(ProtocolError::Truncated { .. })));
    }

    #[test]
    fn decode_rejects_oversize_payload() {
        // A 100-byte payload is fine for the spec max but exceeds a smaller
        // negotiated limit of 50 bytes — decode_with_max rejects it before
        // touching the buffer.
        let big = Frame::new(0x21, 0, 1, vec![0xAB; 100]);
        let bytes = big.encode();
        let result = Frame::decode_with_max(&bytes, 50);
        assert!(matches!(
            result,
            Err(ProtocolError::OversizePayload { len: 100, max: 50 })
        ));
    }

    #[test]
    fn encode_seals_against_caller_drift() {
        // Build a frame, then mutate the payload after construction. The
        // CRC fields stored in `frame.header` are now stale, but `encode`
        // recomputes them — the resulting bytes round-trip cleanly.
        let mut frame = sample_frame();
        frame.payload.extend_from_slice(b"!!! more text");
        let bytes = frame.encode();
        let (decoded, _) = Frame::decode(&bytes).expect("encode reseals stale CRCs");
        assert_eq!(decoded.payload, frame.payload);
    }

    proptest! {
        /// Round-trip property: any well-formed `Frame::new(...).encode()`
        /// must decode back to a frame with the same opcode, flags, stream
        /// id, and payload. Payloads bounded at 8 KiB to keep test runtime
        /// reasonable (spec hard cap is 16 MiB - 1).
        #[test]
        fn encode_decode_round_trip(
            opcode in any::<u8>(),
            flags in any::<u16>(),
            stream_id in any::<u32>(),
            payload in vec(any::<u8>(), 0..=8192usize),
        ) {
            let original = Frame::new(opcode, flags, stream_id, payload.clone());
            let bytes = original.encode();
            let (decoded, rest) = Frame::decode(&bytes)
                .expect("Frame::new+encode must always produce a decodable frame");
            prop_assert!(rest.is_empty());
            prop_assert_eq!(decoded.header.opcode, opcode);
            prop_assert_eq!(decoded.header.flags_u16(), flags);
            prop_assert_eq!(decoded.header.stream_id_u32(), stream_id);
            prop_assert_eq!(decoded.header.payload_len_u32() as usize, payload.len());
            prop_assert_eq!(decoded.payload, payload);
        }

        /// Robustness property: `Frame::decode` MUST NOT panic on arbitrary
        /// input. It either returns a structured `ProtocolError`, or
        /// succeeds — and if it succeeds, the consumed prefix must
        /// re-encode to itself (the decoder accepted only canonical bytes).
        ///
        /// Spec §03/11 §1: validation is layered and deterministic — so an
        /// adversary feeding garbage cannot cause UB or panics.
        #[test]
        fn decode_arbitrary_bytes_is_total(
            bytes in vec(any::<u8>(), 0..=8192usize),
        ) {
            match Frame::decode(&bytes) {
                Ok((frame, rest)) => {
                    let consumed = bytes.len() - rest.len();
                    let reencoded = frame.encode();
                    prop_assert_eq!(reencoded.as_slice(), &bytes[..consumed]);
                }
                Err(_) => {
                    // Any structured error variant is acceptable; the
                    // contract is just "no panic, return a Result".
                }
            }
        }
    }
}
