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

/// Tabulated in the spec and deliberately not implemented.
///
/// Empty, and that is the point: the spec was reconciled against this enum, so
/// every opcode it tabulates is one the server can actually be sent.
///
/// What used to live here was the typed-graph admin range. `spec/` declared
/// nine opcodes at `0x0170–0x0178` and exactly one of them —
/// `ADMIN_LIST_PENDING_CONTRADICTIONS` (`0x0178`) — existed. The other eight
/// have been removed from the spec rather than excused here; `0x0170–0x0177`
/// are now documented as unallocated.
///
/// One of the eight was not missing at all. `ADMIN_BACKFILL` was tabulated
/// twice — at `0x0176` in the typed-graph admin range, where nothing
/// implemented it, and at `0x006E` in the substrate admin range, where it is
/// implemented with a scope selector, an extractor-id list, a dry-run flag and
/// a cancel verb at `0x006F`. The spec declared one operation at two opcodes;
/// it now declares it at the one that exists.
///
/// A future gap goes here with its reason, not without one.
const UNIMPLEMENTED: &[(u16, &str)] = &[];

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

// ---------------------------------------------------------------------------
// The reverse direction, narrowed to where it can be asserted cleanly
// ---------------------------------------------------------------------------

/// `(lo, hi)` for every range the spec declares **reserved** or
/// **unallocated**, from the one shape that is unambiguous to parse: a bullet
/// naming a 4-hex-digit range in backticks.
///
/// A full enum → spec check is not worth asserting (see the module docs: the
/// spec declares opcodes in three shapes and only tables parse cleanly, so the
/// reverse direction reports every prose-declared opcode as missing). This is
/// the subset that has no false positives: the spec states these ranges hold
/// nothing, so an implemented opcode inside one is drift by the spec's own
/// words, with no interpretation needed.
fn reserved_ranges() -> Vec<(u16, u16, String)> {
    let mut out = Vec::new();
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
                let trimmed = line.trim();
                if !trimmed.starts_with('-') {
                    continue;
                }
                let lowered = trimmed.to_ascii_lowercase();
                if !lowered.contains("reserved") && !lowered.contains("unallocated") {
                    continue;
                }
                if let Some(range) = parse_backticked_range(trimmed) {
                    out.push((range.0, range.1, trimmed.to_string()));
                }
            }
        }
    }
    out
}

/// Pulls `` `0x007A–0x007F` `` out of a bullet. Accepts an en dash or a
/// hyphen, and requires both ends to be 4 hex digits so a prose mention of a
/// single opcode is not read as a range.
fn parse_backticked_range(line: &str) -> Option<(u16, u16)> {
    // Every `?` here must stay inside a helper: used directly in the loop it
    // returns from the whole function on the first chunk that does not parse,
    // so a bullet's leading "- " would abort before the backticked range was
    // ever reached. The floor test above is what caught that.
    fn range_of(chunk: &str) -> Option<(u16, u16)> {
        let normalized = chunk.replace(['\u{2013}', '\u{2014}'], "-");
        let (lo, hi) = normalized.split_once('-')?;
        let lo = lo.trim().strip_prefix("0x")?;
        let hi = hi.trim().strip_prefix("0x")?;
        if lo.len() != 4 || hi.len() != 4 {
            return None;
        }
        let lo = u16::from_str_radix(lo, 16).ok()?;
        let hi = u16::from_str_radix(hi, 16).ok()?;
        (lo <= hi).then_some((lo, hi))
    }

    // Odd indices are the backticked spans.
    line.split('`').skip(1).step_by(2).find_map(range_of)
}

#[test]
fn the_spec_declares_reserved_ranges_we_can_read() {
    // The same floor as above: without this, the assertion below passes
    // against an empty set.
    let ranges = reserved_ranges();
    assert!(
        ranges.len() >= 2,
        "only {} reserved ranges parsed out of the spec (expected >=2). The \
         bullet format changed, so the check below is vacuous.",
        ranges.len()
    );
}

#[test]
fn no_implemented_opcode_sits_in_a_range_the_spec_calls_reserved() {
    // This is the check whose absence let eight LLM_CREDENTIAL opcodes be
    // allocated out of `0x0076-0x007F` while the spec still described that
    // whole range as reserved for future use. The spec and the enum each
    // said something true about themselves and contradicted each other, and
    // nothing compared them in this direction.
    let ranges = reserved_ranges();
    let mut violations = Vec::new();
    // The enum has no public enumeration, so decode the space: every value
    // `from_u16` accepts is an implemented opcode.
    for value in 0u16..=u16::MAX {
        let Ok(op) = Opcode::from_u16(value) else {
            continue;
        };
        for (lo, hi, source) in &ranges {
            if value >= *lo && value <= *hi {
                violations.push(format!(
                    "{op:?} ({value:#06x}) is implemented but the spec says:\n    {source}"
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "implemented opcodes inside spec-reserved ranges:\n{}\n\nEither allocate \
         the range in the spec (and narrow the reserved bullet) or remove the \
         opcode.",
        violations.join("\n")
    );
}
