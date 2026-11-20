//! Byte-length caps on client-supplied text and blob input, and the one
//! refusal they all produce.
//!
//! Every value here bounds the byte length of a single string or blob a
//! client can send. Count caps — how many aliases, filter entries or ops one
//! request may name — are per-op policy and stay with their handler.
//!
//! These are write-time bounds, and that is the point. `SCHEMA_GET`,
//! `STATEMENT_GET`, `RELATION_GET` and `MEMORY_INSPECT` take no limit
//! argument, so a stored value too large to frame makes that resource
//! permanently unreadable with nothing the client can pass to recover it.
//! A refused write is something the caller can retry smaller; a refused read
//! of data already stored is not.
//!
//! Each cap is either derived from a constraint named in its own comment or
//! labelled policy; there is no third kind.

use crate::error::OpError;

/// Identifier-ish string cap from the wire spec's universal field-cap table
/// (§16.9.1): a name a caller types, looks up and indexes on.
const IDENTIFIER_STRING_MAX_BYTES: usize = 256;

/// Free-form text cap from the wire spec's universal field-cap table (§16.9.1):
/// prose a human writes for another human to read back.
const FREE_FORM_TEXT_MAX_BYTES: usize = 4096;

/// Opaque blob cap from the wire spec's universal field-cap table (§16.9.1):
/// bytes the server stores and returns without interpreting.
const OPAQUE_BLOB_MAX_BYTES: usize = 64 * 1024;

/// Maximum bytes in relation properties. The WAL `RelationLink` payload reads
/// this blob with `read_blob`, which refuses any length above its own cap, so a
/// larger stored value makes the record undecodable on replay.
pub const RELATION_PROPERTIES_MAX_BYTES: usize =
    brain_storage::wal::payload::RESPONSE_BLOB_MAX as usize;

/// The WAL `RelationLink` payload must store relation properties within the
/// blob length that its replay reader accepts, or the record becomes
/// undecodable on replay.
const _: () = assert!(
    RELATION_PROPERTIES_MAX_BYTES <= brain_storage::wal::payload::RESPONSE_BLOB_MAX as usize,
    "relation properties must fit the WAL reader's response blob cap"
);

/// Maximum bytes in an entity name or alias — the spec's identifier-ish cap.
/// `ENTITY_RESOLVE` checks a candidate against this same bound, so every name
/// that can be stored is a name that can be resolved; normalized names are also
/// redb index keys, which the same cap keeps small.
pub const ENTITY_NAME_MAX_BYTES: usize = IDENTIFIER_STRING_MAX_BYTES;

/// Maximum bytes in a session title — the spec's identifier-ish cap. A title is
/// a short label a caller names a session by and reads back on every
/// `SESSION_LIST` row, which is the field shape that table governs.
pub const SESSION_TITLE_MAX_BYTES: usize = IDENTIFIER_STRING_MAX_BYTES;

/// Maximum bytes in one audit reason message — the spec's free-form text cap.
/// A reason is prose an operator writes and another operator reads off a
/// tombstone row.
pub const REASON_MESSAGE_MAX_BYTES: usize = FREE_FORM_TEXT_MAX_BYTES;

/// Maximum bytes in a statement text or blob value — the spec's opaque-blob
/// cap. The server stores the value and hands it back uninterpreted, which is
/// the field shape that row governs. The framing and WAL ceilings that also
/// bound it sit two orders of magnitude higher, so this is what binds.
pub const STATEMENT_VALUE_MAX_BYTES: usize = OPAQUE_BLOB_MAX_BYTES;

/// Maximum bytes in a predicate or relation-type qualified name.
///
/// Policy. Nothing binds this: both halves of a `namespace:name` intern to
/// integer ids before any key is built, and the spec's identifier-ish cap
/// applied to a two-part name would give 513. Ninety-six bounds the registry
/// key while covering any namespace and identifier a caller declares.
pub const QNAME_MAX_BYTES: usize = 96;

