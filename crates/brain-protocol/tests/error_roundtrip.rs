//! Wire round-trip coverage for the schema-strictness error codes:
//! `PredicateNotInSchema`, `RelationTypeNotInSchema`,
//! `CardinalityViolation`. Plus the malformed-input safety net:
//! `MalformedPayload` instead of a panic when bytes don't parse.
//!
//! Code/category mapping is covered by the colocated tests in
//! `error.rs`. This file walks the codes through the full
//! `ResponseBody::Error(...)` encode → decode cycle so a regression
//! in either the CBOR codec or the `ErrorCodeWire ↔ ErrorCode` table
//! surfaces here.

use brain_protocol::envelope::error::{ErrorDetails, ErrorResponse};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::error::{ErrorCategory, ErrorCode, ProtocolError};
use brain_protocol::shared::enums::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::Opcode;

fn round_trip_error(body: ErrorResponse) -> ErrorResponse {
    let resp = ResponseBody::Error(body);
    let bytes = resp.encode();
    let decoded = ResponseBody::decode(resp.opcode(), &bytes).expect("decode ok");
    match decoded {
        ResponseBody::Error(e) => e,
        other => panic!("expected Error variant, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// New schema-strictness error variants round-trip.
// ---------------------------------------------------------------------------

#[test]
fn predicate_not_in_schema_round_trips() {
    let original = ErrorResponse {
        code: ErrorCodeWire::PredicateNotInSchema,
        category: ErrorCategoryWire::Validation,
        message: "predicate 'acme:ghost' not in active schema v3".into(),
        details: Some(ErrorDetails {
            field: Some("predicate".into()),
            expected: Some("acme:prefers | acme:dislikes".into()),
            actual: Some("acme:ghost".into()),
        }),
        retry_after_ms: None,
    };
    let decoded = round_trip_error(original.clone());
    assert_eq!(decoded, original);
    assert_eq!(decoded.code, ErrorCodeWire::PredicateNotInSchema);
    // And the wire → in-memory bridge stays in sync.
    let code: ErrorCode = decoded.code.into();
    assert_eq!(code, ErrorCode::PredicateNotInSchema);
    assert_eq!(code.category(), ErrorCategory::Validation);
}

#[test]
fn relation_type_not_in_schema_round_trips() {
    let original = ErrorResponse {
        code: ErrorCodeWire::RelationTypeNotInSchema,
        category: ErrorCategoryWire::Validation,
        message: "relation type 'acme:rivals' not in active schema v1".into(),
        details: None,
        retry_after_ms: None,
    };
    let decoded = round_trip_error(original.clone());
    assert_eq!(decoded, original);
    let code: ErrorCode = decoded.code.into();
    assert_eq!(code, ErrorCode::RelationTypeNotInSchema);
    assert_eq!(code.category(), ErrorCategory::Validation);
}

#[test]
fn cardinality_violation_round_trips() {
    let original = ErrorResponse {
        code: ErrorCodeWire::CardinalityViolation,
        category: ErrorCategoryWire::Conflict,
        message: "OneToOne on 'acme:married_to' violated: 2 existing current rows".into(),
        details: Some(ErrorDetails {
            field: Some("relation_type".into()),
            expected: Some("0 or 1".into()),
            actual: Some("2".into()),
        }),
        retry_after_ms: None,
    };
    let decoded = round_trip_error(original.clone());
    assert_eq!(decoded, original);
    let code: ErrorCode = decoded.code.into();
    assert_eq!(code, ErrorCode::CardinalityViolation);
    assert_eq!(code.category(), ErrorCategory::Conflict);
}

// ---------------------------------------------------------------------------
// Wire numeric reprs are pinned. Bumping these means an over-the-wire
// breaking change.
// ---------------------------------------------------------------------------

#[test]
fn schema_strict_wire_codes_have_stable_reprs() {
    assert_eq!(ErrorCodeWire::PredicateNotInSchema as u16, 0x004B);
    assert_eq!(ErrorCodeWire::RelationTypeNotInSchema as u16, 0x004C);
    assert_eq!(ErrorCodeWire::CardinalityViolation as u16, 0x0065);
}

// ---------------------------------------------------------------------------
// Code ↔ wire bridge is a bijection for the three new variants.
// ---------------------------------------------------------------------------

#[test]
fn code_to_wire_and_back_is_identity_for_new_variants() {
    for code in [
        ErrorCode::PredicateNotInSchema,
        ErrorCode::RelationTypeNotInSchema,
        ErrorCode::CardinalityViolation,
    ] {
        let wire: ErrorCodeWire = code.into();
        let back: ErrorCode = wire.into();
        assert_eq!(back, code, "round-trip failed for {code:?}");
    }
}

// ---------------------------------------------------------------------------
// Malformed bytes return MalformedPayload — never panic.
//
// The CBOR decoder must surface garbage as a
// structured error. The four shapes below stress the validator from
// each direction: empty buffer, undersized, oversized garbage, bytes
// that look ALMOST like a valid frame.
// ---------------------------------------------------------------------------

fn assert_malformed(err: ProtocolError) {
    matches!(err, ProtocolError::MalformedPayload(_))
        .then_some(())
        .unwrap_or_else(|| panic!("expected MalformedPayload, got {err:?}"));
}

#[test]
fn empty_bytes_to_error_response_returns_malformed_not_panic() {
    let err = ResponseBody::decode(Opcode::Error, &[]).unwrap_err();
    assert_malformed(err);
}

#[test]
fn random_bytes_to_error_response_returns_malformed_not_panic() {
    let garbage: Vec<u8> = (0..512u32).map(|i| (i & 0xFF) as u8).collect();
    let err = ResponseBody::decode(Opcode::Error, &garbage).unwrap_err();
    assert_malformed(err);
}

#[test]
fn bytes_with_corrupt_middle_return_malformed_not_panic() {
    // Build a real ErrorResponse, encode, then corrupt the middle of
    // the buffer. The CBOR decoder must reject this without
    // panicking. Resolution-in-force: `UnknownErrorCode` doesn't
    // panic — the decoder rejects invalid u16 discriminants and
    // surfaces them as MalformedPayload.
    let resp = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::InvalidArgument,
        category: ErrorCategoryWire::Validation,
        message: "x".repeat(64),
        details: None,
        retry_after_ms: None,
    });
    let mut bytes = resp.encode();
    let len = bytes.len();
    let mid = len / 2;
    let hi = mid.saturating_add(8).min(len);
    // Flip a contiguous slab in the middle to 0xFF. Big enough to
    // disturb both a discriminant byte and a length field if either
    // lives in this region.
    for b in &mut bytes[mid.saturating_sub(8)..hi] {
        *b = 0xFF;
    }
    let err = ResponseBody::decode(Opcode::Error, &bytes).unwrap_err();
    assert_malformed(err);
}

#[test]
fn truncated_real_payload_returns_malformed_not_panic() {
    let resp = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::CardinalityViolation,
        category: ErrorCategoryWire::Conflict,
        message: "long message to force payload size > 16".repeat(8),
        details: None,
        retry_after_ms: None,
    });
    let bytes = resp.encode();
    // Lop the last quarter off so the CBOR body is truncated.
    let truncated = &bytes[..bytes.len() * 3 / 4];
    let err = ResponseBody::decode(Opcode::Error, truncated).unwrap_err();
    assert_malformed(err);
}

