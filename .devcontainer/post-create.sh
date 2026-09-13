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

# ---------------------------------------------------------------------------
# Required model bootstrap.
#
# brain-server refuses to start without the BGE embedding model and the
# GLiNER zero-shot NER classifier on disk. Downloading them here makes
# every fresh container a `cargo run --bin brain-server` away from a
# working extraction stack — no out-of-band setup step.
#
# Pure download: brain loads the GLiNER pickle directly via candle's
# PthTensors, so there's no torch / safetensors conversion to run.
# Only curl is required.
#
# Idempotent: the script skips files that already exist. For persistent
# model storage across container rebuilds, mount a volume at
# /root/.local/share/brain/models in devcontainer.json.
#
# Downloads are gated behind BRAIN_SKIP_MODEL_BOOTSTRAP=1 so CI / offline
# environments can opt out without editing this script.
# ---------------------------------------------------------------------------
if [[ "${BRAIN_SKIP_MODEL_BOOTSTRAP:-0}" == "1" ]]; then
  echo
  echo "==> Skipping model bootstrap (BRAIN_SKIP_MODEL_BOOTSTRAP=1)"
else
  echo
  echo "==> Bootstrapping required models (BGE-small embed + urchade/gliner_small-v2.1)"
  echo "    set BRAIN_SKIP_MODEL_BOOTSTRAP=1 to opt out of this step"
  if ! ./.devcontainer/bootstrap-model.sh; then
    echo
    echo "warning: model bootstrap failed; brain-server will refuse to start" >&2
    echo "  re-run manually:  ./.devcontainer/bootstrap-model.sh" >&2
    echo "  or skip for now:  BRAIN_SKIP_MODEL_BOOTSTRAP=1 (env var)" >&2
  fi
fi

cat <<'EOF'

Container ready. Useful commands:

  just verify          # full verify (fmt + build + clippy + test + check-skills)
  cargo test -p brain-protocol
  cargo +nightly fuzz run protocol_frame -- -max_total_time=15
  cargo run --bin brain-server -- --config config/dev.toml

Models live at:
  /root/.local/share/brain/models/{bge-small-en-v1.5,gliner-small-v2.1}/

See README.md "Development environment" for the full reference.
EOF
