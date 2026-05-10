# Linux dev container for Brain.
#
# See README.md "Development environment" for usage. This image gives
# non-Linux contributors a kernel ≥ 5.15 environment with the Rust
# toolchain, project conventions, and helper tools pre-installed.
#
# Built and entered via `just shell`. CI uses ubuntu-latest directly
# (not this image) — they're close-enough siblings (both glibc, both
# Debian-family).

FROM rust:1-bookworm

ARG NIGHTLY_DATE=
ENV CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1 \
    DEBIAN_FRONTEND=noninteractive

# System tooling. Installed in one layer for image size; pinned only to
# major versions because Brain doesn't depend on minor-release behavior.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        curl \
        git \
        gnupg \
        jq \
        less \
        lldb \
        man-db \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# `gh` (GitHub CLI) — official apt repo. Used by some project workflows.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | gpg --dearmor -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchains: stable (with rustfmt + clippy) is already in the base
# image. Add nightly for miri + cargo-fuzz, plus the rust-src component
# miri needs. `just` and the cargo subcommands install via cargo so the
# image doesn't depend on third-party install scripts.
RUN rustup component add rustfmt clippy \
    && rustup toolchain install nightly --component miri --component rust-src \
    && cargo install --locked just cargo-fuzz cargo-audit

# Workspace mount point. Source bind-mounts here at runtime via
# devcontainer.json.
WORKDIR /workspaces/brain

# Persistent cache locations. Volumes mounted here in devcontainer.json
# so cargo doesn't re-download deps every container restart.
ENV CARGO_HOME=/usr/local/cargo \
    CARGO_TARGET_DIR=/workspaces/brain/target

# Default shell + sensible prompt.
SHELL ["/bin/bash", "-lc"]
RUN echo 'export PS1="\\[\\e[36m\\][brain-dev]\\[\\e[0m\\] \\w$ "' >> /etc/bash.bashrc

CMD ["bash"]
