//! Every opcode the spec tabulates must exist in the `Opcode` enum.
//!
//! `spec/` is authoritative and the code follows it. Nothing enforced that:
//! `opcode.rs`'s own `ALL` table guards the enum against *itself*, which
//! catches a variant added without a row, and cannot catch an opcode the spec
//! declares that was never implemented at all. Those are two independent
//! transcriptions of one contract with nothing in between — the arrangement
//! that has produced every drift found in this codebase and its SDKs.
//!
//! # Direction
//!
//! This asserts spec → enum only, deliberately. The spec declares opcodes in
//! three different shapes:
//!
//!   1. table rows — `| 0x0001 | `HELLO` | C → S | … |`
//!   2. prose — ``SPACE_CREATE 0x0070` · `SPACE_LIST 0x0071``
//!   3. by convention — "Responses live at `0x01F0–0x01FF`"
//!
//! Only the first is unambiguous to parse. A reverse check (enum → spec) run
//! against table rows alone reports every prose-declared and
//! convention-declared opcode as missing: 46 false positives, including all 34
//! typed-graph responses and the entire tenancy range, all of which are
//! properly specified. So the reverse direction is left to human review rather
//! than asserted badly — a check with a 75% false-positive rate is one people
//! learn to ignore, which is worse than no check.
//!
//! The direction kept is the one that matters anyway: the spec promising
//! something the server does not implement.

use std::collections::BTreeMap;
use std::path::PathBuf;

use brain_protocol::codec::opcode::Opcode;

/// Specified, tabulated, and deliberately not implemented yet.
///
/// `spec/04_wire_protocol/03_opcodes.md` §2.6 tabulates nine typed-graph admin
/// opcodes at `0x0170–0x0178`. Exactly one of them — `0x0178`
/// `ADMIN_LIST_PENDING_CONTRADICTIONS` — exists. The other eight are listed
/// here so the gap is counted rather than merely absent, and so a *ninth*
/// unimplemented opcode is a test failure instead of a silent addition.
///
/// `README.md`'s "Future scope" tracks one of these conceptually, as
/// "ADMIN_TANTIVY_REBUILD" — a different name from the spec's
/// `ADMIN_REINDEX_TANTIVY`, which is why searching for it finds nothing. The
/// other seven are not tracked anywhere.
const UNIMPLEMENTED: &[(u16, &str)] = &[
    (0x0170, "ADMIN_REBUILD_INDEX"),
    (0x0171, "ADMIN_REINDEX_TANTIVY"),
    (0x0172, "ADMIN_LIST_PENDING_RESOLUTIONS"),
    (0x0173, "ADMIN_RESOLVE_AMBIGUITY"),
    (0x0174, "ADMIN_GET_AUDIT"),
    (0x0175, "ADMIN_LIST_STALE_STATEMENTS"),
    // Also tabulated at 0x006E in the substrate admin range (§1), where it IS
    // implemented, with the same body and the same response. The spec declares
    // one operation at two opcodes; the duplicate is recorded here rather than
    // resolved, because `spec/` is read-only.
    (0x0176, "ADMIN_BACKFILL"),
    (0x0177, "ADMIN_JOB_STATUS"),
];

fn spec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec")
}

/// `(value, name)` for every opcode declared as a markdown table row.
fn tabulated_opcodes() -> BTreeMap<u16, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![spec_dir()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                if let Some((value, name)) = parse_row(line) {
                    out.insert(value, name);
                }
            }
        }
    }
    out
}

/// `| 0x0001 | `HELLO` | …` -> `(0x0001, "HELLO")`.
fn parse_row(line: &str) -> Option<(u16, String)> {
    let mut cells = line.trim().strip_prefix('|')?.split('|');
    let value = cells.next()?.trim();
    let name = cells.next()?.trim().trim_matches('`');
    let hex = value.strip_prefix("0x")?;
    if hex.len() != 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    Some((u16::from_str_radix(hex, 16).ok()?, name.to_string()))
}

#[test]
fn the_spec_is_readable_and_tabulates_opcodes() {
    // A floor. If the spec moved or the row format changed, every assertion
    // below would pass against an empty set — green because nothing was read.
    let found = tabulated_opcodes();
    assert!(
        found.len() > 90,
        "only {} opcodes parsed out of the spec tables (expected >90). Either \
         `spec/` moved relative to {}, or the table format changed — either way \
         the assertions below are now vacuous.",
        found.len(),
        spec_dir().display()
    );
}

#[test]
fn every_tabulated_opcode_exists_in_the_enum() {
    let excused: BTreeMap<u16, &str> = UNIMPLEMENTED.iter().copied().collect();

    let missing: Vec<String> = tabulated_opcodes()
        .into_iter()
        .filter(|(value, _)| !excused.contains_key(value))
        .filter(|(value, _)| Opcode::from_u16(*value).is_err())
        .map(|(value, name)| format!("  {value:#06x}  {name}"))
        .collect();

    assert!(
        missing.is_empty(),
        "{} opcode(s) are tabulated in the spec but absent from the `Opcode` \
         enum. The spec is authoritative, so these are unimplemented surface — \
         add them, or add them to UNIMPLEMENTED with the reason:\n{}",
        missing.len(),
        missing.join("\n")
    );
}

#[test]
fn the_unimplemented_list_does_not_outlive_the_gap() {
    // An entry that now exists has been implemented. Leaving it listed makes
    // the list stop meaning "not implemented", which is how a gap register
    // decays into noise nobody reads.
    let implemented: Vec<String> = UNIMPLEMENTED
        .iter()
        .filter(|(value, _)| Opcode::from_u16(*value).is_ok())
        .map(|(value, name)| format!("  {value:#06x}  {name}"))
        .collect();

    assert!(
        implemented.is_empty(),
        "these opcodes now exist, so they are no longer unimplemented — delete \
         them from UNIMPLEMENTED:\n{}",
        implemented.join("\n")
    );
}

#[test]
fn every_unimplemented_entry_is_actually_tabulated() {
    // An excuse for an opcode the spec does not declare would hide nothing and
    // mislead the next reader, and a typo'd value silently excuses the wrong
    // one while leaving the real gap unguarded.
    let tabulated = tabulated_opcodes();
    for (value, name) in UNIMPLEMENTED {
        let found = tabulated.get(value);
        assert!(
            found.is_some(),
            "UNIMPLEMENTED lists {value:#06x} {name}, which no spec table declares"
        );
        assert_eq!(
            found.map(String::as_str),
            Some(*name),
            "UNIMPLEMENTED calls {value:#06x} `{name}`, the spec calls it \
             `{}` — one of them is wrong",
            found.map(String::as_str).unwrap_or("?")
        );
    }
}
