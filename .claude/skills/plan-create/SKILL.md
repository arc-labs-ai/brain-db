---
name: plan-create
description: Drive AUTONOMY §21 plan-first — read spec, web-search if needed, draft a plan in .claude/plans/, surface for confirmation. Use when starting a new phase or substantial sub-task.
when-to-use: |
  Triggers:
    - User says "let's start phase N" / "next sub-task is N.M" / "plan for X"
    - About to write substantial code (new framework/lib, multi-crate, non-trivial algorithm, >200 lines)
    - Skill-creator says "this skill is substantial; plan first"
    - Detected drift in implementation that requires a re-plan
spec-refs:
  - .claude/plans/_template.md
---

# Plan Create

## When to use

Starting a new phase OR a substantial sub-task. Per AUTONOMY §21, work that introduces a dep, touches multiple crates, implements a non-trivial algorithm, or spans > 200 lines requires a plan in `.claude/plans/` and user confirmation before implementation.

Trivial sub-tasks (constant pin, one-function helper, doc-only) skip the plan and proceed with a one-line summary.

## What this enforces

- The plan exists at `.claude/plans/<phase-NN>[-task-MM].md`.
- The plan covers Scope, Spec references, External validation, Architecture sketch, Trade-offs, Risks, Test plan, and Commit shape (per `_template.md`).
- For new frameworks/libraries, the plan includes a web-search-validated note ("rkyv 0.7 docs say X — URL").
- The plan ends in `Status: awaiting-confirmation` and Claude pauses for the user.
- After approval, status flips to `approved (implemented)` once work lands.

## Workflow

### 1. Pick the file path

- New phase → `.claude/plans/phase-NN.md`.
- Substantial sub-task → `.claude/plans/phase-NN-task-MM.md`.
- Cross-cutting (skills roadmap, branch policy) → `.claude/plans/<topic>.md`.

### 2. Read the spec

Open every file in the phase doc's "Reads:" list, in full — not skimmed. Quote any binding constraints verbatim.

### 3. External validation (when relevant)

For a new framework / library / algorithm or an architecture decision:

1. WebSearch for `<library> <feature> <year>` to find current docs.
2. WebFetch the official docs page; extract the relevant excerpt.
3. Capture the URL + excerpt in the plan's §3.

Skip when the work is purely internal wiring (renaming, refactoring without dep changes). Write "Not applicable — internal wiring only."

### 4. Draft the plan

Use `.claude/plans/_template.md` as the skeleton. Required sections:

1. **Scope** — what it does, what it doesn't, where it fits in the phase.
2. **Spec references** — files read + quoted constraints.
3. **External validation** — URLs + excerpts, or "N/A".
4. **Architecture sketch** — types, modules, public surface (ASCII or short prose).
5. **Trade-offs considered** — table of alternatives with verdicts.
6. **Risks / open questions** — what could go wrong; spec ambiguities.
7. **Test plan** — every "Done when" mapped to one or more tests.
8. **Commit shape** — one or two-three commits; what each contains.

Set frontmatter `Status: awaiting-confirmation`.

### 5. Surface

Print:

```
PLAN READY: see .claude/plans/<file>.md — confirm to proceed.
```

Stop. Don't write code until the user confirms.

### 6. After approval

When the user says "go" / "approved":

1. Implement per the plan.
2. As work lands, update `Status:` to `approved (implemented)` with a reference to the landing commit hash.
3. If the implementation diverges from the plan, update the plan inline with a "## Implementation note" section explaining why.

## Plan quality bar

A plan is good when a future-Claude can read it cold and understand:

- What was built and what was deliberately deferred.
- Which spec sections it honors.
- Why this approach won out over alternatives.
- What tests prove it's correct.

Avoid:

- **Vague scope.** "Improve X" → "Implement Y per spec §Z, deferring W to phase N+1."
- **No trade-offs.** If you didn't consider alternatives, you didn't make a choice — you just typed.
- **Empty risk section.** Every plan has a risk; surfacing it now is cheaper than surprise later.
- **Tests without spec mapping.** Each test should answer "this proves spec MUST §X.Y."

## Examples

### Golden

User: "Let's start phase 1.9 — handshake."

Workflow:
1. Plan path: `.claude/plans/phase-01-task-09.md`.
2. Read `spec/03_wire_protocol/06_handshake.md` end-to-end.
3. External validation: rkyv reuse — N/A.
4. Draft plan with all 8 sections; confirm naming reconciliation (phase doc says "ClientHello/ServerHello"; spec says HELLO/WELCOME/AUTH/AUTH_OK; spec wins).
5. Surface "PLAN READY: see .claude/plans/phase-01-task-09.md — confirm to proceed." and stop.

### Counter — implement-first

User: "Let's start handshake."
Claude: writes 800 lines of `handshake.rs` without a plan.

Reject. AUTONOMY §21 requires the plan first for substantial sub-tasks. Stop, write the plan, surface for confirmation.

## Cross-references

- AUTONOMY.md §21 — the rule itself.
- `.claude/plans/_template.md` — the plan skeleton.
- `skill-creator` — for authoring new skills (a substantial skill needs its own plan).

## Source / Adaptations

Project-local. Operationalizes AUTONOMY §21.
