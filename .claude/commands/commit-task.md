---
description: Stage and commit the current sub-task with a properly-formatted message per AUTONOMY.md §5
argument-hint: <task-id> <one-line-summary>
allowed-tools: Bash(git status:*), Bash(git add:*), Bash(git diff:*), Bash(git commit:*), Bash(git log:*), Bash(grep:*), Read
---

Commit the current work as the completion of a single sub-task. The argument is the task ID (e.g. `1.3`) and a one-line imperative summary (e.g. `implement frame header decoder with CRC validation`).

Steps:

1. **Pre-commit verification.** Run `just verify` (or the equivalent `cargo` chain). If it fails, abort the commit and surface the failure — never commit red. Per `AUTONOMY.md` §2 rule 3.

2. **Identify the spec file(s) referenced** in the sub-task. From `docs/phases/phase-NN-*.md`, find the sub-task by ID. Extract the entries from its "Reads:" list.

3. **Compose the commit message** in the format from `AUTONOMY.md` §5:

   ```
   <task-id>: <summary in imperative mood>

   <one-paragraph "why" — one or two sentences linking the change to the spec
   section it implements>

   Refs: spec/<section>/<file>.md
   Refs: spec/<section>/<file>.md   (if multiple)
   ```

4. **Show the user the staged diff and the proposed message**. Wait for confirmation.

5. **On confirmation**: `git add -A && git commit -m "..."`. Don't push (push is deny-listed; manual operation only).

6. **Update the phase doc**: change the sub-task's `Done when:` checkboxes from `[ ]` to `[x]` if all criteria are met. If not all criteria are met, refuse to commit — the sub-task isn't done.

If the user invoked this with no arguments, read `git status --porcelain` and infer:
- The sub-task ID by looking at the "Next" line that `/status` would produce.
- The summary from a one-line description of what changed.

Then propose the commit message and wait for confirmation.

Honesty rule (AUTONOMY §18): if the work is partial, say so explicitly in the message ("partial implementation of X; finishes blocker Y") and update the phase doc to reflect what's still outstanding. Never mark a sub-task `[x]` if criteria are unmet.
