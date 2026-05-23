//! Transaction lifecycle renderers (begin / commit / abort).
//!
//! Card-style output that matches the encode + error cards: top /
//! bottom rules, a status badge and short txn id on the heading, a
//! single body row for the operation count, and a footer hint that
//! tells the operator what to do next.

use std::io::{self, Write};

use brain_protocol::envelope::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};
use serde_json::{json, Value};

use crate::render::fmt_txn_id;
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Card width cap — matches `encode::CARD_MAX_WIDTH` so two cards
/// side-by-side line up cleanly.
const CARD_MAX_WIDTH: usize = 80;
/// Body indent + label column, matching encode + error cards.
const LABEL_COL_WIDTH: usize = 8;
const BODY_INDENT: &str = "  ";

pub struct TxnBeginRendered(pub TxnBeginResponse);
pub struct TxnCommitRendered(pub TxnCommitResponse);
pub struct TxnAbortRendered(pub TxnAbortResponse);

impl Render for TxnBeginRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        render_card(
            w,
            ctx,
            CardSpec {
                badge: "◆ TXN OPEN",
                badge_token: Token::Success,
                right_cluster: format!("idle {} s", r.timeout_seconds),
                txn_id: &r.txn_id,
                body_label: "idle",
                body_value: format!("{} s — every op resets the deadline", r.timeout_seconds),
                footer: Some(FooterHint {
                    glyph: "→",
                    glyph_token: Token::Accent,
                    text: "encode / link / forget now inherits this txn — \
                           `txn commit` to durabilize"
                        .into(),
                }),
            },
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.0;
        json!({
            "txn_id": fmt_txn_id(&r.txn_id),
            "timeout_seconds": r.timeout_seconds,
            "started_at_unix_nanos": r.started_at_unix_nanos,
        })
    }
}

impl Render for TxnCommitRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let count = r.operations_applied;
        let ops_word = if count == 1 { "op" } else { "ops" };
        render_card(
            w,
            ctx,
            CardSpec {
                badge: "✓ TXN COMMITTED",
                badge_token: Token::Success,
                right_cluster: format!("{count} {ops_word}"),
                txn_id: &r.txn_id,
                body_label: "applied",
                body_value: format!(
                    "{count} operation{plural} durably committed",
                    plural = if count == 1 { "" } else { "s" },
                ),
                footer: None,
            },
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.0;
        json!({
            "txn_id": fmt_txn_id(&r.txn_id),
            "operations_applied": r.operations_applied,
            "committed_at_unix_nanos": r.committed_at_unix_nanos,
        })
    }
}

impl Render for TxnAbortRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let count = r.operations_discarded;
        let ops_word = if count == 1 { "op" } else { "ops" };
        render_card(
            w,
            ctx,
            CardSpec {
                badge: "⟲ TXN ABORTED",
                badge_token: Token::Warn,
                right_cluster: format!("{count} {ops_word}"),
                txn_id: &r.txn_id,
                body_label: "discarded",
                body_value: format!(
                    "{count} operation{plural} dropped without writing",
                    plural = if count == 1 { "" } else { "s" },
                ),
                footer: None,
            },
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.0;
        json!({
            "txn_id": fmt_txn_id(&r.txn_id),
            "operations_discarded": r.operations_discarded,
        })
    }
}

// ─── Shared card scaffolding ──────────────────────────────────────────────

struct CardSpec<'a> {
    badge: &'a str,
    badge_token: Token,
    right_cluster: String,
    txn_id: &'a [u8; 16],
    body_label: &'a str,
    body_value: String,
    footer: Option<FooterHint>,
}

struct FooterHint {
    glyph: &'static str,
    glyph_token: Token,
    text: String,
}

fn render_card(w: &mut dyn Write, ctx: &RenderCtx, spec: CardSpec<'_>) -> io::Result<()> {
    let policy = ctx.policy;
    let theme = &ctx.theme;
    let width = policy.width.min(CARD_MAX_WIDTH);

    // ── Top rule ──────────────────────────────────────────────────
    let rule = "─".repeat(width);
    writeln!(w, "{rule}")?;

    // ── Heading: badge left, txn-short · right_cluster right ─────
    let badge_painted = theme
        .paint(spec.badge_token, spec.badge, policy)
        .to_string();
    let short_txn = fmt_txn_short(spec.txn_id);
    let right_plain = format!("{} · {}", short_txn, spec.right_cluster);
    let short_painted = theme.paint(Token::Accent, &short_txn, policy);
    let cluster_painted = theme.paint(Token::Muted, &spec.right_cluster, policy);
    let right_painted = format!("{short_painted} · {cluster_painted}");
    write_heading(
        w,
        width,
        spec.badge,
        &badge_painted,
        &right_plain,
        &right_painted,
    )?;
    writeln!(w)?;

    // ── Body: txn id (full canonical) + the count row ────────────
    let txn_full = fmt_txn_id(spec.txn_id);
    let txn_painted = theme.paint(Token::Accent, &txn_full, policy).to_string();
    write_row(w, ctx, "txn", &txn_painted)?;
    let value_painted = theme
        .paint(Token::Value, &spec.body_value, policy)
        .to_string();
    write_row(w, ctx, spec.body_label, &value_painted)?;

    // ── Footer hint ──────────────────────────────────────────────
    if let Some(hint) = spec.footer {
        writeln!(w)?;
        let glyph = theme.paint(hint.glyph_token, hint.glyph, policy);
        let text = theme.paint(Token::Muted, &hint.text, policy);
        writeln!(w, "{BODY_INDENT}{glyph} {text}")?;
    }

    // ── Bottom rule ──────────────────────────────────────────────
    writeln!(w, "{rule}")?;
    Ok(())
}

