//! User-domain renderers. Admin-domain renderers (shard health,
//! snapshot manifest, etc.) live in brain-cli — see
//! .claude/plans/brain-explore-ui-layer.md §4.3.1.
//!
//! Each submodule implements [`crate::Render`] for one user-domain
//! response shape (encode result, recall hit list, plan steps,
//! subscription event, …). The rendering primitives in `theme`, `term`,
//! `table`, and `util` give every renderer the same color, hyperlink,
//! and truncation behaviour so the two consumer binaries (brain-shell
//! and brain-cli) render identically.

pub mod adhoc;
pub mod audit_card;
pub mod banner;
pub mod encode;
pub mod entity_card;
pub mod error;
pub mod forget;
pub mod graph_tree;
pub mod help;
pub mod info;
pub mod link;
pub mod memory;
pub mod plan;
pub mod reason;
pub mod relation_card;
pub mod statement_card;
pub mod subscribe;
pub mod txn;

pub use adhoc::AdHocTable;
pub use audit_card::{AuditCard, TierOutcome};
pub use banner::{BannerAgentSource, WelcomeBanner};
pub use encode::{
    AutoEdgeSummary, AutoEdgesDelta, EncodeRendered, StageKindLabel, StageOutcomeLabel,
    StageResult, StageResultsDelta,
};
pub use entity_card::{EntityCard, MemorySummary, RelationSummary, StatementSummary};
pub use error::RenderableError;
pub use graph_tree::{GraphNode, GraphTree};
pub use help::{
    HelpFlagRow, HelpItem, HelpReference, HelpSection, HelpTopLevel, HelpUnknown, HelpVerb,
};
pub use info::{AgentInfo, ConnectionInfo, InfoCard, ServerInfo, ServerWelcomeFields, SessionInfo};
pub use link::{LinkRendered, UnlinkRendered};
pub use memory::RecallResults;
pub use plan::PlanSteps;
pub use reason::ReasonSteps;
pub use relation_card::{EntityRef, RelationCard};
pub use statement_card::{ObjectRef, StatementCard};
pub use subscribe::{SubscriptionEventList, SubscriptionEventRendered};
pub use txn::{TxnAbortRendered, TxnBeginRendered, TxnCommitRendered};

use brain_core::MemoryId;

use crate::util::humanize::humanize_age;

// ─── id formatters shared across renderers ──────────────────────

/// Full `0x` + 32 hex form of a [`brain_protocol::envelope::request::WireMemoryId`].
/// Used in JSON output where a tool wants the canonical id.
#[must_use]
pub fn fmt_id(raw: u128) -> String {
    format!("0x{raw:032x}")
}

/// Compact `s{shard}/m{slot}/v{version}` form for table rendering.
#[must_use]
pub fn fmt_short_id(raw: u128) -> String {
    let id = MemoryId::from_be_bytes(raw.to_be_bytes());
    format!("s{}/m{}/v{}", id.shard(), id.slot(), id.version())
}

/// First 4 hex chars + `…`. Used for agent_id and model fingerprints
/// in compact views where the full form would dominate the line.
#[must_use]
pub fn fmt_short_hex_16(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}…",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// `0x` + 32 hex chars. Used in JSON output so scripts can grep
/// without parsing rkyv.
#[must_use]
pub fn fmt_hex_16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 32 hex chars, no `0x` prefix. Bare-bytes form for opaque
/// fingerprints (BLAKE3-truncated, content hashes, etc.) where the
/// `0x` is just visual noise — operators copying the value into a
/// log search want exactly the same bytes that go on the wire.
#[must_use]
pub fn fmt_hex_16_bare(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Canonical UUID 8-4-4-4-12 dashed form: `01927a8b-4c2f-7000-8000-deadbeeffeed`.
/// Use for `AgentId` / any other field that's logically a UUID — the
/// dashes are the standard, scripts and humans both parse this shape.
/// Skip `0x`-prefixed `fmt_hex_16` for these: the prefix is wrong for
/// UUIDs and tools that interpret UUIDs reject it.
#[must_use]
pub fn fmt_uuid(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    let pairs = [(0, 4), (4, 6), (6, 8), (8, 10), (10, 16)];
    for (i, (start, end)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push('-');
        }
        for b in &bytes[*start..*end] {
            s.push_str(&format!("{b:02x}"));
        }
    }
    s
}

