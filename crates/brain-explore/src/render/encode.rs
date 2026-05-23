//! ENCODE response renderer.
//!
//! The card-style layout takes the screenshot the user approved as
//! its target shape:
//!   * a single horizontal rule opens and closes the card so an
//!     operator scanning a long shell session can spot one ENCODE
//!     among many at a glance;
//!   * the heading line carries the status verb on the left and a
//!     right-aligned routing cluster (LSN · short-id · age) on the
//!     right, computed against `policy.width` so it looks crisp at
//!     any terminal width;
//!   * the body uses a fixed two-space indent + 8-char label column
//!     so every row lines up. Operators reading two encode cards
//!     side-by-side see fields in the same screen-column.
//!
//! Default mode keeps only what a developer wants when eyeballing a
//! REPL — heading, content echo, type/salience/context, footer hint.
//! `-o wide` adds the agent / embedder / edges / created / dedup
//! block. The split keeps default output scannable while making the
//! audit trail one flag away.
//!
//! The renderer carries the source text so the line under the body
//! can echo what was encoded. brain-shell sets this via
//! [`EncodeRendered::with_source`]; callers that don't (e.g. JSON
//! consumers or a CLI path that doesn't have the original) just get
//! the heading + structured rows.

use std::io::{self, Write};

use brain_protocol::envelope::response::EncodeResponse;
use serde_json::{json, Value};

use crate::render::{
    fmt_hex_16, fmt_hex_16_chunked_dot, fmt_id, fmt_short_id, fmt_time, fmt_time_with_note,
    fmt_uuid,
};
use crate::table::truncate::middle_truncate;
use crate::theme::Token;
use crate::util::humanize::humanize_age;
use crate::{Render, RenderCtx};

/// Card width cap. The body's content never exceeds this so the card
/// stays readable on a 4K terminal where `policy.width` might be 200+
/// columns. 80 is the usual sweet spot — wide enough for full UUIDs
/// and chunked fingerprints, narrow enough for a side-by-side diff
/// pane.
const CARD_MAX_WIDTH: usize = 80;

/// The body indent: two spaces + an 8-char label column + a two-space
/// gap. Values land at column 12 in every row, so an operator's eye
/// can scan the values down a column without re-anchoring on each row.
const LABEL_COL_WIDTH: usize = 8;
const BODY_INDENT: &str = "  ";

/// Display wrapper for an ENCODE response.
///
/// Carries the original text so the renderer can echo it back —
/// confirmation that the substrate received what the user sent.
/// brain-shell sets `source_text` via [`Self::with_source`];
/// brain-cli (which doesn't issue ENCODE today) won't need this.
pub struct EncodeRendered {
    pub response: EncodeResponse,
    pub source_text: Option<String>,
    /// Optional post-encode amendment: auto-edges the AutoEdgeWorker
    /// landed for this memory after the encode response returned.
    /// `Some(non_empty)` → render a "→ N auto-edges landed in …ms"
    /// delta line below the card. `Some(empty)` and `None` both
    /// suppress the line (the latter means the caller didn't ask
    /// via `--wait-auto-edges-ms`; the former means the worker had
    /// time to run and produced nothing — silent by design).
    pub auto_edges_delta: Option<AutoEdgesDelta>,
    /// Outcomes captured by `--wait*` from `StageCompleted` events.
    /// `Some(_)` when the user asked the shell to block on
    /// background stages; `None` means no wait was requested. The
    /// renderer prints one line per result so the operator sees
    /// exactly what each stage produced (or didn't).
    pub stage_results: Option<StageResultsDelta>,
}

/// Post-encode amendment carrying per-stage outcomes. Populated by
/// `wait_for_stages` in brain-shell after the encode response
/// returned. The wall-clock window in `elapsed_ms` is the total
/// time the wait helper spent listening (NOT per-stage runtime).
#[derive(Debug, Clone)]
pub struct StageResultsDelta {
    pub elapsed_ms: u64,
    pub results: Vec<StageResult>,
    /// Stages that were on the pending checklist but didn't emit a
    /// `StageCompleted` before the deadline. Empty on the happy
    /// path. Surfaced so the operator sees "extractor stage timed
    /// out" instead of a silent return.
    pub timed_out: Vec<StageKindLabel>,
}

/// Wire-decoupled view of one `StageCompleted` event. The shell
/// flattens the StagePayload variants into a single struct so the
/// renderer doesn't have to know the wire enum.
#[derive(Debug, Clone)]
pub struct StageResult {
    pub kind: StageKindLabel,
    pub outcome: StageOutcomeLabel,
    /// One-line human summary of what the stage produced, e.g.
    /// `"2 SimilarTo edges"` or `"3 entities, 5 statements, 0
    /// relations · succeeded"`.
    pub summary: String,
}

/// Stage discriminator. Mirrors `brain_protocol::StageKind`
/// but kept as a free enum here so the brain-explore crate doesn't
/// re-export the protocol's wire type into its public surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKindLabel {
    AutoEdge,
    TemporalEdge,
    Extractor,
}

impl StageKindLabel {
    /// Snake-case label used in the rendered `stages` row and the
    /// per-result lines. Stable so scripts can grep for it.
    #[must_use]
    pub fn snake(&self) -> &'static str {
        match self {
            StageKindLabel::AutoEdge => "auto_edge",
            StageKindLabel::TemporalEdge => "temporal_edge",
            StageKindLabel::Extractor => "extractor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcomeLabel {
    /// Stage produced at least one derived item.
    Ok,
    /// Stage ran cleanly but produced nothing — e.g. zero-vector
    /// input, no extractable content. Distinct from `Ok` so we can
    /// render `⟲` instead of `✓` and explain "nothing to add."
    Empty,
    /// Stage errored. The summary carries the reason.
    Failed,
}

/// Per-encode summary of auto-edges that landed during the
/// `--wait-auto-edges-ms` window. Populated by the shell's filtered
/// subscribe watcher after the encode response returns.
#[derive(Debug, Clone)]
pub struct AutoEdgesDelta {
    /// Wall-clock time the watcher spent listening.
    pub elapsed_ms: u64,
    /// One entry per `EdgeAdded(AUTO_DERIVED)` event the watcher saw.
    pub edges: Vec<AutoEdgeSummary>,
}

/// One auto-edge surfaced in the delta line. Mirrors `EdgeView` plus
/// the origin tag so the renderer can label this as "auto" without
/// assuming the field's discriminant.
#[derive(Debug, Clone)]
pub struct AutoEdgeSummary {
    /// Target memory id, raw `u128` form (matches `EdgeView.target`).
    pub target: u128,
    /// Edge kind label (`"SimilarTo"`, etc.).
    pub kind: String,
    /// Similarity-derived weight from `EdgeData.weight`.
    pub weight: f32,
}

impl EncodeRendered {
    /// Wrap a response without source-text echo. Use
    /// [`Self::with_source`] to attach the text that was sent.
    #[must_use]
    pub fn new(response: EncodeResponse) -> Self {
        Self {
            response,
            source_text: None,
            auto_edges_delta: None,
            stage_results: None,
        }
    }

    /// Attach the text that was sent in the original `ENCODE`. The
    /// renderer will quote it back in the body as confirmation the
    /// substrate received what the user expected.
    #[must_use]
    pub fn with_source(mut self, text: impl Into<String>) -> Self {
        self.source_text = Some(text.into());
        self
    }