fn write_heading(
    w: &mut dyn Write,
    width: usize,
    badge_plain: &str,
    badge_painted: &str,
    right_plain: &str,
    right_painted: &str,
) -> io::Result<()> {
    let indent = BODY_INDENT.len();
    let badge_w = badge_plain.chars().count();
    let right_w = right_plain.chars().count();
    let gap = width
        .saturating_sub(indent)
        .saturating_sub(badge_w)
        .saturating_sub(right_w)
        .max(2);
    let spaces = " ".repeat(gap);
    writeln!(w, "{BODY_INDENT}{badge_painted}{spaces}{right_painted}")
}

fn write_row(w: &mut dyn Write, ctx: &RenderCtx, label: &str, value: &str) -> io::Result<()> {
    let painted = ctx.theme.paint(Token::Label, label, ctx.policy);
    let pad = LABEL_COL_WIDTH.saturating_sub(label.chars().count());
    let spaces = " ".repeat(pad);
    writeln!(w, "{BODY_INDENT}{painted}{spaces}  {value}")
}

/// First 8 hex chars of the canonical txn id (matches the short
/// form Brain uses for memory ids elsewhere). Operators recognise
/// the txn at a glance without needing the full 32-char form on
/// the heading row.
fn fmt_txn_short(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(10);
    s.push_str("0x");
    for b in &bytes[..4] {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    fn render<R: Render>(r: &R) -> String {
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn begin_card_shows_badge_and_idle_window() {
        let r = TxnBeginRendered(TxnBeginResponse {
            txn_id: [0xAB; 16],
            timeout_seconds: 300,
            started_at_unix_nanos: 0,
        });
        let s = render(&r);
        assert!(s.contains("◆ TXN OPEN"), "badge: {s}");
        assert!(s.contains("idle 300 s"), "right cluster: {s}");
        assert!(s.contains("0xabababab…"), "short txn id: {s}");
        assert!(s.contains("every op resets the deadline"), "body: {s}");
        assert!(s.contains("→"), "footer arrow: {s}");
        // Rules
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines.first().is_some_and(|l| l.chars().all(|c| c == '─')));
        assert!(lines.last().is_some_and(|l| l.chars().all(|c| c == '─')));
    }

    #[test]
    fn commit_card_pluralizes_correctly() {
        let one = TxnCommitRendered(TxnCommitResponse {
            txn_id: [0x12; 16],
            committed_at_unix_nanos: 0,
            operations_applied: 1,
        });
        let s = render(&one);
        assert!(s.contains("1 op"), "singular: {s}");
        assert!(s.contains("1 operation durably committed"), "body: {s}");
        assert!(!s.contains("ops"), "should not pluralize: {s}");

        let many = TxnCommitRendered(TxnCommitResponse {
            txn_id: [0x34; 16],
            committed_at_unix_nanos: 0,
            operations_applied: 7,
        });
        let s = render(&many);
        assert!(s.contains("7 ops"), "plural: {s}");
        assert!(s.contains("7 operations durably committed"), "body: {s}");
    }

    #[test]
    fn abort_card_uses_warn_token_for_discarded() {
        let r = TxnAbortRendered(TxnAbortResponse {
            txn_id: [0x99; 16],
            operations_discarded: 3,
        });
        let s = render(&r);
        assert!(s.contains("⟲ TXN ABORTED"), "badge: {s}");
        assert!(s.contains("3 ops"), "count: {s}");
        assert!(
            s.contains("3 operations dropped without writing"),
            "body: {s}"
        );
    }

    #[test]
    fn json_preserves_existing_field_shape() {
        let r = TxnBeginRendered(TxnBeginResponse {
            txn_id: [0xCD; 16],
            timeout_seconds: 60,
            started_at_unix_nanos: 1_700_000_000_000_000_000,
        });
        let v = r.render_json(&ctx());
        assert_eq!(v["timeout_seconds"], 60);
        assert_eq!(v["txn_id"], "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
    }
}
