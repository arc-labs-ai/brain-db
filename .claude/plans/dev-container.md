# Plan: Linux Dev Container

**Status:** awaiting-confirmation (revised: single-file instructions in README.md)
**Date:** 2026-05-10
**Author:** Claude (autonomous)
**Estimated commits:** 2

---

## 1. Scope

Set up a Linux dev container so `feature/brain-storage` work (and every Linux-only crate after it) is verifiable from non-Linux dev hosts. Lands the infra; updates `DEV_SETUP.md` to make this the recommended path; adds a `just shell` recipe to enter the container. After this branch merges, Phase 2 starts.

**Out of scope:**

- Production / runtime container. This is purely a dev convenience.
- CI changes — `.github/workflows/ci.yml` already runs `ubuntu-latest`; no change needed.
- VS Code / JetBrains / Cursor specific tweaks beyond `devcontainer.json`. The standard format works across all.
- Pre-baking Brain binaries. The container is for source dev; binaries get built on first use.

## 2. Constraints

| Concern | Constraint | Source |
|---|---|---|
| Kernel | ≥ 5.15 for `io_uring`, `pwritev2(RWF_DSYNC)`, `mremap MAY_MOVE`, `fallocate(FALLOC_FL_KEEP_SIZE)` | spec §01/05 §1 |
| Filesystem | `O_DIRECT`-capable for storage tests (ext4 / xfs); not tmpfs/overlayfs | spec §01/05 §1.1 |
| `io_uring` syscall access | Docker Desktop 4.42+ **blocks** `io_uring_setup`/`enter`/`register` via seccomp by default. Workarounds: custom seccomp profile, `--security-opt seccomp=unconfined`, or use OrbStack/Colima which are more permissive. **Not needed until Phase 9** — Phase 2's `pwritev2`/`mmap`/`mremap` are not blocked. | [Docker for-mac #7707](https://github.com/docker/for-mac/issues/7707), [moby #47532](https://github.com/moby/moby/issues/47532) |
| Workflow parity | Match `ubuntu-latest` (currently 24.04 noble). Official `rust:1-bookworm` is close enough; both glibc, both x86_64/aarch64. | `.github/workflows/ci.yml` |

## 3. External validation

Web-searched (May 2026):

