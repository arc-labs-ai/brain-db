---
description: Show current phase progress, last commit, and what to do next
allowed-tools: Bash(git log:*), Bash(git tag:*), Bash(git status:*), Bash(grep:*), Bash(find:*), Bash(wc:*), Bash(head:*), Bash(ls:*), Read, Glob
---

Print a concise status report on where the project currently stands. The user (or you) should be able to run this at any time and immediately know what's done, what's next, and whether anything needs attention.

Steps:

1. **Git state**
   - `git status --short` — any uncommitted changes?
   - `git log --oneline -10` — last 10 commits.
   - `git tag --list 'phase-*'` — which phases are tagged complete.

2. **Active phase**
   - The lowest-numbered phase that does NOT have a `phase-N-complete` tag is the active phase.
   - Open `docs/phases/phase-NN-*.md` for that phase.
   - Count `[x]` vs `[ ]` checkboxes in sub-task headers and the phase exit checklist.

3. **Next sub-task**
   - The lowest-numbered sub-task in the active phase doc that is NOT yet `[x]`.
   - Print its ID, title, and the first line of its "Reads" list.

4. **Health checks**
   - Is the working tree clean? (If no, list the dirty files.)
   - Does the last commit message match the format from `AUTONOMY.md` §5?
   - Are there any `CONTEXT.md` files at the project root? (If yes, surface them — they signal a stop-and-surface in progress.)
   - Run `cargo check --workspace` (silent unless it fails). If it fails, surface the failure.

5. **Output format** — one block, easy to scan:

   ```
   Phase: <N> — <Title>          (e.g. "Phase 2 — Storage")
   Progress: <done>/<total> sub-tasks (<%>)
   Last tag: <phase-N-complete or "none yet">
   Last commit: <hash> <subject>
   Working tree: <clean | dirty (M files)>

   Next: <task-id> — <title>
   Reads: <first spec file>

   Health:
   - cargo check: ✓ | ✗ <error>
   - CONTEXT.md present: ✓ none | ✗ see CONTEXT.md
   - last commit format: ✓ | ✗ does not match AUTONOMY §5
   ```

If multiple things need attention, list them all under "Health" with `✗`. If everything is green and there's a clear next sub-task, the user can just say "go" and you proceed.

Keep the output to ≤ 25 lines. Don't editorialize unless something is wrong.
