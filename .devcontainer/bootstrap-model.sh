#!/usr/bin/env bash
#
# Download the models Brain refuses to start without:
#
#   1. Embedding model — BGE-small-en-v1.5
#   2. NER model       — urchade/gliner_small-v2.1 (zero-shot NER,
#                        DeBERTa-v3-small backbone)
#   3. Reranker        — BAAI/bge-reranker-base (cross-encoder,
#                        BertForSequenceClassification, num_labels=1)
#                        — drives the W2.2 rerank pass
#
# GLiNER is zero-shot: labels are the active schema's entity-type
# qnames, passed per `predict()` call. No per-schema retraining,
# no OntoNotes relabel layer.
#
# brain-server refuses to start without all three on disk: the
# embedder and NER are always required, and the reranker is required
# whenever `[rerank] enabled = true` — which is the default. An
# enabled-but-missing reranker is a hard spawn failure, so this script
# fetches it by default. Operators who set `[rerank] enabled = false`
# skip the ~1.1 GiB download with --no-rerank. This is the one-line
# bootstrap for new operators and dev containers.
#
# What it places where:
#
#   $XDG_DATA_HOME/brain/models/bge-small-en-v1.5/
#                          ├── config.json
#                          ├── tokenizer.json
#                          └── model.safetensors      (~130 MiB)
#
#   $XDG_DATA_HOME/brain/models/gliner-small-v2.1/
#                          ├── pytorch_model.bin      (~611 MiB, fp32 pickle; loaded by candle)
#                          ├── tokenizer.json         (DeBERTa-v3 SPM; [ENT] added by brain at load)
#                          ├── spm.model              (SentencePiece source for the tokenizer)
#                          ├── config.json            (DeBERTa-v3-small backbone config)
#                          └── gliner_config.json     (max_width / max_len / hidden_size)
#
#   $XDG_DATA_HOME/brain/models/bge-reranker-base/
#                          ├── config.json
#                          ├── tokenizer.json
#                          └── model.safetensors      (~1.1 GiB, BertForSequenceClassification fp32)
#
#   ($XDG_DATA_HOME defaults to $HOME/.local/share)
#
# Override destinations with $BRAIN_EMBED_MODEL_DIR,
# $BRAIN_NER_MODEL_PATH, and $BRAIN_RERANK_MODEL_DIR respectively.
# When set, brain-server uses the env value verbatim — no XDG
# resolution. When unset (the common case) brain-server auto-
# discovers each model at the XDG default location, so the typical
# operator flow is: run this script once, then start the server
# with no env vars at all.
#
# Usage:
#   ./.devcontainer/bootstrap-model.sh                  # embed + ner + reranker (default)
#   ./.devcontainer/bootstrap-model.sh --only embed     # embedding only
#   ./.devcontainer/bootstrap-model.sh --only ner       # NER only
#   ./.devcontainer/bootstrap-model.sh --only rerank    # reranker only
#   ./.devcontainer/bootstrap-model.sh --no-rerank      # embed + ner; skip the ~1.1 GiB reranker
#   ./.devcontainer/bootstrap-model.sh --force          # re-download even if files exist
#   ./.devcontainer/bootstrap-model.sh --verify         # check existing files; no download

set -euo pipefail

# ---------------------------------------------------------------------------
# Model configuration. Each model is a named scope; selecting which
# scopes to process happens via --only.
# ---------------------------------------------------------------------------

configure_embed() {
  MODEL_KEY="embed"
  MODEL_REPO="BAAI/bge-small-en-v1.5"
  MODEL_NAME="bge-small-en-v1.5"
  HF_BASE="https://huggingface.co/${MODEL_REPO}/resolve/main"
  FILES_REMOTE=(config.json tokenizer.json model.safetensors)
  FILES_VERIFY=("${FILES_REMOTE[@]}")
  WEIGHT_FLOOR=10000000

  if [[ -n "${BRAIN_EMBED_MODEL_DIR:-}" ]]; then
    DEST="${BRAIN_EMBED_MODEL_DIR}"
  elif [[ -n "${XDG_DATA_HOME:-}" ]]; then
    DEST="${XDG_DATA_HOME}/brain/models/${MODEL_NAME}"
  else
    DEST="${HOME}/.local/share/brain/models/${MODEL_NAME}"
  fi

  ENV_NAME="BRAIN_EMBED_MODEL_DIR"
  ENV_VALUE="${BRAIN_EMBED_MODEL_DIR:-}"
  POST_STEP=""
}

