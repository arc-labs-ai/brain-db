//! Hand-rolled column-aligning table renderer plus [`Render`]
//! impls for every response type.

use std::io::{self, Write};

use brain_core::MemoryId;
use brain_protocol::response::{
    EncodeResponse, ForgetResponse, InferenceStep, LinkResponse, MemoryResult, PlanStatus,
    PlanStep, SubscriptionEvent, TxnAbortResponse, TxnBeginResponse, TxnCommitResponse,
    UnlinkResponse,
};
use serde_json::{json, Value};

use crate::parser::format_txn_id;

use super::Render;

/// Default terminal width when we can't probe (we don't add a dep
/// to detect it). Wide enough for most cases without truncating
/// memory ids or scores.
pub const DEFAULT_WIDTH: usize = 100;

// ─── pure-helper renderers ──────────────────────────────────────

/// Render a list of rows (each a Vec of cell strings) under the
/// given headers, aligning columns and drawing a light box.
pub fn render_rows(
    w: &mut dyn Write,
    headers: &[&str],
    rows: &[Vec<String>],
    max_width: usize,
) -> io::Result<()> {
    if rows.is_empty() {
        writeln!(w, "(no rows)")?;
        return Ok(());
    }
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            let w = display_width(cell);
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    // Reserve `2 + 3*(cols-1) + 2` for borders/separators. Then cap
    // the last column so total width <= max_width.
    let chrome = 2 + 3 * cols.saturating_sub(1) + 2;
    let used: usize = widths.iter().sum::<usize>() + chrome;
    if used > max_width && cols > 0 {
        let last = cols - 1;
        let slack = used - max_width;
        if widths[last] > slack {
            widths[last] -= slack;
        } else {
            widths[last] = 8;
        }
    }

    let sep = build_border(&widths, '┬', '─', '┌', '┐');
    let mid = build_border(&widths, '┼', '─', '├', '┤');
    let bot = build_border(&widths, '┴', '─', '└', '┘');

    writeln!(w, "{sep}")?;
    write_row(w, headers, &widths)?;
    writeln!(w, "{mid}")?;
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| truncate_to(c, widths[i]))
            .collect();
        let cell_refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        write_row(w, &cell_refs, &widths)?;
    }
    writeln!(w, "{bot}")?;
    Ok(())
}

fn build_border(widths: &[usize], joiner: char, fill: char, left: char, right: char) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(joiner);
        }
        for _ in 0..(w + 2) {
            s.push(fill);
        }
    }
    s.push(right);
    s
}

fn write_row(w: &mut dyn Write, cells: &[&str], widths: &[usize]) -> io::Result<()> {
    write!(w, "│")?;
    for (i, cell) in cells.iter().enumerate() {
        let pad = widths[i].saturating_sub(display_width(cell));
        write!(w, " {}{} │", cell, " ".repeat(pad))?;
    }
    writeln!(w)
}

/// Truncate `s` so its displayed width is at most `max`, appending
/// `…` if anything was elided.
fn truncate_to(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Approximate "display width" — we don't depend on `unicode-width`
/// so this is a simple char count, treating control chars as 0 and
/// everything else as 1.
fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    if ch.is_control() {
        0
    } else {
        1
    }
}

/// Full `0x` + 32 hex form of a MemoryId. Used in JSON output and
/// anywhere a tool wants the canonical id (e.g. forget/link
/// arguments). Tables use [`fmt_short_id`] instead.
fn fmt_id(raw: u128) -> String {
    format!("0x{:032x}", raw)
}

/// Compact, human-scannable MemoryId for table rendering:
/// `s{shard}/m{slot}/v{version}`. Encodes the same info as the
/// full 32-hex form in ~10 chars instead of 34. The full form
/// remains available via JSON output and `fmt_id`.
fn fmt_short_id(raw: u128) -> String {
    let id = MemoryId::from_be_bytes(raw.to_be_bytes());
    format!("s{}/m{}/v{}", id.shard(), id.slot(), id.version())
}