- **`rust:1-bookworm` official image** — Rust on Debian 12. Used by every devcontainer template I checked. Has `cargo`, `rustc`, `rustup`. Add `just`, `clippy`, `rustfmt`, `nightly` ourselves.
- **devcontainer.json spec** — [containers.dev](https://containers.dev/implementors/json_reference/). Standard across VS Code, Cursor, JetBrains, GitHub Codespaces. Key fields: `name`, `build.dockerfile`, `mounts`, `features`, `postCreateCommand`, `customizations`.
- **Cargo cache strategy** — [devcontainers/templates#117](https://github.com/devcontainers/templates/issues/117): persistent named volume mounted at `/usr/local/cargo/registry` (downloaded crates) and `/workspace/target` (build artifacts). Avoids re-downloading 100s of crates each container restart.
- **`io_uring` in Docker Desktop 4.42+** — blocked by default seccomp. Two relevant tickets: [for-mac#7707](https://github.com/docker/for-mac/issues/7707) (issue), [moby#47532](https://github.com/moby/moby/issues/47532) (rationale). OrbStack runs a recent kernel and is more permissive; recommended for macOS users.
- **`cargo-chef` / `sccache`** — production-image optimizations. **Skip for dev container** — the persistent volume approach is enough; cargo-chef is for shipping minimal multi-stage images, which we don't need.

## 4. What the container provides

Pre-installed:

- Rust stable (rustfmt, clippy)
- Rust nightly (miri, fuzz)
- `just`
- `cargo-fuzz`, `cargo-audit`
- `git`, `gh`, `jq` (project workflows)
- `lldb` + standard build essentials (`build-essential`, `pkg-config`, `libssl-dev`)
- `bash` as default shell

Mounts:

- Source repo at `/workspaces/brain` (devcontainer convention)
- Persistent named volume `brain-cargo-cache` → `/usr/local/cargo/registry`
- Persistent named volume `brain-target-cache` → `/workspaces/brain/target`
- Git config bind-mounted from host (read-only) so `git commit` uses host identity

Runtime flags:

- Default seccomp (no syscall workarounds yet — Phase 2 doesn't need io_uring).
- A note in the container README: when Phase 9 begins, the runtime that drives Glommio will need `--security-opt seccomp=…` or OrbStack.

## 5. File layout

```text
.devcontainer/
├── devcontainer.json     editor-side config
├── Dockerfile            container image definition
└── post-create.sh        runs once after container creation; rustup setup
```

No `.devcontainer/README.md`. All instructions live in the root `README.md`.

Plus:

- `justfile` — new recipe `just shell` (enters the container interactively).
- `README.md` — absorbs DEV_SETUP.md content; gains a "Development environment" section that covers native Linux, the dev container path (recommended for non-Linux hosts), the cross-compile-only path, and the per-crate "what compiles where" table.
- `DEV_SETUP.md` — **deleted**. Its content moves into `README.md`.
- `AUTONOMY.md` §22 — references updated from `DEV_SETUP.md` to the corresponding `README.md` section.

## 6. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** `.devcontainer/` (Microsoft standard) + `just shell` recipe + named volumes for cargo + target. | ✓ Editor-portable; familiar to Rust developers; matches CI base. |
| Custom Docker setup with bespoke compose file. | rejected — devcontainer.json is the lingua franca; tools auto-detect it. |
| Multi-stage Dockerfile with `cargo-chef`. | rejected — that's for *shipping* images, not for dev; adds complexity without payback for dev. |
| Skip the container entirely; document Lima/QEMU directly. | rejected — high friction for new contributors; devcontainer.json gets editor integration for free. |
| Pre-bake Brain's deps into the image. | rejected — Cargo.lock evolves; better to use volumes that persist `~/.cargo/registry`. |
| Use `rust:1-noble` for closer CI parity. | considered; bookworm is more battle-tested and one Debian behind. Either works. Going with `bookworm` (default for `rust:1`). |

## 7. Risks / open questions

- **Docker Desktop io_uring blocking.** Doesn't bite Phase 2. When Phase 9 lands, we'll either:
  - Recommend OrbStack on macOS (its kernel + seccomp are more permissive).
  - Add a `seccomp.json` to `.devcontainer/` allowing `io_uring_*` syscalls and update `runArgs`.
  - Both. For now: document the future need; don't pre-emptively unconfine seccomp (security cost).
- **`O_DIRECT` on bind-mounted host directories.** macOS-hosted Docker mounts via VirtioFS / 9P; these may not support `O_DIRECT`. Phase 2 storage tests should write under `/tmp` (tmpfs in container) or a named-volume path (real ext4) — not `/workspaces/brain` (the bind mount). Document in `.devcontainer/README.md` and in the storage test helpers.
- **Cargo cache volume on first run.** First `cargo build` downloads ~hundred crates (~100 MB). The persistent volume keeps them across rebuilds; first run is slow.
- **Image size.** `rust:1-bookworm` is ~2 GB. Acceptable for dev; this isn't a CI image.
- **Non-VS-Code editors.** devcontainer.json is portable but plain Cursor / JetBrains may need a slightly different invocation. Document in `.devcontainer/README.md`.

## 8. Test plan

- **Build the image:** `docker build .devcontainer/` succeeds.
- **Enter via `just shell`:** drops into a bash shell at `/workspaces/brain`.
- **Run the workspace verify inside:** `just verify` passes (122 tests as on host today).
- **Re-enter and re-verify:** persistent volumes mean second `just verify` is fast (no recompile of unchanged deps).
- **`cargo +nightly fuzz run protocol_frame -- -max_total_time=15` succeeds** (fuzz target builds + runs in container).
- **VS Code / Cursor "Reopen in Container":** confirmed working (manual test by the user in their editor).

I will run all of these locally if you can give me Docker access (the bash tool can call `docker` if Docker Desktop is running). If Docker isn't ready, I'll commit the config and you'll do the verification.

## 9. Commit shape

Two commits:

1. `chore(docs): consolidate setup instructions into README.md`
   - Move `DEV_SETUP.md` content into `README.md`'s "Development environment" section.
   - Delete `DEV_SETUP.md`.
   - Update `AUTONOMY.md` §22 references and the planned `brain-storage` `compile_error!` message to point at `README.md` instead.
2. `chore(dev-container): add .devcontainer/ + just shell recipe`
   - `.devcontainer/Dockerfile`, `.devcontainer/devcontainer.json`, `.devcontainer/post-create.sh` (no sub-README).
   - `justfile` adds `shell` recipe.
   - Final pass on `README.md` — "Development environment" section gains the `.devcontainer/` instructions inline.

Both commits land on `feature/dev-container`; merged via dev → main.

## 10. After confirmation

1. Land the two commits on `feature/dev-container`.
2. **You verify locally** — `cd brain && just shell` should drop you into the container; `just verify` inside should be green.
3. Merge `feature/dev-container` → `dev` → `main`.
4. Branch `feature/brain-storage` from updated `dev`.
5. Resume Phase 2: sub-task 2.1.

## 11. Confirmation

Awaiting "go". One choice point worth flagging:

- **Base image: `rust:1-bookworm` (chosen) or `mcr.microsoft.com/devcontainers/rust:1-bookworm` (Microsoft's pre-tooled image)?**
  - Microsoft's image is heavier (~3.5 GB) but pre-installs `git`, `gh`, common shell tools, and applies `vscode` user — saves a few RUN lines.
  - `rust:1-bookworm` is leaner; we install what we need explicitly.
  - Either works. I'll use `rust:1-bookworm` unless you prefer Microsoft's flavor.

---

## Appendix A — Sources cited

- [containers.dev — devcontainer.json reference](https://containers.dev/implementors/json_reference/)
- [devcontainers/templates#117 — Cargo cache persistence](https://github.com/devcontainers/templates/issues/117)
- [Docker for-mac #7707 — io_uring in 4.42](https://github.com/docker/for-mac/issues/7707)
- [moby #47532 — io_uring blocked by default seccomp](https://github.com/moby/moby/issues/47532)
- [Depot blog — Rust Dockerfile best practices](https://depot.dev/blog/rust-dockerfile-best-practices)
