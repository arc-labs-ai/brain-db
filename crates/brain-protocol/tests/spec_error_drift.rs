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
//!
//! Prose promises a code with a returning verb, an arrow, or a hex annotation;
//! `no` and `not` mark a code the document says does not exist.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Named in the spec's error tables, absent from `ErrorCodeWire`.
///
/// The list is empty, and that is the point: the spec is reconciled against
/// this enum, so every code it names is one a caller can actually receive.
///
/// Two kinds of name would otherwise land here, and each is resolved in the
/// spec rather than excused in this list.
///
/// **One condition, named twice.** The §3.x tables and the limits table
/// describe the same conditions, so naming them with a `*LimitExceeded`
/// family in one and a `TooMany*` family in the other promises a code no
/// caller receives. The implementation names them once —
/// `StreamLimitExceeded = 0x0074`, `TransactionLimitExceeded = 0x0076` — and
/// the limits table says the same, one code per limit.
///
/// **A condition that surfaces under another code.** `NamespaceRequired`,
/// `NamespaceUnknown` and `WriteToSystemNamespace` are real, enforced
/// invariants, but all three are denials of access to a namespace and all
/// three report `PermissionDenied` — deliberately, since a distinct code per
/// cause would tell a prober which namespaces exist. A bad session is
/// `BadSessionId` and nothing else. `TooManyEdges` and `TooManyContexts` have
/// no request shape left to cap: `ENCODE` carries no edges, and `session_id`
/// is a client-chosen `u64` out of no bounded table. And there is no
/// per-namespace schema gate for a `SchemaNotDeclared` to guard.
///
/// A future gap goes here with its reason, not without one.
const UNIMPLEMENTED: &[&str] = &[];

fn spec_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/04_wire_protocol/07_error_handling.md")
}

fn spec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/04_wire_protocol")
}

fn spec_md_files() -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(spec_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    files
}

/// Every error-code name the spec's tables and prose declare.
fn table_codes() -> BTreeSet<String> {
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

/// The prose rule is cue-gated, so it stays quiet in files that are not about
/// errors. `table_codes` accepts any backticked cell in any table row — correct
/// for `07_error_handling.md`, whose tables are error tables, and wrong for the
/// field-layout tables elsewhere in this directory, which would contribute
/// payload field types.
fn prose_codes() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for path in spec_md_files() {
        let file_name = path.file_name().unwrap().to_string_lossy();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (name, line) in prose_codes_in(&text) {
            out.entry(name)
                .or_insert_with(|| format!("{file_name}:{line}"));
        }
    }
    out
}

/// Finds backticked `UpperCamelCase` names that prose promises as returnable
/// codes: one of the nine return cues in the match immediately before the
/// name, `→` immediately before it, or a `(0xNNNN)` annotation immediately
/// after it. Those are cue rules rather than name-shape rules because prose
/// that merely names a type, such as `the `ErrorDetails` carries` or a valid
/// `MemoryId`, uses the same typography for request structs, AST variants,
/// and other enums' variants.
/// `no` and `not` immediately before a name mark a code the document says does
/// not exist or contrasts against the one really returned, so neither is a
/// promise. Fenced blocks are source listings rather than promises, and table
/// rows are owned by `table_codes`, so both are skipped.
fn prose_codes_in(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut fenced = false;

    for (line_number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || trimmed.starts_with('|') {
            continue;
        }

        let mut offset = 0;
        while let Some(open_relative) = line[offset..].find('`') {
            let open = offset + open_relative;
            let Some(close_relative) = line[open + 1..].find('`') else {
                break;
            };
            let close = open + 1 + close_relative;
            let name = &line[open + 1..close];
            if name.chars().all(|c| c.is_ascii_alphanumeric())
                && is_code_name(name)
                && !is_category(name)
                && !is_enum_container(name)
            {
                let before = &line[..open];
                let arrow = before.trim_end().ends_with('→');
                let before_bytes = before.as_bytes();
                let mut end = before_bytes.len();
                while end > 0 && !before_bytes[end - 1].is_ascii_alphabetic() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0 && before_bytes[start - 1].is_ascii_alphabetic() {
                    start -= 1;
                }
                let prev = before[start..end].to_ascii_lowercase();
                let after = &line[close + 1..];
                let after = after.trim_start_matches(' ');
                let hex = after.strip_prefix("(0x").is_some_and(|s| {
                    s.find(')').is_some_and(|end| {
                        let digits = &s[..end];
                        (1..=4).contains(&digits.len())
                            && digits.chars().all(|c| c.is_ascii_hexdigit())
                    })
                });
                let cue = matches!(
                    prev.as_str(),
                    "return"
                        | "returns"
                        | "returned"
                        | "returning"
                        | "with"
                        | "otherwise"
                        | "raises"
                        | "raised"
                        | "gets"
                );
                if !matches!(prev.as_str(), "no" | "not") && (arrow || hex || cue) {
                    out.entry(name.to_string()).or_insert(line_number + 1);
                }
            }
            offset = close + 1;
        }
    }
    out
}

