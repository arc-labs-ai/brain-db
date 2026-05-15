# 02 — Build and verify

Compile the workspace and run the full verification suite (`fmt` +
`build` + `clippy -D warnings` + `test`).

## 1. Build the workspace

**Input (inside container):**

```bash
cargo build --workspace
```

**Or from host:**

```bash
just docker cargo build --workspace
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
just docker-verify
```

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
just docker-test -p brain-protocol -- --nocapture
```

**Clippy only:**

```bash
just docker-clippy
```

**One specific test:**

```bash
just docker cargo test -p brain-server --test e2e
```

**One specific test function with output:**

```bash
just docker cargo test -p brain-storage --lib -- arena::tests::crc_mismatch_halts --nocapture
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
