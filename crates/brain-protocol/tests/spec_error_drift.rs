//! Every error code the spec names must exist in `ErrorCodeWire`.
//!
//! `spec/` is authoritative. The error taxonomy is the part of it a client
//! branches on — the SDKs' `is_retryable`, the edge's HTTP status mapping, and
//! any caller's error handling all key off these codes — and nothing compared
//! the spec's list to the enum.
//!
//! # Direction
//!
//! spec → enum only, deliberately, for the same reason as
//! `spec_opcode_drift.rs`: the spec declares error codes in at least four
//! shapes, and a parser that misses one reports correct code as missing.
//!
//!   1. `| `BadMagic` | Frame's magic bytes aren't "BRN0" |`
//!   2. `| `0x0130` | `EntityNotFound` | Entity | NotFound |`
//!   3. `| `Internal` (0x0080) | Generic internal error |`
//!   4. `| Max operations per transaction | 1000 | `TransactionTooLarge` |`
//!
//! Each shape found while writing this removed false positives from the
//! reverse direction: format 2 accounted for nine, format 3 for `Internal`,
//! format 4 for `TransactionTooLarge`. A fifth shape almost certainly exists
//! somewhere in 54,000 lines of spec, so the reverse check is left to human
//! review rather than asserted on an extraction that has been wrong four times.
//!
//! The direction kept is the one with consequences: the spec promising a code
//! that clients cannot receive.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Named in the spec's error tables, absent from `ErrorCodeWire`.
///
/// The list is empty, and that is the point: the spec was reconciled against
/// this enum, so every code it names is one a caller can actually receive.
///
/// The entries that used to live here fell into two groups, and both were
/// fixed in the spec rather than excused here.
///
/// **The spec named one condition twice.** Its §3.x tables used a
/// `*LimitExceeded` family and its limits table used a `TooMany*` family for
/// the same conditions. The implementation follows the first —
/// `StreamLimitExceeded = 0x0074`, `TransactionLimitExceeded = 0x0076` — and
/// the limits table now says so, with one code per limit.
///
/// **The rest described behaviour that surfaces under another code.**
/// `NamespaceRequired`, `NamespaceUnknown` and `WriteToSystemNamespace` are
/// real, enforced invariants, but all three are denials of access to a
/// namespace and all three report `PermissionDenied` — deliberately, since a
/// distinct code per cause would tell a prober which namespaces exist.
/// `BadContextId` was the pre-rename spelling of `BadSessionId`; `TooManyEdges`
/// and `TooManyContexts` capped request shapes that no longer exist (`ENCODE`
/// carries no edges, and `session_id` is a client-chosen `u64` out of no
/// bounded table); and `SchemaNotDeclared` was removed with the per-namespace
/// schema gate it belonged to.
///
/// A future gap goes here with its reason, not without one.
const UNIMPLEMENTED: &[&str] = &[];

fn spec_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/04_wire_protocol/07_error_handling.md")
}

/// Every error-code name the spec's tables and prose declare.
fn specified_codes() -> BTreeSet<String> {
    let text = std::fs::read_to_string(spec_file()).unwrap_or_default();
    let mut out = BTreeSet::new();
    for line in text.lines() {
        // Formats 1, 2 and 4: any backticked CamelCase cell in a table row.
        if line.trim_start().starts_with('|') {
            for cell in line.split('|') {
                let cell = cell.trim();
                // `Name`, or `Name` (0xNNNN). The backticks are required: an
                // unquoted CamelCase cell is a table HEADER ("Category",
                // "Action"), and accepting those reported 29 phantom codes.
                let candidate = cell
                    .split_once(" (0x")
                    .map_or(cell, |(head, _)| head)
                    .trim();
                let Some(candidate) = candidate
                    .strip_prefix('`')
                    .and_then(|s| s.strip_suffix('`'))
                else {
                    continue;
                };
                if is_code_name(candidate) && !is_category(candidate) {
                    out.insert(candidate.to_string());
                }
            }
        }
    }
    out
}