#[must_use]
pub fn fmt_kind(k: brain_protocol::envelope::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::envelope::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::envelope::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::envelope::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

#[must_use]
pub fn fmt_edge_kind(k: brain_protocol::envelope::request::EdgeKindWire) -> &'static str {
    match k {
        brain_protocol::envelope::request::EdgeKindWire::Caused => "Caused",
        brain_protocol::envelope::request::EdgeKindWire::FollowedBy => "FollowedBy",
        brain_protocol::envelope::request::EdgeKindWire::DerivedFrom => "DerivedFrom",
        brain_protocol::envelope::request::EdgeKindWire::SimilarTo => "SimilarTo",
        brain_protocol::envelope::request::EdgeKindWire::Contradicts => "Contradicts",
        brain_protocol::envelope::request::EdgeKindWire::Supports => "Supports",
        brain_protocol::envelope::request::EdgeKindWire::References => "References",
        brain_protocol::envelope::request::EdgeKindWire::PartOf => "PartOf",
    }
}

/// Format a 16-byte transaction id as `0x…` hex (canonical wire form).
#[must_use]
pub fn fmt_txn_id(bytes: &[u8; 16]) -> String {
    fmt_hex_16(bytes)
}

/// 16 bytes rendered as four 8-hex-char chunks joined by ` · `.
///
/// A 32-character hex run is visually unreadable; chunking lets the
/// eye anchor on the first chunk (which is what most operators copy
/// when they're sanity-checking a fingerprint against a model
/// directory). The dot separator stays grep-friendly — operators
/// copy-paste the whole string into a log search and the chunks
/// rejoin transparently.
///
/// Example: `e541b06c · d9f93744 · 389938c8 · 34997bf4`
#[must_use]
pub fn fmt_hex_16_chunked_dot(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(35); // 4*8 hex + 3*3 separators
    for chunk_idx in 0..4 {
        if chunk_idx > 0 {
            s.push_str(" · ");
        }
        let base = chunk_idx * 4;
        for b in &bytes[base..base + 4] {
            s.push_str(&format!("{b:02x}"));
        }
    }
    s
}

// ─── time formatters shared across renderers ───────────────────

/// Format a unix-nanos timestamp as raw-primary with human-readable
/// form in brackets. Returns:
///
/// ```text
/// "<nanos> unix-nanos (<RFC3339>, <relative>)"
/// ```
///
/// Used by every renderer that displays a stored timestamp — keeps
/// the format consistent across encode / subscribe / audit / info /
/// entity / statement.
///
/// Edge cases:
/// - `nanos == 0` → `"0 unix-nanos (unknown)"`; some wire paths use
///   `0` as a "not set" sentinel and there's no useful RFC3339 to
///   derive.
/// - nanos from the future → still renders RFC3339 + "in N seconds"
///   form; we don't special-case (clock skew is rare enough that we
///   prefer letting the operator see the anomaly).
#[must_use]
pub fn fmt_time(nanos: u64) -> String {
    if nanos == 0 {
        return "0 unix-nanos (unknown)".to_string();
    }
    let rfc = rfc3339_from_nanos(nanos);
    let rel = relative_age_from_nanos(nanos);
    format!("{nanos} unix-nanos ({rfc}, {rel})")
}

/// Same as [`fmt_time`] but with an extra clarifying note inside the
/// brackets. Use for row-specific context like `"dedup hit time"` or
/// `"+12 ms clock skew"` that belongs with the timestamp itself,
/// not as a trailing comment that an operator might miss when
/// skimming.
#[must_use]
pub fn fmt_time_with_note(nanos: u64, note: &str) -> String {
    if nanos == 0 {
        return format!("0 unix-nanos (unknown, {note})");
    }
    let rfc = rfc3339_from_nanos(nanos);
    let rel = relative_age_from_nanos(nanos);
    format!("{nanos} unix-nanos ({rfc}, {rel}, {note})")
}

