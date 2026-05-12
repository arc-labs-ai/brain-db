//! Idempotency helpers — request hashing + response payload codec.
//!
//! Spec §07/06 §5: a duplicate `request_id` with a different
//! `request_hash` returns `Conflict`. The hash covers every
//! canonical field of the request **except** the `request_id` itself
//! (that's the table key). Fields are joined with NUL separators to
//! prevent canonicalisation ambiguity.
//!
//! The response payload is a small hand-rolled compact binary format
//! with a leading version byte. We avoid rkyv on `EncodeAck` /
//! `ForgetAck` because those types live in brain-planner and don't
//! derive rkyv; the hand-rolled codec is ~50 lines and lets us evolve
//! the format independently (the version byte gates future schema
//! changes).

use brain_core::MemoryId;
use brain_planner::{EdgeOutcome, EncodeOp, ForgetOp, ForgetOutcome, LinkOp, UnlinkOp};
use brain_protocol::request::ForgetMode;

/// Spec §07/06 §2 `response_kind` discriminants.
pub(crate) const RESPONSE_KIND_ENCODE: u8 = 1;
pub(crate) const RESPONSE_KIND_FORGET: u8 = 2;
pub(crate) const RESPONSE_KIND_LINK: u8 = 3;
pub(crate) const RESPONSE_KIND_UNLINK: u8 = 4;

const PAYLOAD_VERSION_V1: u8 = 1;

const EDGE_OUTCOME_INSERTED: u8 = 0;
const EDGE_OUTCOME_TARGET_MISSING: u8 = 1;

