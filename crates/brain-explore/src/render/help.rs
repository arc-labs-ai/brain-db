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

        // ── Flags block ──
        // Per-section col-1 width keeps short-flag verbs (forget) tight
        // while still accommodating long flags (--wait-for-extraction).
        if !self.flags.is_empty() {
            writeln!(w)?;
            let flags_label = lbl(&pad_verb_label("Flags"));
            writeln!(w, "  {flags_label}")?;
            write_flag_rows(w, theme, policy, width, &self.flags)?;
        }

        // ── Sources block ──
        // Same row shape as Flags; separate section because "alternate
        // ways to supply the verb's main argument" is a distinct mental
        // category from "knobs that tweak behaviour."
        if !self.sources.is_empty() {
            writeln!(w)?;
            let sources_label = lbl(&pad_verb_label("Sources"));
            writeln!(w, "  {sources_label}")?;
            write_flag_rows(w, theme, policy, width, &self.sources)?;
        }

        // ── Notes (prose) ──
        // Renamed from "Description" because the description shape moved
        // out into Flags + Sources tables; what's left is genuine notes.
        if !self.description.is_empty() {
            writeln!(w)?;
            let notes_label = lbl(&pad_verb_label("Notes"));
            writeln!(w, "  {notes_label}")?;
            // Body wraps to card width minus the 4-char hanging indent.
            let body_width = width.saturating_sub(4).max(20);
            for (idx, para) in self.description.iter().enumerate() {
                if idx > 0 {
                    writeln!(w)?;
                }
                for line in wrap_lines(para, body_width) {
                    writeln!(w, "    {line}")?;
                }
            }
        }

        // ── Example ──
        // One line; sits between prose and pointers because it's the
        // "show me how to use it" pivot point.
        if let Some(example) = &self.example {
            writeln!(w)?;
            let example_label = lbl(&pad_verb_label("Example"));
            let example_painted = val(example);
            writeln!(w, "  {example_label}  {example_painted}")?;
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

        // ── Reference (clap + markdown deep dive) ──
        // Goes after See also because it's the "fall through to the
        // canonical source" pointer — clap is authoritative on syntax,
        // the markdown doc is authoritative on the deep dive.
        if let Some(reference) = &self.reference {
            writeln!(w)?;
            let ref_label = lbl(&pad_verb_label("Reference"));
            let clap_painted = muted(&reference.clap_command);
            writeln!(w, "  {ref_label}  {clap_painted}")?;
            if let Some(path) = &reference.doc_path {
                let blank_label: String = " ".repeat(VERB_LABEL_WIDTH);
                let path_painted = muted(path);
                writeln!(w, "  {blank_label}  {path_painted}")?;
            }
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

/// Per-section signature column width is the longest signature in
/// the section, clamped to this range. The lower bound keeps short-flag
/// verbs from collapsing the description column against the flag column;
/// the upper bound prevents a single absurdly long flag from pushing the
/// description column off the right edge of the card.
const FLAG_SIG_MIN_WIDTH: usize = 12;
const FLAG_SIG_MAX_WIDTH: usize = 24;

/// Render a Flags / Sources block. Computes the column-1 width from
/// the rows themselves (so each card pays only for what it needs);
/// signatures that exceed the cap drop their description to the next
/// line, indented to column 2.
fn write_flag_rows(
    w: &mut dyn Write,
    theme: &crate::theme::Theme,
    policy: crate::TermPolicy,
    card_width: usize,
    rows: &[HelpFlagRow],
) -> io::Result<()> {
    let sig_col = rows
        .iter()
        .map(|r| r.signature.len())
        .max()
        .unwrap_or(FLAG_SIG_MIN_WIDTH)
        .clamp(FLAG_SIG_MIN_WIDTH, FLAG_SIG_MAX_WIDTH);

    // Left indent is 4 (matches the Notes paragraph indent), then sig
    // column, then a 2-space gap, then description. Wrap budget reserves
    // those columns so wrapped continuation lines line up under col-2.
    let row_indent = 4;
    let col_gap = 2;
    let desc_budget = card_width
        .saturating_sub(row_indent + sig_col + col_gap)
        .max(20);

    let pad = |s: &str, w: usize| -> String {
        if s.len() < w {
            format!("{s:<w$}")
        } else {
            s.to_string()
        }
    };

    for row in rows {
        let sig_padded = pad(&row.signature, sig_col);
        let sig_painted = theme.paint(Token::Label, &sig_padded, policy);
        let mut lines = wrap_lines(&row.description, desc_budget).into_iter();
        let first = lines.next().unwrap_or_default();
        let first_painted = theme.paint(Token::Value, &first, policy);
        if row.signature.len() > FLAG_SIG_MAX_WIDTH {
            // Long signature: print it alone, drop description to the
            // continuation column on the next line.
            writeln!(w, "    {sig_painted}")?;
            let cont_indent = " ".repeat(row_indent + sig_col + col_gap);
            writeln!(w, "{cont_indent}{first_painted}")?;
        } else {
            writeln!(w, "    {sig_painted}  {first_painted}")?;
        }
        let cont_indent = " ".repeat(row_indent + sig_col + col_gap);
        for cont in lines {
            let cont_painted = theme.paint(Token::Value, &cont, policy);
            writeln!(w, "{cont_indent}{cont_painted}")?;
        }
    }
    Ok(())
}

/// Greedy word wrap. Splits on whitespace and packs words into lines
/// of at most `width` characters. Words longer than `width` get their
/// own line (no hyphenation). Used for Notes paragraphs and the
/// description column of Flags / Sources rows.
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
    fn verb_render_shows_notes_paragraphs() {
        // Prose paragraphs render under the `Notes` label (renamed from
        // `Description` in H2 — structured Flags/Sources blocks now
        // carry the description-shaped content).
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(out.contains("Notes"), "missing Notes label");
        assert!(out.contains("Stores text as a memory"));
        assert!(out.contains("Deduplication is ON by default"));
    }

    #[test]
    fn verb_render_omits_blocks_when_empty() {
        // Empty flags/sources/example/reference must suppress their
        // entire section — including the label — so cards that only
        // want Usage + Notes + See also render at the old shape.
        let out = render(&sample_verb(), OutputFormat::Table);
        assert!(!out.contains("Flags"), "Flags block must suppress");
        assert!(!out.contains("Sources"), "Sources block must suppress");
        assert!(!out.contains("Example"), "Example block must suppress");
        assert!(!out.contains("Reference"), "Reference block must suppress");
    }

    #[test]
    fn verb_render_flags_block_shows_rows() {
        let mut verb = sample_verb();
        verb.flags = vec![
            HelpFlagRow {
                signature: "--context N".into(),
                description: "u64; default 0".into(),
            },
            HelpFlagRow {
                signature: "--allow-duplicate".into(),
                description: "force fresh write".into(),
            },
        ];
        let out = render(&verb, OutputFormat::Table);
        assert!(out.contains("Flags"), "missing Flags label: {out}");
        assert!(out.contains("--context N"), "missing flag row: {out}");
        assert!(out.contains("u64; default 0"), "missing flag desc: {out}");
        assert!(
            out.contains("--allow-duplicate"),
            "missing second flag: {out}"
        );
    }

    #[test]
    fn verb_render_flags_align_descriptions_under_column() {
        // Two-flag block: the description column-2 must start at the
        // same screen column on both rows so the eye reads them as a
        // table, not as misaligned text. Use a fixture with empty
        // usage to avoid the `lines().find("--x")` ambiguity that
        // arises when the usage line happens to contain a `--flag`
        // substring of one of our row signatures.
        let mut verb = sample_verb();
        verb.usage.clear();
        verb.description.clear();
        verb.flags = vec![
            HelpFlagRow {
                signature: "--xx".into(),
                description: "short".into(),
            },
            HelpFlagRow {
                signature: "--yy".into(),
                description: "also short".into(),
            },
        ];
        let out = render(&verb, OutputFormat::Table);
        let a_line = out.lines().find(|l| l.contains("--xx")).expect("xx row");
        let b_line = out.lines().find(|l| l.contains("--yy")).expect("yy row");
        let a_desc = a_line.find("short").expect("xx desc");
        let b_desc = b_line.find("also short").expect("yy desc");
        assert_eq!(
            a_desc, b_desc,
            "description column must line up across rows: {a_line:?} vs {b_line:?}"
        );
    }

    #[test]
    fn verb_render_sources_block_shows_rows() {
        let mut verb = sample_verb();
        verb.sources = vec![
            HelpFlagRow {
                signature: "<TEXT>".into(),
                description: "inline string".into(),
            },
            HelpFlagRow {
                signature: "--from-file P".into(),
                description: "read from file".into(),
            },
        ];
        let out = render(&verb, OutputFormat::Table);
        assert!(out.contains("Sources"), "missing Sources label: {out}");
        assert!(out.contains("--from-file P"), "missing source row: {out}");
        assert!(out.contains("inline string"), "missing source desc: {out}");
    }

    #[test]
    fn verb_render_example_block_shows_line() {
        let mut verb = sample_verb();
        verb.example = Some(r#"encode "hello" --context 7"#.into());
        let out = render(&verb, OutputFormat::Table);
        assert!(out.contains("Example"), "missing Example label: {out}");
        assert!(
            out.contains(r#"encode "hello" --context 7"#),
            "missing example body: {out}"
        );
    }

    #[test]
    fn verb_render_reference_block_shows_clap_and_doc() {
        let mut verb = sample_verb();
        verb.reference = Some(HelpReference {
            clap_command: "encode --help".into(),
            doc_path: Some("docs/reference/shell/commands/encode.md".into()),
        });
        let out = render(&verb, OutputFormat::Table);
        assert!(out.contains("Reference"), "missing Reference label: {out}");
        assert!(out.contains("encode --help"), "missing clap pointer: {out}");
        assert!(
            out.contains("docs/reference/shell/commands/encode.md"),
            "missing doc pointer: {out}"
        );
    }

    #[test]
    fn verb_render_reference_block_without_doc_path() {
        let mut verb = sample_verb();
        verb.reference = Some(HelpReference {
            clap_command: "plan --help".into(),
            doc_path: None,
        });
        let out = render(&verb, OutputFormat::Table);
        assert!(out.contains("plan --help"));
        // No "docs/" line when doc_path is None.
        assert!(
            !out.contains("docs/"),
            "doc_path None must not render a path line: {out}"
        );
    }

    #[test]
    fn wrap_lines_handles_empty_and_basic_inputs() {
        assert_eq!(wrap_lines("", 10), vec![String::new()]);
        assert_eq!(
            wrap_lines("hello world", 20),
            vec!["hello world".to_string()]
        );
        let wrapped = wrap_lines("one two three four five", 10);
        assert!(
            wrapped.iter().all(|l| l.len() <= 10),
            "wrapped lines must fit budget: {wrapped:?}"
        );
        assert!(wrapped.len() > 1, "must wrap: {wrapped:?}");
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
        assert!(v["flags"].is_array());
        assert!(v["sources"].is_array());
        assert!(v["description"].is_array());
        assert!(v["example"].is_null());
        assert!(v["see_also"].is_array());
        assert!(v["reference"].is_null());

        let unknown = HelpUnknown {
            verb: "wibble".into(),
        };
        let out = render(&unknown, OutputFormat::Json);
        let v: Value = serde_json::from_str(&out).expect("parse unknown json");
        assert_eq!(v["kind"], "help-unknown");
        assert_eq!(v["verb"], "wibble");
    }
}
