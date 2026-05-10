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

# The full verification suite — what CI runs.
verify: fmt-check build clippy test check-skills

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