/// The ten `ErrorCategoryWire` names. They are backticked in the spec's
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
            | "OperatorIntervention"
    )
}

/// These are the enum containers themselves, never members of themselves, so
/// no sentence can make one of them a code. This set cannot grow to absorb a
/// wrong code name: a real code name would be visibly out of place in it.
fn is_enum_container(s: &str) -> bool {
    matches!(
        s,
        "ErrorCode" | "ErrorCodeWire" | "ErrorCategory" | "ErrorCategoryWire"
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

/// Reads the name and numeric value of every variant from the declaration;
/// `implemented_codes()` is its key set so the two can never disagree about
/// what exists.
fn discriminants() -> BTreeMap<String, u16> {
    let text = std::fs::read_to_string(enums_file()).unwrap_or_default();
    let Some(start) = text.find("pub enum ErrorCodeWire {") else {
        return BTreeMap::new();
    };
    let body = &text[start..];
    let end = body.find("\n}").unwrap_or(body.len());
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, rest) = line.split_once(" = 0x")?;
            let digits = rest.trim_end_matches(',');
            if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let value = u16::from_str_radix(digits, 16).ok()?;
            is_code_name(name).then(|| (name.to_string(), value))
        })
        .collect()
}

/// Every `ErrorCodeWire` variant name, read from its declaration.
///
/// The crate's CBOR helpers are `pub(crate)`, so an integration test cannot
/// enumerate the vocabulary at runtime. Reading the declaration is the
/// honest alternative: this is a drift test between two documents, and it
/// reads both as text rather than widening the library API for a test.
fn implemented_codes() -> BTreeSet<String> {
    discriminants().into_keys().collect()
}

/// Every `` `Name` (0xNNNN) `` annotation, as (name, value, line).
/// A snake spelling is read only when a hex annotation makes it unambiguous:
/// the directory is full of SCREAMING_SNAKE opcode names that are not codes,
/// and an annotation whose folded name is not a known variant is skipped
/// rather than guessed at.
fn hex_annotations_in(text: &str) -> Vec<(String, u16, usize)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (line_number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut offset = 0;
        while let Some(open_relative) = line[offset..].find('`') {
            let open = offset + open_relative;
            let Some(close_relative) = line[open + 1..].find('`') else {
                break;
            };
            let close = open + 1 + close_relative;
            let written = &line[open + 1..close];
            if !written.is_empty()
                && written
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let after = line[close + 1..].trim_start_matches(' ');
                if let Some(rest) = after.strip_prefix("(0x") {
                    if let Some(end) = rest.find(')') {
                        let digits = &rest[..end];
                        if (1..=4).contains(&digits.len())
                            && digits.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            let name = fold_snake_name(written);
                            out.push((
                                name,
                                u16::from_str_radix(digits, 16).unwrap(),
                                line_number + 1,
                            ));
                        }
                    }
                }
            }
            offset = close + 1;
        }
    }
    out
}

/// An underscore marks a SCREAMING_SNAKE spelling; a CamelCase name is already
/// the enum's spelling.
fn fold_snake_name(written: &str) -> String {
    if !written.contains('_') {
        return written.to_string();
    }
    written
        .split('_')
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let mut chars = lower.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect()
}

fn written_hex_name(text: &str, folded: &str, line_number: usize) -> String {
    text.lines()
        .nth(line_number - 1)
        .into_iter()
        .flat_map(|line| line.split('`').skip(1).step_by(2))
        .find(|written| fold_snake_name(written) == folded)
        .map_or_else(|| folded.to_string(), str::to_string)
}

