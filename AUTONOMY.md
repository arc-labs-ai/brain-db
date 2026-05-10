# Autonomy Contract

This file defines how Claude operates on this project **when running without permission prompts** (e.g. `claude --dangerously-skip-permissions`). It's the operating system for autonomous work.

The user has chosen to delegate routine decisions. In return, Claude commits to operating predictably, conservatively, and recoverably. **No surprises, no spec drift, no broken commits.**

---

## 1. The execution loop

Every working session, Claude runs this loop until told to stop or until a phase completes:

```
LOOP:
  1. Read current state:
     - git status, git log -5
     - last commit's tag (phase-N-task-M-complete or similar)
     - check ROADMAP.md and the active docs/phases/phase-NN-*.md

  2. Decide the next sub-task:
     - The lowest-numbered sub-task in the current phase that isn't ✓ done
     - Open its phase doc; re-read its "Reads" list

  2a. Plan first (see §21):
     - For a new phase OR a substantial sub-task, write a plan to
       `.claude/plans/phase-NN[-task-MM].md` and STOP for user confirmation.
     - For trivial sub-tasks, surface a one-line summary and proceed.

  3. Implement the sub-task:
     - Read the listed spec sections in full
     - Write the listed code files
     - Add the listed tests
     - Run the local verify loop (cargo check / test / clippy)
  
  4. If verify is green:
     - git add + commit with the prescribed message format
     - Mark the task ✓ in the phase doc
     - Continue the loop
  
  5. If verify fails:
     - Read the failure carefully; do not pile on changes
     - Fix the immediate cause, run verify again
     - If still failing after 3 attempts: STOP, write CONTEXT.md, surface to user
  
  6. If the sub-task involves a decision the docs don't resolve:
     - Read the spec's open_questions.md for that section
     - If still unclear: STOP, write CONTEXT.md, surface to user
  
  7. At end of phase:
     - Run the phase exit checklist
     - Tag the commit phase-N-complete
     - Don't proceed to phase N+1 without checking ROADMAP for explicit "ready" signal
```

This loop is deliberately tight. No exploration, no refactors-on-the-side, no "while I'm here" detours.

## 2. Hard rules

These rules don't bend, regardless of how appealing the deviation looks:

1. **The spec is the truth.** Code follows spec. If they conflict, the spec wins. To change behavior, propose a spec change first; never edit code to "patch over" a spec issue.

2. **Don't edit `spec/`.** It's read-only for autonomous Claude. Spec changes require the user.

3. **Every commit compiles and tests pass.** If you can't make it green, stash and surface. Never commit red.

4. **One sub-task per commit.** Makes bisect trivial. Rarely, a sub-task naturally splits into 2-3 commits — that's fine; never combine them.

5. **No skipping phases.** Phase N+1 doesn't start before phase N is exited and tagged.

6. **No new dependencies without justification.** Approved set is in `CLAUDE.md` §5. Adding one requires writing the rationale into the commit message and the relevant phase doc's "Decisions" log.

7. **No `unsafe` outside `crates/brain-storage`.** That crate has explicit need (mmap). Other crates use safe Rust.

8. **No public-API breakage in published crates.** Once a crate has been published (none yet, but eventually), public-API changes go through a SemVer bump and a deprecation cycle.

9. **No edits to `.github/`, `.claude/`, `Cargo.toml` workspace block, or `rust-toolchain.toml`** without surfacing first. These are infrastructure; routine work doesn't touch them.

10. **Three strikes you stop.** If the same operation fails three times, stop and surface. Don't enter a doom loop.

## 3. The "STOP and surface" protocol

When stuck, don't thrash. Execute this:

1. Stop all tool use.
2. Write `CONTEXT.md` at the project root with:
   - **What I was doing**: phase, sub-task ID, intended change.
   - **What went wrong**: error output, your interpretation.
   - **What I tried**: attempts so far, with outcomes.
   - **What I think is happening**: best hypothesis.
   - **Options I see**: 2-4 ways forward, with trade-offs.
   - **What I need from you**: specific question or decision.
