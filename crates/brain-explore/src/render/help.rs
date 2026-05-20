//! In-REPL help renderers.
//!
//! Three shapes share this file because they're variants of the same
//! concern (in-REPL help) and routing branches in brain-shell pick the
//! variant to instantiate:
//!
//!   * [`HelpTopLevel`] — sectioned verb directory. No card frame: a
//!     full directory of cognitive verbs + knowledge verbs + meta
//!     commands runs ~50 lines, and a frame around that height would
//!     dominate the screen and bury the content. Sections delineated
//!     by accent-coloured headers do the job a frame would otherwise.
//!   * [`HelpVerb`] — per-verb detail card with top + bottom rules.
//!     One concept per card, so the frame works the way it does on
//!     encode / info / banner — visually anchors the focused content.
//!   * [`HelpUnknown`] — single muted line for a bad lookup. No frame,
//!     no decoration; an error message that mimics every other "try
//!     `help` for the list" hint in the shell.
//!
//! All three impl [`Render`] so the shell's dispatch loop routes them
//! through the same `brain_explore::dispatch` call every other verb
//! uses.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Width of the signature column on top-level help. Sized to fit the
/// longest verb signature row (`subscribe`) plus a comfortable gap
/// before the description column begins.
const TOP_LEVEL_SIGNATURE_WIDTH: usize = 12;

/// Width of the label column on per-verb cards (`Usage`, `Description`,
/// `See also`). Matches the convention in info.rs so two cards rendered
/// back-to-back have their label gutters at the same screen column.
const VERB_LABEL_WIDTH: usize = 10;

/// Card width cap mirrored from encode.rs. The rule that frames a
/// per-verb card honours the same width so the visual language stays
/// consistent.
const CARD_MAX_WIDTH: usize = 80;

// ── HelpTopLevel ────────────────────────────────────────────────────

/// Sectioned listing of every verb + meta command, plus optional
/// footer hints. Renders without a frame: see the module doc for the
/// "too tall for a card" reasoning.
pub struct HelpTopLevel {
    pub sections: Vec<HelpSection>,
    pub footer: Vec<String>,
}

/// One labeled section inside [`HelpTopLevel`]. `title` is the screen
/// row (painted accent); `items` are the rows underneath; `note` is an
/// optional muted parenthetical printed right after the title — used
/// for "(session-only by default; `\config set` persists)" annotations
/// that explain a section's scope without inflating the row list.
pub struct HelpSection {
    pub title: String,
    pub note: Option<String>,
    pub items: Vec<HelpItem>,
}

/// One row inside a [`HelpSection`]. `signature` is the verb form
/// (e.g. `"encode <TEXT> [--context N]"`); `description` is a brief
/// one-liner. Description may be empty — some rows (e.g. `\agent`)
/// are self-explanatory from the signature alone.
pub struct HelpItem {
    pub signature: String,
    pub description: String,
}

impl Render for HelpTopLevel {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;

        for (idx, section) in self.sections.iter().enumerate() {
            if idx > 0 {
                writeln!(w)?;
            }
            // Section header line: ACCENT title + optional muted note.
            // The note appears after two spaces so the eye groups it
            // with the title without competing visually.
            let title = theme.paint(Token::Accent, &section.title, policy);
            match &section.note {
                Some(note) => {
                    let note_painted = theme.paint(Token::Muted, note, policy);
                    writeln!(w, "{title}  {note_painted}")?;
                }
                None => writeln!(w, "{title}")?,
            }
            for item in &section.items {
                write_top_level_row(w, theme, policy, item)?;
            }
        }

