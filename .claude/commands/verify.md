---
description: Run the full verification suite (build, tests, clippy, fmt)
allowed-tools: Bash(cargo build:*), Bash(cargo test:*), Bash(cargo clippy:*), Bash(cargo fmt:*)
---

Run the full verification suite for the Brain workspace. Report results compactly.

Steps (run in order; stop on first failure unless told otherwise):

1. **Format check.**
   ```
   cargo fmt --all -- --check
   ```
   If this fails, report what would change but don't auto-fix unless asked.

2. **Build the workspace.**
   ```
   cargo build --workspace --all-targets
   ```

3. **Run all tests.**
   ```
   cargo test --workspace --all-targets
   ```

4. **Clippy with strict lints.**
   ```
   cargo clippy --workspace --all-targets -- -D warnings
   ```

5. **Doc tests.**
   ```
   cargo test --workspace --doc
   ```

For each step:
- Report whether it passed.
- If it failed, show the relevant error output (not the entire command output).
- Flag any warnings even when the command "passed."

At the end, give a one-line summary: "✓ All checks passed" or "✗ N failures: <list>".

If `$ARGUMENTS` is "fix", attempt to auto-fix what's fixable (`cargo fmt`, `cargo clippy --fix`) and re-run.