/// Maximum bytes in one procedural category.
///
/// Policy. A category is a short label with no storage or wire constraint on
/// it; the spec's identifier-ish cap would allow 256. Sixty-four bounds registry
/// and renderer work without reaching any label in use.
pub const CATEGORY_MAX_BYTES: usize = 64;

/// Maximum bytes in query text.
///
/// Policy, and deliberately not derived. The only real bound on query text is
/// the request frame, a thousand times wider, and a cap that large would stop
/// being a cap on parser and planner work — which is the whole reason this one
/// exists. Sixteen KiB leaves room for substantial filter expressions.
pub const QUERY_TEXT_MAX_BYTES: usize = 16 * 1024;

/// Maximum bytes in one uploaded schema document.
///
/// Policy. The framing and WAL ceilings that also bound an upload sit sixteen
/// times higher, and the merged result is held separately by
/// [`MERGED_SCHEMA_SOURCE_MAX_BYTES`]. One MiB is what bounds parsing and
/// validation work for a single client-supplied document.
pub const SCHEMA_DOCUMENT_MAX_BYTES: usize = 1024 * 1024;

/// CBOR envelope around the one copy of the merged source in a
/// `SchemaGetResponse`: the map header, five text map keys, the namespace at
/// `ENTITY_NAME_MAX_BYTES`, three integers, and the source's own length prefix.
/// Those parts sum to under 512 bytes; one KiB is that rounded up, so the
/// budget below is never optimistic.
const SCHEMA_GET_ENVELOPE_MAX_BYTES: usize = 1024;

/// Maximum bytes in the merged schema source. `SCHEMA_GET` returns the whole
/// source in one CBOR body and takes no limit argument, so a source that cannot
/// be framed is a schema that can never be read back. The response carries the
/// source once, which makes the budget the frame payload ceiling less the
/// envelope around it.
pub const MERGED_SCHEMA_SOURCE_MAX_BYTES: usize =
    brain_protocol::MAX_PAYLOAD_BYTES - SCHEMA_GET_ENVELOPE_MAX_BYTES;

/// A `SchemaGetResponse` carries the merged source once plus its envelope,
/// and `SCHEMA_GET` takes no limit argument, so both must fit one frame
/// payload or a stored schema becomes permanently unreadable.
const _: () = assert!(
    MERGED_SCHEMA_SOURCE_MAX_BYTES + SCHEMA_GET_ENVELOPE_MAX_BYTES
        <= brain_protocol::MAX_PAYLOAD_BYTES,
    "the merged schema source and its envelope must fit one frame payload"
);

/// One uploaded document is merged into the source, so a document cap above
/// the merged-source cap would admit an upload that can never be read back.
const _: () = assert!(
    SCHEMA_DOCUMENT_MAX_BYTES <= MERGED_SCHEMA_SOURCE_MAX_BYTES,
    "an uploaded document must fit the merged schema source cap"
);

/// Reject one client-supplied field when its byte length exceeds its cap.
///
/// The rejection names the offending input and both sides of the comparison
/// as separate parts, so the dispatcher can put them on the wire as
/// `ErrorDetails` and a client can branch on which input it got wrong
/// without parsing the sentence.
pub fn check_byte_len(field: &str, len: usize, max: usize) -> Result<(), OpError> {
    if len > max {
        return Err(OpError::InvalidField {
            field: field.to_owned(),
            expected: Some(format!("at most {max} bytes")),
            actual: Some(format!("{len} bytes")),
            reason: format!("{field} exceeds cap ({len} > {max} bytes)"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_byte_len_reports_client_actionable_error() {
        let err = check_byte_len("title", 5, 4).expect_err("oversize input must fail");
        let OpError::InvalidField {
            field,
            expected,
            actual,
            reason,
        } = err
        else {
            panic!("oversize input must report the offending field");
        };
        assert_eq!(field, "title");
        assert_eq!(expected.as_deref(), Some("at most 4 bytes"));
        assert_eq!(actual.as_deref(), Some("5 bytes"));
        assert!(reason.contains("title") && reason.contains('5') && reason.contains('4'));
    }
}