/// The nine `ErrorCategoryWire` names. They are backticked in the spec's
/// category table exactly like codes are, but they are a different enum;
/// `Internal` is deliberately BOTH, so this is checked by position rather than
/// by subtracting the set (which would drop the real `Internal` code).
fn is_category(s: &str) -> bool {
    matches!(
        s,
        "Protocol"
            | "Authentication"
            | "Authorization"
            | "Validation"
            | "NotFound"
            | "Conflict"
            | "ResourceExhausted"
            | "Unavailable"
    )
}

/// A code name is CamelCase starting uppercase, letters and digits only.
fn is_code_name(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().any(|c: char| c.is_ascii_lowercase())
}

fn enums_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/shared/enums.rs")
}

/// Every `ErrorCodeWire` variant name, read from its declaration.
///
/// `ErrorCodeWire` has no `from_u16` (unlike `Opcode`) and the crate's CBOR
/// helpers are `pub(crate)`, so there is no runtime way to enumerate it from an
/// integration test. Reading the declaration is the honest alternative: this is
/// a drift test between two documents, and it now reads both as text rather
/// than widening the library's API to suit a test. The floor assertion below is
/// what keeps a parser regression from turning into a vacuous pass.
fn implemented_codes() -> BTreeSet<String> {
    let text = std::fs::read_to_string(enums_file()).unwrap_or_default();
    let Some(start) = text.find("pub enum ErrorCodeWire {") else {
        return BTreeSet::new();
    };
    let body = &text[start..];
    let end = body.find("\n}").unwrap_or(body.len());
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, rest) = line.split_once(" = 0x")?;
            rest.trim_end_matches(',')
                .chars()
                .all(|c| c.is_ascii_hexdigit())
                .then(|| name.to_string())
        })
        .filter(|n| is_code_name(n))
        .collect()
}

#[test]
fn the_spec_and_the_enum_are_both_readable() {
    // Floors. Either side reading empty would make the assertion below pass
    // against nothing — green because nothing was compared.
    let spec = specified_codes();
    assert!(
        spec.len() > 60,
        "only {} error names parsed from {} (expected >60); the spec moved or \
         its table format changed, and the check below is now vacuous",
        spec.len(),
        spec_file().display()
    );
    let implemented = implemented_codes();
    assert!(
        implemented.len() > 60,
        "only {} ErrorCodeWire variants enumerated (expected >60)",
        implemented.len()
    );
}

#[test]
fn every_specified_error_code_exists() {
    let excused: BTreeSet<&str> = UNIMPLEMENTED.iter().copied().collect();
    let implemented = implemented_codes();

    let missing: Vec<String> = specified_codes()
        .into_iter()
        .filter(|name| !excused.contains(name.as_str()))
        .filter(|name| !implemented.contains(name))
        .collect();

    assert!(
        missing.is_empty(),
        "{} error code(s) are named in the spec but absent from `ErrorCodeWire`. \
         A caller cannot branch on a code the server can never send — add them, \
         or add them to UNIMPLEMENTED with the reason:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn the_unimplemented_list_does_not_outlive_the_gap() {
    let implemented = implemented_codes();
    let done: Vec<&str> = UNIMPLEMENTED
        .iter()
        .copied()
        .filter(|name| implemented.contains(*name))
        .collect();

    assert!(
        done.is_empty(),
        "these codes now exist, so they are no longer unimplemented — delete \
         them from UNIMPLEMENTED:\n  {}",
        done.join("\n  ")
    );
}

#[test]
fn every_unimplemented_entry_is_actually_specified() {
    // An excuse for a code the spec never names would hide nothing and mislead
    // the next reader; a typo would excuse the wrong one and leave the real
    // gap unguarded.
    let spec = specified_codes();
    for name in UNIMPLEMENTED {
        assert!(
            spec.contains(*name),
            "UNIMPLEMENTED lists `{name}`, which the error spec does not name"
        );
    }
}