/// For values ALREADY in RFC3339 form (e.g. config-stored strings
/// like agent `created_at`). Appends the relative age in brackets:
///
/// ```text
/// "<rfc3339> (<relative>)"
/// ```
///
/// If the input doesn't parse as RFC3339, returns it unchanged.
/// The renderer never gates on a parse; a one-off malformed value
/// shouldn't blank out a whole row.
#[must_use]
pub fn fmt_time_rfc3339_with_age(rfc3339: &str) -> String {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    let Ok(dt) = OffsetDateTime::parse(rfc3339, &Rfc3339) else {
        return rfc3339.to_string();
    };
    // OffsetDateTime → unix nanos. Clamp negatives (pre-1970) and
    // overflow (post-2554, which won't ship a Brain deployment) to
    // an effective "unknown" rendering.
    let nanos_i128 = dt.unix_timestamp_nanos();
    let Ok(nanos) = u64::try_from(nanos_i128) else {
        return rfc3339.to_string();
    };
    let rel = relative_age_from_nanos(nanos);
    format!("{rfc3339} ({rel})")
}

/// Convert a unix-nanos value to an RFC3339 string. Internal helper:
/// the public surface is `fmt_time*`. Returns `"unknown"` on overflow
/// (we'd rather render that than panic from an `.expect`).
fn rfc3339_from_nanos(nanos: u64) -> String {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    let Ok(dt) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)) else {
        return "unknown".to_string();
    };
    dt.format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Compute a relative-age phrase like `"~4 min ago"` for a unix-nanos
/// timestamp. Wraps [`humanize_age`] and prefixes with `~` to convey
/// "approximate"; the raw nanos in the same row carry the precise
/// value for anyone who needs it.
fn relative_age_from_nanos(nanos: u64) -> String {
    let age = humanize_age(nanos);
    if age == "just now" {
        // "just now" already reads as approximate; the `~` prefix
        // would just add noise.
        age
    } else {
        format!("~{age}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_hex_16_chunked_dot_returns_four_dot_separated_chunks() {
        let bytes: [u8; 16] = [
            0xe5, 0x41, 0xb0, 0x6c, 0xd9, 0xf9, 0x37, 0x44, 0x38, 0x99, 0x38, 0xc8, 0x34, 0x99,
            0x7b, 0xf4,
        ];
        let s = fmt_hex_16_chunked_dot(&bytes);
        assert_eq!(s, "e541b06c · d9f93744 · 389938c8 · 34997bf4");
        assert_eq!(s.matches(" · ").count(), 3, "expected exactly 3 separators");
    }

    #[test]
    fn fmt_time_emits_raw_then_brackets() {
        let s = fmt_time(1_700_000_000_000_000_000);
        assert!(s.starts_with("1700000000000000000 unix-nanos ("));
        assert!(s.contains("2023-11-"), "expected RFC3339 prefix: {s}");
        assert!(s.ends_with(" ago)"), "expected relative-age suffix: {s}");
    }

    #[test]
    fn fmt_time_zero_emits_unknown() {
        assert_eq!(fmt_time(0), "0 unix-nanos (unknown)");
    }

    #[test]
    fn fmt_time_with_note_includes_note_in_brackets() {
        let s = fmt_time_with_note(1_700_000_000_000_000_000, "dedup hit time");
        assert!(
            s.contains("dedup hit time)"),
            "note must land inside the brackets: {s}"
        );
        assert!(
            s.starts_with("1700000000000000000 unix-nanos ("),
            "still raw-primary: {s}"
        );
    }

    #[test]
    fn fmt_time_rfc3339_with_age_round_trips() {
        // Use a value old enough that relative-age math is deterministic
        // ("years ago" → days bucket → "N d ago").
        let s = fmt_time_rfc3339_with_age("2023-11-14T22:13:20Z");
        assert!(
            s.starts_with("2023-11-14T22:13:20Z ("),
            "must keep original prefix: {s}"
        );
        assert!(s.ends_with(" ago)"), "must end with relative-age: {s}");
    }

    #[test]
    fn fmt_time_rfc3339_with_age_invalid_input_passthrough() {
        assert_eq!(
            fmt_time_rfc3339_with_age("not-a-timestamp"),
            "not-a-timestamp"
        );
    }
}
