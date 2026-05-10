# Plan: Project Skills Roadmap

**Status:** awaiting-confirmation
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 4–6 (one per skill batch)

---

## 1. Scope

Stand up a curated set of Claude Code **skills** under `.claude/skills/` so that day-to-day work on Brain produces production-grade Rust optimized for our specific domain (cognitive-substrate / vector memory store / agentic-AI infra) instead of generic best-effort code.

Two sources for each skill:

1. **Pull** — copy or vendor a high-quality community skill, attribute it, adapt the trigger to our codebase.
2. **Author** — write project-specific skills that encode our spec invariants and architectural rules (Glommio/Tokio split, WAL contract, single-writer-per-shard, the seven invariants in CLAUDE.md §5).

**Out of scope for this plan:**

- The eventual MCP server / external tool integrations (separate roadmap).
- Skills that duplicate built-in Claude Code commands (we already have `bench`, `verify`, `lint`, `commit-task`, etc.).
- Importing the entire skill marketplace — bloats context, dilutes signal.

## 2. What a skill is (calibration)

A skill is a folder under `.claude/skills/<name>/` containing:

```text
.claude/skills/<name>/
├── SKILL.md          required — YAML frontmatter (name, description, when-to-use) + instructions
├── references/       optional — long-form docs the skill links to but doesn't load eagerly
├── scripts/          optional — helper bash/python invoked by the skill
└── examples/         optional — golden inputs + expected outputs
```

Anthropic introduced the format in October 2025 and released it as an open standard in December 2025. Format is portable across Claude Code, Claude.ai, Codex, Cursor, Gemini CLI, Antigravity, Windsurf — so our investment isn't tool-locked.

The `description` and `when-to-use` lines are how Claude decides whether to invoke a skill on its own. They must be specific enough that "review this unsafe block" maps to `rust-unsafe-review` and not to generic `code-review`.

## 3. Existing local skills (already shipped with this repo)

Listed in the system prompt's `available skills` block:

| Skill | Purpose |
|---|---|
| `bench` | Run criterion benchmarks for a crate |
| `commit-task` | Stage + commit a sub-task with the AUTONOMY §5 message format |
| `lint` | rustfmt + clippy with `-D warnings` |
| `status` | Phase progress, last commit, next sub-task |
| `next-task` | Identify the next sub-task to work on |
| `verify` | Full verify suite (build + test + clippy + fmt) |
| `new-crate` | Scaffold a workspace crate per project conventions |
| `spec` | Read a specific spec section |
| `audit-spec` | Audit implementation against the spec for a crate |
| `simplify` | Review changed code for reuse, quality, efficiency |
| `init` | Initialize a CLAUDE.md |
| `review`, `security-review` | PR review |
| `loop`, `schedule` | Recurring task automation |
| `fewer-permission-prompts`, `update-config`, `keybindings-help` | Harness / config |

Coverage: workflow + lints + benchmarks + PR review. **Gap:** nothing Brain-specific (spec invariants, Glommio rules, WAL contract, lock-free patterns), and nothing about Rust performance/unsafe/profiling.

## 4. Survey of community sources

| Source | URL | Notes |
|---|---|---|
| Anthropic official skills | <https://github.com/anthropics/skills> | 17 skills: mostly document / brand / web / claude-api / mcp-builder / **skill-creator** / webapp-testing. Polished but mostly off-domain for us. |
| `actionbook/rust-skills` | <https://github.com/actionbook/rust-skills> | Strong Rust taxonomy: `m02-resource`, `m04-zero-cost`, `m06-error-handling`, `m07-concurrency`, `m10-performance`, `m15-anti-pattern`, `unsafe-checker`, `coding-guidelines`, plus `domain-*` family (cli, cloud-native, embedded, ml, web). |
| `awesome-skills/code-review-skill` | <https://github.com/awesome-skills/code-review-skill> | Multi-language code review including Rust. |
| `VoltAgent/awesome-agent-skills` | <https://github.com/VoltAgent/awesome-agent-skills> | 1000+ skills aggregator (Anthropic, Google, Vercel, Stripe, Cloudflare, Trail of Bits, Sentry, etc.). Index, not a single source — we cherry-pick. |
| `ComposioHQ/awesome-claude-skills` | <https://github.com/ComposioHQ/awesome-claude-skills> | Curated index. Useful for discovery; not a source we vendor directly. |
| `sickn33/antigravity-awesome-skills` | <https://github.com/sickn33/antigravity-awesome-skills> | 1400+ skills with installer CLI. Heavyweight. |
| `mcpmarket.com` (rust-best-practices, rust-unsafe-ffi, rust-development-workflow, rust-coding-standards, rust-anti-pattern-refactor) | mcpmarket | Per-skill landing pages; check repo source before vendoring. |

