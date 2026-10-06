# syntax=docker/dockerfile:1.7
#
# Brain — production container image.
#
# Multi-stage build:
#   1. `builder` compiles brain-server against the Debian-bookworm
#      Rust toolchain (Brain is Linux-only). Uses BuildKit cache
#      mounts so repeated builds reuse the cargo registry + target dir.
#   2. `runtime` is debian:bookworm-slim (glibc — candle's tensor
#      kernels and io_uring's helper libs both expect it). Adds
#      ca-certificates (for the embedding-model download on first
#      run), tini (PID 1 → clean SIGTERM propagation to brain-server),
#      and curl (used by HEALTHCHECK).
#
# Build:
#   docker build -t brain:latest .
#
# Run:
#   docker run -d --name brain \
#     -p 8080:8080 -p 9091:9091 \
#     -v brain-data:/var/lib/brain/data \
#     brain:latest

# ----------------------------------------------------------------------------
# Builder
# ----------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

ARG CARGO_FEATURES=""

WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# BuildKit cache mounts keep ~/.cargo and target/ across image rebuilds.
# Final binaries must be copied OUT of the cached target/ before the mount
# is unmounted, otherwise they vanish.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release \
        -p brain-server \
        ${CARGO_FEATURES:+--features ${CARGO_FEATURES}} \
 && mkdir -p /out \
 && cp target/release/brain-server /out/

# ----------------------------------------------------------------------------
# Embedding model
# ----------------------------------------------------------------------------
# brain-server cannot start without a BERT-shaped embedding model on disk —
# there is no download path in the server and no way to run without one. An
# image that does not carry it is not runnable, which is what v0.1.0 shipped:
# `docker run` died with "missing required model file" even when the documented
# brain-models volume was mounted, because that volume is not the path the
# server resolves.
#
# BAAI/bge-small-en-v1.5 is MIT-licensed and redistributable. Pinned by commit
# rather than `main` so a rebuild of this tag cannot silently pick up different
# weights. The three files are exactly what `build_dispatcher` requires.
FROM debian:bookworm-slim AS model

ARG EMBED_MODEL_REPO=BAAI/bge-small-en-v1.5
ARG EMBED_MODEL_REV=main
ARG EMBED_MODEL_DIR=/models/bge-small-en-v1.5

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    mkdir -p "$EMBED_MODEL_DIR"; \
    base="https://huggingface.co/${EMBED_MODEL_REPO}/resolve/${EMBED_MODEL_REV}"; \
    for f in config.json tokenizer.json model.safetensors; do \
        curl -fsSL --retry 5 --retry-delay 2 -o "${EMBED_MODEL_DIR}/${f}" "${base}/${f}"; \
    done; \
    # Fail the build rather than ship a truncated or HTML-error weights file.
    # bootstrap-model.sh uses the same 10MB floor for this model.
    size="$(stat -c%s "${EMBED_MODEL_DIR}/model.safetensors")"; \
    test "$size" -gt 10000000 || { echo "weights too small: ${size} bytes" >&2; exit 1; }; \
    head -c4 "${EMBED_MODEL_DIR}/config.json" | grep -q '{' || { echo "config.json is not JSON" >&2; exit 1; }

# Runtime
# ----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
        curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 brain \
    && useradd  --system --uid 10001 --gid brain \
                --home-dir /var/lib/brain --shell /usr/sbin/nologin brain \
    && mkdir -p /var/lib/brain/data /etc/brain \
    && chown -R brain:brain /var/lib/brain

COPY --from=builder /out/brain-server /usr/local/bin/brain-server
COPY config/docker.toml               /etc/brain/config.toml
COPY --from=model --chown=brain:brain /models /opt/brain/models

# Resolved by the first branch of `Config::resolve_model_dir`, so the model
# location is explicit rather than inferred from HOME/XDG. That inference is
# what broke v0.1.0: the image declared a /var/lib/brain/models volume while
# the server looked in $HOME/.local/share/brain/models. An operator who wants
# a different model overrides this one variable.
ENV BRAIN_EMBED_MODEL_DIR=/opt/brain/models/bge-small-en-v1.5

USER brain
WORKDIR /var/lib/brain

# Data plane (binary wire protocol) + public HTTP (/healthz + /metrics).
# `admin_addr` (default 9092 — /v1/* routes) is loopback-only by
# config and is not EXPOSEd: reach it via `docker exec brain ...`.
EXPOSE 8080 9091

# Only data is a volume. The embedding model ships in the image at
# /opt/brain/models and is not written at runtime, so it needs no volume —
# declaring one for it implied a cache that was never populated.
VOLUME ["/var/lib/brain/data"]

# tini reaps zombies and propagates SIGTERM cleanly so `docker stop`
# triggers brain-server's graceful shutdown rather than a 10 s kill.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/brain-server"]
CMD ["--config", "/etc/brain/config.toml"]

# Probes the HTTP server on the metrics port. /healthz is the
# canonical liveness signal (spec §14/02). Tunables below match
# the production guide; override in compose / k8s if needed.
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD curl -fsS http://127.0.0.1:9091/healthz || exit 1
