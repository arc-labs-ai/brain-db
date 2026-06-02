---
description: Stage and commit the current sub-task with a properly-formatted message per AUTONOMY.md §5
argument-hint: <task-id> <one-line-summary>
allowed-tools: Bash(git status:*), Bash(git add:*), Bash(git diff:*), Bash(git commit:*), Bash(git log:*), Bash(grep:*), Read
---

Commit the current work as the completion of a single sub-task. The argument is the task ID (e.g. `1.3`) and a one-line imperative summary (e.g. `implement frame header decoder with CRC validation`).

Steps:

1. **Pre-commit verification.** Run `just verify` (or the equivalent `cargo` chain). If it fails, abort the commit and surface the failure — never commit red. Per `AUTONOMY.md` §2 rule 3.

2. **Identify the spec file(s)** the change implements — the sections you read while doing the work — for the `Refs:` trailer.

3. **Compose the commit message** in the format from `AUTONOMY.md` §5:

   ```
   <task-id>: <summary in imperative mood>

   <one-paragraph "why" — one or two sentences linking the change to the spec
   section it implements>

   Refs: spec/<section>/<file>.md
   Refs: spec/<section>/<file>.md   (if multiple)
   ```

4. **Show the user the staged diff and the proposed message**. Wait for confirmation.

5. **On confirmation**: `git add -A && git commit -m "..."`. Don't push (push is deny-listed; manual operation only). Never append a `Co-Authored-By` trailer — commits show only the repo owner's authorship.

If the user invoked this with no arguments, read `git status --porcelain` and `git diff` and infer the task ID and a one-line summary from what changed. Then propose the commit message and wait for confirmation.

Honesty rule (AUTONOMY §18): if the work is partial, say so explicitly in the message ("partial implementation of X; finishes blocker Y"). Never describe unfinished work as done.
