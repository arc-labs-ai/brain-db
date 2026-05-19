//! Tabular renderers built on `comfy_table`.
//!
//! Why comfy_table: per-cell wrap, multi-line cells, terminal-width
//! auto-fit, alignment, borders, and color all in one dependency.
//! Replaces the hand-rolled box-drawing renderer that didn't wrap,
//! didn't color, and didn't detect width.
//!
//! Every public response type implements [`Render`] (in `super`).
//! `render_table` builds a `comfy_table::Table` and writes its
//! `to_string()`; the table layout is responsible for honoring terminal
//! width.

use std::io::{self, Write};

use brain_core::MemoryId;
use brain_protocol::response::{
    EncodeResponse, ForgetResponse, InferenceStep, LinkResponse, MemoryResult, PlanStatus,
    PlanStep, SubscriptionEvent, TxnAbortResponse, TxnBeginResponse, TxnCommitResponse,
    UnlinkResponse,
};
use comfy_table::{Cell, ContentArrangement, Row, Table};
use owo_colors::OwoColorize;
use serde_json::{json, Value};

use crate::output::term::TermPolicy;
use crate::parser::format_txn_id;

use super::Render;

// ─── shared formatters ──────────────────────────────────────────

/// Build the standard comfy_table base — dynamic content arrangement,
/// terminal-width adaptive, no internal borders so output stays scannable
/// in the common case.
fn build_table(policy: TermPolicy) -> Table {
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(u16::try_from(policy.width.max(40)).unwrap_or(u16::MAX));
    table
}

/// Full `0x` + 32 hex form of a MemoryId. Used in JSON output and
/// anywhere a tool wants the canonical id.
pub(crate) fn fmt_id(raw: u128) -> String {
    format!("0x{:032x}", raw)
}

/// Compact `s{shard}/m{slot}/v{version}` form for table rendering.
pub(crate) fn fmt_short_id(raw: u128) -> String {
    let id = MemoryId::from_be_bytes(raw.to_be_bytes());
    format!("s{}/m{}/v{}", id.shard(), id.slot(), id.version())
}

