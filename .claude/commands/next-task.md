---
description: Identify the next sub-task to work on, using the per-phase docs
allowed-tools: Read, Glob, Grep, Bash(git log:*), Bash(git tag:*), Bash(git status:*), Bash(cargo check:*), Bash(ls:*)
---

Find the next concrete sub-task. Be specific — name the task ID, the spec sections to read first, and the files to write.

Algorithm:

1. **Determine the active phase.**
   - List git tags: `git tag --list 'phase-*-complete'`.
   - The active phase is the lowest N such that `phase-N-complete` is **not** tagged.
   - If no `phase-*-complete` tag exists at all, active phase is **Phase 0** (verify scaffolding) or **Phase 1** (start wire protocol) depending on whether `just verify` passes.

2. **Open the relevant phase doc.**
   - `docs/phases/phase-NN-*.md` — the file matching the active phase.
   - For Phase 0, there is no detailed doc — the work is just `just verify` then tag.

3. **Find the lowest unfinished sub-task.**
   - In the phase doc, scan the sub-task headers (`### Task N.M — ...`).
   - For each, check the "Done when:" block — if any `[ ]` remains, this sub-task is unfinished.
   - The first unfinished sub-task in document order is the next one.

4. **Sanity check.**
   - Run `git status --short`. Are there uncommitted changes? If yes, the user may already be mid-task. Surface that.
   - Run `cargo check --workspace` (silent). If it fails, the next task is "fix the build before proceeding."

5. **Propose, don't implement.** Output:

   ```
   Active phase: <N> — <Title>
   Phase doc: docs/phases/phase-NN-*.md
   
   Next sub-task: <task-id> — <title>
   
   Reads (in order):
   - spec/<section>/<file>.md
   - spec/<section>/<file>.md
   
   Writes:
   - <file>
   - <file>
   
   Done when:
   - [ ] <criterion>
   - [ ] <criterion>
   
   Pitfalls to watch for:
   - <pitfall>
   ```

6. **End with a single-sentence summary** like "Ready to start 1.3 when you are." Don't begin work without confirmation, even in autonomous mode — the user might want to defer or skip.

Special cases:

- **Phase 0 not verified yet**: output is "Run `just verify`. If green, tag `phase-0-complete` and re-run /next-task."
- **CONTEXT.md exists at project root**: surface it. The previous session stopped and surfaced; the next action is to resolve that, not start a new sub-task.
- **All phases tagged complete**: output is "All phases complete. Next: cut a release per phase 11.12 or start v2 planning."