const FORGET_OUTCOME_TOMBSTONED: u8 = 0;
const FORGET_OUTCOME_ALREADY_TOMBSTONED: u8 = 1;
const FORGET_OUTCOME_NOT_FOUND: u8 = 2;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum DecodeError {
    #[error("malformed idempotency payload: {0}")]
    Malformed(&'static str),
}

// ---------------------------------------------------------------------------
// Request hashing.
// ---------------------------------------------------------------------------

/// BLAKE3 over the canonical encode-request fields. Excludes
/// `request_id` (the table key) — see spec §07/06 §5.
#[must_use]
pub fn hash_encode_request(op: &EncodeOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"encode:");
    h.update(op.text.as_bytes());
    h.update(b"\0");
    h.update(&op.context_id.raw().to_le_bytes());
    h.update(b"\0");
    h.update(&[memory_kind_byte(op.kind)]);
    h.update(b"\0");
    h.update(&op.salience_initial.to_le_bytes());
    h.update(b"\0");
    h.update(&op.fingerprint);
    h.update(b"\0");
    h.update(&(op.edges.len() as u32).to_le_bytes());
    for edge in &op.edges {
        h.update(&edge.target.to_be_bytes());
        h.update(&[edge_kind_byte(edge.kind)]);
        h.update(&edge.weight.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_forget_request(op: &ForgetOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"forget:");
    h.update(&op.memory_id.to_be_bytes());
    h.update(b"\0");
    h.update(&[forget_mode_byte(op.mode)]);
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_link_request(op: &LinkOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"link:");
    h.update(&op.source.to_be_bytes());
    h.update(b"\0");
    h.update(&op.target.to_be_bytes());
    h.update(b"\0");
    h.update(&[edge_kind_byte(op.kind)]);
    h.update(b"\0");
    h.update(&op.weight.to_le_bytes());
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_unlink_request(op: &UnlinkOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"unlink:");
    h.update(&op.source.to_be_bytes());
    h.update(b"\0");
    h.update(&op.target.to_be_bytes());
    h.update(b"\0");
    h.update(&[edge_kind_byte(op.kind)]);
    *h.finalize().as_bytes()
}

fn memory_kind_byte(k: brain_core::MemoryKind) -> u8 {
    match k {
        brain_core::MemoryKind::Episodic => 0,
        brain_core::MemoryKind::Semantic => 1,
        brain_core::MemoryKind::Consolidated => 2,
    }
}

fn edge_kind_byte(k: brain_core::EdgeKind) -> u8 {
    match k {
        brain_core::EdgeKind::Caused => 0,
        brain_core::EdgeKind::FollowedBy => 1,
        brain_core::EdgeKind::DerivedFrom => 2,
        brain_core::EdgeKind::SimilarTo => 3,
        brain_core::EdgeKind::Contradicts => 4,
        brain_core::EdgeKind::Supports => 5,
        brain_core::EdgeKind::References => 6,
        brain_core::EdgeKind::PartOf => 7,
    }
}

fn forget_mode_byte(m: ForgetMode) -> u8 {
    match m {
        ForgetMode::Soft => 0,
        ForgetMode::Hard => 1,
    }
}

// ---------------------------------------------------------------------------
// Payload encoding/decoding.
// ---------------------------------------------------------------------------

/// Encode an ENCODE outcome into bytes for `IdempotencyEntry::response_payload`.
///
/// Layout:
/// - bytes 0..16 — `memory_id` (big-endian)
/// - byte 16     — version (= 1)
/// - bytes 17..19 — `edge_count` (u16, little-endian)
/// - bytes 19..  — `edge_count × u8` outcome discriminants
#[must_use]
pub fn encode_encode_payload(memory_id: MemoryId, outcomes: &[EdgeOutcome]) -> Vec<u8> {
    let edge_count = u16::try_from(outcomes.len()).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(19 + outcomes.len());
    out.extend_from_slice(&memory_id.to_be_bytes());
    out.push(PAYLOAD_VERSION_V1);
    out.extend_from_slice(&edge_count.to_le_bytes());
    for o in outcomes {
        out.push(match o {
            EdgeOutcome::Inserted => EDGE_OUTCOME_INSERTED,
            EdgeOutcome::TargetMissing => EDGE_OUTCOME_TARGET_MISSING,
        });
    }
    out
}

pub fn decode_encode_payload(bytes: &[u8]) -> Result<(MemoryId, Vec<EdgeOutcome>), DecodeError> {
    if bytes.len() < 19 {
        return Err(DecodeError::Malformed("encode payload too short"));
    }
    let mid_bytes: [u8; 16] = bytes[..16]
        .try_into()
        .map_err(|_| DecodeError::Malformed("encode memory_id slice"))?;
    let version = bytes[16];
    if version != PAYLOAD_VERSION_V1 {
        return Err(DecodeError::Malformed("encode payload unknown version"));
    }
    let edge_count = u16::from_le_bytes(
        bytes[17..19]
            .try_into()
            .map_err(|_| DecodeError::Malformed("encode edge_count"))?,
    ) as usize;
    if bytes.len() != 19 + edge_count {
        return Err(DecodeError::Malformed(
            "encode edge_count vs payload length",
        ));
    }
    let mut outcomes = Vec::with_capacity(edge_count);
    for &b in &bytes[19..] {
        outcomes.push(match b {
            EDGE_OUTCOME_INSERTED => EdgeOutcome::Inserted,
            EDGE_OUTCOME_TARGET_MISSING => EdgeOutcome::TargetMissing,
            _ => return Err(DecodeError::Malformed("encode unknown edge outcome")),
        });
    }
    Ok((MemoryId::from_be_bytes(mid_bytes), outcomes))
}

/// Encode a FORGET outcome.
///
/// Layout:
/// - bytes 0..16 — `memory_id` (big-endian)
/// - byte 16     — version (= 1)
/// - byte 17     — outcome discriminant
#[must_use]
pub fn encode_forget_payload(memory_id: MemoryId, outcome: ForgetOutcome) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&memory_id.to_be_bytes());
    out.push(PAYLOAD_VERSION_V1);
    out.push(match outcome {
        ForgetOutcome::Tombstoned => FORGET_OUTCOME_TOMBSTONED,
        ForgetOutcome::AlreadyTombstoned => FORGET_OUTCOME_ALREADY_TOMBSTONED,
        ForgetOutcome::MemoryNotFound => FORGET_OUTCOME_NOT_FOUND,
    });
    out
}