configure_rerank() {
  MODEL_KEY="rerank"
  MODEL_REPO="BAAI/bge-reranker-base"
  MODEL_NAME="bge-reranker-base"
  HF_BASE="https://huggingface.co/${MODEL_REPO}/resolve/main"
  FILES_REMOTE=(config.json tokenizer.json model.safetensors)
  FILES_VERIFY=("${FILES_REMOTE[@]}")
  WEIGHT_FLOOR=100000000

  if [[ -n "${BRAIN_RERANK_MODEL_DIR:-}" ]]; then
    DEST="${BRAIN_RERANK_MODEL_DIR}"
  elif [[ -n "${XDG_DATA_HOME:-}" ]]; then
    DEST="${XDG_DATA_HOME}/brain/models/${MODEL_NAME}"
  else
    DEST="${HOME}/.local/share/brain/models/${MODEL_NAME}"
  fi

  ENV_NAME="BRAIN_RERANK_MODEL_DIR"
  ENV_VALUE="${BRAIN_RERANK_MODEL_DIR:-}"
  POST_STEP=""
}

configure_ner() {
  MODEL_KEY="ner"
  MODEL_REPO="urchade/gliner_small-v2.1"
  MODEL_NAME="gliner-small-v2.1"
  HF_BASE="https://huggingface.co/${MODEL_REPO}/resolve/main"
  # Tokenizer + backbone config come from DeBERTa-v3-small; GLiNER's
  # repo only ships the pickle weights + gliner_config.json. We pull
  # the fast (`tokenizer.json`) tokenizer from `onnx-community/deberta-v3-small`
  # because the upstream `microsoft/deberta-v3-small` repo only ships
  # `spm.model` + `tokenizer_config.json`, and the `tokenizers` crate
  # needs a JSON tokenizer file.
  TOKENIZER_REPO="onnx-community/deberta-v3-small"
  TOKENIZER_HF_BASE="https://huggingface.co/${TOKENIZER_REPO}/resolve/main"
  # Brain loads `pytorch_model.bin` directly via candle's PthTensors —
  # no conversion step, no torch dependency.
  FILES_REMOTE=(gliner_config.json pytorch_model.bin)
  FILES_VERIFY=(pytorch_model.bin tokenizer.json config.json gliner_config.json spm.model)
  WEIGHT_FLOOR=10000000

  if [[ -n "${BRAIN_NER_MODEL_PATH:-}" ]]; then
    DEST="${BRAIN_NER_MODEL_PATH}"
  elif [[ -n "${XDG_DATA_HOME:-}" ]]; then
    DEST="${XDG_DATA_HOME}/brain/models/${MODEL_NAME}"
  else
    DEST="${HOME}/.local/share/brain/models/${MODEL_NAME}"
  fi

  ENV_NAME="BRAIN_NER_MODEL_PATH"
  ENV_VALUE="${BRAIN_NER_MODEL_PATH:-}"
  POST_STEP="finalise_gliner"
}

# ---------------------------------------------------------------------------
# Flags.
# ---------------------------------------------------------------------------

FORCE=0
VERIFY_ONLY=0
ONLY=""
EXCLUDE_RERANK=0
i=0
args=("$@")
while [[ $i -lt $# ]]; do
  arg="${args[$i]}"
  case "$arg" in
    --force)  FORCE=1 ;;
    --verify) VERIFY_ONLY=1 ;;
    --no-rerank) EXCLUDE_RERANK=1 ;;
    --only)
      i=$((i + 1))
      if [[ $i -ge $# ]]; then
        echo "error: --only requires an argument (embed | ner | rerank)" >&2
        exit 2
      fi
      ONLY="${args[$i]}"
      case "$ONLY" in
        embed|ner|rerank) ;;
        *)
          echo "error: --only must be 'embed', 'ner', or 'rerank', got: $ONLY" >&2
          exit 2
          ;;
      esac
      ;;
    -h|--help)
      grep -E '^# ' "$0" | sed -e 's/^# //' -e 's/^#$//'
      exit 0
      ;;
    *)
      echo "error: unknown flag: $arg" >&2
      echo "use --help for usage" >&2
      exit 2
      ;;
  esac
  i=$((i + 1))
done