    /// Attach an auto-edges delta gathered after the encode response.
    /// `None` and `Some(delta with empty edges)` both suppress the
    /// rendered delta line — only a non-empty edge list prints.
    #[must_use]
    pub fn with_auto_edges_delta(mut self, delta: AutoEdgesDelta) -> Self {
        self.auto_edges_delta = Some(delta);
        self
    }

    /// Attach stage-completion outcomes the wait helper captured.
    /// The renderer prints one line per stage, plus a header line
    /// summarising elapsed time and a footer if any stages timed
    /// out.
    #[must_use]
    pub fn with_stage_results(mut self, delta: StageResultsDelta) -> Self {
        self.stage_results = Some(delta);
        self
    }
}

/// `From<EncodeResponse>` keeps zero-arg conversion sites working;
/// the `source_text` slot defaults to `None`. New call sites that
/// have the text should use `EncodeRendered::new(resp).with_source(text)`.
impl From<EncodeResponse> for EncodeRendered {
    fn from(response: EncodeResponse) -> Self {
        Self::new(response)
    }
}

impl Render for EncodeRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        render_human(self, ctx, w, /*wide=*/ false)
    }

    fn render_wide(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        render_human(self, ctx, w, /*wide=*/ true)
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.response;
        json!({
            "memory_id": fmt_id(r.memory_id),
            "lsn": r.lsn,
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

/// Compose one row of the body block — a label-prefixed line. Always
/// painted Token::Label for the label; the value is passed already
/// painted (so callers can choose Value / Success / Muted etc per row).
fn write_row(w: &mut dyn Write, ctx: &RenderCtx, label: &str, value: &str) -> io::Result<()> {
    let painted = ctx.theme.paint(Token::Label, label, ctx.policy);
    // Pad the label inside its column WITHOUT the ANSI escapes — pad
    // first, paint after, otherwise the escape bytes count toward the
    // width and we under-align on color-on terminals.
    let pad = LABEL_COL_WIDTH.saturating_sub(label.chars().count());
    let spaces = " ".repeat(pad);
    writeln!(w, "{BODY_INDENT}{painted}{spaces}  {value}")
}

/// Card-style layout shared by default + wide modes.
fn render_human(
    rendered: &EncodeRendered,
    ctx: &RenderCtx,
    w: &mut dyn Write,
    wide: bool,
) -> io::Result<()> {
    let r = &rendered.response;
    let policy = ctx.policy;
    let theme = &ctx.theme;
    let width = policy.width.min(CARD_MAX_WIDTH);

    // ── Top rule ──────────────────────────────────────────────────
    // No leading blank inside the rule: content sits flush against
    // the framing so the card reads as a single visual unit.
    let rule = "─".repeat(width);
    writeln!(w, "{rule}")?;

    // ── Heading ──────────────────────────────────────────────────
    // Status badge on the left + right-aligned routing cluster.
    let id_short = fmt_short_id(r.memory_id);
    let (badge_plain, badge_painted, right_plain, right_painted) = if r.was_deduplicated {
        // Dedup hits never carry a fresh LSN, and the wire's
        // created_at on a dedup is the *hit time* — not the original
        // encode time — so a relative age in the heading would
        // mislead. We surface that age in the body's `created` row
        // with an explicit "dedup hit time" note instead.
        let badge_plain = "⟳ DEDUP HIT";
        let badge_p = theme.paint(Token::Warn, badge_plain, policy).to_string();
        let right_plain = format!("matched · {id_short}");
        let id_painted = theme.paint(Token::MemoryId, &id_short, policy);
        let right_p = format!("matched · {id_painted}");
        (badge_plain.to_string(), badge_p, right_plain, right_p)
    } else if r.lsn == 0 {
        // Buffered inside a txn — no durable LSN until commit. Print
        // a clear marker instead of "LSN 0" which reads like a real
        // (albeit zero-th) WAL position. The encode is still real —
        // a memory_id has been reserved and the buffer holds the
        // vector + text — but the durability story doesn't begin
        // until TXN_COMMIT.
        let badge_plain = "◐ BUFFERED";
        let badge_p = theme.paint(Token::Warn, badge_plain, policy).to_string();
        let age = humanize_age(r.created_at_unix_nanos);
        let right_plain = format!("in-txn · {id_short} · {age}");
        let id_painted = theme.paint(Token::MemoryId, &id_short, policy);
        let age_painted = theme.paint(Token::Muted, &age, policy);
        let right_p = format!("in-txn · {id_painted} · {age_painted}");
        (badge_plain.to_string(), badge_p, right_plain, right_p)
    } else {
        let badge_plain = "✓ ENCODED";
        let badge_p = theme.paint(Token::Success, badge_plain, policy).to_string();
        let lsn = r.lsn;
        let age = humanize_age(r.created_at_unix_nanos);
        let right_plain = format!("LSN {lsn} · {id_short} · {age}");
        let lsn_str = lsn.to_string();
        let lsn_painted = theme.paint(Token::Value, &lsn_str, policy);
        let id_painted = theme.paint(Token::MemoryId, &id_short, policy);
        let age_painted = theme.paint(Token::Muted, &age, policy);
        let right_p = format!("LSN {lsn_painted} · {id_painted} · {age_painted}");
        (badge_plain.to_string(), badge_p, right_plain, right_p)
    };

    write_heading(
        w,
        width,
        &badge_plain,
        &badge_painted,
        &right_plain,
        &right_painted,
    )?;
    writeln!(w)?;

    // ── Body: id ────────────────────────────────────────────────
    // Always print the id as a labeled row, regardless of dedup state.
    // The heading already carries the short form (`s1/m1/v1`); the
    // body row carries the *canonical* full id so operators can
    // paste it into any follow-up command without parsing the heading
    // or wondering which form belongs where. One row, one form.
    let id_hex = fmt_id(r.memory_id);
    let id_value = theme.paint(Token::MemoryId, &id_hex, policy).to_string();
    write_row(w, ctx, "id", &id_value)?;

    // ── Body: content echo ──────────────────────────────────────
    if let Some(text) = rendered.source_text.as_deref() {
        // The content row gets the full body width minus the label
        // column + indent + the surrounding quotes. We always quote
        // so it reads as "this is the text" rather than a hanging
        // bare string.
        let budget = width
            .saturating_sub(BODY_INDENT.len() + LABEL_COL_WIDTH + 2 + 2)
            .max(8);
        let body = middle_truncate(text, budget);
        let quoted = format!("\"{body}\"");
        let painted = theme.paint(Token::Value, &quoted, policy);
        write_row(w, ctx, "content", &painted)?;
    }

    // ── Body: type / salience / context (fresh) or match (dedup) ─
    if r.was_deduplicated {
        // Dedup hit replaces the type/salience/context row with a
        // single explanatory match row. There's no fresh salience to
        // report and the type/kind of the matched memory isn't on
        // the wire response.
        let ctx_text = r.context_id.to_string();
        let painted = format!(
            "same content in context {}",
            theme.paint(Token::Value, &ctx_text, policy),
        );
        write_row(w, ctx, "match", &painted)?;
    } else {
        write_type_row(w, ctx, r)?;
    }

    // ── Body: pending background stages ─────────────────────────
    // Lists the workers the write queued — `auto_edge`,
    // `temporal_edge`, `extractor` — so the operator sees what's
    // about to run in the background even without `--wait*`. Empty
    // pending_stages renders `none queued` so a deployment with no
    // background workers wired reads honestly.
    if !r.was_deduplicated {
        write_stages_row(w, ctx, &r.pending_stages)?;
    }

    // ── Wide-mode block ─────────────────────────────────────────
    if wide {
        writeln!(w)?;

        // Agent — render as canonical 8-4-4-4-12 UUID. Nil UUID is
        // the unauthenticated / test path; show "default" rather
        // than a string of zeroes.
        let agent_value = if r.agent_id == [0u8; 16] {
            theme.paint(Token::Muted, "default", policy).to_string()
        } else {
            let uuid = fmt_uuid(&r.agent_id);
            theme.paint(Token::Value, &uuid, policy).to_string()
        };
        write_row(w, ctx, "agent", &agent_value)?;

        // Embedder fingerprint, chunked. 32 hex chars in a row are
        // unreadable; the chunking gives the eye an anchor at the
        // first 8 chars which is what operators typically compare
        // against a model directory hash.
        let embedder_value = if r.embedding_model_fp == [0u8; 16] {
            theme
                .paint(
                    Token::Muted,
                    "(stub — NopDispatcher; semantic search inactive)",
                    policy,
                )
                .to_string()
        } else {
            let chunked = fmt_hex_16_chunked_dot(&r.embedding_model_fp);
            format!("fp {}", theme.paint(Token::Value, &chunked, policy))
        };
        write_row(w, ctx, "embedder", &embedder_value)?;

        // Edges: auto / explicit / total split. Saturating-sub keeps
        // the math honest if a future server emits inconsistent
        // counts.
        let explicit = r.edges_out_count.saturating_sub(r.auto_edges_added);
        let edges_value = format!(
            "{auto} auto · {explicit} explicit · {total} total",
            auto = r.auto_edges_added,
            total = r.edges_out_count,
        );
        write_row(w, ctx, "edges", &edges_value)?;

        // Created — raw nanos primary, RFC3339 + relative in brackets.
        // For dedup hits we annotate the bracket so an operator
        // doesn't mistake it for the original encode time.
        let created_value = if r.was_deduplicated {
            fmt_time_with_note(r.created_at_unix_nanos, "dedup hit time")
        } else {
            fmt_time(r.created_at_unix_nanos)
        };
        write_row(w, ctx, "created", &created_value)?;

        // Dedup row: glyph + bool + clause. Scriptable check
        // (`grep '^  dedup'`) plus a clear human signal in the same
        // line.
        let dedup_value = if r.was_deduplicated {
            let glyph = theme.paint(Token::Success, "✓", policy);
            format!("{glyph} yes — existing memory returned; no write")
        } else {
            let glyph = theme.paint(Token::Error, "✗", policy);
            format!("{glyph} no — fresh write")
        };
        write_row(w, ctx, "dedup", &dedup_value)?;
    }

    // ── Footer hint ──────────────────────────────────────────────
    writeln!(w)?;
    if r.was_deduplicated {
        // No fresh LSN to chain off; tell the operator there's
        // nothing to do rather than emit a dead-end hint.
        let glyph = theme.paint(Token::Muted, "×", policy);
        let phrase = theme.paint(Token::Muted, "no fresh write — nothing to do", policy);
        writeln!(w, "{BODY_INDENT}{glyph} {phrase}")?;
    } else if r.lsn == 0 {
        // Buffered inside a txn — no LSN until commit. The user's
        // mental model is "what happens next?"; the honest answer
        // is "TXN_COMMIT". No subscribe hint because nothing is on
        // the wire yet for this memory.
        let glyph = theme.paint(Token::Muted, "◐", policy);
        let phrase = theme.paint(
            Token::Muted,
            "buffered — TXN_COMMIT to durabilize and broadcast",
            policy,
        );
        writeln!(w, "{BODY_INDENT}{glyph} {phrase}")?;
    } else if r.lsn > 0 {
        // The chain-on hint. `lsn + 1` is the position the next
        // event would land at, so a `subscribe --start-lsn <lsn+1>`
        // follows extraction / consolidation / edges without
        // re-receiving this encode itself.
        let next = r.lsn.saturating_add(1);
        let arrow = theme.paint(Token::Accent, "→", policy);
        let label_painted = theme.paint(Token::Label, "next", policy);
        let pad = LABEL_COL_WIDTH.saturating_sub("next".chars().count());
        let label_spaces = " ".repeat(pad);
        let cmd = format!("subscribe --start-lsn {next}");
        let cmd_painted = theme.paint(Token::Value, &cmd, policy);
        let tail = theme.paint(Token::Muted, "to watch for extraction", policy);
        writeln!(
            w,
            "{arrow} {label_painted}{label_spaces}   {cmd_painted}   {tail}"
        )?;
    }

    // ── Bottom rule ──────────────────────────────────────────────
    // No trailing blank inside the rule: footer hint sits flush
    // against the bottom framing, matching the top.
    writeln!(w, "{rule}")?;

    // ── Async amendment: auto-edges delta ─────────────────────────
    // Sits below the card (NOT inside the rules) so the card itself
    // stays the same shape whether or not the watcher ran. Only
    // prints when the caller passed `--wait-auto-edges-ms N` AND the
    // worker landed ≥1 auto-edge in that window. Empty/None: silent.
    if let Some(delta) = &rendered.auto_edges_delta {
        if !delta.edges.is_empty() {
            let arrow = theme.paint(Token::Accent, "→", policy);
            let n = delta.edges.len();
            let plural = if n == 1 { "" } else { "s" };
            let head_text = format!("{n} auto-edge{plural} landed in {} ms", delta.elapsed_ms);
            let head_painted = theme.paint(Token::Value, &head_text, policy);
            writeln!(w, "{arrow} {head_painted}")?;
            for e in &delta.edges {
                let tgt_short = crate::render::fmt_short_id(e.target);
                let tgt_painted = theme.paint(Token::MemoryId, &tgt_short, policy);
                let kind_painted = theme.paint(Token::Muted, &e.kind, policy);
                writeln!(
                    w,
                    "    {kind_painted} {tgt_painted}  weight={:.3}",
                    e.weight,
                )?;
            }
        }
    }

    // ── Async amendment: stage results from `--wait*` ─────────────
    // Per-stage outcomes captured by the shell's wait helper.
    // Distinct from `auto_edges_delta`: this is the "stage done"
    // signal (one line per stage), not the per-edge dump. Renders
    // even when results are empty — the operator asked us to wait,
    // so we have to report what happened (including "nothing").
    if let Some(delta) = &rendered.stage_results {
        let arrow = theme.paint(Token::Accent, "→", policy);
        let header_plain = format!("waited {} ms for background stages", delta.elapsed_ms);
        let header_painted = theme.paint(Token::Value, &header_plain, policy);
        writeln!(w, "{arrow} {header_painted}")?;
        for res in &delta.results {
            let (glyph, glyph_token) = match res.outcome {
                StageOutcomeLabel::Ok => ("✓", Token::Success),
                StageOutcomeLabel::Empty => ("⟲", Token::Muted),
                StageOutcomeLabel::Failed => ("✗", Token::Error),
            };
            let glyph_painted = theme.paint(glyph_token, glyph, policy);
            let kind_painted = theme.paint(Token::Label, res.kind.snake(), policy);
            // 13-char column for the stage name so the summaries
            // line up across rows (`temporal_edge` is the longest).
            let pad = 13_usize.saturating_sub(res.kind.snake().chars().count());
            let kind_spaces = " ".repeat(pad);
            // Append a "why" suffix for the empty-statement/empty-
            // relation extractor case: without the hint, "0 statements,
            // 0 relations" reads like a worker failure rather than
            // a deployment-config or content-coverage outcome.
            let summary_with_hint = augment_extractor_summary(
                res,
                rendered.response.has_active_schema,
                rendered.response.has_llm_extractor,
            );
            let summary_painted = theme.paint(Token::Value, &summary_with_hint, policy);
            writeln!(
                w,
                "    {glyph_painted} {kind_painted}{kind_spaces}  {summary_painted}"
            )?;
        }
        if !delta.timed_out.is_empty() {
            let warn = theme.paint(Token::Warn, "!", policy);
            let names: Vec<&'static str> = delta.timed_out.iter().map(|k| k.snake()).collect();
            let phrase = format!(
                "timed out waiting for: {} — server may still finish them later",
                names.join(", "),
            );
            let phrase_painted = theme.paint(Token::Muted, &phrase, policy);
            writeln!(w, "    {warn} {phrase_painted}")?;
        }
    }

    Ok(())
}

/// When an extractor stage reports `0 statements, 0 relations`, the
/// raw count line doesn't tell the operator WHY. Three distinct
/// conditions land at the same number:
///
/// 1. The deployment has no user-declared schema. Every proposed
///    statement / relation is dropped by the predicate/relation_type
///    admission filter because no predicate is registered. UX hint:
///    "no schema declares matching predicates".
///
/// 2. A schema IS declared but the deployment has no LLM tier wired
///    (no `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`, or every LLM
///    extractor disabled). Pattern + classifier tiers can produce
///    entities but never statements/relations. UX hint tells the
///    operator exactly which env var to set so the missing capability
///    isn't a guessing game.
///
/// 3. Schema declared, LLM tier wired, but the input sentence
///    didn't carry any predicate the schema covers. UX hint:
///    "extractor produced no statements".
///
/// The hint is appended after the existing `· <status>` suffix so
/// scripts that parse the leading count fields stay unaffected.
fn augment_extractor_summary(
    res: &StageResult,
    has_active_schema: bool,
    has_llm_extractor: bool,
) -> String {
    if res.kind != StageKindLabel::Extractor {
        return res.summary.clone();
    }
    // The summary string format is owned by brain-shell — we look
    // for the "0 statements, 0 relations" sentinel (handling both
    // singular/plural via the shared "0 " prefix). If the kept
    // counts are non-zero, leave the summary alone.
    let zero_stmts = res.summary.contains("0 statement") || res.summary.contains("0 statements");
    let zero_rels = res.summary.contains("0 relation") || res.summary.contains("0 relations");
    if !(zero_stmts && zero_rels) {
        return res.summary.clone();
    }
    // Skipped / Failed don't carry the "why nothing was kept" question
    // — they already explain themselves. Empty maps to a different
    // story (the extractor wasn't asked to produce anything) and we
    // only annotate when the extractor actually ran.
    if !matches!(res.outcome, StageOutcomeLabel::Ok) {
        return res.summary.clone();
    }
    let hint = if !has_active_schema {
        // No schema → drop all proposed statements/relations. This
        // dominates the explanation: even with an LLM, predicate
        // admission would reject everything.
        "no schema declares matching predicates"
    } else if !has_llm_extractor {
        // Schema declared but no LLM tier. Pattern + classifier can
        // resolve entities but never produce statements/relations.
        // Surface the env vars by name so the operator knows the fix.
        "statements require an LLM tier — set ANTHROPIC_API_KEY or OPENAI_API_KEY"
    } else {
        "extractor produced no statements"
    };
    format!("{} · {hint}", res.summary)
}

/// Render the labelled `stages` row carrying the pending background
/// stages from the encode ack. Empty → `none queued` so a deployment
/// with no workers wired reads honestly; non-empty → snake-case list
/// with `·` separators (e.g. `auto_edge · temporal_edge · extractor`).
fn write_stages_row(
    w: &mut dyn Write,
    ctx: &RenderCtx,
    pending: &[brain_protocol::StageKind],
) -> io::Result<()> {
    let theme = &ctx.theme;
    let policy = ctx.policy;
    let value = if pending.is_empty() {
        theme
            .paint(
                Token::Muted,
                "none queued (no schema declared or workers disabled)",
                policy,
            )
            .to_string()
    } else {
        let names: Vec<&'static str> = pending
            .iter()
            .map(|s| match s {
                brain_protocol::StageKind::AutoEdge => "auto_edge",
                brain_protocol::StageKind::TemporalEdge => "temporal_edge",
                brain_protocol::StageKind::Extractor => "extractor",
            })
            .collect();
        let joined = names.join(" · ");
        let painted = theme.paint(Token::Value, &joined, policy).to_string();
        let count = pending.len();
        let suffix = format!("  ({count} pending)");
        let suffix_painted = theme.paint(Token::Muted, &suffix, policy);
        format!("{painted}{suffix_painted}")
    };
    write_row(w, ctx, "stages", &value)
}

/// Heading composer: place `badge` at the left, `right` flush against
/// `width`, fill the middle with spaces. We pad against the *plain*
/// lengths (escape-stripped) so coloring doesn't throw off alignment;
/// we then write the *painted* versions.
fn write_heading(
    w: &mut dyn Write,
    width: usize,
    badge_plain: &str,
    badge_painted: &str,
    right_plain: &str,
    right_painted: &str,
) -> io::Result<()> {
    let body_indent = BODY_INDENT.len();
    let badge_w = badge_plain.chars().count();
    let right_w = right_plain.chars().count();
    let gap = width
        .saturating_sub(body_indent)
        .saturating_sub(badge_w)
        .saturating_sub(right_w)
        .max(2);
    let spaces = " ".repeat(gap);
    writeln!(w, "{BODY_INDENT}{badge_painted}{spaces}{right_painted}")
}

/// Render the labelled `metadata` row carrying type / salience /
/// context as three sub-columns separated by a fixed 6-space gap.
/// The row label is `metadata` so it reads naturally with the rest
/// of the body (`id`, `content`, `metadata`); each inner field
/// keeps its own sub-label so a glance picks out the three values.
fn write_type_row(w: &mut dyn Write, ctx: &RenderCtx, r: &EncodeResponse) -> io::Result<()> {
    let theme = &ctx.theme;
    let policy = ctx.policy;
    let kind = kind_label(r.kind);
    let kind_painted = theme.paint(Token::Value, kind, policy);
    let sal_text = format!("{:.2}", r.salience);
    let sal_painted = theme.paint(Token::Value, &sal_text, policy);
    let ctx_text = r.context_id.to_string();
    let ctx_painted = theme.paint(Token::Value, &ctx_text, policy);

    // Sub-labels are muted so values pop. Fixed 6-space inter-
    // column gap keeps cards aligned side-by-side; long context ids
    // push the right side further out instead of wrapping.
    let type_label = theme.paint(Token::Label, "type", policy);
    let sal_label = theme.paint(Token::Label, "salience", policy);
    let ctx_label = theme.paint(Token::Label, "context", policy);
    let value = format!(
        "{type_label}  {kind_painted}      {sal_label}  {sal_painted}      {ctx_label}  {ctx_painted}"
    );
    write_row(w, ctx, "metadata", &value)
}

/// Wire variant → canonical lower-case string used in the table view.
fn kind_label(k: brain_protocol::envelope::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::envelope::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::envelope::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::envelope::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;
    use brain_core::MemoryId;
    use brain_protocol::envelope::request::MemoryKindWire;

    /// Build a baseline rendered encode. Tests override the fields
    /// they care about so a new field never silently bypasses
    /// existing test coverage.
    fn sample() -> EncodeRendered {
        EncodeRendered::new(EncodeResponse {
            memory_id: MemoryId::pack(1, 1, 1).raw(),
            was_deduplicated: false,
            salience: 0.70,
            auto_edges_added: 0,
            lsn: 1,
            agent_id: [0u8; 16],
            context_id: 7,
            kind: MemoryKindWire::Episodic,
            created_at_unix_nanos: 0,
            edges_out_count: 0,
            embedding_model_fp: [0u8; 16],
            pending_stages: Vec::new(),
            has_active_schema: false,
            has_llm_extractor: false,
        })
    }

    fn ctx(format: OutputFormat) -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format,
        }
    }

    fn render(rendered: &EncodeRendered, format: OutputFormat) -> String {
        let mut buf = Vec::new();
        let c = ctx(format.clone());
        match format {
            OutputFormat::Wide => rendered.render_wide(&c, &mut buf).unwrap(),
            _ => rendered.render_table(&c, &mut buf).unwrap(),
        }
        String::from_utf8(buf).unwrap()
    }

    // 1. Fresh-encode heading: status verb + LSN + short id.
    #[test]
    fn render_table_fresh_encode_shows_check_and_lsn() {
        let mut r = sample();
        r.response.lsn = 2;
        r.source_text = Some("Alice merged the auth-rewrite branch".into());
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("✓ ENCODED"), "missing ok badge: {out}");
        assert!(out.contains("LSN 2"), "missing LSN value: {out}");
        assert!(out.contains("s1/m1/v1"), "missing memory id: {out}");
        assert!(out.contains("Alice merged"), "missing echoed text: {out}");
    }

    // 2. Dedup-hit heading swaps glyph + phrase.
    #[test]
    fn render_table_dedup_hit_shows_alt_glyph() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("⟳ DEDUP HIT"), "missing dedup badge: {out}");
        assert!(out.contains("matched"), "missing dedup phrase: {out}");
        assert!(out.contains("s1/m1/v1"), "must still show short id: {out}");
    }

    // 3. Dedup hit must NOT emit an LSN cell.
    #[test]
    fn render_table_dedup_hit_omits_lsn() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(
            !out.contains("LSN"),
            "dedup hit should not mention LSN: {out}"
        );
    }

    // 4. Default mode hides the nil agent UUID entirely. (The id row
    // legitimately contains zero bytes when slots/shards/versions are
    // small — assert on the labeled `agent` row instead of raw zeroes.)
    #[test]
    fn render_table_omits_nil_agent() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.lines().any(|l| l.contains("  agent  ")),
            "default mode must not surface agent row: {out}"
        );
    }

    // 5. Wide mode shows "default" for nil agent.
    #[test]
    fn render_table_wide_mode_shows_default_for_nil_agent() {
        let out = render(&sample(), OutputFormat::Wide);
        assert!(out.contains("agent"), "wide must label agent row: {out}");
        assert!(
            out.contains("default"),
            "nil agent must surface as 'default': {out}"
        );
    }

    // 5b. Wide mode renders the full block on dedup hits too.
    #[test]
    fn render_wide_mode_renders_block_on_dedup_hit() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Wide);
        assert!(out.contains("⟳ DEDUP HIT"), "wide+dedup heading: {out}");
        assert!(out.contains("agent"), "wide+dedup agent row: {out}");
        assert!(out.contains("embedder"), "wide+dedup embedder row: {out}");
        assert!(out.contains("edges"), "wide+dedup edges row: {out}");
        assert!(
            !out.contains("subscribe --start-lsn"),
            "wide+dedup must not emit subscribe hint: {out}"
        );
        assert!(
            out.contains("no fresh write"),
            "wide+dedup must keep dedup footer: {out}"
        );
    }

    // 6. Wide mode shows stub warning for zero fingerprint.
    #[test]
    fn render_table_wide_mode_shows_stub_warning_for_zero_fingerprint() {
        let out = render(&sample(), OutputFormat::Wide);
        assert!(
            out.contains("stub — NopDispatcher"),
            "wide must surface NopDispatcher hint: {out}"
        );
        assert!(
            out.contains("semantic search inactive"),
            "wide must say semantic search inactive: {out}"
        );
    }

    // 7. Wide mode shows the chunked fingerprint for a real fp.
    #[test]
    fn render_table_wide_mode_shows_full_fp_for_real_fingerprint() {
        let mut r = sample();
        r.response.embedding_model_fp = [
            0xe5, 0x41, 0xb0, 0x6c, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a,
            0x0b, 0x0c,
        ];
        let out = render(&r, OutputFormat::Wide);
        assert!(
            out.contains("fp e541b06c · 01020304 · 05060708 · 090a0b0c"),
            "expected chunked 4×8-hex fp: {out}"
        );
        assert!(
            !out.contains("e541b06c…"),
            "must not use the …-truncated short form in wide: {out}"
        );
        assert!(
            !out.contains("NopDispatcher"),
            "must not show stub warning with real fp: {out}"
        );
    }

    // 8. Wide mode renders the agent as a canonical UUID.
    #[test]
    fn render_table_wide_mode_shows_full_agent_uuid_for_real_agent() {
        let mut r = sample();
        r.response.agent_id = [
            0x01, 0x92, 0x7a, 0x8b, 0x4c, 0x2f, 0x70, 0x00, 0x80, 0x00, 0xde, 0xad, 0xbe, 0xef,
            0xfe, 0xed,
        ];
        let out = render(&r, OutputFormat::Wide);
        assert!(
            out.contains("01927a8b-4c2f-7000-8000-deadbeeffeed"),
            "expected canonical UUID dashed form: {out}"
        );
        assert!(
            !out.contains("default"),
            "real agent must not render as 'default': {out}"
        );
    }

    // 9. Wide mode surfaces the absolute created_at timestamp via fmt_time.
    #[test]
    fn render_table_wide_mode_shows_created_timestamp_in_unix_nanos() {
        let mut r = sample();
        r.response.created_at_unix_nanos = 1_700_000_000_000_000_000;
        let out = render(&r, OutputFormat::Wide);
        assert!(
            out.contains("created"),
            "wide must label created row: {out}"
        );
        assert!(
            out.contains("1700000000000000000 unix-nanos"),
            "wide must show raw nanos: {out}"
        );
        // RFC3339 form lives inside the brackets — assert the year
        // prefix rather than a pinned TZ offset.
        assert!(
            out.contains("unix-nanos (20"),
            "expected RFC3339 in brackets: {out}"
        );
    }

    // 10. Wide mode labels the dedup state explicitly.
    #[test]
    fn render_table_wide_mode_surfaces_dedup_state() {
        let fresh = render(&sample(), OutputFormat::Wide);
        assert!(
            fresh.contains("dedup") && fresh.contains("✗ no — fresh write"),
            "fresh write must report dedup=no: {fresh}"
        );

        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let hit = render(&r, OutputFormat::Wide);
        assert!(
            hit.contains("dedup") && hit.contains("✓ yes — existing memory returned; no write"),
            "dedup hit must report dedup=yes: {hit}"
        );
    }

    // 10b. Dedup-hit created row carries the "dedup hit time" note.
    #[test]
    fn render_table_dedup_hit_created_row_has_note() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        r.response.created_at_unix_nanos = 1_700_000_000_000_000_000;
        let out = render(&r, OutputFormat::Wide);
        assert!(
            out.contains("dedup hit time"),
            "dedup-hit created row must annotate the note: {out}"
        );
    }

    // 11. Wide mode reports the total edge count alongside the
    // auto/explicit split via dot separators.
    #[test]
    fn render_table_wide_mode_shows_total_edge_count() {
        let mut r = sample();
        r.response.auto_edges_added = 2;
        r.response.edges_out_count = 5;
        let out = render(&r, OutputFormat::Wide);
        assert!(
            out.contains("2 auto · 3 explicit · 5 total"),
            "wide must split + total edges with dots: {out}"
        );
    }

    // 12. Source text echoed when present (now as a content row).
    #[test]
    fn render_table_echoes_source_text_when_provided() {
        let r = sample().with_source("hello world");
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("content") && out.contains("\"hello world\""),
            "expected content row with quoted echo: {out}"
        );
    }

    // 13. No source → no content row at all.
    #[test]
    fn render_table_omits_source_when_none() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.contains("\"\""),
            "must not emit empty quoted line: {out}"
        );
        assert!(!out.contains("content"), "must not emit content row: {out}");
    }

    // 14. Long text gets middle-truncated.
    #[test]
    fn render_table_truncates_long_text_to_terminal_width() {
        let long: String = "x".repeat(200);
        let r = sample().with_source(long);
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains('…'), "expected ellipsis on long text: {out}");
        // The content line itself should fit within the card width
        // plus a small alignment tolerance.
        let content_line = out
            .lines()
            .find(|l| l.contains("content"))
            .expect("content line");
        assert!(
            content_line.chars().count() <= CARD_MAX_WIDTH + 4,
            "content line over budget: {content_line}"
        );
    }

    // 15. Next-hint increments LSN by one.
    #[test]
    fn render_table_next_hint_increments_lsn() {
        let mut r = sample();
        r.response.lsn = 5;
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("subscribe --start-lsn 6"),
            "expected next-hint LSN+1: {out}"
        );
        assert!(out.contains("→"), "expected arrow glyph: {out}");
        assert!(out.contains("next"), "expected next label: {out}");
    }

    // 16. No-color output strips ANSI escapes (the unicode glyphs
    // ✓ ✗ ⟳ → · — ↳ ─ are plain chars and still appear).
    #[test]
    fn render_table_no_color_strips_glyphs_or_colors() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.contains('\x1b'),
            "no-color render must not embed ANSI escapes: {out:?}"
        );
        assert!(
            out.contains("✓ ENCODED"),
            "badge stays under no-color: {out}"
        );
        assert!(out.contains("─"), "rules stay under no-color: {out}");
    }

    // 17. Narrow terminal — text truncates, lines don't wrap past width.
    #[test]
    fn render_table_narrow_terminal_clamps_text() {
        let mut policy = TermPolicy::plain();
        policy.width = 40;
        let c = RenderCtx {
            policy,
            theme: Theme::default(),
            format: OutputFormat::Table,
        };
        let r = sample().with_source("x".repeat(200));
        let mut buf = Vec::new();
        r.render_table(&c, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let echo_line = out
            .lines()
            .find(|l| l.contains('"') && l.contains('x'))
            .expect("expected quoted echo line in narrow render");
        // 40-wide card + indent + label column tolerance.
        assert!(
            echo_line.chars().count() <= 60,
            "narrow render let echo line exceed budget: {echo_line}"
        );
        assert!(
            echo_line.contains('…'),
            "narrow render must middle-truncate long text: {echo_line}"
        );
    }

    // 18. JSON view keeps the raw zero LSN — sentinel is table-only.
    #[test]
    fn render_json_emits_raw_zero_lsn() {
        let mut r = sample();
        r.response.lsn = 0;
        let v = r.render_json(&ctx(OutputFormat::Json));
        assert_eq!(v["lsn"], 0);
    }

    // 19. JSON view exposes the dedup bool.
    #[test]
    fn render_json_carries_was_deduplicated() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        let v = r.render_json(&ctx(OutputFormat::Json));
        assert_eq!(v["was_deduplicated"], true);
    }

    // 20. Each kind variant renders as its canonical string.
    #[test]
    fn render_table_kind_text_matches_wire_variant() {
        for (k, label) in [
            (MemoryKindWire::Episodic, "episodic"),
            (MemoryKindWire::Semantic, "semantic"),
            (MemoryKindWire::Consolidated, "consolidated"),
        ] {
            let mut r = sample();
            r.response.kind = k;
            let out = render(&r, OutputFormat::Table);
            assert!(
                out.contains(label),
                "kind {k:?} should render as `{label}`: {out}"
            );
        }
    }

    // 21. Memory id renders as s{shard}/m{slot}/v{version}.
    #[test]
    fn render_table_short_id_format_is_sslot_mslot_vslot() {
        let mut r = sample();
        r.response.memory_id = MemoryId::pack(3, 42, 7).raw();
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("s3/m42/v7"), "expected short id form: {out}");
    }

    // 21b. The id row carries the canonical full id (0x + 32 hex)
    // in both default and wide modes. The short form lives only in
    // the heading; the body row gives operators the single pasteable
    // canonical form.
    #[test]
    fn render_table_id_row_shows_canonical_hex_in_default_mode() {
        let mut r = sample();
        r.response.memory_id = MemoryId::pack(2, 17, 4).raw();
        let out = render(&r, OutputFormat::Table);
        let id_line = out
            .lines()
            .find(|l| l.contains("  id  "))
            .expect("missing id row");
        assert!(
            id_line.contains("0x"),
            "id row must carry canonical hex form: {id_line}"
        );
        // The full id is 0x + 32 hex chars.
        assert!(
            id_line.chars().filter(|c| c.is_ascii_hexdigit()).count() >= 32,
            "id row must include the full 32-hex-digit id: {id_line}"
        );
        assert!(
            !id_line.contains("s2/m17/v4"),
            "the short form belongs in the heading, not the id row: {id_line}"
        );
    }

    #[test]
    fn render_table_id_row_shows_canonical_hex_in_wide_mode() {
        let mut r = sample();
        r.response.memory_id = MemoryId::pack(2, 17, 4).raw();
        let out = render(&r, OutputFormat::Wide);
        let id_line = out
            .lines()
            .find(|l| l.contains("  id  "))
            .expect("missing id row");
        assert!(
            id_line.contains("0x"),
            "wide id row must carry canonical hex form: {id_line}"
        );
        assert!(
            !id_line.contains("s2/m17/v4"),
            "wide id row must NOT duplicate the heading's short form: {id_line}"
        );
    }

    // 21d. Dedup hits also get the id row — same shape, same column.
    #[test]
    fn render_table_id_row_present_on_dedup_hit() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.lines().any(|l| l.contains("  id  ")),
            "dedup-hit card must still surface id row: {out}"
        );
    }

    // 21e. `lsn == 0` (buffered inside a txn) shows the BUFFERED
    // badge and an `in-txn` marker instead of "LSN 0". The footer
    // hint is the durability nudge, not a subscribe one.
    #[test]
    fn render_table_buffered_heading_when_lsn_zero() {
        let mut r = sample();
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("◐ BUFFERED"), "missing BUFFERED badge: {out}");
        assert!(out.contains("in-txn"), "missing in-txn marker: {out}");
        assert!(
            !out.contains("LSN 0"),
            "buffered heading must not show 'LSN 0': {out}"
        );
        assert!(
            out.contains("TXN_COMMIT to durabilize"),
            "buffered footer must hint at commit: {out}"
        );
        assert!(
            !out.contains("subscribe --start-lsn"),
            "buffered card must not emit subscribe hint (nothing on the wire yet): {out}"
        );
    }

    // 21f. A real LSN keeps the existing fresh-encode heading; no
    // regression on the established shape.
    #[test]
    fn render_table_fresh_heading_when_lsn_nonzero() {
        let mut r = sample();
        r.response.lsn = 42;
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("✓ ENCODED"), "fresh badge missing: {out}");
        assert!(out.contains("LSN 42"), "fresh LSN missing: {out}");
        assert!(
            !out.contains("BUFFERED"),
            "fresh card must not borrow BUFFERED badge: {out}"
        );
    }

    // ─── New layout-shape tests ──────────────────────────────────

    // 22. Heading right-side aligns to the card width.
    #[test]
    fn render_table_header_right_side_aligns_to_terminal_width() {
        let mut r = sample();
        r.response.lsn = 2;
        let out = render(&r, OutputFormat::Table);
        let heading = out
            .lines()
            .find(|l| l.contains("ENCODED"))
            .expect("heading line");
        // Width is 80 (policy.plain) capped at CARD_MAX_WIDTH=80.
        // Heading line is the indented composite — it should run
        // close to that width.
        assert!(
            heading.chars().count() >= 60,
            "heading too narrow — right cluster collapsed: {heading}"
        );
    }

    // 23. Horizontal rules open and close the card.
    #[test]
    fn render_table_horizontal_rules_open_and_close_the_card() {
        let out = render(&sample(), OutputFormat::Table);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.first().is_some_and(|l| l.chars().all(|c| c == '─')),
            "first line must be a rule: {:?}",
            lines.first()
        );
        assert!(
            lines.last().is_some_and(|l| l.chars().all(|c| c == '─')),
            "last line must be a rule: {:?}",
            lines.last()
        );
    }

    // 24. Label column alignment — every body row's value starts at
    // the same column.
    #[test]
    fn render_table_label_column_alignment_is_consistent() {
        let mut r = sample().with_source("hello");
        r.response.auto_edges_added = 1;
        r.response.edges_out_count = 2;
        let out = render(&r, OutputFormat::Wide);
        // Find each row by its label, then extract the column where
        // the value begins (first non-space after the label + gap).
        for label in [
            "id", "content", "agent", "embedder", "edges", "created", "dedup",
        ] {
            let line = out
                .lines()
                .find(|l| l.contains(label))
                .unwrap_or_else(|| panic!("missing row for label `{label}`: {out}"));
            // The line begins with two spaces of indent. After the
            // label and its padding gap, the value lands at column
            // BODY_INDENT.len() + LABEL_COL_WIDTH + 2 = 12.
            let chars: Vec<char> = line.chars().collect();
            assert!(
                chars.get(12).copied().is_some(),
                "row `{label}` too short to reach col 12: {line}"
            );
        }
    }

    // 25. Fingerprint chunked with dot separators (exactly 3 ` · `s).
    #[test]
    fn render_table_fingerprint_chunked_with_dot_separators() {
        let mut r = sample();
        r.response.embedding_model_fp = [
            0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        let out = render(&r, OutputFormat::Wide);
        let embedder_line = out
            .lines()
            .find(|l| l.contains("embedder"))
            .expect("embedder line");
        let dot_count = embedder_line.matches(" · ").count();
        assert_eq!(
            dot_count, 3,
            "expected exactly 3 separators on embedder line: {embedder_line}"
        );
    }

    // 26. Created row shows raw + human forms in one line.
    #[test]
    fn render_table_created_row_shows_raw_and_human_forms() {
        let mut r = sample();
        r.response.created_at_unix_nanos = 1_700_000_000_000_000_000;
        let out = render(&r, OutputFormat::Wide);
        let line = out
            .lines()
            .find(|l| l.contains("created"))
            .expect("created line");
        assert!(
            line.contains("unix-nanos ("),
            "expected human bracket: {line}"
        );
        assert!(line.contains("20"), "expected year prefix: {line}");
    }

    // 27. Default mode omits the wide block but keeps the rules.
    #[test]
    fn render_table_default_mode_omits_wide_block_but_keeps_rules() {
        let r = sample().with_source("hello");
        let out = render(&r, OutputFormat::Table);
        for absent in ["agent", "embedder", "edges", "created", "dedup"] {
            assert!(
                !out.contains(absent),
                "default mode leaked wide row `{absent}`: {out}"
            );
        }
        // Rules + heading + id + content + metadata + stages + footer.
        assert!(out.contains("─"), "rules must remain in default mode");
        assert!(out.contains("✓ ENCODED"), "heading must remain");
        assert!(out.contains("  id  "), "id row must remain in default mode");
        assert!(out.contains("content"), "content row must remain");
        assert!(out.contains("metadata"), "metadata row must remain");
        assert!(out.contains("stages"), "stages row must remain");
        assert!(
            out.contains("type") && out.contains("salience") && out.contains("context"),
            "metadata row must carry type/salience/context sub-labels: {out}"
        );
    }

    // 28. The `stages` row honestly says "none queued" when no
    // background work was triggered (no-schema deployment,
    // workers disabled, or no UpsertMemory phase).
    #[test]
    fn render_stages_row_says_none_queued_when_empty() {
        let r = sample();
        let out = render(&r, OutputFormat::Table);
        let line = out
            .lines()
            .find(|l| l.contains("  stages"))
            .expect("stages row missing");
        assert!(
            line.contains("none queued"),
            "empty stages row should explain why: {line}"
        );
    }

    // 29. The `stages` row lists each queued stage by snake_case
    // name plus a `(N pending)` count suffix.
    #[test]
    fn render_stages_row_lists_pending_stages() {
        use brain_protocol::StageKind;
        let mut r = sample();
        r.response.pending_stages = vec![
            StageKind::AutoEdge,
            StageKind::TemporalEdge,
            StageKind::Extractor,
        ];
        let out = render(&r, OutputFormat::Table);
        let line = out
            .lines()
            .find(|l| l.contains("  stages"))
            .expect("stages row missing");
        assert!(
            line.contains("auto_edge"),
            "auto_edge in stages row: {line}"
        );
        assert!(
            line.contains("temporal_edge"),
            "temporal_edge in stages row: {line}"
        );
        assert!(
            line.contains("extractor"),
            "extractor in stages row: {line}"
        );
        assert!(line.contains("(3 pending)"), "count suffix: {line}");
    }

    // 30. Stage results from `--wait*` render below the card as
    // a "→ waited Nms" header plus one line per stage with the
    // right glyph (✓ for Ok, ⟲ for Empty, ✗ for Failed).
    #[test]
    fn render_stage_results_amendment_section() {
        let r = sample().with_stage_results(StageResultsDelta {
            elapsed_ms: 350,
            results: vec![
                StageResult {
                    kind: StageKindLabel::AutoEdge,
                    outcome: StageOutcomeLabel::Ok,
                    summary: "2 SimilarTo edges written".into(),
                },
                StageResult {
                    kind: StageKindLabel::TemporalEdge,
                    outcome: StageOutcomeLabel::Empty,
                    summary: "no FollowedBy edge".into(),
                },
                StageResult {
                    kind: StageKindLabel::Extractor,
                    outcome: StageOutcomeLabel::Ok,
                    summary: "3 entities, 5 statements, 0 relations · succeeded".into(),
                },
            ],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("waited 350 ms for background stages"),
            "missing header: {out}"
        );
        assert!(out.contains("✓ auto_edge"), "Ok stage uses ✓ glyph: {out}");
        assert!(
            out.contains("⟲ temporal_edge"),
            "Empty stage uses ⟲ glyph: {out}"
        );
        assert!(
            out.contains("✓ extractor"),
            "Ok extractor uses ✓ glyph: {out}"
        );
        assert!(out.contains("2 SimilarTo edges"), "auto summary: {out}");
        assert!(
            out.contains("3 entities, 5 statements"),
            "extractor summary: {out}"
        );
    }

    // 31. Timed-out stages get an explicit row instead of silently
    // disappearing. The user asked us to wait — silence is wrong.
    #[test]
    fn render_stage_results_surfaces_timeouts() {
        let r = sample().with_stage_results(StageResultsDelta {
            elapsed_ms: 60_000,
            results: vec![StageResult {
                kind: StageKindLabel::AutoEdge,
                outcome: StageOutcomeLabel::Ok,
                summary: "1 SimilarTo edge written".into(),
            }],
            timed_out: vec![StageKindLabel::Extractor],
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("timed out waiting for: extractor"),
            "timeout row missing: {out}"
        );
    }

    // 32. Dedup hits don't render a `stages` row (no fresh write
    // → no background work).
    #[test]
    fn render_dedup_hit_omits_stages_row() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(
            !out.lines().any(|l| l.contains("  stages")),
            "dedup hit must not render stages row: {out}"
        );
    }

    // 33. Extractor stage with 0 statements + 0 relations on a
    // no-schema deployment renders the "no schema declares matching
    // predicates" hint, so the operator understands why nothing
    // landed (deployment story, not a worker failure).
    #[test]
    fn render_extractor_zero_stmts_no_schema_appends_schema_hint() {
        let mut r = sample();
        r.response.has_active_schema = false;
        let r = r.with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::Extractor,
                outcome: StageOutcomeLabel::Ok,
                summary: "1 entity, 0 statements, 0 relations · succeeded".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("no schema declares matching predicates"),
            "no-schema hint missing on extractor row: {out}"
        );
    }

    // 34. Extractor stage with 0 statements + 0 relations BUT a
    // schema IS declared + an LLM tier IS wired — the extractor ran
    // with both prerequisites satisfied but found no sentence
    // matching any declared predicate. Content-coverage story.
    #[test]
    fn render_extractor_zero_stmts_with_schema_appends_content_hint() {
        let mut r = sample();
        r.response.has_active_schema = true;
        r.response.has_llm_extractor = true;
        let r = r.with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::Extractor,
                outcome: StageOutcomeLabel::Ok,
                summary: "1 entity, 0 statements, 0 relations · succeeded".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("extractor produced no statements"),
            "schema-present + llm-wired hint missing on extractor row: {out}"
        );
        assert!(
            !out.contains("no schema declares matching predicates"),
            "must not emit the no-schema hint when schema IS declared: {out}"
        );
        assert!(
            !out.contains("statements require an LLM tier"),
            "must not emit the llm-missing hint when an LLM tier is wired: {out}"
        );
    }

    // 34b. Schema declared but no LLM tier wired — the renderer
    // tells the operator exactly which env var unlocks statements.
    // Without this hint, the user reads "0 statements" and assumes
    // the system is broken; with it, they know it's a one-line fix.
    #[test]
    fn render_extractor_zero_stmts_schema_but_no_llm_appends_llm_hint() {
        let mut r = sample();
        r.response.has_active_schema = true;
        r.response.has_llm_extractor = false;
        let r = r.with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::Extractor,
                outcome: StageOutcomeLabel::Ok,
                summary: "5 entities, 0 statements, 0 relations · succeeded".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("statements require an LLM tier"),
            "llm-missing hint missing on extractor row: {out}"
        );
        assert!(
            out.contains("ANTHROPIC_API_KEY") && out.contains("OPENAI_API_KEY"),
            "hint must name both env vars so operator knows the fix: {out}"
        );
        assert!(
            !out.contains("extractor produced no statements"),
            "must not also emit the content-coverage hint: {out}"
        );
        assert!(
            !out.contains("partially applied"),
            "must not slap a 'partially applied' badge on a clean classifier run: {out}"
        );
    }

    // 35. Non-zero statement/relation counts leave the summary alone.
    // The hint only fires for the structural ambiguity case.
    #[test]
    fn render_extractor_nonzero_counts_no_hint() {
        let r = sample().with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::Extractor,
                outcome: StageOutcomeLabel::Ok,
                summary: "1 entity, 3 statements, 0 relations · succeeded".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            !out.contains("no schema declares matching predicates"),
            "must not emit no-schema hint when statements landed: {out}"
        );
        assert!(
            !out.contains("extractor produced no statements"),
            "must not emit content hint when statements landed: {out}"
        );
    }

    // 36. Non-Ok outcomes (Skipped / Failed / Empty) don't get the
    // hint — they already carry their own explanation.
    #[test]
    fn render_extractor_zero_stmts_skipped_outcome_no_hint() {
        let r = sample().with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::Extractor,
                outcome: StageOutcomeLabel::Empty,
                summary: "0 entities, 0 statements, 0 relations · skipped".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            !out.contains("no schema declares matching predicates"),
            "must not annotate already-explained skipped row: {out}"
        );
        assert!(
            !out.contains("extractor produced no statements"),
            "must not annotate already-explained skipped row: {out}"
        );
    }

    // 37. Hint only applies to extractor rows. Edge stages with
    // their own "0 X" phrasing must not be touched.
    #[test]
    fn render_non_extractor_stage_summary_untouched() {
        let r = sample().with_stage_results(StageResultsDelta {
            elapsed_ms: 200,
            results: vec![StageResult {
                kind: StageKindLabel::AutoEdge,
                outcome: StageOutcomeLabel::Empty,
                summary: "no SimilarTo edges (below threshold or zero-vector)".into(),
            }],
            timed_out: Vec::new(),
        });
        let out = render(&r, OutputFormat::Table);
        assert!(
            !out.contains("no schema declares matching predicates"),
            "must not annotate non-extractor stage: {out}"
        );
    }
}