3. Write a single one-line summary to stdout: `STOPPED at <phase>.<task> — see CONTEXT.md`.
4. Stop the session. Don't try to recover by writing more code.

When the user resumes, they'll either resolve the question, edit code themselves, or update the phase doc. Don't pre-empt their decision.

## 4. The verify loop

Before any commit:

```
just verify
```

which runs:

```
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
```

All must pass. If `cargo fmt` would change something, run `cargo fmt --all` and re-verify.

For multi-crate work, verify the whole workspace, not just the touched crate. Cross-crate breakage is the most common autonomous-mode failure.

## 5. Commit message format

```
<phase>.<task>: <summary in imperative mood>

<one-paragraph "why" — link to spec section if relevant>

Refs: spec/<section>/<file>.md
```

Example:

```
1.3: implement frame header decoder with CRC validation

The decoder validates magic bytes, version, header CRC32C, and length
bounds before returning. Per spec §11 ("Validation"), invalid frames
return InvalidFrame errors with no partial state.

Refs: spec/03_wire_protocol/03_frame_header.md
Refs: spec/03_wire_protocol/11_validation.md
```

Tags at end-of-phase:

```
git tag phase-1-complete
git tag phase-1-complete-recovery  # if from CONTEXT.md recovery
```

## 6. Branching strategy

Three long-lived branches:

- **`main`** — release/stable. Only advances when a phase is fully complete and signed off; `phase-N-complete` tags live here.
- **`dev`** — integration. Feature branches merge into `dev` first; this is where cross-feature interactions are exercised before promotion.
- **`feature/<topic>`** — where day-to-day work happens. One branch per phase or per major sub-system (e.g. `feature/brain-protocol`).

Flow per sub-task:

1. Be on the relevant `feature/<topic>` branch (create it from `dev` if it doesn't exist).
2. Commit each sub-task on the feature branch using the format from §5.
3. When a sub-system / phase milestone is reached, fast-forward or `--no-ff` merge `feature/<topic>` → `dev`. Run the verify suite on `dev`.
4. At a phase boundary (all sub-tasks `[x]`, exit checklist green): merge `dev` → `main`, then tag `phase-N-complete` on `main`.
5. Long-running feature branches stay alive across sub-tasks but should be merged forward to `dev` at least once per phase to avoid drift.

Exception: experimental work that may not pan out goes on `experiment/<topic>` and doesn't have to merge. If it pans out, promote it to a `feature/<topic>` and follow the standard flow.

Never commit directly to `main` or `dev` — those are merge-only.

## 7. What "done" means for a sub-task

A sub-task is done when **all** of these hold:

- The "Done when" criteria in the phase doc are met (each `[ ]` is `[x]`).
- The relevant tests pass (added or pre-existing).
- Workspace is green (`just verify`).
- Commit is made with the prescribed message.
- Phase doc is updated to reflect completion.

If any one of these is missing, the sub-task isn't done — keep working or stop and surface.

## 8. What "done" means for a phase

A phase is done when **all** of these hold:

- Every sub-task in its phase doc is marked `[x]`.
- The phase exit checklist (last section of the phase doc) is fully `[x]`.
- A `phase-N-complete` git tag exists at the most recent green commit.
- The next phase's prerequisites are satisfied.

Don't tag prematurely. The tag is the contract that this is a stable point.

## 9. When the spec is silent

Specs don't cover everything. When you encounter a gap:

1. Re-read the relevant spec section carefully — sometimes the answer is implied.
2. Read that spec's `*_open_questions.md`.
3. Look for prior art: how did Phase 0/1 handle similar gaps?

If still unclear, **stop and surface**. Don't invent.

The cost of pausing for 5 minutes to ask is far lower than the cost of a wrong choice that compounds for weeks.

## 10. Performance work

Don't optimize during initial implementation. Make it correct, make it tested, make it clear. Performance work is its own phase (Phase 11) with explicit benchmarks.

The exception: if a sub-task's spec section has a hard performance requirement (e.g. "must be lock-free"), implement to that requirement.

## 11. Documentation discipline

For every public item (function, type, constant): rustdoc with at least one example for non-trivial cases.

For every module: a `//!` doc comment explaining what's in it and which spec section it implements.

For every TODO: a tracking comment in the format `// TODO(phase-N): <what>`. Don't leave bare TODOs.

Documentation is part of "done." A correct but undocumented function is not done.

## 12. The "scope creep" guard

The biggest autonomous-mode failure mode is scope creep — "while I'm here, let me also...". Resist it. Specifically:

- ❌ Don't refactor unrelated code.
- ❌ Don't "improve" the spec.
- ❌ Don't add features the spec doesn't ask for.
- ❌ Don't optimize speculatively.
- ❌ Don't add abstractions for hypothetical future users.
- ✓ Do exactly what the sub-task says.
- ✓ Do file an issue (or note in `docs/notes/`) if you spot something worth doing later.

Scope creep is how 1-day phases become 1-week phases.

## 13. The "doc/notes/" practice

For thoughts that don't belong in the spec or in a commit message — observations, future-work ideas, gotchas — write a dated note in `docs/notes/YYYY-MM-DD-topic.md`.

Format:

```
# <topic>

Date: YYYY-MM-DD
Phase: <N>
Status: observation | follow-up | resolved

## Observation
<what>

## Why it matters
<why>

## Suggested follow-up
<optional>
```

These are for the user's later review. Don't act on them autonomously.

## 14. Test-driven where practical

When implementing a sub-task with non-trivial logic:

1. Write the test first (it will fail to compile or run).
2. Implement minimally to make it pass.
3. Refactor for clarity.
4. Run the verify loop.

For trivial sub-tasks (constants, simple struct definitions), skip the TDD cycle. Just write the code with the inline tests.

## 15. The "I have to use unsafe" decision

Outside `brain-storage`, the answer is: **don't**. Find a safe abstraction. If genuinely impossible, **stop and surface** — this is a design-level decision.

Inside `brain-storage`, `unsafe` is allowed for memory-mapping and pointer arithmetic on the arena. Every `unsafe` block must:
- Have a safety comment (`// SAFETY: ...`) explaining why the invariants hold.
- Be the smallest scope possible.
- Have a test that exercises it.

## 16. Time pressure and quality

There's no time pressure on this project. The user values correctness over speed. If a sub-task takes longer than expected, that's fine — don't cut corners.

If you find yourself thinking "I'll just skip this test, the next sub-task can add it" — stop. That's how regressions slip in.

## 17. Reading the spec is mandatory, not optional

Each sub-task lists `Reads:` — those spec files **must** be read before writing code. Skimming doesn't count. The spec encodes constraints that aren't obvious from the API surface.

If a sub-task's "Reads" is empty, the sub-task is too trivial to need spec consultation (e.g. "add `lazy_static` to deps"). These are rare.

## 18. The "honest commit" rule

Commit messages reflect what was actually done, not what was planned. If a sub-task didn't fully complete:

- Either complete it before committing,
- Or commit the partial work with an honest message ("partial implementation of X; finishes blocker Y") and update the phase doc to reflect what's still pending.

Never commit "done" when it isn't.

## 19. The reset condition

If at any point Claude realizes the codebase has drifted from the spec — that something was implemented wrong and other code now depends on it — execute "STOP and surface" with `CONTEXT.md` describing the drift. Don't try to fix it autonomously; the user needs to make the call about whether to revert, fix forward, or update the spec.

Detecting drift is good. Trying to silently fix it makes things worse.

## 20. What this contract is for

This contract exists so the user can run Claude Code, walk away, and trust that progress will be:

- **Bounded**: stops cleanly when uncertain.
- **Reversible**: every step has a clean commit, easy bisect.
- **Transparent**: every decision is in a commit message or `CONTEXT.md`.
- **Spec-faithful**: the implementation tracks the design, always.

The user gets to do other work. Claude does the substrate. The contract is the bridge.

## 21. The planning step

Before implementation begins, Claude writes a plan and pauses for user confirmation. The plan is:

1. **Always required** for a new **phase** (any phase-NN-*.md transition). Write `.claude/plans/phase-NN.md`.
2. **Required** for a **substantial sub-task** — one that:
   - Introduces a new dependency, framework, or library.
   - Touches multiple crates or alters cross-crate boundaries.
   - Implements a non-trivial algorithm (CRC layout, allocator, codec, etc.).
   - Spans more than ~200 lines of new code.
   Write `.claude/plans/phase-NN-task-MM.md`.
3. **Skippable** for **trivial sub-tasks** — a constant pin, a one-function helper, a doc-only change. Surface a one-line "I'm doing X, then committing" and proceed without a plan file.

### What the plan must cover

- **Scope**: what the sub-task does, what it does NOT do, and where it fits in the phase.
- **Spec references**: the spec files read, with section anchors. Quote any constraints that bind the design.
- **External validation** (when relevant): for new frameworks/libraries/algorithms, search the web for current best practices, version pins, breaking-change notes. Capture URLs and the relevant excerpt — "rkyv 0.7 docs say X". Skip when the work is purely internal wiring.
- **Architecture sketch**: the types/modules introduced, how they compose, what the public surface looks like.
- **Trade-offs considered**: 2–4 alternative designs and why the chosen one wins.
- **Risks / open questions**: what could go wrong; what the spec leaves ambiguous (cross-reference any `*_open_questions.md`).
- **Test plan**: which tests prove correctness; which `Done when` items each maps to.
- **Estimated commit shape**: one commit or 2–3? What goes where?

### The confirmation gate

After writing the plan, Claude prints:

```
PLAN READY: see .claude/plans/<file>.md — confirm to proceed.
```

and stops. Claude does not write code until the user confirms (a "go" / "approved" / "confirmed" or specific revisions). If the user requests changes, Claude updates the plan and re-surfaces.

### Plan files are durable

Plan files stay in the repo. They serve as durable design artifacts, complementing commit messages. Treat them like ADRs: future-Claude reads them to understand why a phase was built the way it was.

## 22. Platform

Brain is **Linux-only**, kernel ≥ 5.15. Spec §01/05 §1.1 lists the Linux-specific facilities Brain depends on (`io_uring`, `O_DIRECT`, `pwritev2(RWF_DSYNC)`, `madvise(MADV_RANDOM/MADV_DONTDUMP)`, `fallocate(FALLOC_FL_KEEP_SIZE)`) and the explicit decision to *not* abstract them: "for a system whose value proposition is latency, a single optimized backend is better than a portable one."

Operationally:

- Crates that touch the runtime, storage, ANN persistence, or any libc syscall (`brain-storage`, `brain-server`, `brain-workers`, `brain-index` once it persists, `brain-embed` once it lands) MUST gate their entire crate behind `#[cfg(target_os = "linux")]`. On non-Linux the crate emits a `compile_error!` with a friendly message pointing to `README.md (Development environment)`.
- Cross-platform crates (the data-model + wire-protocol layer): `brain-core`, `brain-protocol`. These compile on darwin/Windows for SDK consumers but their tests run on Linux too.
- Tests for runtime/storage code MUST run on Linux. CI gates this; local dev on non-Linux uses a Linux container (Docker / OrbStack / Lima — see `README.md (Development environment)`).
- Glommio, `liburing-sys`, and similar Linux-only crates may be unconditional dependencies of platform-gated crates. They may *not* leak into `brain-core` or `brain-protocol`.

Spec changes to platform stance require user direction (per §2). The current stance is locked in through `spec/01_system_architecture/05_hardware.md` §1.1 — read it before considering "should we be portable here?"

### Implications for autonomous Claude

When Claude is running on a non-Linux host (e.g. darwin):

- `cargo build --workspace` and `cargo test --workspace` will fail at link time on Linux-only crates. This is expected, not a bug.
- The verify loop (§4) is satisfied via the project-provided Linux dev container. `README.md (Development environment)` documents the entry point.
- For fast feedback before a container test cycle, `cargo check --target x86_64-unknown-linux-gnu` (with the target installed) validates compilation without running.
- If neither container nor cross-target is available: STOP and surface (§3). Don't try to make Linux code "work" on the local OS by adding portability shims — that violates this section.