/// First 4 hex chars + `…` for a 16-byte array. Used for agent_id
/// and model fingerprints in compact views where the full 32-char
/// form would dominate the line.
fn fmt_short_hex_16(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}…",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// `0x` prefix + 32 hex chars for a 16-byte array (UUID-shaped). Used
/// for `agent_id` and `embedding_model_fp` in JSON output so a script
/// can grep without parsing rkyv.
fn fmt_hex_16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn fmt_memory_id(id: MemoryId) -> String {
    fmt_id(id.raw())
}

fn fmt_kind(k: brain_protocol::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

fn fmt_edge_kind(k: brain_protocol::request::EdgeKindWire) -> &'static str {
    match k {
        brain_protocol::request::EdgeKindWire::Caused => "Caused",
        brain_protocol::request::EdgeKindWire::FollowedBy => "FollowedBy",
        brain_protocol::request::EdgeKindWire::DerivedFrom => "DerivedFrom",
        brain_protocol::request::EdgeKindWire::SimilarTo => "SimilarTo",
        brain_protocol::request::EdgeKindWire::Contradicts => "Contradicts",
        brain_protocol::request::EdgeKindWire::Supports => "Supports",
        brain_protocol::request::EdgeKindWire::References => "References",
        brain_protocol::request::EdgeKindWire::PartOf => "PartOf",
    }
}

// ─── Render impls ───────────────────────────────────────────────

/// Render-time wrapper around [`EncodeResponse`] that carries the
/// request's `deduplicate` flag, so we can distinguish three states
/// instead of the misleading raw `was_deduplicated` boolean:
///
/// - **`off`**  — caller did not pass `--deduplicate`. The server
///   never consulted the fingerprint table; this is a fresh slot.
///   Sibling memories with identical text are also `off`.
/// - **`hit`**  — caller asked for dedup AND the server found a
///   matching fingerprint, so it returned the existing memory id
///   instead of allocating a new slot. `was_deduplicated = true`.
/// - **`miss`** — caller asked for dedup, no existing match, fresh
///   slot allocated. `was_deduplicated = false`.
///
/// The raw `was_deduplicated` bool is still emitted in the JSON
/// envelope for scripts that already parse it.
pub struct EncodeRendered {
    pub response: EncodeResponse,
    pub dedup_requested: bool,
}

impl EncodeRendered {
    fn dedup_state(&self) -> &'static str {
        match (self.dedup_requested, self.response.was_deduplicated) {
            (false, _) => "off",
            (true, true) => "hit",
            (true, false) => "miss",
        }
    }
}

impl Render for EncodeRendered {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.response;
        // First line — outcome + the two ids the caller most likely
        // wants to chain off: memory_id (for LINK/FORGET/RECALL by
        // id) and lsn (for `subscribe --start-lsn lsn+1`).
        writeln!(w, "ok  {}  lsn={}", fmt_short_id(r.memory_id), r.lsn)?;
        // Second line — provenance + classification. Indented so
        // the eye groups it as a continuation of the first line.
        // `dedup=off` is suppressed (the common case) to reduce
        // noise; `hit`/`miss` are surfaced.
        let kind = format!("{:?}", r.kind).to_lowercase();
        let mut parts: Vec<String> = vec![
            format!("agent={}", fmt_short_hex_16(&r.agent_id)),
            format!("ctx={}", r.context_id),
            kind,
            format!("sal={:.3}", r.salience),
        ];
        if r.edges_out_count > 0 {
            parts.push(format!("edges_out={}", r.edges_out_count));
        }
        let dedup = self.dedup_state();
        if dedup != "off" {
            parts.push(format!("dedup={dedup}"));
        }
        parts.push(format!("fp={}", fmt_short_hex_16(&r.embedding_model_fp)));
        writeln!(w, "    {}", parts.join(" · "))
    }

    fn to_json_value(&self) -> Value {
        let r = &self.response;
        json!({
            "memory_id": fmt_id(r.memory_id),
            "lsn": r.lsn,
            "dedup": self.dedup_state(),
            "was_deduplicated": r.was_deduplicated,
            "salience": r.salience,
            "auto_edges_added": r.auto_edges_added,
            "agent_id": fmt_hex_16(&r.agent_id),
            "context_id": r.context_id,
            "kind": format!("{:?}", r.kind),
            "created_at_unix_nanos": r.created_at_unix_nanos,
            "edges_out_count": r.edges_out_count,
            "embedding_model_fp": fmt_hex_16(&r.embedding_model_fp),
        })
    }
}

