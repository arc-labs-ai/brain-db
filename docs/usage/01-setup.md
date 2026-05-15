# 01 — Setup

Get the source, bring up the dev container, drop into a shell.

## Prerequisites

| Tool | Install | Purpose |
|---|---|---|
| Docker Desktop 4.x+ | https://www.docker.com/products/docker-desktop | Container runtime |
| `devcontainer` CLI | `npm install -g @devcontainers/cli` | Manages the dev container |
| `just` (host, optional) | `cargo install just` | Task runner shortcut |
| Rust stable (host, optional) | https://rustup.rs | Only for host-side editing; all builds run in container |

Brain depends on Linux-only kernel features (io_uring via Glommio,
`O_DIRECT` WAL writes, `pwritev2(RWF_DSYNC)` group commit). All
runtime work happens inside a Linux dev container built `FROM
rust:1-bookworm` with the memlock rlimit raised and seccomp set to
unconfined so io_uring syscalls are allowed.

## 1. Clone

**Input:**

```bash
git clone https://github.com/brain-db-io/brain-db.git
cd brain-db
```

**Expected output:**

```
Cloning into 'brain-db'...
remote: ...
Receiving objects: 100% ...
Resolving deltas: 100% ...
```

**Verify:**

```bash
ls
```

You should see `crates/`, `spec/`, `docs/`, `Cargo.toml`,
`Justfile`, `.devcontainer/`, etc.

## 2. Bring the container up

**Input:**

```bash
just docker-up
```

Builds the image on first run (2–5 minutes); subsequent runs
re-attach in seconds.

**Expected output (first run):**

```
[+] Building 47.3s (11/11) FINISHED
==> brain-dev container post-create
rustc 1.95.0 (stable)
cargo 1.95.0
just 1.x.x
gh 2.x.x
git 2.x.x

==> Quick verify (skips cargo work; just lints conventions)
Container ready. Useful commands:
  just verify
  cargo test -p brain-protocol
  ...
```

**Expected output (subsequent runs):**

```
[+] Running 1/0
 Container brain-dev Running
```

**Verify:**

```bash
docker ps --filter "name=brain-dev"
```

Should list one running container named `brain-dev`.

## 3. Enter the container shell

**Input:**

```bash
just docker-shell
```

**Expected output:**

```
[brain-dev] /workspaces/brain$
```

You're now inside the container. All commands in the next pages
run inside this shell unless noted.

**Verify:**

```bash
uname -a
rustc --version
```

Output should show Linux + Rust 1.95+.

To run a single command without entering interactively:

```bash
just docker <command>
# example
just docker cargo check --workspace
```

## 4. Volume layout

The container mounts three persistent named volumes so incremental
build state survives restarts:

```
brain-cargo-registry   /usr/local/cargo/registry
brain-cargo-git        /usr/local/cargo/git
brain-target-cache     /workspaces/brain/target
```

After a major dependency change, if you need to nuke the build
cache:

```bash
docker volume rm brain-target-cache
just docker-rebuild
```

## Next

[`02-build-and-verify.md`](02-build-and-verify.md) — compile the
workspace and run the verification suite.
