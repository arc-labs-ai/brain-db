# Plan: Phase 0 — Workspace Skeleton (retrospective)

**Status:** implemented (commit `8fe398c`, tag `phase-0-complete`)
**Date drafted:** 2026-05-10 (retrospective; original work pre-dated the plan workflow)
**Author:** starter template + Claude follow-up

---

## 1. Scope

Phase 0 is the workspace scaffold provided by the starter template. The work that landed:

- `Cargo.toml` workspace with shared dependency table — all crates listed in CLAUDE.md §6 pinned at the workspace level.
- 12 stub crates under `crates/` (`brain-core`, `brain-protocol`, `brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`, `brain-planner`, `brain-ops`, `brain-workers`, `brain-server`, `brain-sdk-rust`, `brain-cli`).
- Toolchain pin (`rust-toolchain.toml`), formatter config (`rustfmt.toml`), lint config (`clippy.toml`).
- CI scaffold under `.github/workflows/ci.yml` — build + test + clippy + fmt + miri + cargo-audit jobs.
- Shared dev infra: `.gitignore`, `justfile`, `config/dev.toml`, `fuzz/` directory.

Claude's contribution this phase: a single fmt-cleanup commit (`1bbed33` — "0.fmt: apply rustfmt to scaffold") to make `cargo fmt --all -- --check` green so the verify loop could pass before Phase 1 work began.

**Out of scope:**

- Any actual functionality. Stub crates compile but expose nothing.
- Spec implementation. That starts Phase 1.

## 2. Spec references

Not applicable — Phase 0 is build-system / repo-layout, not spec-driven.

## 3. External validation

Not applicable — pinned deps were chosen by the project owner before Claude was involved (see CLAUDE.md §6 for the approved set and rationale).

## 4. Architecture sketch

```text
brain/
├── Cargo.toml                  workspace + shared deps
├── rust-toolchain.toml         stable - 1
├── rustfmt.toml, clippy.toml   formatter + lint config
├── justfile                    verify, build, test, run-server, doc
├── .github/workflows/ci.yml    CI gates
├── config/dev.toml             dev config
├── fuzz/                       cargo-fuzz scaffold (targets are stubs)
├── crates/
│   ├── brain-core           value types
│   ├── brain-protocol       wire protocol
│   ├── brain-storage        arena + WAL
│   ├── brain-metadata       redb wrapper
│   ├── brain-index          HNSW
│   ├── brain-embed          BGE
│   ├── brain-planner        query planner
│   ├── brain-ops            cognitive ops
│   ├── brain-workers        background workers
│   ├── brain-server         binary
│   ├── brain-sdk-rust       Rust SDK
│   └── brain-cli            admin CLI
└── docs/phases/, spec/, AUTONOMY.md, CLAUDE.md, ROADMAP.md
```

The 12-crate split mirrors the spec subsystems and was chosen so each layer has a clear owner and can be tested in isolation.

## 5. Trade-offs considered

The workspace structure was given. Two scaffold-level decisions worth noting:

| Decision | Rationale |
|---|---|
| Single-workspace, multi-crate (vs. monorepo of independent crates) | Shared `Cargo.lock` and dep versions; easier cross-crate refactoring. |
| Crates per spec subsystem (vs. layered: `core` / `infra` / `app`) | Maps directly to spec sections; future readers can navigate from spec → crate without indirection. |

## 6. Risks / open questions

- **Pre-existing fmt drift** in `brain-core/src/ids.rs` and `brain-server/src/main.rs` — landed in commit `1bbed33` before Phase 1 work.
- **Stub fuzz targets** — placeholder `fuzz/fuzz_targets/protocol_frame.rs` exists but doesn't decode anything; Task 1.10 wires it to the real codec.
- **No miri exclusions yet** — `unsafe` only allowed in `brain-storage`; that constraint is documented but not enforced via CI rule.

## 7. Test plan

CI runs `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt -- --check`, miri, cargo-audit. All green after `1bbed33`.

## 8. Commit shape

One commit landed by Claude this phase:

- `1bbed33` — "0.fmt: apply rustfmt to scaffold" — corrects the two pre-existing fmt issues so verify is green before Phase 1.

The initial scaffold (`8fe398c`) was a project-owner commit prior to Claude's involvement.

## 9. Lessons / handoff

- **Verify the verify loop early.** Claude found the fmt drift only when running `cargo fmt --all -- --check` after Task 1.1 — a one-step earlier check would have surfaced it cleanly.
- **`phase-N-complete` tags are placed manually** — Phase 0's tag (`phase-0-complete`) was added by user request after the scaffold was confirmed green; future phases will create the tag as part of the phase exit checklist.