SCOPES=()
if [[ -z "$ONLY" || "$ONLY" == "embed" ]];  then SCOPES+=(embed);  fi
if [[ -z "$ONLY" || "$ONLY" == "ner" ]];    then SCOPES+=(ner);    fi
# The reranker is on by default — the server's `[rerank]` section
# defaults to enabled, and an enabled-but-missing reranker is a hard
# spawn failure. --no-rerank skips the ~1.1 GiB download for operators
# who set `[rerank] enabled = false`; --only rerank fetches just it.
if [[ "$ONLY" == "rerank" ]] || { [[ -z "$ONLY" ]] && [[ "$EXCLUDE_RERANK" -eq 0 ]]; }; then
  SCOPES+=(rerank)
fi

# ---------------------------------------------------------------------------
# Tooling preflight.
# ---------------------------------------------------------------------------

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: $1 is required but not found in PATH" >&2
    case "$1" in
      curl) echo "install with: apt-get install curl  /  apk add curl  /  brew install curl" >&2 ;;
    esac
    exit 1
  fi
}

if [[ "$VERIFY_ONLY" -eq 0 ]]; then
  require_tool curl
fi

# ---------------------------------------------------------------------------
# Helpers.
# ---------------------------------------------------------------------------

file_size() {
  stat -c %s "$1" 2>/dev/null || stat -f %z "$1"
}

# After GLiNER's own files land, pull the DeBERTa-v3-small companion
# files (tokenizer, backbone config, SentencePiece model). No torch,
# no conversion. The tokenizer.json is then patched in place to inject
# the two regular added tokens GLiNER v2.1 trained with:
#
#   <<ENT>> at id 128001   (label marker, pooled to build label vectors)
#   <<SEP>> at id 128002   (terminates the label prompt before the words)
#
# Order is load-bearing: v2.1's TokenRepLayer calls
# `tokenizer.add_tokens(["<<ENT>>", "<<SEP>>"])` against a flair-prepared
# DeBERTa-v3-small (len=128001 — base 128000 plus `[MASK]` at 128000), so
# the trained embedding matrix is sized 128004 and the two tokens land at
# 128001 / 128002. (The third extra row, id 128003, is an artefact of
# flair's slow-tokenizer init and is never indexed at inference.)
finalise_gliner() {
  local pickle_path="${DEST}/pytorch_model.bin"

  for fname in tokenizer.json config.json spm.model; do
    local target="${DEST}/${fname}"
    if [[ -f "${target}" && "${FORCE}" -eq 0 ]]; then
      echo "  ✓ ${fname} already present (use --force to re-download)"
      continue
    fi
    local url="${TOKENIZER_HF_BASE}/${fname}"
    echo "  → ${fname} (from ${TOKENIZER_REPO})"
    if ! curl --fail --location --retry 3 --retry-delay 2 \
              --progress-bar --output "${target}" "${url}"; then
      echo "error: download failed for ${fname}" >&2
      echo "  url: ${url}" >&2
      rm -f "${target}"
      exit 1
    fi
  done

  for f in pytorch_model.bin tokenizer.json config.json gliner_config.json spm.model; do
    if [[ ! -f "${DEST}/${f}" ]]; then
      echo "error: post-finalise check: ${f} missing in ${DEST}" >&2
      exit 1
    fi
  done

  local weight_size
  weight_size=$(file_size "${pickle_path}")
  if [[ "${weight_size}" -lt "${WEIGHT_FLOOR}" ]]; then
    echo "error: pytorch_model.bin only ${weight_size} bytes; download likely truncated" >&2
    exit 1
  fi

  patch_gliner_tokenizer
}

# Idempotently insert <<ENT>> and <<SEP>> into tokenizer.json at the
# exact IDs GLiNER v2.1 trained against. Re-running is safe: existing
# entries with matching id+content are left untouched; a mismatch fails
# loud so we never silently load a corrupt vocab.
patch_gliner_tokenizer() {
  local tok="${DEST}/tokenizer.json"
  echo "  → patching ${tok##*/} with <<ENT>>@128001 / <<SEP>>@128002"
  python3 - "$tok" <<'PY'
import json, sys
path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    tok = json.load(f)

REQUIRED = [
    (128001, "<<ENT>>"),
    (128002, "<<SEP>>"),
]

def added_token(tid: int, content: str) -> dict:
    return {
        "id": tid,
        "content": content,
        "single_word": False,
        "lstrip": False,
        "rstrip": False,
        "normalized": True,
        "special": False,
    }

added = tok.setdefault("added_tokens", [])
by_id = {a["id"]: a for a in added}
for tid, content in REQUIRED:
    if tid in by_id:
        existing = by_id[tid]
        if existing["content"] != content:
            sys.exit(
                f"tokenizer.json id={tid} is already taken by "
                f"{existing['content']!r}; refusing to clobber"
            )
        continue
    by_content = next((a for a in added if a["content"] == content), None)
    if by_content is not None:
        sys.exit(
            f"tokenizer.json already has {content!r} at id={by_content['id']}, "
            f"expected {tid}"
        )
    added.append(added_token(tid, content))

added.sort(key=lambda a: a["id"])
tok["added_tokens"] = added

with open(path, "w", encoding="utf-8") as f:
    json.dump(tok, f, ensure_ascii=False)
    f.write("\n")
PY
}

