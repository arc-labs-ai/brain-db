//! ENCODE response renderer.
//!
//! The default-mode table is meant for a developer eyeballing the
//! shell: a status glyph, the new id, the LSN you'd chain a
//! `subscribe --start-lsn` to, the text the substrate actually
//! received, and (when a dedup happened) a clear "nothing was
//! written" signal. The "noise" — agent uuid, embedder fingerprint,
//! edge counts — moves into `-o wide` so the normal output stays
//! scannable.
//!
//! The renderer carries the source text so the line under the
//! heading can echo what was encoded. brain-shell sets this via
//! [`EncodeRendered::with_source`]; callers that don't (e.g. JSON
//! consumers or a CLI path that doesn't have the original) just get
//! the heading + metadata block.

use std::borrow::Cow;
use std::io::{self, Write};

use brain_protocol::response::EncodeResponse;
use serde_json::{json, Value};

use crate::render::{fmt_hex_16, fmt_id, fmt_short_hex_16, fmt_short_id};
use crate::table::truncate::middle_truncate;
use crate::theme::Token;
use crate::util::humanize::humanize_age;
use crate::{Render, RenderCtx};

/// Display wrapper for an ENCODE response.
///
/// Carries the original text so the renderer can echo it back —
/// confirmation that the substrate received what the user sent.
/// brain-shell sets `source_text` via [`Self::with_source`];
/// brain-cli (which doesn't issue ENCODE today) won't need this.
pub struct EncodeRendered {
    pub response: EncodeResponse,
    pub source_text: Option<String>,
}

impl EncodeRendered {
    /// Wrap a response without source-text echo. Use
    /// [`Self::with_source`] to attach the text that was sent.
    #[must_use]
    pub fn new(response: EncodeResponse) -> Self {
        Self {
            response,
            source_text: None,
        }
    }

    /// Attach the text that was sent in the original `ENCODE`. The
    /// renderer will quote it back under the heading as confirmation
    /// the substrate received what the user expected.
    #[must_use]
    pub fn with_source(mut self, text: impl Into<String>) -> Self {
        self.source_text = Some(text.into());
        self
    }