#[test]
fn malformed_bytes_on_unrelated_opcode_also_returns_structured_error() {
    // The "no panic on bad bytes" guarantee is per-opcode. Confirm
    // it holds for at least one non-error opcode too — guards against
    // the CBOR decode helper being inconsistently routed.
    let garbage = vec![0xFFu8; 64];
    let err = ResponseBody::decode(Opcode::EncodeResp, &garbage).unwrap_err();
    assert_malformed(err);

    // And for a request opcode.
    let err = RequestBody::decode(Opcode::EncodeReq, &garbage).unwrap_err();
    assert_malformed(err);
}

// ---------------------------------------------------------------------------
// Bookend: a known-good ErrorResponse continues to round-trip
// cleanly so a regression in the CBOR codec could not masquerade as
// "everything's malformed."
// ---------------------------------------------------------------------------

#[test]
fn baseline_invalid_argument_round_trip_still_works() {
    let original = ErrorResponse {
        code: ErrorCodeWire::InvalidArgument,
        category: ErrorCategoryWire::Validation,
        message: "top_k out of range".into(),
        details: Some(ErrorDetails {
            field: Some("top_k".into()),
            expected: Some("[1, 1000]".into()),
            actual: Some("5000".into()),
        }),
        retry_after_ms: None,
    };
    let decoded = round_trip_error(original.clone());
    assert_eq!(decoded, original);
}