/// Newtype so we can `impl Render for Vec<MemoryResult>` without
/// running into the orphan rule (Vec is not local).
pub struct RecallResults(pub Vec<MemoryResult>);

impl Render for RecallResults {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        // Two-line per result. Line 1 is rank + id + classification
        // + decay/access/edges/score; line 2 is the full text on its
        // own row so it can breathe (the most useful column in the
        // common `--include-text` case). A blank line separates
        // results. Trailing footer surfaces a cluster warning when
        // every top-K score is within a tight epsilon — that's the
        // "your scores aren't actually ranking anything" signal.
        let results = &self.0;
        if results.is_empty() {
            return writeln!(w, "(no results)");
        }
        for (idx, r) in results.iter().enumerate() {
            // ── Line 1: rank + identity + metadata ────────────────
            let kind_str = if r.consolidated_at_unix_nanos.is_some() {
                // † marks consolidated rows so a reader spots the
                // semantic difference from raw memories at a glance.
                format!("{}†", fmt_kind(r.kind))
            } else {
                fmt_kind(r.kind).to_string()
            };
            let salience = if (r.salience - r.salience_initial).abs() < 0.001 {
                format!("sal={:.3}", r.salience)
            } else {
                // ↓ when decayed, ↑ when boosted. Inline shows the
                // delta without an extra column.
                let arrow = if r.salience < r.salience_initial {
                    "↓"
                } else {
                    "↑"
                };
                format!("sal={:.3}{arrow}{:.3}", r.salience, r.salience_initial)
            };
            let mut meta: Vec<String> = vec![
                fmt_short_id(r.memory_id),
                kind_str,
                format!("ctx={}", r.context_id),
                salience,
                format!("score={:.4}", r.similarity_score),
            ];
            // Surface access_count + edge counts only when non-zero —
            // a fresh shard has all zeros and the noise hurts more
            // than the missing info helps.
            if r.access_count > 0 {
                meta.push(format!("acc={}", r.access_count));
            }
            if r.edges_in_count > 0 || r.edges_out_count > 0 {
                meta.push(format!(
                    "edges={}in/{}out",
                    r.edges_in_count, r.edges_out_count
                ));
            }
            writeln!(w, "#{}  {}", idx + 1, meta.join("  "))?;
            // ── Line 2: text, indented to align under the metadata.
            // Empty text (no --include-text) just prints "(text not
            // fetched — re-run with --include-text)" so the user
            // doesn't think the memory is empty.
            if r.text.is_empty() {
                writeln!(w, "    (text not fetched — re-run with --include-text)")?;
            } else {
                writeln!(w, "    {}", r.text)?;
            }
            // Blank line between results for visual separation.
            if idx + 1 < results.len() {
                writeln!(w)?;
            }
        }
        // ── Footer: result count + uninformative-ranking signal.
        // "Tightly clustered" = the spread between the top and
        // bottom score in this page is less than 0.001. That
        // condition reliably fires on Nop embedders and on queries
        // that genuinely don't discriminate; either way the user
        // should know the ranking is suspect.
        writeln!(w)?;
        let n = results.len();
        let score_spread = {
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            for r in results {
                if r.similarity_score < min {
                    min = r.similarity_score;
                }
                if r.similarity_score > max {
                    max = r.similarity_score;
                }
            }
            max - min
        };
        if n >= 2 && score_spread < 0.001 {
            writeln!(
                w,
                "{n} results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful"
            )
        } else {
            writeln!(w, "{n} results")
        }
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .0
            .iter()
            .map(|r| {
                json!({
                    "memory_id": fmt_id(r.memory_id),
                    "similarity_score": r.similarity_score,
                    "confidence": r.confidence,
                    "salience": r.salience,
                    "salience_initial": r.salience_initial,
                    "access_count": r.access_count,
                    "lsn": r.lsn,
                    "flags": r.flags,
                    "kind": fmt_kind(r.kind),
                    "context_id": r.context_id,
                    "created_at_unix_nanos": r.created_at_unix_nanos,
                    "last_accessed_at_unix_nanos": r.last_accessed_at_unix_nanos,
                    "consolidated_at_unix_nanos": r.consolidated_at_unix_nanos,
                    "edges_out_count": r.edges_out_count,
                    "edges_in_count": r.edges_in_count,
                    "fused_score": r.fused_score,
                    "text": r.text,
                })
            })
            .collect();
        Value::Array(items)
    }
}