/// Splits an `UpperCamelCase` name into its component words so a missing code
/// can be compared with implemented variants by shared meaning rather than
/// by spelling alone.
fn camel_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_uppercase() {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() && chars[i].is_ascii_uppercase() {
            i += 1;
        }
        if i - start >= 2 && i < chars.len() && chars[i].is_ascii_lowercase() {
            i -= 1;
        }
        while i < chars.len() && chars[i].is_ascii_lowercase() {
            i += 1;
        }
        words.push(chars[start..i].iter().collect());
    }
    words
}

/// Computes character edit distance for the final tie-break between names
/// that share the same CamelCase words.
fn levenshtein(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, left_char) in left.chars().enumerate() {
        let mut current = vec![i + 1; right.len() + 1];
        for (j, right_char) in right.iter().enumerate() {
            current[j + 1] = (current[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + usize::from(left_char != *right_char));
        }
        previous = current;
    }
    previous[right.len()]
}

/// Returns up to three hint variants for a missing code, because a missing
/// code is nearly always a near-miss of a real one. Shared CamelCase words
/// rank first — `UnknownOpcode` should find `BadOpcode` despite being six
/// edits away — and edit distance breaks ties; the result is a hint, not an
/// answer.
fn nearest_variants(name: &str, implemented: &BTreeSet<String>) -> Vec<String> {
    let words = camel_words(name);
    let mut candidates: Vec<(usize, usize, &String)> = implemented
        .iter()
        .filter_map(|variant| {
            let shared = camel_words(variant)
                .iter()
                .filter(|word| words.contains(word))
                .count();
            let distance = levenshtein(name, variant);
            (shared > 0 || distance <= 3.max(name.len() / 3)).then_some((shared, distance, variant))
        })
        .collect();
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });
    candidates
        .into_iter()
        .take(3)
        .map(|(_, _, name)| name.clone())
        .collect()
}

#[test]
fn the_spec_and_the_enum_are_both_readable() {
    // Floors. Either side reading empty would make the assertion below pass
    // against nothing — green because nothing was compared.
    let spec = table_codes();
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
    let prose = prose_codes();
    assert!(
        prose.len() > 20,
        "only {} prose error names parsed from {}; the prose scanner went blind and the prose half of the reconciliation is now vacuous",
        prose.len(),
        spec_file().display()
    );
}

#[test]
fn every_specified_error_code_exists() {
    let excused: BTreeSet<&str> = UNIMPLEMENTED.iter().copied().collect();
    let implemented = implemented_codes();
    let prose = prose_codes();
    let mut specified = table_codes();
    specified.extend(prose.keys().cloned());

    let missing: Vec<String> = specified
        .into_iter()
        .filter(|name| !excused.contains(name.as_str()))
        .filter(|name| !implemented.contains(name))
        .map(|name| {
            let location = prose
                .get(&name)
                .map_or_else(|| "table".to_string(), String::clone);
            let suggestions = nearest_variants(&name, &implemented);
            if suggestions.is_empty() {
                format!("{name} ({location})")
            } else {
                format!(
                    "{name} ({location}) — did you mean {}?",
                    suggestions.join(", ")
                )
            }
        })
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
    let mut spec = table_codes();
    spec.extend(prose_codes().into_keys());
    for name in UNIMPLEMENTED {
        assert!(
            spec.contains(*name),
            "UNIMPLEMENTED lists `{name}`, which the error spec does not name"
        );
    }
}