**Selection bias:** We optimize for *spec-faithfulness* and *correctness invariants* over breadth. Better to ship 8 sharp skills than to dump 50 generic ones into context.

## 5. Proposed skill set

### 5.1 Pull / vendor from community

Each will be copied into `.claude/skills/<name>/` with attribution in `SKILL.md` (`# Source: <repo>@<commit>`) and a project-specific `# Adaptations:` section noting any tailoring.

| Skill | Source | Why we want it |
|---|---|---|
| **rust-unsafe-checker** | `actionbook/rust-skills/skills/unsafe-checker` | `crates/brain-storage` is the only `unsafe`-allowed crate. We need a checker that audits each block for `// SAFETY:` comment, smallest scope, and miri test coverage (CLAUDE.md §7, AUTONOMY §15). |
| **rust-anti-pattern** | `actionbook/rust-skills/skills/m15-anti-pattern` | Catches `unwrap()` outside tests, `.clone()` in hot path, holding locks across `.await`, `Send + Sync` on per-shard types — all anti-patterns we've codified in CLAUDE.md §9. |
| **rust-perf** | `actionbook/rust-skills/skills/m10-performance` | Hot-path discipline: object pools, branch hints, SIMD via `wide`, `matrixmultiply`. Aligns with CLAUDE.md §9 ("Don't allocate in the hot path"). |
| **rust-error-handling** | `actionbook/rust-skills/skills/m06-error-handling` | `thiserror` (libs) vs `anyhow` (binaries) discipline per CLAUDE.md §7. |
| **rust-concurrency** | `actionbook/rust-skills/skills/m07-concurrency` | General concurrency patterns; we'll layer Glommio-specifics on top in `brain-glommio-rules`. |
| **skill-creator** | `anthropics/skills/skills/skill-creator` | Meta — helps us author future project-specific skills with correct frontmatter and structure. |

License/attribution: most are MIT-licensed; we copy + attribute. If a skill has a stricter license, we read-only-link rather than vendor.

### 5.2 Author from scratch (project-specific)

These encode rules unique to Brain and don't exist upstream.

| Skill | Purpose | Trigger |
|---|---|---|
| **brain-invariants** | Cross-check CLAUDE.md §5 invariants (WAL-before-ack, single-writer-per-shard, CRC everywhere, slot-version on MemoryId, idempotency by RequestId, tombstone grace, no silent corruption) against any code change touching the relevant crates. | Diff includes `brain-storage`, `brain-ops`, `brain-workers`, `brain-server` |
| **brain-spec-invariant** | Given a spec §X.Y MUST clause, verify the implementation honors it; surface violations with file/line. Companion to existing `audit-spec` but more focused — one MUST at a time. | "verify spec MUST 03/03 §3.6" |
| **brain-glommio-rules** | Glommio-specific rules: no Tokio inside shard, no `tokio::fs`, types are `!Send`, no thread pool for parallel work. CLAUDE.md §9 anti-patterns. | Diff in `brain-server` shard executor or any shard-runtime code |
| **brain-tokio-boundary** | The connection layer (Tokio) and shard layer (Glommio) split — verify nothing leaks across. Channels between layers are explicit; types don't accidentally derive `Send` for shard-side state. | New code that touches both `tokio::` and `glommio::` symbols |
| **brain-wal-audit** | WAL discipline: `pwritev2(RWF_DSYNC)` group commit, O_DIRECT alignment, no ack-before-fsync, recovery idempotency. spec §05/03+§05/08. | Diff in `brain-storage/wal/` |
| **brain-arena-audit** | Arena discipline: 1600-byte slots, slot-version stamping, CRC per slot, mmap safety. spec §05/02. | Diff in `brain-storage/arena/` |
| **brain-redb-schema** | Metadata-store schema discipline: redb table layout matches spec §07/02; migrations require version bump. | Diff in `brain-metadata/` |
| **brain-hnsw-tuning** | HNSW parameter selection (M=16, ef defaults) per spec §06/02; flag deviations. | Diff in `brain-index/` or HNSW config |
| **brain-protocol-version-bump** | Wire-protocol changes (new opcode, field reorder, new error code) require spec change + version bump per spec §03/12. Catches accidental wire-breaking changes. | Diff in `brain-protocol/{header,opcode,frame,request,response,error}` |
| **brain-zero-copy-review** | Verifies rkyv/bytemuck usage achieves zero copy in hot paths — `cast_slice` not `Vec::from`, `check_archived_root` then deref, no intermediate `.to_vec()` calls. spec §03/04 + §05/02. | Diff in `brain-protocol/{request,response}.rs` or any rkyv-touching code |
| **brain-fuzz-target** | Design + scaffold a fuzz target for a new wire surface (Frame, RequestBody, ResponseBody, handshake). Updates `fuzz/fuzz_targets/`. | "new fuzz target for X" |
| **brain-loom-design** | Identify concurrency-critical paths needing `loom` tests; scaffold the test. CLAUDE.md §10. | Diff in lock-free or epoch-GC code |
| **brain-chaos-test** | Design kill-during-operation tests for recovery code per spec §05/08 + §16/06. | Diff in WAL recovery, snapshot, restore |
| **brain-obs-trace** | Verify `tracing` spans, OpenTelemetry attributes, structured logs at the right layer. spec §15. | Diff in any operation handler |
| **brain-perf-target** | Compare changes against latency targets in spec §16/02; flag regressions. | After running `bench` |
| **plan-create** | Walk through AUTONOMY §21's plan-first workflow: read spec, web-search for external validation, draft plan in `.claude/plans/`, surface for confirmation. | Starting a new phase or substantial sub-task |
| **production-checklist** | Pre-merge checklist: error handling at boundaries, no `unwrap()` outside tests, tracing on the new code path, metrics emitted, tests cover golden + edge cases, no drift from spec. | Before merging `feature/*` → `dev` |

