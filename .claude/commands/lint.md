---
description: Run rustfmt and clippy with strict lints
argument-hint: [crate-name | --fix]
allowed-tools: Bash(cargo clippy:*), Bash(cargo fmt:*)
---

Run formatting and lint checks. Argument: `$ARGUMENTS`

If `$ARGUMENTS` contains `--fix`, automatically fix what's fixable.

If `$ARGUMENTS` names a specific crate (e.g. `brain-storage`), scope to that crate only with `-p`.

Otherwise, run on the whole workspace.

Default flow:
```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings -W clippy::pedantic -A clippy::module_name_repetitions -A clippy::missing_errors_doc -A clippy::missing_panics_doc
```

The pedantic-minus-noisy-lints set matches the project's `clippy.toml`.

If `--fix` is passed:
```
cargo fmt --all
cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged -- -D warnings
```

Report what was fixed and what couldn't be auto-fixed.
