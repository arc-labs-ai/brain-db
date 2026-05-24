//! End-to-end frame round-trip for the backfill control opcodes.
//!
//! The unit tests in `envelope::request` / `envelope::response`
//! exercise the rkyv encode/decode at the body level. This file
//! goes one layer up: it wraps each body in a [`Frame`], serialises
//! the full frame to bytes, decodes it back, and asserts every
//! field survives — mirroring the client ↔ server hop.
//!
//! Covers:
//! - `AdminBackfillRequest` (both `BackfillScope::All` and
//!   `BackfillScope::MemoryRange`) wraps + parses.
//! - `AdminBackfillResponse` carries the assigned id + initial
//!   progress.
//! - `AdminBackfillCancelRequest` / `AdminBackfillCancelResponse`
//!   round-trip with both `cancelled=true` and `cancelled=false`.
//! - `is_admin()` classifies all four opcodes correctly.

use brain_protocol::envelope::request::{
    AdminBackfillCancelRequest, AdminBackfillRequest, BackfillScope, RequestBody,
};
use brain_protocol::envelope::response::{
    AdminBackfillCancelResponse, AdminBackfillResponse, BackfillProgress, ResponseBody,
};
use brain_protocol::{Frame, Opcode};

fn uuid(seed: u8) -> [u8; 16] {
    let mut u = [0u8; 16];
    for (i, b) in u.iter_mut().enumerate() {
        *b = seed.wrapping_add(i as u8);
    }
    u
}

/// Wrap `body` in a frame, encode → decode, and return the
/// reconstructed body. The frame's opcode is taken from `body`.
fn frame_round_trip_request(body: RequestBody) -> RequestBody {
    let opcode = body.opcode();
    let frame = Frame::new(opcode.as_u16(), 0x80 /* EOS */, 7, body.encode());
    let bytes = frame.encode();
    let (decoded, rest) = Frame::decode(&bytes).expect("frame parses");
    assert!(rest.is_empty(), "frame consumed the whole buffer");
    assert_eq!(decoded.header.opcode_u16(), opcode.as_u16());
    RequestBody::decode(opcode, &decoded.payload).expect("body decodes")
}

fn frame_round_trip_response(body: ResponseBody) -> ResponseBody {
    let opcode = body.opcode();
    let frame = Frame::new(opcode.as_u16(), 0x80 /* EOS */, 8, body.encode());
    let bytes = frame.encode();
    let (decoded, rest) = Frame::decode(&bytes).expect("frame parses");
    assert!(rest.is_empty(), "frame consumed the whole buffer");
    assert_eq!(decoded.header.opcode_u16(), opcode.as_u16());
    ResponseBody::decode(opcode, &decoded.payload).expect("body decodes")
}

#[test]
fn backfill_request_all_scope_round_trips_through_frame() {
    let original = RequestBody::AdminBackfill(AdminBackfillRequest {
        scope: BackfillScope::All,
        extractor_ids: vec![1, 2, 3, 4],
        dry_run: false,
        request_id: uuid(1),
    });
    let decoded = frame_round_trip_request(original.clone());
    assert_eq!(decoded, original);
}

#[test]
fn backfill_request_memory_range_round_trips_through_frame() {
    let original = RequestBody::AdminBackfill(AdminBackfillRequest {
        scope: BackfillScope::MemoryRange {
            start: 100,
            end_inclusive: 999,
        },
        extractor_ids: vec![7],
        dry_run: true,
        request_id: uuid(2),
    });
    let decoded = frame_round_trip_request(original.clone());
    assert_eq!(decoded, original);
}

#[test]
fn backfill_cancel_request_round_trips_through_frame() {
    let original = RequestBody::AdminBackfillCancel(AdminBackfillCancelRequest {
        backfill_id: uuid(3),
        request_id: uuid(4),
    });
    let decoded = frame_round_trip_request(original.clone());
    assert_eq!(decoded, original);
}

#[test]
fn backfill_response_carries_id_and_running_progress() {
    let original = ResponseBody::AdminBackfill(AdminBackfillResponse {
        backfill_id: uuid(5),
        progress: BackfillProgress {
            running: true,
            completed: 42,
            failed: 3,
            skipped_already_completed: 7,
            last_processed_memory_id_present: true,
            last_processed_memory_id: 12345,
        },
    });
    let decoded = frame_round_trip_response(original.clone());
    assert_eq!(decoded, original);
}