/// First 4 hex chars + `…`. Used for agent_id and model fingerprints
/// in compact views where the full form would dominate the line.
pub(crate) fn fmt_short_hex_16(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}…",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// `0x` + 32 hex chars. Used in JSON output so scripts can grep
/// without parsing rkyv.
pub(crate) fn fmt_hex_16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub(crate) fn fmt_kind(k: brain_protocol::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

pub(crate) fn fmt_edge_kind(k: brain_protocol::request::EdgeKindWire) -> &'static str {
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

/// Middle-truncate `s` so its displayed width is at most `max`. The
/// pattern is `<head>…<tail>` — preserves both the human-recognizable
/// start and the discriminating tail (often where the noun lives in a
/// memory text).
pub(crate) fn middle_truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // 1 char for the ellipsis; rest split between head and tail. Bias
    // a touch towards the head so the "what kind of text" cue lands first.
    let budget = max - 1;
    let head_chars = (budget + 1) / 2;
    let tail_chars = budget - head_chars;
    let head: String = s.chars().take(head_chars).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

// ─── Render impls ───────────────────────────────────────────────

/// Render-time wrapper around [`EncodeResponse`] carrying the request's
/// `deduplicate` flag, so we can distinguish off / hit / miss instead of
/// the misleading raw bool.
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
        let policy = TermPolicy::plain();
        let id_short = fmt_short_id(r.memory_id);
        let id_cell = if policy.color {
            id_short.green().to_string()
        } else {
            id_short
        };
        writeln!(w, "ok  {}  lsn={}", id_cell, r.lsn)?;
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
        let results = &self.0;
        if results.is_empty() {
            return writeln!(w, "(no results)");
        }
        let policy = TermPolicy::plain();
        for (idx, r) in results.iter().enumerate() {
            let kind_str = if r.consolidated_at_unix_nanos.is_some() {
                format!("{}†", fmt_kind(r.kind))
            } else {
                fmt_kind(r.kind).to_string()
            };
            let salience = if (r.salience - r.salience_initial).abs() < 0.001 {
                format!("sal={:.3}", r.salience)
            } else {
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
            if r.text.is_empty() {
                writeln!(w, "    (text not fetched — re-run with --include-text)")?;
            } else {
                // Reserve four chars for the indent + a small margin so
                // long memory text wraps cleanly to the terminal width.
                let max = policy.width.saturating_sub(6);
                writeln!(w, "    {}", middle_truncate(&r.text, max))?;
            }
            if idx + 1 < results.len() {
                writeln!(w)?;
            }
        }
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
        let policy = TermPolicy::plain();
        let mut table = build_table(policy);
        table.set_header(vec![
            "step",
            "id",
            "transition",
            "conf",
            "remaining",
            "text",
        ]);
        for s in &self.steps {
            let mut row = Row::new();
            row.add_cell(Cell::new(s.step_index));
            row.add_cell(Cell::new(fmt_short_id(s.memory_id)));
            row.add_cell(Cell::new(format!("{:?}", s.transition_kind)));
            row.add_cell(Cell::new(format!("{:.4}", s.confidence)));
            row.add_cell(Cell::new(format!("{:.4}", s.estimated_distance_to_goal)));
            row.add_cell(Cell::new(&s.text));
            table.add_row(row);
        }
        writeln!(w, "{table}")?;
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
        let policy = TermPolicy::plain();
        let mut table = build_table(policy);
        table.set_header(vec![
            "step",
            "kind",
            "conf",
            "supports",
            "contradicts",
            "claim",
        ]);
        for s in &self.0 {
            table.add_row(vec![
                Cell::new(s.step_index),
                Cell::new(format!("{:?}", s.inference_kind)),
                Cell::new(format!("{:.4}", s.confidence)),
                Cell::new(s.supporting_memories.len()),
                Cell::new(s.contradicting_memories.len()),
                Cell::new(&s.claim),
            ]);
        }
        writeln!(w, "{table}")
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
        let policy = TermPolicy::plain();
        let mut table = build_table(policy);
        table.set_header(vec!["lsn", "type", "id", "ctx", "kind", "text"]);
        for e in &self.0 {
            table.add_row(vec![
                Cell::new(e.lsn),
                Cell::new(format!("{:?}", e.event_type)),
                Cell::new(fmt_short_id(e.memory_id)),
                Cell::new(e.context_id),
                Cell::new(fmt_kind(e.kind)),
                Cell::new(&e.text),
            ]);
        }
        writeln!(w, "{table}")
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

/// Lightweight wrapper for ad-hoc tables built by knowledge-layer
/// browse commands (entity list / statement list / mention list / …).
/// Keeps the comfy_table dependency local to the table module.
pub struct AdHocTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Render for AdHocTable {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.rows.is_empty() {
            return writeln!(w, "(no rows)");
        }
        let policy = TermPolicy::plain();
        let mut table = build_table(policy);
        table.set_header(
            self.headers
                .iter()
                .map(|h| Cell::new(h))
                .collect::<Vec<_>>(),
        );
        for row in &self.rows {
            table.add_row(row.iter().map(Cell::new).collect::<Vec<_>>());
        }
        writeln!(w, "{table}")?;
        writeln!(w, "{} rows", self.rows.len())
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (h, v) in self.headers.iter().zip(row.iter()) {
                    obj.insert(h.clone(), Value::String(v.clone()));
                }
                Value::Object(obj)
            })
            .collect();
        Value::Array(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_truncate_short_is_identity() {
        assert_eq!(middle_truncate("hello", 10), "hello");
    }

    #[test]
    fn middle_truncate_long_keeps_head_and_tail() {
        let s = middle_truncate("the quick brown fox jumps over the lazy dog", 11);
        assert!(s.contains('…'));
        assert!(s.starts_with("the"));
        assert!(s.ends_with("dog"));
    }

    #[test]
    fn middle_truncate_tiny_returns_ellipsis() {
        assert_eq!(middle_truncate("hello", 1), "…");
    }

    #[test]
    fn fmt_id_pads_to_32_hex() {
        let s = fmt_id(0x10001000100000000u128);
        assert_eq!(s.len(), 2 + 32);
        assert!(s.starts_with("0x"));
    }

    #[test]
    fn fmt_short_id_round_trip_components() {
        let raw = MemoryId::pack(7, 42, 3).raw();
        assert_eq!(fmt_short_id(raw), "s7/m42/v3");
    }

    #[test]
    fn adhoc_table_empty_writes_no_rows_marker() {
        let t = AdHocTable {
            headers: vec!["a".into()],
            rows: vec![],
        };
        let mut buf = Vec::new();
        t.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("(no rows)"));
    }

    #[test]
    fn adhoc_table_renders_rows() {
        let t = AdHocTable {
            headers: vec!["name".into(), "kind".into()],
            rows: vec![
                vec!["alice".into(), "Person".into()],
                vec!["bob".into(), "Person".into()],
            ],
        };
        let mut buf = Vec::new();
        t.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("alice"));
        assert!(s.contains("bob"));
        assert!(s.contains("2 rows"));
    }

    // ─── plan_status_footer ─────────────────────────────────────────

    #[test]
    fn plan_footer_silent_on_goal_reached() {
        assert!(plan_status_footer(Some(PlanStatus::GoalReached), 3).is_none());
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
}
