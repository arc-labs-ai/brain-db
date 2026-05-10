# Dev setup

Brain is a Linux database. Most crates compile and test on Linux only. This doc walks the supported dev environments.

## TL;DR

- **Linux x86_64 / aarch64, kernel ≥ 5.15.** Native — no setup beyond `rustup`.
- **macOS or Windows.** Use a Linux container. Three good options below; pick what's installed.

The reason for the constraint is in the spec — `spec/01_system_architecture/05_hardware.md` §1.1. Short version: Brain depends on `io_uring`, `O_DIRECT`, `pwritev2(RWF_DSYNC)`, and a few `madvise` / `fallocate` flags that are Linux-specific and have no portable equivalent at the latency we target.

## What compiles on what

| Crate | Linux | macOS / Windows native |
|---|---|---|
| `brain-core` | ✓ | ✓ (pure value types, no I/O) |
| `brain-protocol` | ✓ | ✓ (codec only) |
| `brain-cli` | ✓ | ✓ (no runtime dep yet) |
| `brain-sdk-rust` | ✓ | ✓ (client-side only) |
| `brain-storage` | ✓ | ✗ — `compile_error!` |
| `brain-metadata` | ✓ | ✗ once redb is wired with O_DIRECT-aware paths (Phase 3) |
| `brain-index` | ✓ | ✗ once HNSW persistence lands (Phase 4) |
| `brain-embed` | ✓ | ✗ once candle wiring lands (Phase 5) |
| `brain-planner` | ✓ | ✓ (pure logic) |
| `brain-ops` | ✓ | △ partial — wires runtime crates |
| `brain-workers` | ✓ | ✗ — runs on Glommio |
| `brain-server` | ✓ | ✗ — Glommio + Tokio runtime |
| `fuzz/*` | ✓ (nightly) | ✗ |

CI is the source of truth: `.github/workflows/ci.yml` runs everything on `ubuntu-latest`.

## Option A — Native Linux

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
just verify
```

Done.

## Option B — Linux container on macOS (Docker / OrbStack / Colima)

Recommended for macOS users. Pick one runtime; install once.

### B.1 OrbStack (lightweight, recommended on Apple Silicon)

```bash
brew install orbstack
orb create --image ubuntu:24.04 brain-dev
orb shell brain-dev
# inside container:
sudo apt-get update && sudo apt-get install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source $HOME/.cargo/env
cd /Users/<you>/Desktop/brain    # OrbStack auto-mounts your home dir
just verify
```

### B.2 Docker Desktop or Colima

```bash
docker run --rm -it \
  -v "$PWD":/workspace -w /workspace \
  -v cargo-cache:/usr/local/cargo/registry \
  rust:1-bookworm \
  bash -c 'apt-get update && apt-get install -y just && just verify'
```

The `cargo-cache` named volume avoids re-downloading deps each run.

### B.3 Lima (closer to a full Linux VM)

```bash
limactl create --name brain-dev template://docker
limactl shell brain-dev
# inside VM, similar to B.1
```

## Option C — Cross-compile-only on macOS

Validates compilation without running. Useful for fast in-editor feedback; tests still need a container or CI.

```bash
rustup target add x86_64-unknown-linux-gnu
brew install lld           # or: cargo install --locked cargo-zigbuild
```

Then add to `.cargo/config.toml` (gitignored, local only):

```toml
[target.x86_64-unknown-linux-gnu]
linker = "x86_64-linux-gnu-gcc"   # or "ld.lld"; depends on toolchain
```

Run:

```bash
cargo check --workspace --target x86_64-unknown-linux-gnu
```

This won't run tests, just checks that the code compiles for Linux. Pair with **Option B** for actual test runs.

## CI is the test gate

Every PR runs the full suite on `ubuntu-latest` via `.github/workflows/ci.yml`:

- `cargo fmt --all -- --check`
- `cargo build --workspace --all-targets`
- `cargo test --workspace --all-targets`
- `cargo test --workspace --doc`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Miri on `brain-storage` (PRs only, nightly)
- `cargo audit`

If your local container can't run a particular test (e.g., io_uring availability inside the container), CI is the authoritative result.

## Container constraints

A few things to watch for inside containers:

- **`io_uring`** — works inside Docker on most modern Linux hosts. macOS-hosted Linux VMs (OrbStack, Colima with QEMU, Docker Desktop with VirtioFS) all support it. If `io_uring_setup` returns `ENOSYS`, the host kernel needs upgrade or syscall filtering needs adjusting. Affects only Phase 9+ runtime tests.
- **`O_DIRECT` against bind-mounts** — tmpfs and overlayfs may not support `O_DIRECT`. Use a Linux native filesystem path inside the container (e.g., write tests under `/tmp` or a dedicated volume) for storage tests.
- **Latency** — containers on macOS hosts incur a virtualization tax. Functional correctness and basic throughput are unaffected; perf benchmarks belong on native Linux hardware. Spec §01/05 §1 line 28 says this explicitly.

## Quick reference

```bash
# Verify the workspace (runs check-skills, fmt, build, clippy, test):
just verify

# Just storage:
cargo test -p brain-storage

# Random-kill recovery test (Phase 2):
cargo test -p brain-storage --test random_kill -- --nocapture

# Fuzz (nightly only, Linux only):
cargo +nightly fuzz run protocol_frame -- -max_total_time=60
```

## When something doesn't work

- **`liburing-sys` link error on macOS native:** expected. Use a container.
- **`compile_error!` mentioning DEV_SETUP.md:** that's the friendly Linux gate; switch to a container.
- **`io_uring_setup: Function not implemented`:** kernel too old or seccomp restricted; check host kernel and container runtime.
- **`O_DIRECT` returns `EINVAL`:** the filesystem under the test path doesn't support direct I/O; use a different mount.
