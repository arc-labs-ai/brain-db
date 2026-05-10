# Skill Conventions

Project-local rules for skills under `.claude/skills/`. Format-compatible with Anthropic's [open skill standard](https://github.com/anthropics/skills/tree/main/spec) (Dec 2025) — portable to Codex, Cursor, Gemini CLI, Antigravity, Windsurf, etc.

See [`.claude/plans/skills-roadmap.md`](../plans/skills-roadmap.md) for the rationale and the curated set of skills planned for this repo.

---

## 1. File layout

Each skill is a folder:

```
.claude/skills/<kebab-case-name>/
├── SKILL.md          required
├── references/       optional — long-form docs the skill links to but doesn't load eagerly
├── scripts/          optional — helper bash/python invoked by the skill
└── examples/         optional — golden inputs + expected outputs
```

Folder name **MUST** match the `name` field in the SKILL.md frontmatter (kebab-case).

## 2. SKILL.md frontmatter (YAML)

Required keys: `name`, `description`, `when-to-use`. Optional keys flagged below.

```yaml
---
name: <kebab-case-name>
description: One sentence answering "what does this do." ≤ 120 chars.
when-to-use: |
  Concrete trigger phrases or diff signatures.
  - User says "review this unsafe block"
  - Diff touches files matching crates/brain-storage/**/*.rs

# Optional below this line.

trigger-files:                      # globs that auto-route this skill
  - crates/brain-storage/**/*.rs

spec-refs:                          # spec sections this skill enforces
  - spec/05_storage_arena_wal/02_arena_layout.md

license: MIT                        # only for vendored skills
source: https://github.com/.../tree/<sha>/skills/<name>   # only for vendored
---
```

### 2.1 Why these keys

- `name` — folder + frontmatter must agree; the lint checks both.
- `description` — what Claude reads at skill-discovery time. Be specific and short.
- `when-to-use` — block scalar (`|`), readable across multiple lines, lists concrete phrases or signatures. Vague triggers ("when working with code") fail review.
- `trigger-files` — used by host integrations to auto-suggest a skill when a diff matches. Optional but recommended for diff-driven skills.
- `spec-refs` — links to authoritative spec; the lint verifies referenced files exist.
- `license` / `source` — present iff the skill is vendored.

## 3. SKILL.md body

```markdown
# <Skill Title>

## When to use
<one paragraph; matches but expands the `when-to-use` frontmatter>

## What this enforces
<3–8 bullet rules; cross-link to spec sections>

## Workflow
<numbered steps Claude executes>

## Examples
<golden case + counter-example>

## Source / Adaptations
<for vendored skills: source URL, commit SHA, what we changed>
```

The body is what Claude actually reads when it decides to invoke the skill. Keep it tight: rules > prose, links > restated context.

## 4. Anti-patterns to avoid

- **Loading huge reference docs eagerly.** Move them into `references/` and link from the body — Claude reads them only when needed.
- **Restating CLAUDE.md.** CLAUDE.md is auto-loaded; a skill that just paraphrases it adds noise. A skill should *operationalize* — concrete checks, scripts, workflows — not duplicate.
- **Vague triggers.** "When working with code" matches everything; matches nothing. Triggers must point at concrete phrases, file paths, or diff signatures.
- **Conflicts with the spec.** When a skill and the spec disagree, the spec wins. Skills layer on top.
- **Authoring without a plan.** Per AUTONOMY §21, substantial skill additions go through `.claude/plans/` first.

## 5. Lint

`scripts/check-skills.sh` validates every `SKILL.md`:

- Required frontmatter keys present (`name`, `description`, `when-to-use`).
- `name` matches the parent directory.
- `description` is non-empty and ≤ 200 chars.
- `when-to-use` is non-empty.
- `spec-refs:` paths exist on disk.
- For vendored skills, `license` and `source` are present.

Run via:

```bash
just check-skills      # or scripts/check-skills.sh
```

Wired into `just verify` so CI catches drift.

## 6. Vendoring rules

When pulling a skill from an external repo:

1. Verify the upstream license is MIT, Apache-2.0, or similar permissive. GPL-family licenses must be discussed before vendoring.
2. Copy the skill into `.claude/skills/<name>/`. Pin the upstream commit SHA in the `source:` frontmatter key.
3. Add a `## Source / Adaptations` section at the bottom of the body documenting:
   - Upstream URL + commit SHA.
   - What we changed (renamed triggers, added `spec-refs`, removed irrelevant sections).
4. Track vendored-skill drift via a per-phase audit (Phase 11's exit checklist).

If a skill's license is incompatible or the skill is too large to vendor cleanly, link to upstream from a body section instead of vendoring.

## 7. Authoring new skills

Use the `skill-creator` skill (vendored from `anthropics/skills`) as the entry point — it walks the frontmatter / body / references structure interactively. The minimum:

1. Pick a kebab-case name. Confirm it doesn't collide with an existing skill.
2. Write the description in one sentence; aim for the trigger to be unambiguous.
3. Draft `## What this enforces` first — it forces clarity on scope before the workflow.
4. Add 2+ examples (golden + counter-example) so future-you can verify the skill still routes correctly.
5. Run `just check-skills` — green before commit.
6. For substantial skills (cross-crate, new domain), draft a plan in `.claude/plans/` first per AUTONOMY §21.

## 8. Index of skills

See `.claude/plans/skills-roadmap.md` §5 for the curated set. The lint generates no index; rely on `ls .claude/skills/` and the per-skill SKILL.md to discover.