#[test]
fn every_hex_annotation_matches_the_enum() {
    let discriminants = discriminants();
    let files = spec_md_files();

    let mut checked = Vec::new();
    let mut mismatches = Vec::new();
    for path in files {
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (name, value, line) in hex_annotations_in(&text) {
            if let Some(real) = discriminants.get(&name) {
                let written = written_hex_name(&text, &name, line);
                checked.push((file_name.clone(), line, written, name, value, *real));
                if value != *real {
                    let (file, line, written, name, value, real) = checked.last().unwrap();
                    mismatches.push(format!(
                        "  {file}:{line} `{written}` (0x{value:04X}) — {name} is 0x{real:04X}"
                    ));
                }
            }
        }
    }
    assert!(
        checked.len() >= 40,
        "only {} hex annotations checked (expected at least 40); the annotation form moved",
        checked.len()
    );
    assert!(
        mismatches.is_empty(),
        "{} hex annotation(s) disagree with `ErrorCodeWire`:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn the_annotation_scanner_reads_the_value_and_the_snake_spelling() {
    let fixture = concat!(
        "unknown qnames produce `PredicateNotInSchema` (0x004B) in strict mode.\n",
        "mismatches produce `STATEMENT_OBJECT_TYPE_MISMATCH` (0x41) on the wire.\n",
        "- `ContextNotFound` (0x0051) — a referenced session has no registry row.\n",
        "`ENTITY_CREATE` (0x0130) opens the stream.\n",
        "```text\n",
        "// `BadMagic` (0x0001) is not a promise here\n",
        "```\n",
    );
    let found = hex_annotations_in(fixture);
    assert_eq!(
        found,
        vec![
            ("PredicateNotInSchema".to_string(), 0x004B, 1),
            ("StatementObjectTypeMismatch".to_string(), 0x41, 2),
            ("ContextNotFound".to_string(), 0x0051, 3),
            ("EntityCreate".to_string(), 0x0130, 4),
        ]
    );

    let known: BTreeSet<_> = discriminants().into_keys().collect();
    let filtered = found
        .iter()
        .filter(|(name, _, _)| known.contains(name))
        .map(|(name, value, line)| (name.clone(), *value, *line))
        .collect::<Vec<_>>();
    assert_eq!(
        filtered,
        vec![
            ("PredicateNotInSchema".to_string(), 0x004B, 1),
            ("StatementObjectTypeMismatch".to_string(), 0x41, 2),
            ("ContextNotFound".to_string(), 0x0051, 3),
        ]
    );
    assert_ne!(
        0x41,
        *discriminants().get("StatementObjectTypeMismatch").unwrap()
    );
}

#[test]
fn the_prose_scanner_sees_a_promised_code_and_ignores_a_type_name() {
    let fixture = concat!(
        // promises a code
        r#"The server's effective limit returns `PayloadTooLarge` and closes the stream.
The opcode MUST be a known value. Unknown opcodes return `UnknownOpcode`.
Operations are rejected with `NotHandshaked` until WELCOME has been received.
- The session MUST have admin privileges. Otherwise `Unauthorized`.
- `ENTITY_CREATE` with an unknown entity type → `EntityTypeNotInSchema`.
- `RetrievalUnavailable` (0x0083) — a required retriever is not servable.
"#,
        // names a type, not a code
        r#"The `ErrorCategory` enum classifies errors into broad groups.
The `ErrorCodeWire` carries with `ErrorCategoryWire` the category of each code.
The optional `ErrorDetails` carries per-error detail.
- `memory_id` MUST be a valid `MemoryId` belonging to the agent.
- `mode` MUST be either `Soft` or `Hard`.
There is no `BadStream` (0x0099) code. A stream id is either in use or it is not.
A stream-count overrun is `StreamLimitExceeded`, not `TooManyStreams`.
| Max results per RECALL | 1000 | `InvalidArgument` (0x0040) |
```rust
// the server returns `FencedSample`
```"#,
    );
    let found = prose_codes_in(fixture);
    let expected = [
        "EntityTypeNotInSchema",
        "NotHandshaked",
        "PayloadTooLarge",
        "RetrievalUnavailable",
        "Unauthorized",
        "UnknownOpcode",
    ]
    .into_iter()
    .map(String::from)
    .collect::<BTreeSet<_>>();
    assert_eq!(
        found.keys().cloned().collect::<BTreeSet<_>>(),
        expected,
        "returning, arrow, and hex prose cues"
    );
    for (name, shape) in [
        ("ErrorCategory", "type reference"),
        ("ErrorCategoryWire", "enum container"),
        ("ErrorDetails", "type reference"),
        ("MemoryId", "type reference"),
        ("Soft", "enum-variant reference"),
        ("Hard", "enum-variant reference"),
        ("BadStream", "denial"),
        ("StreamLimitExceeded", "contrast"),
        ("TooManyStreams", "contrast"),
        ("InvalidArgument", "table row"),
        ("FencedSample", "fenced code"),
    ] {
        assert!(
            !found.contains_key(name),
            "{shape} should be ignored: {name}"
        );
    }
}