/// LINK payload. Spec §09/07 §3.
///
/// Layout (24 bytes):
/// - bytes 0..4   — version (= 1) + 3 reserved
/// - bytes 4..12  — `created_at_unix_nanos` (u64 little-endian)
/// - bytes 12..16 — `weight` (f32 little-endian)
/// - byte 16      — `already_existed` (0 / 1)
/// - bytes 17..24 — reserved
#[must_use]
pub fn encode_link_payload(
    weight: f32,
    created_at_unix_nanos: u64,
    already_existed: bool,
) -> Vec<u8> {
    let mut out = vec![0u8; 24];
    out[0] = PAYLOAD_VERSION_V1;
    out[4..12].copy_from_slice(&created_at_unix_nanos.to_le_bytes());
    out[12..16].copy_from_slice(&weight.to_le_bytes());
    out[16] = u8::from(already_existed);
    out
}

pub fn decode_link_payload(bytes: &[u8]) -> Result<(f32, u64, bool), DecodeError> {
    if bytes.len() != 24 {
        return Err(DecodeError::Malformed("link payload wrong length"));
    }
    if bytes[0] != PAYLOAD_VERSION_V1 {
        return Err(DecodeError::Malformed("link payload unknown version"));
    }
    let created_at = u64::from_le_bytes(
        bytes[4..12]
            .try_into()
            .map_err(|_| DecodeError::Malformed("link created_at"))?,
    );
    let weight = f32::from_le_bytes(
        bytes[12..16]
            .try_into()
            .map_err(|_| DecodeError::Malformed("link weight"))?,
    );
    let already_existed = bytes[16] != 0;
    Ok((weight, created_at, already_existed))
}

/// UNLINK payload (2 bytes):
/// - byte 0 — version (= 1)
/// - byte 1 — `removed` (0 / 1)
#[must_use]
pub fn encode_unlink_payload(removed: bool) -> Vec<u8> {
    vec![PAYLOAD_VERSION_V1, u8::from(removed)]
}

pub fn decode_unlink_payload(bytes: &[u8]) -> Result<bool, DecodeError> {
    if bytes.len() != 2 {
        return Err(DecodeError::Malformed("unlink payload wrong length"));
    }
    if bytes[0] != PAYLOAD_VERSION_V1 {
        return Err(DecodeError::Malformed("unlink payload unknown version"));
    }
    Ok(bytes[1] != 0)
}

