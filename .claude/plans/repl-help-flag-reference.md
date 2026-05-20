# REPL `help <verb>` — proper flag reference

**Status**: draft, awaiting approval
**Scope**: `crates/brain-explore/src/render/help.rs` + `crates/brain-shell/src/repl/help.rs`
**Out of scope**: clap-generated `--help`, the markdown reference docs, the TUI overlay (stub)

---

## 1 · Why

Today's `help encode` card committed to neither "concept blurb" nor "flag
reference" and ends up unhelpful at both:

- **Usage line lists 5 of 13 flags**, so a reader concludes `--edge`,
  `--request-id`, `--from-file`, `--vector`, `--wait-for-extraction`
  don't exist.
- **Placeholders `N / K / F / HEX` carry no semantics** — no range for
  salience, no default for context, no shape for txn.
- **Sources for `<TEXT>` hidden**: positional / file / stdin / vector
  are coequals in clap but only the positional surfaces.
- **No gated-flag callout**: `--vector` panics with `todo!()`; the help
  card should warn before a user tries it.
- **No example, no pointer** to the per-verb markdown reference.

The reference docs already cover this material in
`docs/reference/shell/commands/<verb>.md`. The REPL card must convey
the same shape in a screen-sized form.

## 2 · Target card shape

One example, every other verb follows the same template:

```
──────────────────────────────────────────────────────────────────────
  ENCODE  ·  write a memory

  Usage      encode <TEXT> [flags]
             encode --from-file <PATH> [flags]
             encode --from-stdin [flags]

  Flags
    --context N              u64; default 0; sticky via \set context
    --kind K                 episodic | semantic | consolidated (default: episodic)
    --salience F             [0.0, 1.0]; default 0.5
    --allow-duplicate        force fresh write; dedup is ON by default
    --edge KIND:ID           add edge at create time; repeatable
    --request-id UUID        idempotency key (24h cache)
    --wait-for-extraction    block until knowledge layer extracts
    --txn HEX                attach to an open transaction

  Sources    <TEXT>           inline string
             --from-file P    read from file (use - for stdin)
             --from-stdin     shorthand for --from-file -
             --vector CSV     gated — panics today (see Notes)

  Notes      Dedup is on by default — encoding the same text twice in
             the same (agent, context) returns the existing memory.
             Pass --allow-duplicate for episodic events where the same
             content recurs.

  Example    encode "Alice merged the auth-rewrite branch" --context 7

  See also   help recall  ·  help forget  ·  help link  ·  help subscribe
             encode --help            full clap reference
             docs/reference/shell/commands/encode.md
──────────────────────────────────────────────────────────────────────
```

Visual rules:
- All section labels left-aligned in the existing `VERB_LABEL_WIDTH=10`
  gutter, matching `info.rs` and existing usage block.
- `Flags` and `Sources` rows are two-column: flag signature in `Label`
  token, description in `Value` token. A blank between flag column and
  description column (computed per-card from the longest flag in that
  card so the description column always lines up *within* a card; not
  globally fixed since `--wait-for-extraction` is 21 chars and would
  swallow shorter verbs).
- `Notes` is prose — wrapped to card width minus label gutter, no
  separate paint token; same indent rules as `Description` today.
- `Example` is one line, `Value` token.
- Top + bottom rule continue to frame the card. No leading or trailing
  blank inside the frame.
- Inter-section blanks land **before** each section, mirroring today's
  discipline so the last section sits flush against the bottom rule.

## 3 · Data model changes

`brain-explore/src/render/help.rs`:

```rust
pub struct HelpVerb {
    pub name: String,
    pub tagline: String,
    pub usage: Vec<String>,            // unchanged
    pub flags: Vec<HelpFlagRow>,       // NEW
    pub sources: Vec<HelpFlagRow>,     // NEW; empty for verbs without
                                       //      alternate sources
    pub description: Vec<String>,      // unchanged; renamed to "Notes"
                                       //      in render
    pub example: Option<String>,       // NEW
    pub see_also: Vec<String>,         // unchanged
    pub reference: Option<HelpReference>, // NEW; clap + markdown link
}

pub struct HelpFlagRow {
    pub signature: String,   // "--context N"
    pub description: String, // "u64; default 0; sticky via \set context"
}

pub struct HelpReference {
    pub clap_command: String,    // "encode --help"
    pub doc_path: String,        // "docs/reference/shell/commands/encode.md"
}
```

Two-column rendering for `flags` + `sources`:
- column 1 width = max(signature.len()) for that section, capped at 24
  (longer signatures spill to a second line, indented to col-2 start)
- column 2 = description, wrapped to card width

JSON envelope keeps `kind: "help-verb"` and adds the new fields with
the existing snake_case naming; safe additive change since nothing
consumes the JSON externally yet (REPL only).

## 4 · Verb-by-verb content

One commit per verb fixture keeps reviewable units small. Order matches
`lookup()` in `repl/help.rs`:

### 4.1 encode
Flags + sources + notes already drafted in §2. Reference =
`encode --help` + `docs/reference/shell/commands/encode.md`.

