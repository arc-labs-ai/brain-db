//! Every declared opcode must actually route to a body.
//!
//! `Opcode`'s own tests already guard the enum against drift: a variant with no
//! row in the `ALL` table fails there. That proves the opcode *exists*. It says
//! nothing about whether anything will do something with it.
//!
//! `RequestBody::decode` and `ResponseBody::decode` both end in
//! `other => Err(ProtocolError::UnknownOpcode(..))`. That is a runtime
//! fallthrough, not a compile-time exhaustiveness check, so an opcode added to
//! the enum — given a name, a number, a spec entry, a conformance vector, and a
//! method on all three SDKs — and then not added to these matches is silently
//! unreachable. The server answers `UnknownOpcode` to a request every other
//! artifact says is supported.
//!
//! That failure mode is invisible from either side alone: the enum test passes,
//! the SDK compiles, the corpus vector round-trips, and only an end-to-end call
//! against a live server reveals it.
//!
//! This test closes that. It walks the entire `u16` space, keeps whatever
//! `from_u16` accepts, and asserts each one reaches a body — requests through
//! `RequestBody`, responses through `ResponseBody`.
//!
//! It deliberately does NOT assert the payload decodes. Feeding an empty CBOR
//! map to a body with required fields fails, and correctly so; that is the
//! corpus's job. The only question here is whether the opcode is *routed* at
//! all, which is exactly the `UnknownOpcode` arm.

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::error::ProtocolError;

/// An empty CBOR map: valid CBOR, so a routed opcode fails on its fields
/// rather than on the bytes, and the two failures stay distinguishable.
const EMPTY_MAP: &[u8] = &[0xA0];

/// Opcodes deliberately allocated without a handler yet.
///
/// `opcode.rs` marks these "handler implementations pending — wire surface
/// allocated alongside the canonical admin range", which is a reasonable thing
/// to do: reserving numbers next to their siblings keeps the range coherent.
/// A comment cannot be checked, though, so the intent lives here instead.
///
/// The list is the point. A *fifth* unrouted opcode is a mistake and fails the
/// tests below; implementing one of these and forgetting to remove it from the
/// list also fails, so the list cannot quietly outlive the gap it describes.
/// Both directions of a pending op are named, since the pair lands together.
const PENDING: &[(&str, u16)] = &[
    ("AdminTokenizeReq", 0x006A),
    ("AdminTokenizeResp", 0x00EA),
    ("AdminRegisterModelReq", 0x006B),
    ("AdminRegisterModelResp", 0x00EB),
    ("AdminAbortMigrationReq", 0x006C),
    ("AdminAbortMigrationResp", 0x00EC),
    ("AdminRetireFingerprintReq", 0x006D),
    ("AdminRetireFingerprintResp", 0x00ED),
];

fn is_pending(op: Opcode) -> bool {
    PENDING.iter().any(|(_, v)| *v == op.as_u16())
}

/// Every opcode the enum accepts, in numeric order.
fn all_opcodes() -> Vec<Opcode> {
    (0..=u16::MAX)
        .filter_map(|v| Opcode::from_u16(v).ok())
        .collect()
}

/// Did routing reject this opcode outright, as opposed to failing on payload?
fn is_unrouted(result: &Result<impl Sized, ProtocolError>) -> bool {
    matches!(result, Err(ProtocolError::UnknownOpcode(_)))
}

#[test]
fn the_enum_is_populated() {
    // A floor. If `from_u16` regressed to rejecting everything, every
    // assertion below would pass vacuously — the shape of green that means
    // nothing was checked.
    let count = all_opcodes().len();
    assert!(
        count > 100,
        "only {count} opcodes decoded from the u16 space; the enum or `from_u16` \
         has regressed and the coverage assertions below would be vacuous"
    );
}

#[test]
fn every_request_opcode_routes_to_a_request_body() {
    let unrouted: Vec<Opcode> = all_opcodes()
        .into_iter()
        .filter(|op| op.is_request())
        .filter(|op| !is_pending(*op))
        .filter(|op| is_unrouted(&RequestBody::decode(*op, EMPTY_MAP)))
        .collect();

    assert!(
        unrouted.is_empty(),
        "{} request opcode(s) are declared but not routed by `RequestBody::decode`. \
         The server answers UnknownOpcode to these, however complete they look \
         everywhere else:\n{}",
        unrouted.len(),
        unrouted
            .iter()
            .map(|op| format!("  {op:?} ({:#06x})", op.as_u16()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_response_opcode_routes_to_a_response_body() {
    let unrouted: Vec<Opcode> = all_opcodes()
        .into_iter()
        .filter(|op| op.is_response())
        .filter(|op| !is_pending(*op))
        .filter(|op| is_unrouted(&ResponseBody::decode(*op, EMPTY_MAP)))
        .collect();

    assert!(
        unrouted.is_empty(),
        "{} response opcode(s) are declared but not routed by `ResponseBody::decode`. \
         A client cannot decode what the server sends for these:\n{}",
        unrouted.len(),
        unrouted
            .iter()
            .map(|op| format!("  {op:?} ({:#06x})", op.as_u16()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_two_directions_do_not_overlap() {
    // A request opcode that also decodes as a response (or vice versa) means
    // the low-bit convention has been broken somewhere, and a frame could be
    // interpreted as either.
    for op in all_opcodes() {
        let as_req = !is_unrouted(&RequestBody::decode(op, EMPTY_MAP));
        let as_resp = !is_unrouted(&ResponseBody::decode(op, EMPTY_MAP));
        assert!(
            !(as_req && as_resp),
            "{op:?} ({:#06x}) routes as BOTH a request and a response",
            op.as_u16()
        );
    }
}

#[test]
fn the_pending_list_does_not_outlive_the_gap() {
    // An entry that now routes has been implemented, and leaving it listed
    // means the list stops meaning "not implemented" — which is how a gap
    // register becomes noise nobody reads.
    let implemented: Vec<&str> = PENDING
        .iter()
        .filter(|(_, value)| {
            let Ok(op) = Opcode::from_u16(*value) else {
                return false;
            };
            if op.is_request() {
                !is_unrouted(&RequestBody::decode(op, EMPTY_MAP))
            } else {
                !is_unrouted(&ResponseBody::decode(op, EMPTY_MAP))
            }
        })
        .map(|(name, _)| *name)
        .collect();

    assert!(
        implemented.is_empty(),
        "these opcodes now route, so they are no longer pending — delete them \
         from PENDING:\n  {}",
        implemented.join("\n  ")
    );
}

#[test]
fn every_pending_entry_names_a_real_opcode() {
    // A typo'd number would silently excuse an opcode that does not exist,
    // while leaving the real one unguarded.
    for (name, value) in PENDING {
        assert!(
            Opcode::from_u16(*value).is_ok(),
            "PENDING lists {name} as {value:#06x}, which is not a declared opcode"
        );
    }
}