verify_scope() {
  echo "verifying ${MODEL_KEY} model files in: ${DEST}"
  local missing=()
  for f in "${FILES_VERIFY[@]}"; do
    if [[ -f "${DEST}/${f}" ]]; then
      echo "  ✓ ${f} ($(du -h "${DEST}/${f}" | cut -f1))"
    else
      echo "  ✗ ${f} (missing)"
      missing+=("$f")
    fi
  done
  if [[ ${#missing[@]} -eq 0 ]]; then
    return 0
  fi
  echo
  echo "missing ${#missing[@]} file(s) in ${MODEL_KEY}; re-run without --verify to download." >&2
  return 1
}

download_scope() {
  mkdir -p "${DEST}"
  echo "downloading ${MODEL_REPO} → ${DEST}"
  echo
  for entry in "${FILES_REMOTE[@]}"; do
    local local_name remote_path
    if [[ "${entry}" == *=* ]]; then
      local_name="${entry%%=*}"
      remote_path="${entry#*=}"
    else
      local_name="${entry}"
      remote_path="${entry}"
    fi
    local target="${DEST}/${local_name}"
    if [[ -f "${target}" && "${FORCE}" -eq 0 ]]; then
      echo "  ✓ ${local_name} already present (use --force to re-download)"
      continue
    fi
    local url="${HF_BASE}/${remote_path}"
    echo "  → ${local_name}"
    if ! curl --fail --location --retry 3 --retry-delay 2 \
              --progress-bar \
              --output "${target}" "${url}"; then
      echo "error: download failed for ${local_name}" >&2
      echo "  url: ${url}" >&2
      echo "  target: ${target}" >&2
      rm -f "${target}"
      exit 1
    fi
  done

  echo
  echo "verifying download:"
  for entry in "${FILES_REMOTE[@]}"; do
    local local_name="${entry%%=*}"
    if [[ -f "${DEST}/${local_name}" ]]; then
      echo "  ✓ ${local_name} ($(file_size "${DEST}/${local_name}") bytes)"
    fi
  done

  # For embed + rerank scopes, sanity-check the safetensors file
  # directly. For NER (GLiNER), the weight check happens after
  # the finalise step (pulls a pickle + tokenizer).
  if [[ "${MODEL_KEY}" == "embed" || "${MODEL_KEY}" == "rerank" ]]; then
    local weight_size
    weight_size=$(file_size "${DEST}/model.safetensors")
    if [[ "${weight_size}" -lt "${WEIGHT_FLOOR}" ]]; then
      echo
      echo "warning: model.safetensors is only ${weight_size} bytes; expected > ${WEIGHT_FLOOR}." >&2
      echo "the download likely failed silently. inspect ${DEST}/model.safetensors" >&2
      echo "and re-run with --force." >&2
      exit 1
    fi
  fi

  if [[ -n "${POST_STEP}" ]]; then
    "${POST_STEP}"
  fi
}

print_start_hint() {
  echo
  echo "${MODEL_KEY} model ready at: ${DEST}"
  if [[ -n "${ENV_VALUE}" ]]; then
    echo "  ${ENV_NAME}=${ENV_VALUE} (explicit override active)"
  else
    echo "  Brain will auto-discover this model at startup. No env var needed."
  fi
}

# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

ANY_MISSING=0
for scope in "${SCOPES[@]}"; do
  "configure_${scope}"
  if [[ "$VERIFY_ONLY" -eq 1 ]]; then
    verify_scope || ANY_MISSING=1
  else
    download_scope
  fi
done

if [[ "$VERIFY_ONLY" -eq 1 ]]; then
  if [[ "${ANY_MISSING}" -eq 0 ]]; then
    echo
    echo "all required files present."
    exit 0
  else
    exit 1
  fi
fi

echo
echo "models ready. start brain-server:"
for scope in "${SCOPES[@]}"; do
  "configure_${scope}"
  print_start_hint
done
echo
echo "  ./target/release/brain-server"
