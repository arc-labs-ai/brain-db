//! User-facing error card.
//!
//! Card-style renderer that mirrors the shape of [`super::encode`] so
//! every error in the shell reads the same way — top/bottom rules,
//! `✗ CATEGORY` badge + numeric code in the heading, labelled body
//! rows for message / details / retry hint. The caller passes the
//! wire `code: u16` (from `ClientError::Server.code` or 0 for
//! client-side failures) plus the message string; the renderer
//! looks the rest up.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Card width cap — matches `encode::CARD_MAX_WIDTH` so two cards
/// side-by-side line up.
const CARD_MAX_WIDTH: usize = 80;
/// Body indent + label column, matching the encode card.
const LABEL_COL_WIDTH: usize = 8;
const BODY_INDENT: &str = "  ";

/// User-facing error wrapper.
///
/// `code` is the wire numeric code from `spec/03_wire_protocol/10_errors.md`.
/// `0` means "client-side / no wire code" (e.g. connect refused,
/// pool closed) — the renderer hides the code row in that case.
/// `message` is the server's (or SDK's) human string.
/// `details` is an optional pretty-printable JSON sidecar for things
/// like validation field paths or contention IDs; `retry_after_ms`
/// surfaces when the server suggests a backoff.
pub struct RenderableError {
    pub code: u16,
    pub message: String,
    pub details: Option<Value>,
    pub retry_after_ms: Option<u32>,
}

impl RenderableError {
    /// Helper: construct from a server frame.
    #[must_use]
    pub fn from_server(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            retry_after_ms: None,
        }
    }

    /// Helper: client-side failure with no wire code.
    #[must_use]
    pub fn client_side(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
            details: None,
            retry_after_ms: None,
        }
    }
}

impl Render for RenderableError {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        render_human(self, ctx, w)
    }

    fn render_wide(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        // Wide adds nothing today — errors are already compact.
        render_human(self, ctx, w)
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let info = code_info(self.code);
        json!({
            "code": self.code,
            "code_hex": format!("0x{:04x}", self.code),
            "name": info.name,
            "category": info.category,
            "retryable": info.retryable,
            "message": self.message,
            "details": self.details,
            "retry_after_ms": self.retry_after_ms,
        })
    }
}