impl Render for LinkResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  {} --[{}]--> {}  weight={:.4}  already_existed={}",
            fmt_id(self.source),
            fmt_edge_kind(self.kind),
            fmt_id(self.target),
            self.weight,
            self.already_existed,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "source": fmt_id(self.source),
            "target": fmt_id(self.target),
            "kind": fmt_edge_kind(self.kind),
            "weight": self.weight,
            "created_at_unix_nanos": self.created_at_unix_nanos,
            "already_existed": self.already_existed,
        })
    }
}

impl Render for UnlinkResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  {} --[{}]--> {}  removed={}",
            fmt_id(self.source),
            fmt_edge_kind(self.kind),
            fmt_id(self.target),
            self.removed,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "source": fmt_id(self.source),
            "target": fmt_id(self.target),
            "kind": fmt_edge_kind(self.kind),
            "removed": self.removed,
        })
    }
}

impl Render for ForgetResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  memory_id={}  was_already_forgotten={}  edges_removed={}",
            fmt_id(self.memory_id),
            self.was_already_forgotten,
            self.edges_removed,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "memory_id": fmt_id(self.memory_id),
            "was_already_forgotten": self.was_already_forgotten,
            "edges_removed": self.edges_removed,
        })
    }
}

pub struct PlanSteps {
    pub steps: Vec<PlanStep>,
    pub status: Option<PlanStatus>,
}

impl Render for PlanSteps {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let headers = ["step", "id", "transition", "conf", "remaining", "text"];
        let rows: Vec<Vec<String>> = self
            .steps
            .iter()
            .map(|s| {
                vec![
                    s.step_index.to_string(),
                    fmt_id(s.memory_id),
                    format!("{:?}", s.transition_kind),
                    format!("{:.4}", s.confidence),
                    format!("{:.4}", s.estimated_distance_to_goal),
                    s.text.clone(),
                ]
            })
            .collect();
        render_rows(w, &headers, &rows, DEFAULT_WIDTH)?;
        if let Some(footer) = plan_status_footer(self.status, self.steps.len()) {
            writeln!(w, "{footer}")?;
        }
        Ok(())
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .steps
            .iter()
            .map(|s| {
                json!({
                    "step_index": s.step_index,
                    "memory_id": fmt_id(s.memory_id),
                    "transition_kind": format!("{:?}", s.transition_kind),
                    "confidence": s.confidence,
                    "estimated_distance_to_goal": s.estimated_distance_to_goal,
                    "text": s.text,
                })
            })
            .collect();
        json!({
            "steps": Value::Array(items),
            "status": self.status.map(fmt_plan_status_json),
        })
    }
}

fn fmt_plan_status_json(s: PlanStatus) -> Value {
    Value::String(
        match s {
            PlanStatus::GoalReached => "GoalReached",
            PlanStatus::BudgetExhausted => "BudgetExhausted",
            PlanStatus::NoPathFound => "NoPathFound",
            PlanStatus::Cancelled => "Cancelled",
        }
        .to_owned(),
    )
}

/// Footer line for the plan table when the status tells the user
/// something the steps alone don't. `GoalReached` is silent because
/// the rendered rows already show the goal was reached. The other
/// variants carry a one-line reason.
fn plan_status_footer(status: Option<PlanStatus>, n_steps: usize) -> Option<String> {
    let status = status?;
    match status {
        PlanStatus::GoalReached => None,
        PlanStatus::NoPathFound => Some(format!(
            "(NoPathFound — no path between the start and goal within the index{})",
            if n_steps <= 1 {
                "; only the start endpoint surfaced"
            } else {
                ""
            },
        )),
        PlanStatus::BudgetExhausted => {
            Some("(BudgetExhausted — try a larger --max-steps or --max-wall-time-ms)".to_owned())
        }
        PlanStatus::Cancelled => Some("(Cancelled)".to_owned()),
    }
}

