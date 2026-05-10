---
description: Audit implementation against the spec for a given crate or component
argument-hint: <crate-name | spec-section>
allowed-tools: Read, Glob, Grep, Bash(rg:*), Bash(cargo doc:*)
---

Audit whether the implementation matches the spec for `$ARGUMENTS`.

This is a careful, slow review — not a quick check. The spec is authoritative; the implementation should be a faithful realization.

Process:

1. **Identify the scope.**
   - If `$ARGUMENTS` is a crate name: audit that crate against its corresponding spec sections (e.g. `brain-storage` → `spec/05_storage_arena_wal/`).
   - If `$ARGUMENTS` is a spec section number (e.g. `09` or `09_cognitive_operations`): audit the relevant code against that spec.

2. **Read the spec section.** All files in the spec directory, including the README and open-questions.

3. **Find the implementation.** Locate the relevant code via grep and the workspace structure.

4. **Cross-reference systematically.**
   - For each MUST in the spec: find the corresponding code or test.
   - For each documented invariant: verify it's enforced.
   - For each error code: verify it's actually raised in the right circumstances.
   - For each parameter (e.g. M=16, ef_search=64): verify the code uses the spec'd value.

5. **Report findings as three lists:**
   - **✓ In spec, in code, correct.** (high-level summary, not exhaustive)
   - **⚠ In spec, in code, possibly wrong.** Each item: spec quote + code reference + concern.
   - **✗ In spec, NOT in code.** Each item: spec quote + missing item.
   - **? In code, NOT in spec.** Each item: code reference + concern that this isn't justified by spec.

6. **Recommend.** What needs fixing, in priority order. Group as: "blocks correctness" / "violates invariant" / "missing feature" / "spec drift" / "minor".

Don't fix anything during the audit — just report. The user reviews and decides.

If the audit finds something the user clearly needs to know about (a violated invariant, a missing critical feature), say so plainly at the top of the report.
