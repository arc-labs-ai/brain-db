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
# Unit + integration run under nextest (parallel process-per-test, faster on
# our large suite); doctests stay on `cargo test --doc` since nextest doesn't
# run them. Install once: `cargo install --locked cargo-nextest`.
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Run a specific crate's tests with output.
test-one CRATE:
    cargo nextest run -p {{CRATE}} --no-capture

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
    @./.claude/scripts/check-skills.sh

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
    @devcontainer exec --workspace-folder . cargo nextest run --workspace
    @devcontainer exec --workspace-folder . cargo test --workspace --doc
    @devcontainer exec --workspace-folder . cargo clippy --workspace --all-targets -- -D warnings

# Linux tests, scoped. Example: just docker-test -p brain-storage -p brain-server
docker-test *ARGS:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . cargo nextest run {{ARGS}}

# Linux clippy.
docker-clippy:
    @devcontainer up --workspace-folder . >/dev/null
    @devcontainer exec --workspace-folder . cargo clippy --workspace --all-targets -- -D warnings

# The full verification suite — what CI runs.
verify: fmt-check build clippy test check-skills

# The production gate. Runs every check CI runs (`.github/workflows/ci.yml`)
# locally in one command. Sequential link (`-j 1`) on the test sweep
# matches CI's OOM-avoidance posture; if you have ≥ 16 GB free, drop
# `-j 1` for faster local runs. Release-build smoke + doc build round
# out the gates. The acceptance benches are not run here — they live
# in the nightly perf workflow because they take 5–15 minutes each.
prod-verify:
    cargo fmt --all -- --check
    cargo clippy --workspace --tests --all-features -- -D warnings
    cargo test --workspace --all-targets --no-fail-fast -j 1
    cargo test --workspace --doc
    cargo doc --workspace --no-deps
    cargo build --release --bin brain-server
    ./.claude/scripts/check-skills.sh

# Run the acceptance benches (asserted p50/p99 from spec §16/02).
# Same gates the nightly-perf workflow runs.
prod-bench:
    cargo bench -p brain-planner --bench relation_traverse
    cargo bench -p brain-index --bench lexical_retrieve

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

# ─────────────────────────────────────────────────────────────────────────────
# Production Docker — distinct from the `.devcontainer/` recipes above
# (those build a dev image for Linux cross-compile from macOS). The recipes
# below build the runtime image that ships to operators and runs Brain itself.
# ─────────────────────────────────────────────────────────────────────────────

# Build the production image. Uses BuildKit cache mounts — first build
# takes ~15 min from cold; subsequent builds reuse the cargo cache.
image TAG="latest":
    DOCKER_BUILDKIT=1 docker build -t brain:{{TAG}} .

# Run the production image with a named volume for data. Foreground;
# Ctrl-C stops it. Smoke-test target after the image-build path.
image-run TAG="latest":
    @docker rm -f brain >/dev/null 2>&1 || true
    docker run --rm --name brain \
        --security-opt seccomp=unconfined --ulimit memlock=-1 \
        -p 8080:8080 -p 9091:9091 \
        -v brain-data:/var/lib/brain/data \
        -v brain-models:/var/lib/brain/models \
        brain:{{TAG}}

# Bring up the brain service via compose (config/docker-compose.yml).
compose-up:
    docker compose -f config/docker-compose.yml up -d --build
    @echo
    @echo "brain  data plane : 127.0.0.1:8080"
    @echo "brain  health     : http://127.0.0.1:9091/healthz"

# Tear down the compose stack. Pass `-v` to also drop data volumes.
compose-down *ARGS:
    docker compose -f config/docker-compose.yml down {{ARGS}}

# ─────────────────────────────────────────────────────────────────────────────
# Local serve — run the production image on macOS/Docker Desktop so native
# (non-container) clients (brain-shell, the SDKs) can reach it. Differs from
# `image-run` (the operator smoke test on Linux) in the ways Docker Desktop +
# a dev laptop need:
#   • --security-opt seccomp=unconfined  → io_uring works under Docker Desktop
#   • bind-mounts the already-bootstrapped BGE model (no HuggingFace download)
#   • env-disables the model-hungry tiers (rerank / classifier / llm) so the
#     shard spawns embed-only instead of hard-failing
#   • PORT is a parameter (8080 is often taken locally; use 18080 etc.)
# ─────────────────────────────────────────────────────────────────────────────

# One command: compile the DB code (Linux builder stage) into the image,
# then start the server in a container with the port bound. This is the
# "spin up Linux → compile → serve" one-liner. `just up 18080` if 8080 is
# taken. The compile step is cached, so re-runs after no code change are fast.
up PORT="8080": image (serve-local PORT)

# Run the DB locally, detached, exposing the data plane on PORT (default 8080).
# Prereqs: `just image` once, and `.devcontainer/bootstrap-model.sh --only embed`.
# Example (8080 busy): `just serve-local 18080`   →  connect to 127.0.0.1:18080
serve-local PORT="8080" TAG="latest":
    @docker rm -f brain-local >/dev/null 2>&1 || true
    docker run -d --name brain-local --security-opt seccomp=unconfined \
        -p {{PORT}}:8080 -p 9091:9091 \
        -v "$HOME/.local/share/brain/models/bge-small-en-v1.5:/models/bge-small-en-v1.5:ro" \
        -v brain-local-data:/var/lib/brain/data \
        -e BRAIN_EMBED_MODEL_DIR=/models/bge-small-en-v1.5 \
        -e BRAIN__SHARD__ARENA_CAPACITY_BYTES=256MiB \
        -e BRAIN__RERANK__ENABLED=false \
        -e BRAIN__EXTRACTORS__CLASSIFIER__ENABLED=false \
        -e BRAIN__EXTRACTORS__LLM__ENABLED=false \
        brain:{{TAG}}
    @echo "Brain DB starting on 127.0.0.1:{{PORT}} (health: http://127.0.0.1:9091/healthz)"
    @echo "Connect a client:  export BRAIN_SERVER=127.0.0.1:{{PORT}}"
    @echo "Stop:  just serve-stop"

# Stop + remove the local serve container (keeps the brain-local-data volume).
serve-stop:
    @docker rm -f brain-local >/dev/null 2>&1 || true
    @echo "stopped brain-local (data volume kept; `docker volume rm brain-local-data` to wipe)"