pub struct ReasonSteps(pub Vec<InferenceStep>);

impl Render for ReasonSteps {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let headers = ["step", "kind", "conf", "supports", "contradicts", "claim"];
        let rows: Vec<Vec<String>> = self
            .0
            .iter()
            .map(|s| {
                vec![
                    s.step_index.to_string(),
                    format!("{:?}", s.inference_kind),
                    format!("{:.4}", s.confidence),
                    s.supporting_memories.len().to_string(),
                    s.contradicting_memories.len().to_string(),
                    s.claim.clone(),
                ]
            })
            .collect();
        render_rows(w, &headers, &rows, DEFAULT_WIDTH)
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .0
            .iter()
            .map(|s| {
                json!({
                    "step_index": s.step_index,
                    "inference_kind": format!("{:?}", s.inference_kind),
                    "claim": s.claim,
                    "confidence": s.confidence,
                    "supporting_memories": s.supporting_memories.iter().map(|m| fmt_id(*m)).collect::<Vec<_>>(),
                    "contradicting_memories": s.contradicting_memories.iter().map(|m| fmt_id(*m)).collect::<Vec<_>>(),
                })
            })
            .collect();
        Value::Array(items)
    }
}

impl Render for TxnBeginResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  txn_id={}  timeout_seconds={}",
            format_txn_id(&self.txn_id),
            self.timeout_seconds,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "txn_id": format_txn_id(&self.txn_id),
            "timeout_seconds": self.timeout_seconds,
            "started_at_unix_nanos": self.started_at_unix_nanos,
        })
    }
}

impl Render for TxnCommitResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  txn_id={}  operations_applied={}",
            format_txn_id(&self.txn_id),
            self.operations_applied,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "txn_id": format_txn_id(&self.txn_id),
            "operations_applied": self.operations_applied,
            "committed_at_unix_nanos": self.committed_at_unix_nanos,
        })
    }
}

impl Render for TxnAbortResponse {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "ok  txn_id={}  operations_discarded={}",
            format_txn_id(&self.txn_id),
            self.operations_discarded,
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "txn_id": format_txn_id(&self.txn_id),
            "operations_discarded": self.operations_discarded,
        })
    }
}

pub struct SubscriptionEvents(pub Vec<SubscriptionEvent>);

impl Render for SubscriptionEvents {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let headers = ["lsn", "type", "id", "ctx", "kind", "text"];
        let rows: Vec<Vec<String>> = self
            .0
            .iter()
            .map(|e| {
                vec![
                    e.lsn.to_string(),
                    format!("{:?}", e.event_type),
                    fmt_id(e.memory_id),
                    e.context_id.to_string(),
                    fmt_kind(e.kind).to_string(),
                    e.text.clone(),
                ]
            })
            .collect();
        render_rows(w, &headers, &rows, DEFAULT_WIDTH)
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .0
            .iter()
            .map(|e| {
                json!({
                    "lsn": e.lsn,
                    "event_type": format!("{:?}", e.event_type),
                    "memory_id": fmt_id(e.memory_id),
                    "context_id": e.context_id,
                    "kind": fmt_kind(e.kind),
                    "salience": e.salience,
                    "timestamp_unix_nanos": e.timestamp_unix_nanos,
                    "text": e.text,
                })
            })
            .collect();
        Value::Array(items)
    }
}