        if !self.footer.is_empty() {
            writeln!(w)?;
            for line in &self.footer {
                let painted = theme.paint(Token::Muted, line, policy);
                writeln!(w, "{painted}")?;
            }
        }
        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        json!({
            "kind": "help-top-level",
            "sections": self.sections.iter().map(|s| json!({
                "title": s.title,
                "note": s.note,
                "items": s.items.iter().map(|i| json!({
                    "signature": i.signature,
                    "description": i.description,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "footer": self.footer,
        })
    }
}

fn write_top_level_row(
    w: &mut dyn Write,
    theme: &crate::theme::Theme,
    policy: crate::TermPolicy,
    item: &HelpItem,
) -> io::Result<()> {
    // Signature goes in a padded column so descriptions line up
    // across rows. Pad before paint so the ANSI escape sequence
    // doesn't shift the visible column boundary.
    let padded = if item.signature.len() < TOP_LEVEL_SIGNATURE_WIDTH {
        format!("{:<TOP_LEVEL_SIGNATURE_WIDTH$}", item.signature)
    } else {
        // Signature already exceeds the column; keep it intact and
        // let the description start one space after. Truncating
        // would lose the meaningful suffix; wrapping would break the
        // grid worse than running long.
        format!("{} ", item.signature)
    };
    let sig = theme.paint(Token::Label, &padded, policy);
    if item.description.is_empty() {
        writeln!(w, "  {sig}")
    } else {
        let desc = theme.paint(Token::Value, &item.description, policy);
        writeln!(w, "  {sig}  {desc}")
    }
}

// ── HelpVerb ────────────────────────────────────────────────────────

/// Per-verb detail card. Top + bottom rules frame the focused content:
/// usage signature, optional flag + source tables, prose notes, an
/// example, see-also pointers, and a reference block.
///
/// Each section suppresses when its field is empty / `None`, so a
/// verb that only wants `usage + notes + see_also` (the pre-H2 shape)
/// renders identically to before.
pub struct HelpVerb {
    pub name: String,
    pub tagline: String,
    pub usage: Vec<String>,
    /// Flag-by-flag reference. Empty → no `Flags` block is rendered.
    pub flags: Vec<HelpFlagRow>,
    /// Alternate sources for the verb's main argument (e.g. encode's
    /// `--from-file`, `--from-stdin`, `--vector`). Empty → no `Sources`
    /// block.
    pub sources: Vec<HelpFlagRow>,
    /// Prose paragraphs. Rendered under a `Notes` label, not
    /// `Description`, so the section name matches its content shape
    /// once Flags + Sources carry the structured part of the card.
    pub description: Vec<String>,
    /// Single-line example. `None` → no `Example` block.
    pub example: Option<String>,
    pub see_also: Vec<String>,
    /// Pointer to clap's full reference + the per-verb markdown doc.
    /// `None` → no `Reference` block.
    pub reference: Option<HelpReference>,
}

/// One row inside the [`HelpVerb`] `flags` or `sources` tables.
/// `signature` is the flag form (`"--context N"`), `description` is a
/// one-line semantics blurb (range, default, gotcha) — clap covers
/// syntax, this carries meaning.
pub struct HelpFlagRow {
    pub signature: String,
    pub description: String,
}

/// Reference pointers shown at the bottom of a [`HelpVerb`] card.
/// Surfaces the canonical clap-generated help and the markdown deep
/// dive so a reader knows where to go next when the card is too brief.
pub struct HelpReference {
    /// Clap-generated form, e.g. `"encode --help"`. Always rendered.
    pub clap_command: String,
    /// Optional markdown doc path, e.g.
    /// `"docs/reference/shell/commands/encode.md"`. `None` → only the
    /// clap line is shown.
    pub doc_path: Option<String>,
}

impl Render for HelpVerb {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;
        let lbl = |s: &str| theme.paint(Token::Label, s, policy).into_owned();
        let val = |s: &str| theme.paint(Token::Value, s, policy).into_owned();
        let muted = |s: &str| theme.paint(Token::Muted, s, policy).into_owned();
        let accent = |s: &str| theme.paint(Token::Accent, s, policy).into_owned();

        // ── Top rule ──
        // No leading blank: content sits flush against the rule so
        // the card reads as a single visual unit (matches banner +
        // encode card discipline).
        let width = policy.width.min(CARD_MAX_WIDTH).max(40);
        let rule: String = "─".repeat(width);
        writeln!(w, "{}", muted(&rule))?;

        // ── Title line: NAME · tagline ──
        // Uppercase the verb so the eye can land on "what does this
        // card cover?" without reading anything else first.
        let name_upper = self.name.to_ascii_uppercase();
        let name_painted = accent(&name_upper);
        let sep = muted("·");
        let tagline_painted = accent(&self.tagline);
        writeln!(w, "  {name_painted}  {sep}  {tagline_painted}")?;

        // Inter-section blanks land BEFORE each section (not after),
        // so the last section sits flush against the bottom rule no
        // matter which one happens to be the last.

        // ── Usage block ──
        if !self.usage.is_empty() {
            writeln!(w)?;
            let usage_label = lbl(&pad_verb_label("Usage"));
            let first = val(&self.usage[0]);
            writeln!(w, "  {usage_label}  {first}")?;
            let blank_label: String = " ".repeat(VERB_LABEL_WIDTH);
            for line in self.usage.iter().skip(1) {
                let cont = val(line);
                writeln!(w, "  {blank_label}  {cont}")?;
            }
        }

        // ── Description ──
        if !self.description.is_empty() {
            writeln!(w)?;
            let desc_label = lbl(&pad_verb_label("Description"));
            // Description paragraphs sit indented under the label —
            // not painted Value (no value token) since description
            // is prose, not data.
            writeln!(w, "  {desc_label}")?;
            for (idx, para) in self.description.iter().enumerate() {
                if idx > 0 {
                    writeln!(w)?;
                }
                // Two-space indent past the label column so the
                // paragraph reads as a block, not as misaligned rows.
                writeln!(w, "    {para}")?;
            }
        }

        // ── See also ──
        if !self.see_also.is_empty() {
            writeln!(w)?;
            let label = lbl(&pad_verb_label("See also"));
            let links: Vec<String> = self
                .see_also
                .iter()
                .map(|v| muted(&format!("help {v}")))
                .collect();
            let joined = links.join(&muted("  ·  "));
            writeln!(w, "  {label}  {joined}")?;
        }

        // ── Bottom rule ──
        writeln!(w, "{}", muted(&rule))?;
        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let flags: Vec<Value> = self
            .flags
            .iter()
            .map(|f| json!({ "signature": f.signature, "description": f.description }))
            .collect();
        let sources: Vec<Value> = self
            .sources
            .iter()
            .map(|f| json!({ "signature": f.signature, "description": f.description }))
            .collect();
        let reference = self.reference.as_ref().map(|r| {
            json!({
                "clap_command": r.clap_command,
                "doc_path": r.doc_path,
            })
        });
        json!({
            "kind": "help-verb",
            "name": self.name,
            "tagline": self.tagline,
            "usage": self.usage,
            "flags": flags,
            "sources": sources,
            "description": self.description,
            "example": self.example,
            "see_also": self.see_also,
            "reference": reference,
        })
    }
}

fn pad_verb_label(label: &str) -> String {
    format!("{label:<VERB_LABEL_WIDTH$}")
}

// ── HelpUnknown ─────────────────────────────────────────────────────

/// "no help for `wibble`" fallback. Renders as a single muted line so
/// it slots into the REPL output the same way other one-line errors do.
pub struct HelpUnknown {
    pub verb: String,
}

impl Render for HelpUnknown {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let line = format!("no help for `{}`. Try `help` for the list.", self.verb);
        let painted = ctx.theme.paint(Token::Muted, &line, ctx.policy);
        writeln!(w, "{painted}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        json!({
            "kind": "help-unknown",
            "verb": self.verb,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn render<R: Render>(item: &R, format: OutputFormat) -> String {
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format,
        };
        let mut buf = Vec::new();
        crate::dispatch(item, &ctx, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    fn sample_top_level() -> HelpTopLevel {
        HelpTopLevel {
            sections: vec![
                HelpSection {
                    title: "COGNITIVE VERBS".into(),
                    note: None,
                    items: vec![
                        HelpItem {
                            signature: "encode".into(),
                            description: "write a memory".into(),
                        },
                        HelpItem {
                            signature: "recall".into(),
                            description: "find similar memories".into(),
                        },
                    ],
                },
                HelpSection {
                    title: "META".into(),
                    note: Some("(session-only by default)".into()),
                    items: vec![HelpItem {
                        signature: "quit".into(),
                        description: "exit the shell".into(),
                    }],
                },
            ],
            footer: vec!["Tip: bare `brain` mints a fresh agent.".into()],
        }
    }

    fn sample_verb() -> HelpVerb {
        HelpVerb {
            name: "encode".into(),
            tagline: "write a memory".into(),
            usage: vec![
                "encode <TEXT> [--context N] [--kind episodic|semantic|consolidated]".into(),
                "       [--salience F] [--allow-duplicate] [--txn HEX]".into(),
            ],
            flags: vec![],
            sources: vec![],
            description: vec![
                "Stores text as a memory. Inherits the session's sticky --context.".into(),
                "Deduplication is ON by default.".into(),
            ],
            example: None,
            see_also: vec!["recall".into(), "forget".into()],
            reference: None,
        }
    }

    // ── HelpTopLevel ────────────────────────────────────────────

    #[test]
    fn top_level_render_shows_each_section_header() {
        let out = render(&sample_top_level(), OutputFormat::Table);
        assert!(out.contains("COGNITIVE VERBS"), "missing section: {out}");
        assert!(out.contains("META"), "missing section: {out}");
    }

    #[test]
    fn top_level_render_shows_signature_then_description_columns() {
        let out = render(&sample_top_level(), OutputFormat::Table);
        // The signature column is padded to TOP_LEVEL_SIGNATURE_WIDTH;
        // even with the short "encode" word, the description should
        // appear on the same line after the padding.
        let line = out
            .lines()
            .find(|l| l.contains("encode") && l.contains("write a memory"))
            .expect("encode row");
        // The signature padding should put at least the column width
        // of space between the start of the signature and the start
        // of the description.
        let sig_idx = line.find("encode").unwrap();
        let desc_idx = line.find("write a memory").unwrap();
        assert!(
            desc_idx >= sig_idx + TOP_LEVEL_SIGNATURE_WIDTH,
            "description not in its column ({sig_idx} → {desc_idx}): {line}"
        );
    }

    #[test]
    fn top_level_render_includes_footer_when_present() {
        let out = render(&sample_top_level(), OutputFormat::Table);
        assert!(
            out.contains("Tip: bare `brain` mints a fresh agent."),
            "missing footer: {out}"
        );
    }

    #[test]
    fn top_level_render_skips_section_note_when_none() {
        // First section has no note; second section has one. Confirm
        // the first section's header line is just the title, no
        // trailing parenthetical-style annotation.
        let out = render(&sample_top_level(), OutputFormat::Table);
        let cog_line = out
            .lines()
            .find(|l| l.contains("COGNITIVE VERBS"))
            .expect("cog header");
        assert!(
            !cog_line.contains('('),
            "section without note must not show parens: {cog_line}"
        );
        // Second section's note should be present somewhere.
        assert!(
            out.contains("(session-only by default)"),
            "section note missing: {out}"
        );
    }

    // ── HelpVerb ────────────────────────────────────────────────

    #[test]
    fn verb_render_has_top_and_bottom_rules() {
        let out = render(&sample_verb(), OutputFormat::Table);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.first().unwrap_or(&"").contains("─"),
            "missing top rule"
        );
        assert!(
            lines.last().unwrap_or(&"").contains("─"),
            "missing bottom rule"
        );
    }

    #[test]
    fn verb_render_shows_uppercase_name_and_tagline() {
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(out.contains("ENCODE"), "missing uppercase name: {out}");
        assert!(out.contains("write a memory"), "missing tagline: {out}");
    }

    #[test]
    fn verb_render_shows_usage_block_under_label() {
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(out.contains("Usage"), "missing Usage label: {out}");
        assert!(
            out.contains("encode <TEXT>"),
            "missing first usage line: {out}"
        );
        assert!(
            out.contains("[--salience F]"),
            "missing continuation usage line: {out}"
        );
    }

    #[test]
    fn verb_render_shows_description_paragraphs() {
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(out.contains("Description"), "missing Description label");
        assert!(out.contains("Stores text as a memory"));
        assert!(out.contains("Deduplication is ON by default"));
    }

    #[test]
    fn verb_render_shows_see_also_when_non_empty() {
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(out.contains("See also"), "missing See also label: {out}");
        assert!(out.contains("help recall"), "missing see-also link");
        assert!(out.contains("help forget"), "missing see-also link");
    }

    #[test]
    fn verb_render_omits_see_also_when_empty() {
        let mut verb = sample_verb();
        verb.see_also.clear();
        let out = render(&verb, OutputFormat::Table);
        assert!(
            !out.contains("See also"),
            "See also must be suppressed when empty: {out}"
        );
    }

    // ── HelpUnknown ─────────────────────────────────────────────

    #[test]
    fn unknown_render_is_one_muted_line() {
        let unknown = HelpUnknown {
            verb: "wibble".into(),
        };
        let out = render(&unknown, OutputFormat::Table);
        assert!(out.contains("no help for `wibble`"));
        // One newline → exactly one line of content.
        assert_eq!(out.lines().count(), 1, "expected single line: {out}");
    }

    // ── JSON envelopes ──────────────────────────────────────────

    #[test]
    fn json_envelope_shape_for_each_variant() {
        let top = render(&sample_top_level(), OutputFormat::Json);
        let v: Value = serde_json::from_str(&top).expect("parse top json");
        assert_eq!(v["kind"], "help-top-level");
        assert!(v["sections"].is_array());
        assert!(v["footer"].is_array());

        let verb = render(&sample_verb(), OutputFormat::Json);
        let v: Value = serde_json::from_str(&verb).expect("parse verb json");
        assert_eq!(v["kind"], "help-verb");
        assert_eq!(v["name"], "encode");
        assert_eq!(v["tagline"], "write a memory");
        assert!(v["usage"].is_array());
        assert!(v["description"].is_array());
        assert!(v["see_also"].is_array());

        let unknown = HelpUnknown {
            verb: "wibble".into(),
        };
        let out = render(&unknown, OutputFormat::Json);
        let v: Value = serde_json::from_str(&out).expect("parse unknown json");
        assert_eq!(v["kind"], "help-unknown");
        assert_eq!(v["verb"], "wibble");
    }
}