### 4.2 recall
Flags: `--top-k N`, `--confidence F`, `--filter-context N`,
`--filter-kind K`, `--include-text`, `--include-graph`, `--txn HEX`.
No alternate sources. Notes: cluster warning, hybrid vs substrate
score semantics. Reference: `recall --help` +
`docs/reference/shell/commands/recall.md`.

### 4.3 plan
Flags: `--max-steps N`, `--max-wall-time-ms N`. Notes: GoalReached
vs partial. Reference: `plan --help` +
`docs/reference/shell/commands/plan.md` (if present; otherwise
omit the doc_path).

### 4.4 reason
Flags: `--depth N`, `--confidence F`, `--max-inferences N`.
Notes: inference chain semantics.

### 4.5 forget
Flags: `--mode soft|hard`. Notes: grace period, hard-erase semantics.

### 4.6 link
Args: `<SRC> <KIND> <TGT>`. Flags: `--weight F`, `--txn HEX`. Notes:
kind whitelist + hyphen/underscore variants. Reference doc:
`commands/link.md`.

### 4.7 unlink
Args: `<SRC> <KIND> <TGT>`. Flags: `--txn HEX`. Notes: idempotent.

### 4.8 txn
Sub-commands instead of flags: `begin`, `commit <ID>`, `abort <ID>`.
Use `usage` for the verb forms (existing pattern), `flags` empty.
Notes: REPL auto-attach.

### 4.9 subscribe
Flags: `--context N`, `--kind K`, `--start-lsn N`, `--collect N`.
Notes: streaming vs batch, ndjson auto-downgrade, signal handling.

### 4.10 meta (kept as a HelpVerb but it's really a list)
Keep current shape — `meta` is the one verb where the existing
unstructured layout fits. Add reference to `docs/reference/shell/meta`.

## 5 · Pre-conditions for description column width

Strategy: per-section column-1 width = `max(sig.len()) for rows in this
section`, clamped to `[12, 24]`. Rows whose signature exceeds 24 get
their description on the next line, indented to column 2.

Card-width clamp stays at `CARD_MAX_WIDTH = 80`. Description wrap
honours `policy.width.min(80) - LABEL_WIDTH(10) - col1_width - 2 (gap)`.

## 6 · Implementation steps

Commit boundaries chosen so each commit is independently reviewable.

| # | Commit | LoC est. | Touches |
|---|---|---|---|
| H1 | Extend `HelpVerb` struct + add `HelpFlagRow`, `HelpReference`; existing fixtures pass an empty `flags` / `sources` / `example` / `reference` (no behaviour change). Tests pass. | ~80 | brain-explore |
| H2 | Renderer: add `flags`, `sources`, `notes` (rename of `description` in card), `example`, `reference` blocks. Two-column row helper. Tests for each block. Existing tests keep passing because all new sections suppress when empty. | ~250 | brain-explore |
| H3 | Fill `help_encode()` with full flag + source rows + notes + example + reference. Snapshot-style test asserts the card contains every flag name. | ~120 | brain-shell |
| H4 | Same for `recall`, `forget`, `link`, `unlink`. | ~180 | brain-shell |
| H5 | Same for `plan`, `reason`, `txn`, `subscribe`. | ~180 | brain-shell |
| H6 | Refresh `meta` card with the new reference pointer; confirm top-level directory stays at current shape. | ~30 | brain-shell |
| H7 | `cargo fmt && cargo clippy -D warnings && cargo test -p brain-explore -p brain-shell`. | — | — |

All seven commits land as one branch push; no co-author trailer per
project convention.

## 7 · Tests

- H1: existing `sample_verb()` still parses and renders unchanged
  (default fields render as no-ops).
- H2: per-block presence/absence tests — empty flags hides "Flags"
  label entirely, single flag renders as one row, multi-row computes
  col-1 width correctly.
- H3-H6: per-verb regression — every documented flag name appears in
  the rendered card body. Cuts the "lies by omission" failure mode.
- All commits: `cargo test -p brain-explore -p brain-shell`.

## 8 · Risk / pushback I'd accept

- **"Card too tall."** Each verb card grows from ~14 lines to ~30. If
  this dominates screens, fall back to: tagline + usage + flags +
  see-also; move notes/sources/example behind a `help <verb> --full`
  switch. Defer until someone complains.
- **"Description column wraps inconsistently."** If reviewers reject
  per-section dynamic col-1 width, fall back to a fixed col-1 width
  of 22 chars across every card.
- **"Duplicate of clap."** Clap describes flag *syntax*. This card
  describes *semantics* (range, default, gotcha, sticky). The
  `See also` row points at clap for the syntactic source of truth.

## 9 · Out of scope

- `brain-explore/src/tui/widgets/help_overlay.rs` (stub today).
- Generating the help text from clap metadata. Tempting but every verb
  has semantics clap can't carry (sticky, idempotency, gating). The
  hand-written fixture is the source of truth for these.
- Localisation.
- Markdown reference docs — already authoritative; this plan brings
  the REPL into line with them, not the other way round.

## 10 · Done when

- `help encode` in REPL shows every documented flag, with one-line
  semantics, with an example, with a pointer to the reference doc.
- Same for all eight other verbs + meta.
- `cargo test` green.
- `cargo clippy -D warnings` green.
- Screenshot of a couple of cards in the PR description for visual
  approval.
