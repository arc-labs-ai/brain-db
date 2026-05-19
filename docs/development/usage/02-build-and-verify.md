# 02 — Build and verify

Compile the workspace and run the full verification suite (`fmt` +
`build` + `clippy -D warnings` + `test`).

## 1. Build the workspace

**Input:**

```bash
cargo build --workspace --all-targets
```

**Expected output:**

```
   Compiling brain-core v0.1.0 (/workspaces/brain/crates/brain-core)
   Compiling brain-protocol v0.1.0 ...
   ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3m 12s
```

First build: 3–8 minutes (deps download). Subsequent builds: under
30 seconds.

**Verify:**

```bash
ls target/debug/brain-server target/debug/brain
```

Both binaries should exist.

## 2. Release build (optional)

**Input:**

```bash
cargo build --workspace --release
```

**Verify:**

```bash
ls target/release/brain-server target/release/brain
```

Release binaries are at `target/release/`.

## 3. Run the full verification suite

This is the gate every commit passes. Runs `fmt --check`, the full
test suite, and `clippy -D warnings`.

**Input:**

```bash
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
./scripts/check-skills.sh
```

## 3a. Run the production gate

`prod-verify` is the superset gate the release pipeline runs — same
checks as `.github/workflows/ci.yml`, in one command.

**Input:**

```bash
just prod-verify
```

What runs, in order:

1. `cargo fmt --all -- --check` — formatting clean.
2. `cargo clippy --workspace --tests --all-features -- -D warnings` —
   lints clean across every feature combination.
3. `cargo test --workspace --all-targets --no-fail-fast -j 1` — full
   test sweep. Sequential link (`-j 1`) keeps the link step under
   ~4 GB RSS so it fits the GitHub-Actions runner; drop it locally
   if you have ≥ 16 GB free.
4. `cargo test --workspace --doc` — doc-tests.
5. `cargo doc --workspace --no-deps` — rustdoc builds.
6. `cargo build --release --bin brain-server --bin brain-cli` —
   shipped binaries link under release flags (LTO, codegen-units=1,
   panic=abort). Catches regressions a debug build masks.
7. `./scripts/check-skills.sh` — `.claude/skills/` frontmatter
   conventions.

A green `prod-verify` is the local equivalent of a green CI run.
Expect 8–25 min wall-time depending on incremental-cache state.

The acceptance benches (which assert spec §16/02 p50 / p99 targets in
process and panic on regression) are not part of `prod-verify` — they
take 5–15 minutes each and run in the `nightly-perf` GitHub workflow.
Drive them manually with `just prod-bench`.

**Expected output:**

```
cargo fmt --all -- --check       (no output = clean)
cargo test --workspace           running N tests ... ok
cargo clippy ...                 (no output = clean)
    Finished `dev` profile in X.XXs
```

**Verify:**

```bash
echo $?
```

Exit code 0 = green. Non-zero = at least one gate failed; scroll
back through the output to find the failing line.

## 4. Run a subset

**Single crate's tests with output:**

```bash
cargo test -p brain-protocol -- --nocapture
```

**Clippy only:**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

**One specific test:**

```bash
cargo test -p brain-server --test e2e
```

**One specific test function with output:**

```bash
cargo test -p brain-storage --lib -- arena::tests::crc_mismatch_halts --nocapture
```

## 5. Reading test output

Successful run:

```
running 47 tests
test arena::tests::alloc_smoke ... ok
test arena::tests::crc_mismatch_halts ... ok
...
test result: ok. 47 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

A failing test prints:

```
---- arena::tests::xxx stdout ----
thread 'arena::tests::xxx' panicked at 'assertion failed: ...'
```

`cargo test` returns non-zero exit code on any failure.

## Next

[`03-run-server.md`](03-run-server.md) — start `brain-server` and
verify it's serving.
