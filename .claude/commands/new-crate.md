---
description: Scaffold a new workspace crate with the project's conventions
argument-hint: <crate-name> [--lib | --bin]
allowed-tools: Bash(cargo new:*), Bash(mkdir:*), Bash(cargo build:*), Read, Write, Edit
---

Scaffold a new crate in the workspace.

Argument: `$ARGUMENTS` — e.g. `brain-storage` or `brain-server --bin`.

The crate name should start with `brain-` for consistency with the rest of the workspace.

Steps:

1. **Determine type.** Default is `--lib`. If `--bin` is in the arguments, create a binary crate instead.

2. **Create the crate.**
   ```
   cd crates/
   cargo new --lib <name>           # or --bin
   ```

3. **Edit `crates/<name>/Cargo.toml`:**
   - Set `edition = "2021"`.
   - Inherit workspace metadata: `version.workspace = true`, etc.
   - Add to dependencies: only what's actually needed.
   - Use workspace dependency table (`x.workspace = true`) for shared deps.

4. **Add to workspace `Cargo.toml`:**
   - Append the new crate path under `[workspace] members`.

5. **Edit `lib.rs` (or `main.rs`):**
   - Add a module-level doc comment referencing the relevant spec section(s).
   - Set crate-wide lints:
     ```rust
     #![warn(clippy::pedantic)]
     #![allow(clippy::module_name_repetitions, clippy::missing_errors_doc)]
     #![forbid(unsafe_code)]  // unless this crate is brain-storage
     ```
   - Leave a `// TODO: spec/<section>` marker.

6. **Verify.**
   ```
   cargo build -p <name>
   cargo clippy -p <name>
   ```

7. **Confirm to the user.** Show the created files, and remind which spec section the crate maps to.

If the crate name doesn't match a known spec section (per `CLAUDE.md` §7), flag this and ask before proceeding.