pub fn decode_forget_payload(bytes: &[u8]) -> Result<(MemoryId, ForgetOutcome), DecodeError> {
    if bytes.len() != 18 {
        return Err(DecodeError::Malformed("forget payload wrong length"));
    }
    let mid_bytes: [u8; 16] = bytes[..16]
        .try_into()
        .map_err(|_| DecodeError::Malformed("forget memory_id slice"))?;
    let version = bytes[16];
    if version != PAYLOAD_VERSION_V1 {
        return Err(DecodeError::Malformed("forget payload unknown version"));
    }
    let outcome = match bytes[17] {
        FORGET_OUTCOME_TOMBSTONED => ForgetOutcome::Tombstoned,
        FORGET_OUTCOME_ALREADY_TOMBSTONED => ForgetOutcome::AlreadyTombstoned,
        FORGET_OUTCOME_NOT_FOUND => ForgetOutcome::MemoryNotFound,
        _ => return Err(DecodeError::Malformed("forget unknown outcome")),
    };
    Ok((MemoryId::from_be_bytes(mid_bytes), outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_planner::EncodeOpEdge;

    fn encode_op() -> EncodeOp {
        EncodeOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            context_id: brain_core::ContextId(42),
            kind: brain_core::MemoryKind::Episodic,
            text: "hello".into(),
            vector: [0.0; brain_embed::VECTOR_DIM],
            salience_initial: 0.5,
            fingerprint: [0x11; 16],
            edges: vec![EncodeOpEdge {
                target: brain_core::MemoryId::from(7u128),
                kind: brain_core::EdgeKind::Caused,
                weight: 0.5,
            }],
        }
    }

    #[test]
    fn encode_hash_is_deterministic() {
        let a = hash_encode_request(&encode_op());
        let b = hash_encode_request(&encode_op());
        assert_eq!(a, b);
    }

    #[test]
    fn encode_hash_changes_on_text_change() {
        let a = hash_encode_request(&encode_op());
        let mut op = encode_op();
        op.text = "HELLO".into();
        let b = hash_encode_request(&op);
        assert_ne!(a, b);
    }

    #[test]
    fn encode_hash_excludes_request_id() {
        // Different request_ids must hash to the same value.
        let mut op_a = encode_op();
        let mut op_b = encode_op();
        op_a.request_id = brain_core::RequestId::from([1u8; 16]);
        op_b.request_id = brain_core::RequestId::from([99u8; 16]);
        assert_eq!(hash_encode_request(&op_a), hash_encode_request(&op_b));
    }

    #[test]
    fn forget_hash_is_deterministic() {
        let op = ForgetOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            memory_id: brain_core::MemoryId::from(7u128),
            mode: ForgetMode::Soft,
        };
        let a = hash_forget_request(&op);
        let b = hash_forget_request(&op);
        assert_eq!(a, b);
    }

    #[test]
    fn forget_hash_changes_on_mode_change() {
        let mut op = ForgetOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            memory_id: brain_core::MemoryId::from(7u128),
            mode: ForgetMode::Soft,
        };
        let soft = hash_forget_request(&op);
        op.mode = ForgetMode::Hard;
        let hard = hash_forget_request(&op);
        assert_ne!(soft, hard);
    }

    #[test]
    fn encode_payload_round_trips() {
        let mid = brain_core::MemoryId::from(12345u128);
        let outcomes = vec![
            EdgeOutcome::Inserted,
            EdgeOutcome::TargetMissing,
            EdgeOutcome::Inserted,
        ];
        let bytes = encode_encode_payload(mid, &outcomes);
        let (decoded_mid, decoded_outcomes) = decode_encode_payload(&bytes).unwrap();
        assert_eq!(decoded_mid, mid);
        assert_eq!(decoded_outcomes, outcomes);
    }

    #[test]
    fn encode_payload_zero_edges_round_trips() {
        let mid = brain_core::MemoryId::from(7u128);
        let bytes = encode_encode_payload(mid, &[]);
        assert_eq!(bytes.len(), 19);
        let (decoded_mid, decoded_outcomes) = decode_encode_payload(&bytes).unwrap();
        assert_eq!(decoded_mid, mid);
        assert!(decoded_outcomes.is_empty());
    }

    #[test]
    fn forget_payload_round_trips_all_outcomes() {
        let mid = brain_core::MemoryId::from(7u128);
        for outcome in [
            ForgetOutcome::Tombstoned,
            ForgetOutcome::AlreadyTombstoned,
            ForgetOutcome::MemoryNotFound,
        ] {
            let bytes = encode_forget_payload(mid, outcome);
            assert_eq!(bytes.len(), 18);
            let (decoded_mid, decoded_outcome) = decode_forget_payload(&bytes).unwrap();
            assert_eq!(decoded_mid, mid);
            assert_eq!(decoded_outcome, outcome);
        }
    }

    #[test]
    fn malformed_payloads_error_out() {
        assert!(decode_encode_payload(b"too short").is_err());
        assert!(decode_forget_payload(b"too short").is_err());

        // Bad version byte.
        let mut bytes =
            encode_forget_payload(brain_core::MemoryId::from(1u128), ForgetOutcome::Tombstoned);
        bytes[16] = 99;
        assert!(decode_forget_payload(&bytes).is_err());
    }
}
