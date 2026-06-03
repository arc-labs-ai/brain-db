//! WAL record framing.
//!
//! On-disk layout:
//!
//! ```text
//! header (32 bytes, all little-endian):
//!    0..8   lsn               u64
//!    8      record_type       u8
//!    9      flags             u8
//!   10..12  reserved          [u8; 2]   (zero)
//!   12..16  payload_length    u32
//!   16..24  timestamp_ns      u64
//!   24..32  agent_id_lo64     u64
//!
//! payload (variable, exactly payload_length bytes)
//!
//! footer (8 bytes):
//!    0..4   payload_crc32c    u32       (CRC32C over header + payload)
//!    4..8   reserved          [u8; 4]   (zero)
//!
//! total = 32 + payload_length + 8
//! ```
//!
//! The decoder distinguishes truncation (ran out of bytes mid-record, normal
//! at the tail of a WAL segment that crashed mid-write) from corruption (CRC
//! mismatch on a fully-present record). Recovery may collapse the two, but
//! at this layer we surface them separately so the reader can decide.

use core::fmt;

use crate::wal::kinds::WalRecordKind;

/// Log Sequence Number. Strictly increasing; `0` is reserved for "no LSN".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

impl Lsn {
    pub const ZERO: Lsn = Lsn(0);

    /// Returns the next LSN. Saturates at `u64::MAX` (which would take ~584
    /// years at 1 GHz; treat saturation as fatal).
    #[must_use]
    pub const fn next(self) -> Lsn {
        Lsn(self.0.saturating_add(1))
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Rendered as a decimal LSN — readable in logs, sortable as a string
        // up to 19 digits (u64::MAX). Pad to 20 so lexicographic = numeric.
        write!(f, "{:020}", self.0)
    }
}

/// Header size in bytes.
pub const HEADER_LEN: usize = 32;

/// Footer size in bytes.
pub const FOOTER_LEN: usize = 8;

/// Maximum payload size (default 16 MiB).
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// `flags` bit marking a record as a **subscribe-replay change-feed
/// event**, not a state mutation.
///
/// Typed-graph ops are WAL-logged twice: once as the durable write
/// record (rkyv-encoded row / first-class payload, the source of truth
/// recovery replays into the metadata store) and once as a CBOR-encoded
/// `GraphEventPayload` so subscribe-replay can reconstruct the change
/// feed after a restart. Both records carry the same `WalRecordKind`
/// (e.g. `EntityCreate`), so the kind byte alone can't tell them apart.
///
/// The event record sets this flag. Recovery skips flagged records (the
/// durable record already carries the state; replaying the CBOR body as
/// a row would fail decode and, if it didn't, would duplicate the row).
/// Subscribe-replay does the inverse — it projects flagged records into
/// change-feed events and ignores the unflagged durable records.
pub const FLAG_SUBSCRIBE_EVENT: u8 = 0x01;

