---
name: skill-creator
description: Author new project-local skills under .claude/skills/. Walks intent → trigger → draft SKILL.md → examples → lint. Use whenever someone says "add a skill" or wants to refactor one.
when-to-use: |
  Triggers:
    - User says "let's create a skill" / "add a skill for X" / "improve this skill"
    - Drafting a new SKILL.md in .claude/skills/
    - User wants to refactor or split a skill that's grown unwieldy (>500 lines)
    - User asks "what should the trigger / description be?"
spec-refs:
  - .claude/skills/CONVENTIONS.md
license: Apache-2.0
source: https://github.com/anthropics/skills/tree/f458cee31a7577a47ba0c9a101976fa599385174/skills/skill-creator
---

# Skill Creator

Author new skills (and improve existing ones) for this repo. Adapted from Anthropic's official `skill-creator`, focused on what we actually need: drafting SKILL.md per project conventions. The upstream skill includes an eval-runner workflow we don't use here — link to upstream if you need it.

## When to use

A user wants to add or revise a skill under `.claude/skills/`. This skill walks the authoring loop:

1. **Capture intent** — what should the new skill do; when should it trigger.
2. **Interview** — gather edge cases, expected output, prior art.
3. **Draft `SKILL.md`** — frontmatter + body per `.claude/skills/CONVENTIONS.md`.
4. **Add examples** — golden case + counter-example.
5. **Lint** — run `just check-skills`.
6. **Surface** — show the user the draft for confirmation per AUTONOMY §21 (substantial skills) or commit directly (trivial ones).

## Workflow

### 1. Capture intent

Ask three questions before drafting anything:

1. What should the skill enable Claude to do? (Outcome, in one sentence.)
2. When should it trigger? (Concrete user phrases or diff signatures — not "when working with code".)
3. What is it *not* — what should other skills handle instead?

If the user is mid-conversation already ("turn this into a skill"), extract the answers from prior turns and confirm before proceeding.

### 2. Interview

Ask about:

- **Edge cases.** What's the boundary between this skill and an adjacent one?
- **Output format.** What should Claude do when the skill fires — surface a checklist? Refactor the code? Stop and surface?
- **Prior art.** Is there an upstream community skill we could vendor + adapt instead of authoring? Check `actionbook/rust-skills`, `awesome-skills/code-review-skill`, `VoltAgent/awesome-agent-skills`, `ComposioHQ/awesome-claude-skills` first.
- **Spec refs.** Which spec sections does this skill enforce? (Frontmatter `spec-refs:` validates them.)
- **Triggers.** Specific phrases? File-path globs? Diff content? The frontmatter `when-to-use` and `trigger-files` carry these.

### 3. Draft SKILL.md

Anatomy (per `.claude/skills/CONVENTIONS.md`):

```text
.claude/skills/<kebab-case-name>/
├── SKILL.md          required
├── references/       optional, long-form docs
├── scripts/          optional, helper bash/python
└── examples/         optional, golden inputs + outputs
```

Frontmatter (required: `name`, `description`, `when-to-use`):

```yaml
---
name: <kebab-case-name>
description: One sentence answering "what does this do." ≤ 200 chars; pushy so the trigger fires reliably.
when-to-use: |
  Triggers:
    - <concrete phrase>
    - <diff signature>
trigger-files:
  - <glob>
spec-refs:
  - <path>.md
license: <SPDX>           # only if vendored
source: <URL>             # only if vendored
---
```

Body sections (in this order):

```markdown
# <Skill Title>

## When to use
<one paragraph; matches the frontmatter expanded>

## What this enforces
<3–8 bullet rules; cross-link spec / CLAUDE.md>

## Workflow
<numbered steps Claude executes>

## Examples
<golden case + counter-example>

## Source / Adaptations           # only if vendored
```

### 4. Description writing

The `description` is the trigger surface. Be **specific** and **slightly pushy** — Claude tends to under-trigger.

Bad: "Helps with database stuff."
Good: "Audit WAL discipline (WAL-before-ack, fsync, recovery idempotency) for any diff in `crates/brain-storage/wal/`. Use whenever a change touches WAL ordering or fsync semantics, even if not explicitly mentioned."

Length: ≤ 200 chars (lint enforces). Be concrete about what the skill does AND when to use it.

### 5. Body writing principles

- **Imperative form.** "Verify X." not "It would be good to verify X."
- **Tables over prose.** Skills are reference material; pattern-match wins.
- **Cross-link, don't restate.** Link to spec / CLAUDE.md / other skills; don't duplicate.
- **Theory of mind.** Explain *why* a rule exists when the why isn't obvious.
- **Examples are mandatory.** A golden case shows the rule. A counter-example shows the failure mode.

### 6. Progressive disclosure (when content gets large)

Skills load three levels:

