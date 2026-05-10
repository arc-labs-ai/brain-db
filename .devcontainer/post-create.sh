#!/usr/bin/env bash
#
# Runs once after the dev container is first created.
#
# devcontainer.json's `postCreateCommand` invokes this. Keep it
# idempotent — it may run again if the container is rebuilt.

set -euo pipefail

cd /workspaces/brain

echo "==> brain-dev container post-create"

# Confirm tool versions so a broken image is obvious from the log.
rustc --version
cargo --version
rustup +nightly --version 2>/dev/null || true
just --version
gh --version | head -1
git --version

# Permissions on the workspace bind-mount can be tight on some hosts.
# Ensure the cargo / target volumes are owned by the runtime user.
chown -R "$(id -u):$(id -g)" /usr/local/cargo /workspaces/brain/target 2>/dev/null || true

echo
echo "==> Quick verify (skips cargo work; just lints conventions)"
just check-skills

cat <<'EOF'

Container ready. Useful commands:

  just verify          # full verify (fmt + build + clippy + test + check-skills)
  cargo test -p brain-protocol
  cargo +nightly fuzz run protocol_frame -- -max_total_time=15

See README.md "Development environment" for the full reference.
EOF