/// In-memory representation of one WAL record.
///
/// Holds every header field so encode/decode is a lossless round-trip.
/// `payload` is the raw bytes; per-kind interpretation is layered above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub lsn: Lsn,
    pub kind: WalRecordKind,
    pub flags: u8,
    pub timestamp_ns: u64,
    pub agent_id_lo64: u64,
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// Total encoded size in bytes.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len() + FOOTER_LEN
    }

    /// Append the encoded record to `out`.
    ///
    /// # Panics
    /// Panics if `payload.len()` exceeds [`MAX_PAYLOAD`]. Caller-side
    /// validation (request layer) prevents this; the panic is a fail-stop on
    /// an internal invariant violation, consistent with invariant #7
    /// (no silent corruption).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let payload_len_u32 =
            u32::try_from(self.payload.len()).expect("invariant: payload length fits in u32");
        assert!(
            payload_len_u32 <= MAX_PAYLOAD,
            "invariant: payload {} exceeds MAX_PAYLOAD {}",
            payload_len_u32,
            MAX_PAYLOAD,
        );

        let start = out.len();
        out.reserve(self.encoded_len());

        // Header.
        out.extend_from_slice(&self.lsn.0.to_le_bytes());
        out.push(self.kind.as_u8());
        out.push(self.flags);
        out.extend_from_slice(&[0u8, 0u8]); // reserved
        out.extend_from_slice(&payload_len_u32.to_le_bytes());
        out.extend_from_slice(&self.timestamp_ns.to_le_bytes());
        out.extend_from_slice(&self.agent_id_lo64.to_le_bytes());

        // Payload.
        out.extend_from_slice(&self.payload);

        // CRC over header + payload (the slice we just appended).
        let crc = crc32c::crc32c(&out[start..start + HEADER_LEN + self.payload.len()]);

        // Footer.
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // reserved
    }

    /// Convenience: encode to a fresh `Vec<u8>`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut buf);
        buf
    }

    /// Try to decode one record from the start of `buf`.
    ///
    /// - `Ok(DecodeOutcome::Record { record, consumed })`: success; `consumed`
    ///   bytes from the start of `buf` belong to this record.
    /// - `Ok(DecodeOutcome::Truncated)`: `buf` does not contain a complete
    ///   record. Normal at the tail of a WAL segment that crashed mid-write.
    /// - `Err(_)`: the prefix *was* fully present and well-formed structurally,
    ///   but failed validation (CRC, unknown kind, non-zero reserved).
    pub fn decode_one(buf: &[u8]) -> Result<DecodeOutcome, WalRecordError> {
        if buf.len() < HEADER_LEN {
            return Ok(DecodeOutcome::Truncated);
        }

        // Parse header.
        let lsn = Lsn(read_u64_le(&buf[0..8]));
        let record_type = buf[8];
        let flags = buf[9];
        let reserved_hi = &buf[10..12];
        let payload_length = read_u32_le(&buf[12..16]);
        let timestamp_ns = read_u64_le(&buf[16..24]);
        let agent_id_lo64 = read_u64_le(&buf[24..32]);

        if reserved_hi != [0, 0] {
            return Err(WalRecordError::NonZeroReserved);
        }

        if payload_length > MAX_PAYLOAD {
            return Err(WalRecordError::PayloadTooLarge(payload_length));
        }

        let payload_len = payload_length as usize;
        let total = HEADER_LEN + payload_len + FOOTER_LEN;
        if buf.len() < total {
            return Ok(DecodeOutcome::Truncated);
        }

        // Validate kind only after we know we have the full record. This way
        // a truncated header can't masquerade as a kind error.
        let kind = WalRecordKind::from_u8(record_type)
            .ok_or(WalRecordError::UnknownRecordType(record_type))?;

        // CRC covers header + payload, not footer.
        let expected = read_u32_le(&buf[HEADER_LEN + payload_len..HEADER_LEN + payload_len + 4]);
        let footer_reserved = &buf[HEADER_LEN + payload_len + 4..total];
        if footer_reserved != [0, 0, 0, 0] {
            return Err(WalRecordError::NonZeroReserved);
        }
        let actual = crc32c::crc32c(&buf[..HEADER_LEN + payload_len]);
        if expected != actual {
            return Err(WalRecordError::CrcMismatch { expected, actual });
        }

        let payload = buf[HEADER_LEN..HEADER_LEN + payload_len].to_vec();

        Ok(DecodeOutcome::Record {
            record: WalRecord {
                lsn,
                kind,
                flags,
                timestamp_ns,
                agent_id_lo64,
                payload,
            },
            consumed: total,
        })
    }

    /// Build a `WalRecord` from a typed payload.
    ///
    /// Convenience for writers: take a `WalPayload`, encode its bytes, set
    /// the matching `kind` discriminator. The framing layer handles CRC.
    #[must_use]
    pub fn from_typed(
        lsn: Lsn,
        flags: u8,
        timestamp_ns: u64,
        agent_id_lo64: u64,
        payload: &crate::wal::payload::WalPayload,
    ) -> Self {
        Self {
            lsn,
            kind: payload.kind(),
            flags,
            timestamp_ns,
            agent_id_lo64,
            payload: payload.encode_to_bytes(),
        }
    }

    /// Decode the typed payload from this record's bytes.
    pub fn typed_payload(
        &self,
    ) -> Result<crate::wal::payload::WalPayload, crate::wal::payload::WalPayloadError> {
        crate::wal::payload::WalPayload::decode(self.kind, &self.payload)
    }
}