fn render_human(e: &RenderableError, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
    let policy = ctx.policy;
    let theme = &ctx.theme;
    let width = policy.width.min(CARD_MAX_WIDTH);
    let info = code_info(e.code);

    // ── Top rule ──────────────────────────────────────────────────
    let rule = "─".repeat(width);
    writeln!(w, "{rule}")?;

    // ── Heading ──────────────────────────────────────────────────
    // `✗ CATEGORY` on the left; the wire code (decimal · hex) on
    // the right. Client-side errors hide the code cluster entirely.
    let badge_plain = format!("✗ {}", info.category.to_uppercase());
    let badge_painted = theme.paint(Token::Error, &badge_plain, policy).to_string();
    let (right_plain, right_painted) = if e.code == 0 {
        // No wire code — surface the short error name in muted text
        // so the heading still has a right cluster.
        let s = info.name.to_string();
        let painted = theme.paint(Token::Muted, &s, policy).to_string();
        (s, painted)
    } else {
        let plain = format!("code {} · 0x{:04x} · {}", e.code, e.code, info.name);
        let code_str = e.code.to_string();
        let hex_str = format!("0x{:04x}", e.code);
        let code_painted = theme.paint(Token::Value, &code_str, policy);
        let hex_painted = theme.paint(Token::Muted, &hex_str, policy);
        let name_painted = theme.paint(Token::Accent, info.name, policy);
        let painted = format!("code {code_painted} · {hex_painted} · {name_painted}");
        (plain, painted)
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

    // ── Body: message ────────────────────────────────────────────
    // Wrap the message into the card body's value column so a long
    // server diagnostic doesn't blow past the right rule. The first
    // visible line lands in the labelled row; continuation lines
    // are indented to the value column.
    let body_budget = width
        .saturating_sub(BODY_INDENT.len() + LABEL_COL_WIDTH + 2)
        .max(20);
    let wrapped = wrap_to_width(&e.message, body_budget);
    let mut lines = wrapped.into_iter();
    if let Some(first) = lines.next() {
        let painted = theme.paint(Token::Value, &first, policy);
        write_row(w, ctx, "message", &painted)?;
        let continuation_indent = " ".repeat(BODY_INDENT.len() + LABEL_COL_WIDTH + 2);
        for cont in lines {
            let painted = theme.paint(Token::Value, &cont, policy);
            writeln!(w, "{continuation_indent}{painted}")?;
        }
    }

    // ── Body: details (optional, pretty-printed JSON) ────────────
    if let Some(details) = &e.details {
        // Pretty-print so nested validation surfaces read naturally;
        // each line lands at the value column for alignment.
        let rendered =
            serde_json::to_string_pretty(details).unwrap_or_else(|_| details.to_string());
        let mut iter = rendered.lines();
        if let Some(first) = iter.next() {
            let painted = theme.paint(Token::Value, first, policy);
            write_row(w, ctx, "details", &painted)?;
            let continuation_indent = " ".repeat(BODY_INDENT.len() + LABEL_COL_WIDTH + 2);
            for cont in iter {
                let painted = theme.paint(Token::Value, cont, policy);
                writeln!(w, "{continuation_indent}{painted}")?;
            }
        }
    }

    // ── Body: retry verdict ─────────────────────────────────────
    // Spec §03/10 §6 maps category → retryability. Print one
    // labelled row so the operator sees the verdict without
    // consulting the spec; `retry_after_ms`, when present, joins
    // the same line.
    let (retry_glyph_token, retry_text) = if info.retryable {
        let mut s = "yes".to_string();
        if let Some(ms) = e.retry_after_ms {
            s.push_str(&format!(" — retry after {ms} ms"));
        } else {
            s.push_str(" — back off and retry");
        }
        (Token::Warn, s)
    } else if e.code == 0 {
        // Client-side errors have no retryability story; surface
        // whatever hint the variant carried.
        (Token::Muted, "n/a — client-side".to_string())
    } else {
        (Token::Error, "no — fix the input and resend".to_string())
    };
    let painted = theme.paint(retry_glyph_token, &retry_text, policy);
    write_row(w, ctx, "retry", &painted)?;

    // ── Bottom rule ──────────────────────────────────────────────
    writeln!(w, "{rule}")?;
    Ok(())
}

/// Place `badge` flush left and `right` flush against `width`, with
/// space-padding between. Pads against the *plain* (escape-stripped)
/// strings so ANSI escapes don't throw off the column math.
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

// ─── Wire-code → category / name / retryability lookup ─────────────────────

struct CodeInfo {
    name: &'static str,
    category: &'static str,
    retryable: bool,
}

/// Map a wire error code (spec §03/10 §3) to its short name, broad
/// category, and the default retry verdict. Unknown codes get a
/// generic `("Unknown", "Internal", true)` triple — clients still
/// see the raw number in the card so support diagnostics work.
fn code_info(code: u16) -> CodeInfo {
    let (name, category, retryable) = match code {
        // §3.1 Protocol — all non-retryable client bugs
        0x0001 => ("BadMagic", "Protocol", false),
        0x0002 => ("BadHeaderCrc", "Protocol", false),
        0x0003 => ("BadPayloadCrc", "Protocol", false),
        0x0004 => ("BadOpcode", "Protocol", false),
        0x0005 => ("BadVersion", "Protocol", false),
        0x0006 => ("BadFrame", "Protocol", false),
        0x0007 => ("OversizePayload", "Protocol", false),
        0x0008 => ("ReservedFieldNonZero", "Protocol", false),
        0x0009 => ("BadFlagCombination", "Protocol", false),
        0x000A => ("MalformedRkyv", "Protocol", false),
        0x000B => ("MalformedVector", "Protocol", false),
        // §3.2 Connection / handshake
        0x0020 => ("VersionNotSupported", "Authentication", false),
        0x0021 => ("NoSuchAuthMethod", "Authentication", false),
        0x0022 => ("Unauthenticated", "Authentication", false),
        0x0023 => ("NotAuthenticated", "Authentication", false),
        0x0024 => ("AuthBackendUnavailable", "Authentication", true),
        0x0025 => ("SessionExpired", "Authentication", false),
        // §3.3 Authorization
        0x0030 => ("PermissionDenied", "Authorization", false),
        0x0031 => ("AdminPermissionRequired", "Authorization", false),
        0x0032 => ("WrongShard", "Authorization", false),
        // §3.4 Validation
        0x0040 => ("InvalidArgument", "Validation", false),
        0x0041 => ("MissingRequiredField", "Validation", false),
        0x0042 => ("TextTooLarge", "Validation", false),
        0x0043 => ("TextEmpty", "Validation", false),
        0x0044 => ("BadContextId", "Validation", false),
        0x0045 => ("BadMemoryKind", "Validation", false),
        0x0046 => ("BadEdgeKind", "Validation", false),
        0x0047 => ("BadStrategyHint", "Validation", false),
        0x0048 => ("TopKOutOfRange", "Validation", false),
        0x0049 => ("BudgetTooLarge", "Validation", false),
        0x004A => ("BadModelFingerprint", "Validation", false),
        0x004B => ("PredicateNotInSchema", "Validation", false),
        0x004C => ("RelationTypeNotInSchema", "Validation", false),
        // §3.5 Not found
        0x0050 => ("MemoryNotFound", "NotFound", false),
        0x0051 => ("ContextNotFound", "NotFound", false),
        0x0052 => ("SubscriptionNotFound", "NotFound", false),
        0x0053 => ("SnapshotNotFound", "NotFound", false),
        0x0054 => ("TxnNotFound", "NotFound", false),
        // §3.6 Conflict
        0x0060 => ("IdempotencyConflict", "Conflict", false),
        0x0061 => ("TransactionConflict", "Conflict", false),
        0x0062 => ("TxnExpired", "Conflict", false),
        0x0063 => ("StreamIdInUse", "Conflict", false),
        0x0064 => ("SubscriptionLsnTooOld", "Conflict", false),
        0x0065 => ("CardinalityViolation", "Conflict", false),
        // §3.7 Resource exhausted — retryable after backoff
        0x0070 => ("OutOfSlots", "ResourceExhausted", true),
        0x0071 => ("OutOfDisk", "ResourceExhausted", true),
        0x0072 => ("OutOfMemory", "ResourceExhausted", true),
        0x0073 => ("RateLimited", "ResourceExhausted", true),
        0x0074 => ("StreamLimitExceeded", "ResourceExhausted", true),
        0x0075 => ("ConnectionLimitExceeded", "ResourceExhausted", true),
        0x0076 => ("TransactionLimitExceeded", "ResourceExhausted", true),
        // §3.8 Internal — retry once
        0x0080 => ("Internal", "Internal", true),
        0x0081 => ("StorageError", "Internal", true),
        0x0082 => ("IndexError", "Internal", true),
        0x0083 => ("EmbeddingError", "Internal", true),
        0x0084 => ("MetadataError", "Internal", true),
        // §3.9 Unavailable — retry per retry_after_ms
        0x0090 => ("ShardUnavailable", "Unavailable", true),
        0x0091 => ("Overloaded", "Unavailable", true),
        0x0092 => ("Restarting", "Unavailable", true),
        0x0093 => ("Maintenance", "Unavailable", true),
        // Client-side / unknown
        0 => ("ClientError", "Client", false),
        _ => ("Unknown", "Internal", true),
    };
    CodeInfo {
        name,
        category,
        retryable,
    }
}

/// Word-wrap `text` to lines no longer than `budget` chars. Breaks
/// on whitespace; single tokens longer than `budget` land alone on
/// their own line uncut (we'd rather overflow than slice a token
/// like a long hex id).
fn wrap_to_width(text: &str, budget: usize) -> Vec<String> {
    if budget == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let projected = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if projected <= budget {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn ctx(format: OutputFormat) -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format,
        }
    }

    fn render(e: &RenderableError) -> String {
        let mut buf = Vec::new();
        e.render_table(&ctx(OutputFormat::Table), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn idempotency_conflict_renders_friendly_card() {
        let e = RenderableError::from_server(
            0x0060,
            "encode request_id replay with different params: request_id=22222222",
        );
        let s = render(&e);
        assert!(s.contains("✗ CONFLICT"), "badge: {s}");
        assert!(s.contains("code 96"), "decimal code: {s}");
        assert!(s.contains("0x0060"), "hex code: {s}");
        assert!(s.contains("IdempotencyConflict"), "name: {s}");
        assert!(s.contains("message"), "message label: {s}");
        assert!(s.contains("retry"), "retry row: {s}");
        assert!(
            s.contains("no — fix the input"),
            "retry verdict for non-retryable: {s}"
        );
        // Top + bottom rules.
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines.first().is_some_and(|l| l.chars().all(|c| c == '─')),
            "missing top rule: {s}"
        );
        assert!(
            lines.last().is_some_and(|l| l.chars().all(|c| c == '─')),
            "missing bottom rule: {s}"
        );
    }

    #[test]
    fn invalid_argument_renders_validation_card() {
        let e = RenderableError::from_server(
            0x0040,
            "kind: consolidated memories are produced by background workers, not by direct encode. Use --kind episodic or --kind semantic.",
        );
        let s = render(&e);
        assert!(s.contains("✗ VALIDATION"), "badge: {s}");
        assert!(s.contains("InvalidArgument"), "name: {s}");
        assert!(s.contains("consolidated memories"), "message body: {s}");
        // Long message should wrap rather than blow past the right rule.
        let body_lines: Vec<&str> = s.lines().filter(|l| l.contains("--kind")).collect();
        for line in body_lines {
            assert!(
                line.chars().count() <= CARD_MAX_WIDTH + 4,
                "body line over budget: {line:?}"
            );
        }
    }

    #[test]
    fn rate_limited_renders_retryable_card_with_hint() {
        let mut e = RenderableError::from_server(0x0073, "slow down");
        e.retry_after_ms = Some(500);
        let s = render(&e);
        assert!(s.contains("✗ RESOURCEEXHAUSTED"), "badge: {s}");
        assert!(s.contains("RateLimited"), "name: {s}");
        assert!(
            s.contains("yes — retry after 500 ms"),
            "retry hint missing: {s}"
        );
    }

    #[test]
    fn client_side_error_hides_code_block() {
        let e = RenderableError::client_side("connect failed: connection refused");
        let s = render(&e);
        assert!(s.contains("✗ CLIENT"), "badge: {s}");
        assert!(
            !s.contains("0x0000"),
            "client side should not show hex code: {s}"
        );
        assert!(
            s.contains("n/a — client-side"),
            "retry row should mark client-side: {s}"
        );
    }

    #[test]
    fn json_carries_code_name_category_and_retryable() {
        let e = RenderableError::from_server(0x0060, "boom");
        let v = e.render_json(&ctx(OutputFormat::Json));
        assert_eq!(v["code"], 0x0060);
        assert_eq!(v["code_hex"], "0x0060");
        assert_eq!(v["name"], "IdempotencyConflict");
        assert_eq!(v["category"], "Conflict");
        assert_eq!(v["retryable"], false);
        assert_eq!(v["message"], "boom");
    }
}