    /// Returns the LSN as `Some(n)` for ergonomic rendering, or
    /// `None` when the wire LSN is the `0` sentinel (no WAL sink,
    /// dedup hit, cached replay). Table view paints that as an
    /// em-dash so operators don't read it as a real position-zero
    /// LSN; JSON view keeps the raw `0`.
    fn lsn_display(&self) -> Option<u64> {
        if self.response.lsn == 0 {
            None
        } else {
            Some(self.response.lsn)
        }
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

/// Shared table layout for default + wide modes.
///
/// Layout:
///   line 1: status heading (icon + short id + LSN/dedup state + age)
///   line 2: (blank)
///   line 3: quoted source-text echo, when available
///   line 4: kind · salience · context
///   line 5: (blank, only in wide)
///   line 6+: agent/embedder/edges rows (wide only)
///   line N-1: (blank)
///   line N: "next" hint / "nothing to do" footer
fn render_human(
    rendered: &EncodeRendered,
    ctx: &RenderCtx,
    w: &mut dyn Write,
    wide: bool,
) -> io::Result<()> {
    let r = &rendered.response;
    let policy = ctx.policy;
    let theme = &ctx.theme;
    let id_short = fmt_short_id(r.memory_id);
    let id_cell = theme.paint(Token::MemoryId, &id_short, policy);

    // ── Heading ───────────────────────────────────────────────────
    if r.was_deduplicated {
        // The substrate found an existing memory with the same content
        // (same agent + context + text). No fresh row was written; the
        // wire LSN is the `0` sentinel. Surface this clearly so users
        // don't try to chain a subscribe to a non-existent LSN.
        let glyph = theme.paint(Token::Warn, "↻ dedup hit", policy);
        writeln!(w, "{glyph}  {id_cell}  ·  matched existing memory")?;
    } else {
        let glyph = theme.paint(Token::Success, "✓ encoded", policy);
        let lsn_text: Cow<'_, str> = match rendered.lsn_display() {
            Some(n) => {
                let s = n.to_string();
                let painted = theme.paint(Token::Value, &s, policy);
                Cow::Owned(format!("LSN {painted}"))
            }
            None => {
                // Fresh path with no LSN. Shouldn't normally happen, but
                // we keep the F2 em-dash convention for the substrate-
                // only / test paths that don't wire a WAL sink.
                let dash = theme.paint(Token::Muted, "—", policy);
                Cow::Owned(format!("LSN {dash}"))
            }
        };
        let age = humanize_age(r.created_at_unix_nanos);
        let age_cell = theme.paint(Token::Muted, &age, policy);
        writeln!(w, "{glyph}  {id_cell}  ·  {lsn_text}  ·  {age_cell}")?;
    }
    writeln!(w)?;

    // ── Source-text echo ──────────────────────────────────────────
    if let Some(text) = rendered.source_text.as_deref() {
        // Reserve room for the two-space indent and the surrounding
        // quotes; everything else of the terminal width is text.
        let budget = policy.width.saturating_sub(4);
        let body = middle_truncate(text, budget);
        let quoted = format!("\"{body}\"");
        let painted = theme.paint(Token::Value, &quoted, policy);
        writeln!(w, "  {painted}")?;
    }

    // ── Kind · salience · context ─────────────────────────────────
    let kind = kind_label(r.kind);
    if r.was_deduplicated {
        // On a dedup hit there's no fresh salience to report; the
        // existing memory's salience hasn't necessarily changed. The
        // most useful thing to confirm is the context the match
        // landed in.
        let label = theme.paint(Token::Label, "same content in context", policy);
        let ctx_text = r.context_id.to_string();
        let val = theme.paint(Token::Value, &ctx_text, policy);
        writeln!(w, "  {label} {val}")?;
    } else {
        let kind_painted = theme.paint(Token::Label, kind, policy);
        let sal_text = format!("{:.2}", r.salience);
        let ctx_text = r.context_id.to_string();
        writeln!(
            w,
            "  {kind_painted} · salience {sal} · context {ctx_v}",
            sal = theme.paint(Token::Value, &sal_text, policy),
            ctx_v = theme.paint(Token::Value, &ctx_text, policy),
        )?;
    }

    // ── Wide-mode extras ──────────────────────────────────────────
    if wide {
        writeln!(w)?;

        // Agent — first-class Brain noun. We label the row "agent"
        // (not "id") per project convention. The nil uuid means the
        // connection authenticated under the default agent and the
        // server didn't override it; surface that as "default" rather
        // than print zeros.
        let agent_value = if r.agent_id == [0u8; 16] {
            "default".to_string()
        } else {
            fmt_short_hex_16(&r.agent_id)
        };
        let agent_label = theme.paint(Token::Label, "agent", policy);
        writeln!(w, "  {agent_label:<10}  {agent_value}")?;

        // Embedder fingerprint. The server runs `NopDispatcher` today
        // (Phase 9.10 isn't wired), so the fingerprint is genuinely
        // all-zeros — not a hash collision, not a missing field. We
        // say so out loud so the operator doesn't go looking for the
        // bug.
        let embedder_label = theme.paint(Token::Label, "embedder", policy);
        if r.embedding_model_fp == [0u8; 16] {
            let warn = theme.paint(
                Token::Muted,
                "(stub — NopDispatcher; semantic search inactive)",
                policy,
            );
            writeln!(w, "  {embedder_label:<10}  {warn}")?;
        } else {
            let fp = fmt_short_hex_16(&r.embedding_model_fp);
            writeln!(w, "  {embedder_label:<10}  fp {fp}")?;
        }

        // Edges. The wire response carries `auto_edges_added` and the
        // total `edges_out_count`; the explicit count is whatever's
        // left over (request-supplied edges that survived target
        // resolution). Saturating-sub keeps the math honest even if
        // a future server emits inconsistent counts.
        let edges_label = theme.paint(Token::Label, "edges", policy);
        let explicit = r.edges_out_count.saturating_sub(r.auto_edges_added);
        writeln!(
            w,
            "  {edges_label:<10}  {auto} auto, {explicit} explicit",
            auto = r.auto_edges_added,
        )?;
    }

    // ── Footer hint ───────────────────────────────────────────────
    writeln!(w)?;
    if r.was_deduplicated {
        let line = theme.paint(Token::Muted, "no fresh write; nothing to do", policy);
        writeln!(w, "  {line}")?;
    } else if let Some(lsn) = rendered.lsn_display() {
        // The chain-on hint. `lsn + 1` is the position the next event
        // would land at, so a `subscribe --start-lsn <lsn+1>` follows
        // downstream events (extraction, edges, consolidation) without
        // re-receiving this encode itself.
        let hint = format!(
            "next: subscribe --start-lsn {next}  to watch for extraction",
            next = lsn.saturating_add(1),
        );
        let painted = theme.paint(Token::Muted, &hint, policy);
        writeln!(w, "  {painted}")?;
    }
    // No-LSN, non-dedup path: silently skip the footer rather than
    // emit a hint pointing at LSN=1 that won't work.

    Ok(())
}

/// Wire variant → canonical lower-case string used in the table view.
fn kind_label(k: brain_protocol::request::MemoryKindWire) -> &'static str {
    match k {
        brain_protocol::request::MemoryKindWire::Episodic => "episodic",
        brain_protocol::request::MemoryKindWire::Semantic => "semantic",
        brain_protocol::request::MemoryKindWire::Consolidated => "consolidated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;
    use brain_core::MemoryId;
    use brain_protocol::request::MemoryKindWire;

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

    // 1. Happy path — heading + lsn echo.
    #[test]
    fn render_table_fresh_encode_shows_check_and_lsn() {
        let mut r = sample();
        r.source_text = Some("Alice merged the auth-rewrite branch".into());
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("✓ encoded"), "missing ok glyph: {out}");
        assert!(out.contains("s1/m1/v1"), "missing memory id: {out}");
        assert!(out.contains("LSN 1"), "missing LSN value: {out}");
        assert!(out.contains("Alice merged"), "missing echoed text: {out}");
    }