/// Result of [`WalRecord::decode_one`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeOutcome {
    /// One record decoded; `consumed` bytes belong to it.
    Record { record: WalRecord, consumed: usize },
    /// Buffer is shorter than the next record claims. Tail of a crashed
    /// segment — recovery treats this as the new end-of-log.
    Truncated,
}

/// Validation failures that are *not* truncation.
///
/// Recovery may collapse these into "truncate here". We
/// keep them distinct at this layer so callers (`WalReader`, recovery, audit
/// tooling) can decide policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WalRecordError {
    #[error("CRC mismatch: expected {expected:#010x}, actual {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },

    #[error("unknown record_type byte: {0}")]
    UnknownRecordType(u8),

    #[error("reserved bytes were non-zero")]
    NonZeroReserved,

    #[error("payload length {0} exceeds MAX_PAYLOAD")]
    PayloadTooLarge(u32),
}

#[inline]
fn read_u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes(
        b.try_into()
            .expect("invariant: caller sized slice as 4 bytes"),
    )
}

#[inline]
fn read_u64_le(b: &[u8]) -> u64 {
    u64::from_le_bytes(
        b.try_into()
            .expect("invariant: caller sized slice as 8 bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::kinds::ALL_KINDS;

    fn sample(kind: WalRecordKind, payload: Vec<u8>) -> WalRecord {
        WalRecord {
            lsn: Lsn(42),
            kind,
            flags: 0b0000_0011,
            timestamp_ns: 1_700_000_000_000_000_000,
            agent_id_lo64: 0xDEAD_BEEF_CAFE_F00D,
            payload,
        }
    }

    #[test]
    fn lsn_next_and_ord() {
        assert_eq!(Lsn(0).next(), Lsn(1));
        assert!(Lsn(1) < Lsn(2));
        assert_eq!(Lsn(u64::MAX).next(), Lsn(u64::MAX)); // saturates
    }

    #[test]
    fn lsn_display_pads_to_20_digits() {
        assert_eq!(format!("{}", Lsn(7)), "00000000000000000007");
        assert_eq!(format!("{}", Lsn(u64::MAX)).len(), 20);
    }

    #[test]
    fn round_trip_every_kind() {
        for (i, &kind) in ALL_KINDS.iter().enumerate() {
            // Pick a varying-size payload per kind so we exercise different
            // lengths.
            let payload: Vec<u8> = (0..(8 + i * 3) as u8).collect();
            let rec = sample(kind, payload);
            let buf = rec.encode();
            assert_eq!(buf.len(), rec.encoded_len());

            match WalRecord::decode_one(&buf).unwrap() {
                DecodeOutcome::Record { record, consumed } => {
                    assert_eq!(record, rec, "round-trip mismatch for {kind:?}");
                    assert_eq!(consumed, buf.len());
                }
                DecodeOutcome::Truncated => panic!("decode said Truncated on full buffer"),
            }
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        let rec = sample(WalRecordKind::CheckpointEnd, vec![]);
        let buf = rec.encode();
        assert_eq!(buf.len(), HEADER_LEN + FOOTER_LEN);
        let DecodeOutcome::Record { record, consumed } = WalRecord::decode_one(&buf).unwrap()
        else {
            panic!("expected Record");
        };
        assert_eq!(record, rec);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn every_short_prefix_is_truncated_not_error() {
        let rec = sample(
            WalRecordKind::Encode,
            (0..200u16).map(|x| x as u8).collect(),
        );
        let buf = rec.encode();
        // Every prefix shorter than the full record must report Truncated,
        // never an error. This is the property recovery relies on.
        for n in 0..buf.len() {
            match WalRecord::decode_one(&buf[..n]) {
                Ok(DecodeOutcome::Truncated) => {}
                Ok(DecodeOutcome::Record { .. }) => {
                    panic!(
                        "decoded a record from {n}-byte prefix of {}-byte record",
                        buf.len()
                    )
                }
                Err(e) => panic!("prefix of length {n} returned error {e:?} (must be Truncated)"),
            }
        }
        // Full buffer decodes.
        assert!(matches!(
            WalRecord::decode_one(&buf).unwrap(),
            DecodeOutcome::Record { .. }
        ));
    }

    #[test]
    fn crc_mismatch_on_payload_corruption() {
        let rec = sample(WalRecordKind::Link, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let mut buf = rec.encode();
        // Flip a payload byte (offset HEADER_LEN..HEADER_LEN+8 is payload).
        buf[HEADER_LEN + 3] ^= 0x01;
        match WalRecord::decode_one(&buf) {
            Err(WalRecordError::CrcMismatch { .. }) => {}
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn crc_mismatch_on_header_corruption() {
        let rec = sample(WalRecordKind::Forget, vec![9; 16]);
        let mut buf = rec.encode();
        // Flip a bit in the timestamp field (offset 16..24).
        buf[20] ^= 0x10;
        assert!(matches!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn unknown_record_type_rejected() {
        let rec = sample(WalRecordKind::Encode, vec![]);
        let mut buf = rec.encode();
        buf[8] = 0; // 0 is reserved
                    // Recompute CRC so we hit UnknownRecordType, not CrcMismatch.
        let crc = crc32c::crc32c(&buf[..HEADER_LEN]);
        let crc_off = HEADER_LEN; // payload is empty
        buf[crc_off..crc_off + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::UnknownRecordType(0))
        );

        // A reserved-future byte (e.g. 0x60 — past the audit kind, still
        // inside the v1 reserved range) likewise rejected.
        buf[8] = 0x60;
        let crc = crc32c::crc32c(&buf[..HEADER_LEN]);
        buf[crc_off..crc_off + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::UnknownRecordType(0x60))
        );
    }

    #[test]
    fn non_zero_header_reserved_rejected() {
        let rec = sample(WalRecordKind::Reclaim, vec![]);
        let mut buf = rec.encode();
        buf[10] = 0xFF; // header reserved byte
                        // Recompute CRC so we isolate NonZeroReserved.
        let crc = crc32c::crc32c(&buf[..HEADER_LEN]);
        let crc_off = HEADER_LEN;
        buf[crc_off..crc_off + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::NonZeroReserved)
        );
    }

    #[test]
    fn non_zero_footer_reserved_rejected() {
        let rec = sample(WalRecordKind::TxnCommit, vec![0xAB; 4]);
        let mut buf = rec.encode();
        // Footer reserved is the last 4 bytes.
        let last = buf.len() - 1;
        buf[last] = 0xFF;
        assert_eq!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::NonZeroReserved)
        );
    }

    #[test]
    fn payload_too_large_rejected_before_buffer_check() {
        // Hand-craft a header that claims 17 MiB so we hit PayloadTooLarge
        // before the truncation check (which would otherwise mask it).
        let mut buf = vec![0u8; HEADER_LEN];
        buf[8] = WalRecordKind::Encode.as_u8();
        let huge = MAX_PAYLOAD + 1;
        buf[12..16].copy_from_slice(&huge.to_le_bytes());
        assert_eq!(
            WalRecord::decode_one(&buf),
            Err(WalRecordError::PayloadTooLarge(huge))
        );
    }

    #[test]
    fn back_to_back_records_decode_in_sequence() {
        let r1 = sample(WalRecordKind::Encode, vec![1; 10]);
        let r2 = sample(WalRecordKind::Forget, vec![2; 30]);
        let r3 = sample(WalRecordKind::CheckpointEnd, vec![]);
        let mut buf = Vec::new();
        r1.encode_into(&mut buf);
        r2.encode_into(&mut buf);
        r3.encode_into(&mut buf);

        let mut cursor = 0;
        for expected in [&r1, &r2, &r3] {
            let DecodeOutcome::Record { record, consumed } =
                WalRecord::decode_one(&buf[cursor..]).unwrap()
            else {
                panic!("expected Record");
            };
            assert_eq!(&record, expected);
            cursor += consumed;
        }
        assert_eq!(cursor, buf.len());
        // After the last record, an empty tail is Truncated (== EOF).
        assert_eq!(
            WalRecord::decode_one(&buf[cursor..]).unwrap(),
            DecodeOutcome::Truncated
        );
    }

    #[test]
    fn header_size_constant_matches_spec() {
        assert_eq!(HEADER_LEN, 32);
        assert_eq!(FOOTER_LEN, 8);
    }

    #[test]
    fn typed_payload_round_trips_through_framing() {
        // End-to-end: WalPayload → WalRecord::from_typed → encode → decode
        // → WalRecord::typed_payload → original. This is the bridge writers
        // and readers will actually use; the per-layer tests above prove
        // each piece, this proves they compose.
        use crate::wal::payload::{ReclaimPayload, WalPayload};

        let payload = WalPayload::Reclaim(ReclaimPayload {
            slot_id: 0xCAFE,
            old_version: 7,
            new_version: 8,
            memory_id: brain_core::MemoryId::pack(1, 0xCAFE, 7),
        });
        let record = WalRecord::from_typed(Lsn(1), 0, 1234, 0xAA, &payload);
        assert_eq!(record.kind, WalRecordKind::Reclaim);

        let buf = record.encode();
        let DecodeOutcome::Record { record: back, .. } = WalRecord::decode_one(&buf).unwrap()
        else {
            panic!("expected Record");
        };
        assert_eq!(back, record);
        assert_eq!(back.typed_payload().unwrap(), payload);
    }

    #[test]
    fn graph_payload_round_trips_through_framing() {
        // Sub-task 15.2: a opaque-body record carries an opaque body
        // through the full framing layer (header + payload + footer +
        // CRC). The framing is byte-identical to substrate records; only
        // the `record_type` byte and body interpretation differ.
        use crate::wal::payload::{PhaseBodyRecord, WalPayload};

        // One example per discriminant boundary in the typed-graph block.
        // RelationCreate / RelationSupersede / RelationTombstone are
        // first-class typed payloads, so they are no longer
        // valid kinds for an opaque-body PhaseBodyRecord.
        for kind in [
            WalRecordKind::EntityCreate,
            WalRecordKind::StatementCreate,
            WalRecordKind::SchemaUpdate,
            WalRecordKind::Audit,
        ] {
            let body: Vec<u8> = (0..48u8).map(|i| i.wrapping_mul(kind.as_u8())).collect();
            let payload = WalPayload::PhaseBody(PhaseBodyRecord::new(
                kind,
                brain_core::AgentId::default(),
                body.clone(),
            ));
            let record = WalRecord::from_typed(Lsn(7), 0, 9999, 0xBB, &payload);
            assert_eq!(record.kind, kind);

            let buf = record.encode();
            let DecodeOutcome::Record { record: back, .. } = WalRecord::decode_one(&buf).unwrap()
            else {
                panic!("expected Record for {kind:?}");
            };
            assert_eq!(back, record);
            match back.typed_payload().unwrap() {
                WalPayload::PhaseBody(r) => {
                    assert_eq!(r.kind, kind);
                    assert_eq!(r.body, body);
                }
                other => panic!("expected PhaseBody, got {other:?}"),
            }
        }
    }
}