### 5.3 Notable non-goals

- **Generic linters** — `cargo clippy -D warnings` already runs in `verify`. A skill that just runs clippy is redundant.
- **MCP / API skills** — we're a server, not an API consumer. The `claude-api` skill stays available but isn't a project priority.
- **UI / design skills** — irrelevant for a database substrate.
- **Skills duplicating CLAUDE.md** — CLAUDE.md is loaded automatically. A skill that just restates it adds noise.

## 6. Skill conventions for this project

Every skill we create or vendor follows these rules:

### 6.1 File layout

```text
.claude/skills/<kebab-case-name>/
├── SKILL.md
├── references/        # only if non-trivial reference material is needed
└── scripts/           # only if the skill wants to run a script
```

### 6.2 SKILL.md frontmatter (YAML)

```yaml
---
name: <kebab-case-name>
description: One sentence answering "what does this do." ≤ 120 chars.
when-to-use: |
  Concrete trigger phrases or diff signatures. E.g.:
    - User says "review unsafe" or "audit this unsafe block"
    - Diff touches files matching crates/brain-storage/**/*.rs
trigger-files:    # optional; for diff-based triggers
  - crates/brain-storage/**/*.rs
spec-refs:        # optional; the spec sections this skill enforces
  - spec/05_storage_arena_wal/02_arena_layout.md
license: MIT      # only for vendored skills
source: https://github.com/.../tree/<sha>/skills/<name>   # only for vendored
---
```

### 6.3 SKILL.md body structure

```markdown
# <Skill Title>

## When to use
<one paragraph; matches `when-to-use` frontmatter but expanded>

## What this enforces
<3–8 bullet rules; cross-link to spec sections>

## Workflow
<numbered steps Claude executes>

## Examples
<golden case + counter-example>

## Source / Adaptations
<for vendored skills: source URL, commit SHA, what we changed>
```

### 6.4 Anti-pattern to avoid

- **Loading huge reference docs eagerly.** Move reference material into `references/`; the SKILL.md links rather than embeds.
- **Skills that contradict CLAUDE.md.** When in conflict, CLAUDE.md and the spec win. Skills are *operational* layers on top.
- **Skills with vague triggers.** "When working with code" is useless; "when diff touches `crates/brain-storage/wal/`" is actionable.

## 7. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** curate 6 vendored + 17 authored, focused on spec invariants and Brain-specific architecture. | ✓ Sharp signal; each skill earns its slot. |
| Vendor every Rust skill from `actionbook/rust-skills`. | rejected — ~38 skills, lots of overlap with CLAUDE.md, dilutes the trigger space. |
| Install `sickn33/antigravity-awesome-skills` (1400+ skills) and rely on its installer. | rejected — context dilution, hard to audit, conflicts with our spec discipline. |
| Author everything from scratch. | rejected — community Rust patterns are already well-codified (`unsafe`, `m10-performance`); reinventing is wasteful. |
| Single mega-skill covering all Brain rules. | rejected — triggers can't fire selectively; defeats the skill model. |

