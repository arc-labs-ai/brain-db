# 01 — Setup

Get the source, install the toolchain, drop into the workspace. Linux only.

## Prerequisites

| Tool | Install | Purpose |
|---|---|---|
| Linux distro, kernel ≥ 5.15 | — | io_uring + `O_DIRECT` + `pwritev2(RWF_DSYNC)` |
| `rustup` + stable toolchain (≥ 1.95) | https://rustup.rs | Compiler + cargo |
| `git` | distro package manager | Source control |
| `gcc` / `clang` + `pkg-config` + OpenSSL headers | distro package manager | Native deps for `aws-lc-sys`, `ring`, etc. |

> **Not on Linux?** The repo ships a dev container — see the README's "Development environment" section. The rest of this page assumes you're on a Linux host.

Brain depends on Linux-only kernel features (io_uring via Glommio,
`O_DIRECT` WAL writes, `pwritev2(RWF_DSYNC)` group commit). The
container path in the README is the supported way to get those on
macOS / Windows.

## 1. Clone

**Input:**

```bash
git clone https://github.com/brain-db-io/brain-db.git
cd brain-db
```

**Verify:**

```bash
ls
```

You should see `crates/`, `spec/`, `docs/`, `Cargo.toml`,
`justfile`, `.devcontainer/`, etc.

## 2. Raise the memlock rlimit

io_uring submission queues are pinned in locked memory. The default
`RLIMIT_MEMLOCK` (64 KiB on most distros) is far too low — Brain's
WAL group-commit path will fail with `ENOMEM` at startup.

**Input** (per-shell — or set in `/etc/security/limits.conf` for permanence):

```bash
ulimit -l unlimited
```

**Verify:**

```bash
ulimit -l
```

Should print `unlimited`.

## 3. Confirm Rust toolchain

**Input:**

```bash
rustc --version
cargo --version
```

**Expected output:**

```
rustc 1.95.0 (stable)
cargo 1.95.0
```

If older, `rustup update stable`.

## Next

[`02-build-and-verify.md`](02-build-and-verify.md) — compile the
workspace and run the verification suite.
