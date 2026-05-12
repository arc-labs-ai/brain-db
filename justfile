# Brain — convenience command runner.
#
# Install just: https://github.com/casey/just
# Then run `just` to see all recipes, or `just <recipe>`.

# Default recipe: list all available recipes.
default:
    @just --list

# Build the entire workspace.
build:
    cargo build --workspace --all-targets

# Build in release mode.
build-release:
    cargo build --workspace --release

# Run all tests (unit, integration, doc tests).
test:
    cargo test --workspace --all-targets
    cargo test --workspace --doc

# Run a specific crate's tests with output.
test-one CRATE:
    cargo test -p {{CRATE}} -- --nocapture

# Run clippy with strict lints.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Check formatting.
fmt-check:
    cargo fmt --all -- --check

# Format the workspace.
fmt:
    cargo fmt --all

# Validate .claude/skills/ frontmatter and references.
check-skills:
    @./scripts/check-skills.sh

# Build (if needed) the Linux dev container and drop into a bash shell.
# One-shot interactive container — for ad-hoc poking. For ongoing dev
# (running tests, clippy, fmt under Linux), prefer `just docker-up` +
# `just docker <cmd>` — keeps a persistent container so each cargo
# invocation reuses incremental build state.
shell:
    @docker build -t brain-dev:latest -f .devcontainer/Dockerfile .
    @docker run --rm -it \
        -v "$(pwd)":/workspaces/brain \
        -v brain-cargo-registry:/usr/local/cargo/registry \
        -v brain-cargo-git:/usr/local/cargo/git \
        -v brain-target-cache:/workspaces/brain/target \
        -w /workspaces/brain \
        -e RUST_BACKTRACE=1 \
        brain-dev:latest \
        bash

# ─────────────────────────────────────────────────────────────────────────────
# Linux dev container (headless) — `.devcontainer/devcontainer.json` is the
# single source of truth. Mounts (cargo registry / git / target), runArgs
# (memlock + seccomp for io_uring), remoteEnv (RUSTFLAGS + RUST_BACKTRACE +
# CARGO_TERM_COLOR), and the postCreateCommand all live there. VS Code /
# Cursor "Reopen in Container" reads the same file.
#
# CLI install:  npm install -g @devcontainers/cli
#
# `devcontainer up` is idempotent — on subsequent runs it re-attaches to the
# existing container (label-matched by workspace folder); postCreateCommand
# does NOT re-fire. Incremental builds work across invocations because
# /workspaces/brain/target is a named volume.
# ─────────────────────────────────────────────────────────────────────────────

# Bring the dev container up (idempotent). Use after a fresh clone, after
# Docker Desktop restart, or any time you want to ensure it's running.
docker-up:
    @devcontainer up --workspace-folder .

# Stop the dev container; cached volumes (cargo registry, target/) preserved.
docker-stop:
    @docker ps -q --filter "label=devcontainer.local_folder={{justfile_directory()}}" | xargs -r docker stop >/dev/null
    @echo "container stopped (volumes preserved)"

# Remove the dev container entirely (volumes still preserved).
# Use this if the container drifts into a bad state; `docker-up` recreates.
docker-down:
    @docker ps -aq --filter "label=devcontainer.local_folder={{justfile_directory()}}" | xargs -r docker rm -f >/dev/null
    @echo "container removed (volumes preserved)"

# Full rebuild: nuke container + cache, re-run the Dockerfile. Use after
# editing .devcontainer/Dockerfile or devcontainer.json's `mounts`/`runArgs`.
docker-rebuild:
    @devcontainer up --workspace-folder . --remove-existing-container --build-no-cache

# Drop into an interactive shell inside the running container.
docker-shell:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . bash

# Exec an arbitrary command. Auto-starts the container.
# Example: just docker cargo test -p brain-storage --tests --lib
docker *CMD:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . {{CMD}}

# Linux verify suite (the gate for committing Linux-touching code).
# Plain `cargo test` excludes benches by default — criterion benches
# build a 100k-vector HNSW index and hang on ARM Linux emulation,
# so we deliberately don't pass `--all-targets` to `test`.
# Clippy DOES use `--all-targets` because it just type-checks benches
# (no execution), which we want covered.
docker-verify:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . cargo fmt --all -- --check
    @devcontainer exec --workspace-folder . cargo test --workspace
    @devcontainer exec --workspace-folder . cargo clippy --workspace --all-targets -- -D warnings

# Linux tests, scoped. Example: just docker-test -p brain-storage -p brain-server
docker-test *ARGS:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . cargo test {{ARGS}}

# Linux clippy.
docker-clippy:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . cargo clippy --workspace --all-targets -- -D warnings

# The full verification suite — what CI runs.
verify: fmt-check build clippy test check-skills

# Run miri on brain-storage's lib tests. Miri doesn't shim our syscalls
# (mmap/mremap/pwritev2/...), so syscall-bound tests are gated under
# `cfg(not(miri))`; the ~47 pure-data tests run. See
# .claude/plans/phase-02-miri.md for scope.
miri:
    cargo +nightly miri test -p brain-storage --lib

# Auto-fix what's fixable.
fix:
    cargo fmt --all
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# Run the server in dev mode.
run-server:
    cargo run --bin brain-server -- --config config/dev.toml

# Run the CLI.
cli *ARGS:
    cargo run --bin brain-cli -- {{ARGS}}

# Run benchmarks for a crate.
bench CRATE:
    cargo bench -p {{CRATE}}

# Generate documentation and open it.
doc:
    cargo doc --workspace --no-deps --open

# Clean build artifacts.
clean:
    cargo clean

# Audit dependencies for security advisories.
audit:
    cargo audit

# Show outdated dependencies.
outdated:
    cargo outdated --workspace

# Count lines of source code.
loc:
    @find crates -name "*.rs" -not -path "*/target/*" | xargs wc -l | tail -1

# List all spec sections.
specs:
    @ls spec/

# Show the spec section count.
spec-stats:
    @echo "Total spec files: $(find spec -name '*.md' | wc -l)"
    @echo "Total spec lines: $(find spec -name '*.md' -exec cat {} \; | wc -l)"