    // 2. Dedup-hit heading swaps glyph + phrase.
    #[test]
    fn render_table_dedup_hit_shows_alt_glyph() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("↻ dedup hit"), "missing dedup glyph: {out}");
        assert!(
            out.contains("matched existing memory"),
            "missing dedup phrase: {out}"
        );
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

    // 4. Fresh path with lsn=0 — em-dash sentinel from F2 stays.
    #[test]
    fn render_table_fresh_with_lsn_zero_shows_dash() {
        let mut r = sample();
        r.response.lsn = 0;
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("LSN —"), "expected em-dash sentinel: {out}");
        assert!(!out.contains("LSN 0"), "must not print literal 0: {out}");
    }

    // 5. Default mode hides nil agent entirely.
    #[test]
    fn render_table_omits_nil_agent() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.contains("agent"),
            "default mode must not surface agent: {out}"
        );
        assert!(
            !out.contains("00000000"),
            "default mode must not surface zero bytes: {out}"
        );
    }

    // 6. Wide mode shows "default" for nil agent.
    #[test]
    fn render_table_wide_mode_shows_default_for_nil_agent() {
        let out = render(&sample(), OutputFormat::Wide);
        assert!(out.contains("agent"), "wide must label agent row: {out}");
        assert!(
            out.contains("default"),
            "nil agent must surface as 'default': {out}"
        );
    }

    // 7. Wide mode shows stub warning for zero fingerprint.
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

    // 8. Wide mode shows short fp for a real fingerprint.
    #[test]
    fn render_table_wide_mode_shows_short_fp_for_real_fingerprint() {
        let mut r = sample();
        r.response.embedding_model_fp = [
            0x7a, 0x8b, 0x3c, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let out = render(&r, OutputFormat::Wide);
        assert!(out.contains("fp 7a8b3c2d…"), "expected short fp: {out}");
        assert!(
            !out.contains("NopDispatcher"),
            "must not show stub warning with real fp: {out}"
        );
    }

    // 9. Source text echoed when present.
    #[test]
    fn render_table_echoes_source_text_when_provided() {
        let r = sample().with_source("hello world");
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("\"hello world\""),
            "expected quoted echo: {out}"
        );
    }

    // 10. No source → no empty quoted line.
    #[test]
    fn render_table_omits_source_when_none() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.contains("\"\""),
            "must not emit empty quoted line: {out}"
        );
    }

    // 11. Long text gets middle-truncated to roughly the policy width.
    #[test]
    fn render_table_truncates_long_text_to_terminal_width() {
        let long: String = "x".repeat(200);
        let r = sample().with_source(long);
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains('…'), "expected ellipsis on long text: {out}");
        // Every line must fit roughly in width budget — TermPolicy::plain
        // is 80 cols. We tolerate the two-space indent and quotes.
        for line in out.lines() {
            assert!(
                line.chars().count() <= 80 + 4,
                "line over budget: {} ({:?})",
                line.chars().count(),
                line
            );
        }
    }

    // 12. Next-hint increments LSN by one.
    #[test]
    fn render_table_next_hint_increments_lsn() {
        let mut r = sample();
        r.response.lsn = 5;
        let out = render(&r, OutputFormat::Table);
        assert!(
            out.contains("subscribe --start-lsn 6"),
            "expected next-hint LSN+1: {out}"
        );
    }

    // 13. No-color output strips ANSI escapes.
    #[test]
    fn render_table_no_color_strips_glyphs_or_colors() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            !out.contains('\x1b'),
            "no-color render must not embed ANSI escapes: {out:?}"
        );
        // The heading still reads cleanly without color markup.
        assert!(out.contains("encoded  s1/m1/v1"), "heading shape: {out}");
    }

    // 14. Narrow terminal — text truncates, lines don't wrap past width.
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
        // The source-text echo is the line that's expected to obey
        // the terminal width. The static "next:" footer is a fixed
        // string and intentionally not clamped.
        let echo_line = out
            .lines()
            .find(|l| l.contains('"') && l.contains('x'))
            .expect("expected quoted echo line in narrow render");
        assert!(
            echo_line.chars().count() <= 44,
            "narrow render let echo line exceed budget: {echo_line}"
        );
        assert!(
            echo_line.contains('…'),
            "narrow render must middle-truncate long text: {echo_line}"
        );
    }

    // 15. JSON view keeps the raw zero LSN — sentinel is table-only.
    #[test]
    fn render_json_emits_raw_zero_lsn() {
        let mut r = sample();
        r.response.lsn = 0;
        let v = r.render_json(&ctx(OutputFormat::Json));
        assert_eq!(v["lsn"], 0);
    }

    // 16. JSON view exposes the dedup bool.
    #[test]
    fn render_json_carries_was_deduplicated() {
        let mut r = sample();
        r.response.was_deduplicated = true;
        let v = r.render_json(&ctx(OutputFormat::Json));
        assert_eq!(v["was_deduplicated"], true);
    }

    // 17. Each kind variant renders as its canonical string.
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

    // 18. Memory id renders as s{shard}/m{slot}/v{version}.
    #[test]
    fn render_table_short_id_format_is_sslot_mslot_vslot() {
        let mut r = sample();
        r.response.memory_id = MemoryId::pack(3, 42, 7).raw();
        let out = render(&r, OutputFormat::Table);
        assert!(out.contains("s3/m42/v7"), "expected short id form: {out}");
    }
}