1. **Metadata** (name + description) — always in context.
2. **SKILL.md body** — in context whenever the skill triggers. Aim < 500 lines.
3. **Bundled `references/` files** — read on demand.

If a SKILL.md crosses 500 lines, split: keep the workflow + checklist in `SKILL.md` and move the long-form docs to `references/<topic>.md`. Link clearly so Claude knows when to load the reference.

### 7. Vendoring vs authoring

Before authoring from scratch, check:

- [`actionbook/rust-skills`](https://github.com/actionbook/rust-skills) — generic Rust m-* taxonomy.
- [`anthropics/skills`](https://github.com/anthropics/skills) — Anthropic-official.
- [`awesome-skills/code-review-skill`](https://github.com/awesome-skills/code-review-skill) — code-review.
- [`VoltAgent/awesome-agent-skills`](https://github.com/VoltAgent/awesome-agent-skills) — 1000+ aggregator.

If a community skill exists and is permissively licensed (MIT, Apache-2.0):

1. Pin to a specific commit SHA in `source:` frontmatter.
2. Copy SKILL.md (+ scripts/refs as needed).
3. Adapt frontmatter to `.claude/skills/CONVENTIONS.md` (rename to kebab-case if needed; replace `globs:` with `trigger-files:`; add `when-to-use:` block scalar; keep `license:`, `source:`).
4. Adapt the body to Brain context (cross-reference CLAUDE.md, spec sections, project crate names).
5. Add a `## Source / Adaptations` footer documenting exactly what changed and why.

If no community skill fits, author from scratch — but bias toward authoring against project-specific concerns (spec invariants, architectural rules) since generic skills are abundant elsewhere.

### 8. Lint and surface

Before commit:

1. Run `just check-skills` — all green.
2. Show the user the draft.
3. If the skill is substantial (introduces a new domain, or > 200 lines), follow AUTONOMY §21 — write a plan in `.claude/plans/` first if you haven't.

## What this enforces

- Every new skill follows `.claude/skills/CONVENTIONS.md` exactly.
- Frontmatter is complete and lints clean.
- Triggers are concrete enough to actually fire.
- Vendored skills carry attribution + adaptations footer.
- Body uses tables + imperative form + examples.
- Progressive disclosure once SKILL.md > 500 lines.

## Examples

### Golden — authoring a project-specific skill

User: "Add a skill for verifying that WAL writes always fsync before ack."

Claude (using this skill):

1. Captures intent: "Audit WAL-before-ack discipline."
2. Confirms trigger: diff in `crates/brain-storage/wal/`.
3. Identifies spec: §05/03 (WAL semantics) + §05/08 (recovery).
4. Drafts `.claude/skills/brain-wal-audit/SKILL.md` per CONVENTIONS, with:
   - Description naming WAL-before-ack and group-commit explicitly.
   - `trigger-files: crates/brain-storage/wal/**/*.rs`.
   - `spec-refs:` linking the two §05 sections.
   - Workflow: (1) grep for `pwritev2` / `fsync`; (2) verify ack happens after; (3) check group-commit batching; (4) check recovery idempotency.
   - Examples: golden = "WAL record written, fsync called, ack returned"; counter = "ack before fsync".
5. Runs `just check-skills` — green.
6. Surfaces draft for user confirmation.

### Counter-example — vague trigger

```yaml
description: Helps with WAL stuff.
when-to-use: |
  When working with WAL.
```

Reject. The trigger could match anything. Rewrite to:

```yaml
description: Audit WAL discipline (WAL-before-ack, fsync, group commit) for diffs touching crates/brain-storage/wal/. Use any time WAL ordering or recovery semantics change.
when-to-use: |
  Triggers:
    - Diff in crates/brain-storage/wal/**/*.rs
    - User says "review WAL" / "fsync correctness"
    - Adding a new WAL record format
```

## Source / Adaptations

- **Source:** [`anthropics/skills@f458cee`](https://github.com/anthropics/skills/tree/f458cee31a7577a47ba0c9a101976fa599385174/skills/skill-creator)
- **License:** Apache-2.0
- **Adaptations:**
  - Slimmed from ~485 lines to a focused authoring workflow that pairs with our `.claude/skills/CONVENTIONS.md`.
  - Dropped the eval-runner / `eval-viewer/` machinery — we don't run skill evals in this repo. If we ever want to, the upstream link above has the full version.
  - Replaced the upstream `description` (eval-focused) with one that's authoring-focused.
  - Pinned project-specific guidance: vendor first from `actionbook/rust-skills`, project-specific skills should encode spec invariants, link `.claude/plans/` per AUTONOMY §21.
  - Added a worked example using `brain-wal-audit` since that's an authoring use case we're about to hit.
  - Kept the original "progressive disclosure" guidance and the "pushy description" tip since those carry across.