#[test]
fn backfill_response_initial_idle_snapshot_round_trips() {
    let original = ResponseBody::AdminBackfill(AdminBackfillResponse {
        backfill_id: uuid(6),
        progress: BackfillProgress::idle(),
    });
    let decoded = frame_round_trip_response(original.clone());
    assert_eq!(decoded, original);
}

#[test]
fn backfill_cancel_response_round_trips_both_outcomes() {
    for cancelled in [true, false] {
        let original = ResponseBody::AdminBackfillCancel(AdminBackfillCancelResponse {
            backfill_id: uuid(7),
            cancelled,
            progress: BackfillProgress {
                running: false,
                completed: 50,
                failed: 0,
                skipped_already_completed: 0,
                last_processed_memory_id_present: true,
                last_processed_memory_id: 9999,
            },
        });
        let decoded = frame_round_trip_response(original.clone());
        assert_eq!(decoded, original);
    }
}

#[test]
fn backfill_opcodes_classify_as_admin() {
    // Authorization wrappers + dispatch tables key off `is_admin`.
    // If the range bound regresses, this test catches it before
    // production sees a misclassification.
    assert!(Opcode::AdminBackfillReq.is_admin());
    assert!(Opcode::AdminBackfillResp.is_admin());
    assert!(Opcode::AdminBackfillCancelReq.is_admin());
    assert!(Opcode::AdminBackfillCancelResp.is_admin());
}

#[test]
fn backfill_opcodes_round_trip_through_u16() {
    for op in [
        Opcode::AdminBackfillReq,
        Opcode::AdminBackfillResp,
        Opcode::AdminBackfillCancelReq,
        Opcode::AdminBackfillCancelResp,
    ] {
        let v = op.as_u16();
        let back = Opcode::from_u16(v).expect("decode known opcode");
        assert_eq!(back, op);
    }
}

#[test]
fn server_cancel_handshake_simulates_client_server_hop() {
    // Simulate the canonical operator flow:
    //   client → ADMIN_BACKFILL_REQ
    //   server → ADMIN_BACKFILL_RESP (returns id + initial progress)
    //   client → ADMIN_BACKFILL_CANCEL_REQ (echoes id)
    //   server → ADMIN_BACKFILL_CANCEL_RESP (cancelled=true + final progress)
    let submission_uuid = uuid(11);
    let submission = AdminBackfillRequest {
        scope: BackfillScope::All,
        extractor_ids: vec![1],
        dry_run: false,
        request_id: uuid(10),
    };
    let on_server = match frame_round_trip_request(RequestBody::AdminBackfill(submission.clone())) {
        RequestBody::AdminBackfill(r) => r,
        other => panic!("expected AdminBackfill, got {other:?}"),
    };
    assert_eq!(on_server, submission);

    let server_ack = AdminBackfillResponse {
        backfill_id: submission_uuid,
        progress: BackfillProgress::idle(),
    };
    let on_client = match frame_round_trip_response(ResponseBody::AdminBackfill(server_ack)) {
        ResponseBody::AdminBackfill(r) => r,
        other => panic!("expected AdminBackfill resp, got {other:?}"),
    };
    assert_eq!(on_client.backfill_id, submission_uuid);

    // Now the client cancels using the id from the ack.
    let cancel = AdminBackfillCancelRequest {
        backfill_id: on_client.backfill_id,
        request_id: uuid(12),
    };
    let on_server_cancel = match frame_round_trip_request(RequestBody::AdminBackfillCancel(cancel))
    {
        RequestBody::AdminBackfillCancel(r) => r,
        other => panic!("expected AdminBackfillCancel, got {other:?}"),
    };
    assert_eq!(on_server_cancel.backfill_id, submission_uuid);

    let cancel_ack = AdminBackfillCancelResponse {
        backfill_id: submission_uuid,
        cancelled: true,
        progress: BackfillProgress {
            running: false,
            completed: 3,
            failed: 0,
            skipped_already_completed: 0,
            last_processed_memory_id_present: true,
            last_processed_memory_id: 99,
        },
    };
    let on_client_cancel =
        match frame_round_trip_response(ResponseBody::AdminBackfillCancel(cancel_ack.clone())) {
            ResponseBody::AdminBackfillCancel(r) => r,
            other => panic!("expected AdminBackfillCancel resp, got {other:?}"),
        };
    assert_eq!(on_client_cancel, cancel_ack);
    assert!(on_client_cancel.cancelled);
}