// Re-export the typed accessor so callers can drop a `MemoryId`
// in without juggling raw conversions.
#[allow(dead_code)]
fn _unused_use_of_fmt_memory_id() -> String {
    fmt_memory_id(MemoryId::NULL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_is_identity() {
        assert_eq!(truncate_to("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let s = truncate_to("hello world", 8);
        assert!(s.ends_with('…'));
        assert!(display_width(&s) <= 8);
    }

    #[test]
    fn truncate_tiny_max_returns_ellipsis() {
        assert_eq!(truncate_to("hello", 1), "…");
    }

    #[test]
    fn fmt_id_pads_to_32_hex() {
        let s = fmt_id(0x10001000100000000u128);
        assert_eq!(s.len(), 2 + 32);
        assert!(s.starts_with("0x"));
    }

    #[test]
    fn render_rows_handles_empty() {
        let mut buf = Vec::new();
        render_rows(&mut buf, &["a", "b"], &[], 80).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "(no rows)\n");
    }

    #[test]
    fn render_rows_aligns_columns() {
        let mut buf = Vec::new();
        let rows = vec![
            vec!["alpha".to_string(), "1".to_string()],
            vec!["beta".to_string(), "200".to_string()],
        ];
        render_rows(&mut buf, &["name", "val"], &rows, 80).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Headers + 2 rows + 3 borders = 6 lines.
        assert_eq!(s.matches('\n').count(), 6);
        assert!(s.contains("name"));
        assert!(s.contains("alpha"));
        assert!(s.contains("200"));
    }

    #[test]
    fn render_rows_truncates_last_column_when_overflow() {
        let mut buf = Vec::new();
        let long = "x".repeat(200);
        let rows = vec![vec!["id".to_string(), long]];
        render_rows(&mut buf, &["k", "v"], &rows, 40).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Each line should be roughly <= 40 + newline.
        for line in s.lines() {
            assert!(line.chars().count() <= 50, "line too long: {line:?}");
        }
        assert!(s.contains('…'));
    }

    // ─── plan_status_footer ─────────────────────────────────────────

    #[test]
    fn plan_footer_silent_on_goal_reached() {
        assert!(plan_status_footer(Some(PlanStatus::GoalReached), 3).is_none());
        // None-status (server didn't classify) is also silent — we
        // don't make up a status the wire didn't carry.
        assert!(plan_status_footer(None, 3).is_none());
    }

    #[test]
    fn plan_footer_explains_no_path_found() {
        let f = plan_status_footer(Some(PlanStatus::NoPathFound), 1).expect("footer present");
        assert!(f.starts_with("(NoPathFound"));
        assert!(
            f.contains("only the start endpoint surfaced"),
            "1-step NoPathFound should explain the lone Initial row: {f}",
        );

        let f2 = plan_status_footer(Some(PlanStatus::NoPathFound), 5).expect("footer present");
        assert!(f2.starts_with("(NoPathFound"));
        assert!(
            !f2.contains("only the start endpoint"),
            ">1 step shouldn't include the single-row footnote: {f2}",
        );
    }

    #[test]
    fn plan_footer_explains_budget_exhausted() {
        let f = plan_status_footer(Some(PlanStatus::BudgetExhausted), 0).expect("footer present");
        assert!(f.contains("BudgetExhausted"));
        assert!(
            f.contains("--max-steps") || f.contains("--max-wall-time-ms"),
            "should hint at the knob to bump: {f}",
        );
    }

    #[test]
    fn plan_footer_marks_cancelled() {
        let f = plan_status_footer(Some(PlanStatus::Cancelled), 2).expect("footer present");
        assert!(f.contains("Cancelled"));
    }

    #[test]
    fn plan_render_emits_footer_below_table() {
        let mut buf = Vec::new();
        let p = PlanSteps {
            steps: vec![PlanStep {
                step_index: 0,
                memory_id: 0x1234,
                text: "start".into(),
                transition_kind: brain_protocol::response::TransitionKind::Initial,
                confidence: 1.0,
                estimated_distance_to_goal: 0.0,
            }],
            status: Some(PlanStatus::NoPathFound),
        };
        p.render_table(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("NoPathFound"),
            "footer must appear in rendered output:\n{out}"
        );
        // Footer must sit AFTER the table's bottom border line.
        let bottom_idx = out.rfind('└').expect("bottom border");
        let footer_idx = out.find("NoPathFound").expect("footer");
        assert!(
            footer_idx > bottom_idx,
            "footer must render below the table",
        );
    }
}