## 8. Risks / open questions

- **License compatibility.** We're MIT-licensed; vendoring MIT skills is fine. Need to check each source's LICENSE before copying. Mitigation: per-skill license check during vendoring.
- **Trigger collision.** Multiple skills firing on the same diff dilutes context. Mitigation: triggers should be increasingly specific (`brain-wal-audit` more specific than `rust-perf` more specific than `rust-anti-pattern`).
- **Drift with upstream.** Vendored skills can become stale. Mitigation: pin to a commit SHA; revisit per-phase exit (a `skill-refresh` task in Phase 11).
- **Skill bloat.** Easy to add 30 skills and have none fire reliably. Mitigation: each skill must list a concrete trigger; review in batches.
- **Open question: should `production-checklist` block merge automatically (hook), or stay advisory?** Defer to user — phase-9 onward when we have something to merge.

## 9. Test plan

Skills don't have unit tests in the cargo sense. The verification model:

- **Frontmatter lint.** A small bash script in `scripts/check-skills.sh` validates every `SKILL.md` has required YAML keys and a non-empty body. Runs in CI alongside `verify`.
- **Trigger smoke.** For each authored skill, write 2–3 example diffs / prompts that should and should not trigger; capture in `examples/` so future-us can verify the skill still routes correctly.
- **Spec cross-reference.** For project-specific skills, `spec-refs:` frontmatter links to the spec sections enforced. A smoke script verifies every referenced file exists.

## 10. Implementation phases

Six commits, in order:

1. **`skills(infra): add skill conventions and CI lint`** — `.claude/skills/CONVENTIONS.md` + `scripts/check-skills.sh` (frontmatter lint) + CI step. Sets the scaffolding without shipping any skills yet.
2. **`skills(vendor): pull rust core skills from actionbook/rust-skills`** — `rust-unsafe-checker`, `rust-anti-pattern`, `rust-perf`, `rust-error-handling`, `rust-concurrency`. With attribution + adaptations.
3. **`skills(vendor): pull skill-creator from anthropics/skills`** — meta-skill we use to author the project-specific ones in step 4.
4. **`skills(brain): author core invariant skills`** — `brain-invariants`, `brain-spec-invariant`, `brain-protocol-version-bump`, `plan-create`, `production-checklist`.
5. **`skills(brain): author runtime / storage skills`** — `brain-glommio-rules`, `brain-tokio-boundary`, `brain-wal-audit`, `brain-arena-audit`, `brain-zero-copy-review`.
6. **`skills(brain): author observability + testing skills`** — `brain-redb-schema`, `brain-hnsw-tuning`, `brain-fuzz-target`, `brain-loom-design`, `brain-chaos-test`, `brain-obs-trace`, `brain-perf-target`.

Each commit independent; safe to merge to `dev` per the `feature/*` workflow.

Estimated time: a few hours per batch since each skill is ~50–150 lines of focused markdown.

## 11. After confirmation

If approved, the next steps are:

1. Confirm scope (you may trim or extend the list — be opinionated).
2. Confirm vendoring strategy: copy + attribute, vs. git submodule, vs. periodic upstream sync.
3. Once scope locked, I execute Phase 1 (CI lint + conventions doc) as the first commit, surface for review, then proceed.

## 12. Confirmation

Awaiting "go" / "approved" / specific revisions.

---

## Appendix A — Sources cited

- [`anthropics/skills`](https://github.com/anthropics/skills)
- [`actionbook/rust-skills`](https://github.com/actionbook/rust-skills)
- [`awesome-skills/code-review-skill`](https://github.com/awesome-skills/code-review-skill)
- [`VoltAgent/awesome-agent-skills`](https://github.com/VoltAgent/awesome-agent-skills)
- [`ComposioHQ/awesome-claude-skills`](https://github.com/ComposioHQ/awesome-claude-skills)
- [`sickn33/antigravity-awesome-skills`](https://github.com/sickn33/antigravity-awesome-skills)
- Anthropic skill format spec: <https://github.com/anthropics/skills/tree/main/spec>
- Skill-format announcement: October 2025 (Claude Code), open-standard release December 2025.
