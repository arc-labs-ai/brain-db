# Brain

> A cognitive substrate for AI agents — a database-like system where the primitives are *cognitive operations* (encode, recall, plan, reason, forget) rather than tables, documents, or vectors.

**Status:** Specification complete (17 spec documents, 218 files, ~42K lines). Implementation in progress.

---

## What Brain is

Brain is to AI agents what SQL is to applications: a substrate where the application says *what* it wants cognitively, and the substrate handles *how*.

```rust
// Encode an experience
brain.encode("Had a difficult conversation with Alex about the project").await?;

// Recall what's relevant later
let memories = brain.recall("conflicts with Alex").await?;
//   → returns ranked memories by semantic similarity, edge proximity,
//     temporal recency, and salience — not just vector distance.
```

The five cognitive primitives:

| Primitive | What it does |
|---|---|
| `ENCODE` | Store a memory (substrate handles embedding, indexing, edge inference) |
| `RECALL` | Find relevant memories given a cue |
| `PLAN` | Walk a chain of memories along temporal/causal edges |
| `REASON` | Multi-hop traversal across memory edges |
| `FORGET` | Soft or hard deletion with grace periods |

Plus `LINK`/`UNLINK` for explicit edges, `TXN_*` for atomicity, `SUBSCRIBE` for streaming, and `ADMIN_*` for operations.

## Why this exists

Vector databases give you `top-k by cosine similarity` and call it done. That's not how memory works. Real cognitive recall blends:

- Semantic similarity (vectors)
- Temporal recency (when)
- Salience (how important)
- Causal/derivational structure (graph edges)
- Spreading activation (recently-accessed neighborhood)

Application developers shouldn't have to wire all of this themselves on top of a vector DB + a graph DB + a key-value store + a queue. Brain provides one substrate that does it natively.

## Tech stack

- **Language:** Rust
- **Async runtime:** [Glommio](https://github.com/DataDog/glommio) (thread-per-core, io_uring, Linux-only)
- **Wire protocol:** Custom binary over TCP+TLS (rkyv + bytemuck)
- **Embedding model:** [BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5) (built-in, substrate owns embedding)
- **Vector storage:** Memory-mapped arena, 1600-byte slots
- **WAL:** O_DIRECT append-only with `pwritev2(RWF_DSYNC)` group commit
- **Metadata:** [redb](https://github.com/cberner/redb) (pure-Rust ACID B-tree)
- **ANN index:** [hnsw_rs](https://github.com/jean-pierreBoth/hnswlib-rs) (HNSW, M=16, ef_construction=200)
- **SIMD math:** matrixmultiply + wide

See [`spec/00_master_overview/02_doc_map.md`](spec/00_master_overview/02_doc_map.md) for the full architectural map.

## Repository layout

```
brain/
├── README.md                     # You are here
├── CLAUDE.md                     # Context loaded by Claude Code each session
├── AUTONOMY.md                   # Autonomous-mode operating contract
├── ROADMAP.md                    # High-level phase index
├── .claude/                      # Claude Code project config
│   ├── settings.json             #   tool permissions + hooks wiring
│   ├── hooks/                    #   PreToolUse safety scripts
│   ├── commands/                 #   custom slash commands
│   └── agents/                   #   specialized subagents
├── docs/
│   └── phases/                   # Per-phase detailed sub-task plans
│       ├── README.md
│       ├── phase-01-wire-protocol.md
│       ├── ...
│       └── phase-11-observability.md
├── spec/                         # The 17-document specification (read-only)
│   ├── 00_master_overview/
│   ├── ... (17 directories)
│   └── 16_benchmarks_acceptance/
├── crates/                       # Rust workspace, 12 stub crates
│   ├── brain-core/
│   ├── brain-protocol/
│   ├── brain-storage/
│   ├── brain-metadata/
│   ├── brain-index/
│   ├── brain-embed/
│   ├── brain-planner/
│   ├── brain-ops/
│   ├── brain-workers/
│   ├── brain-server/
│   ├── brain-sdk-rust/
│   └── brain-cli/
├── fuzz/                         # cargo-fuzz target stubs
├── config/                       # example TOML configs
└── .github/workflows/ci.yml
```

## Building with Claude Code

This repository is configured for development with [Claude Code](https://claude.com/claude-code), including an **autonomous operating mode** for hands-off implementation.

The `.claude/` folder contains:

- **`settings.json`** — tool permissions and pre-tool-use hooks.
- **`hooks/`** — safety scripts (`pre-bash.sh`, `pre-write.sh`) that block destructive operations even in skip-permissions mode.
- **`commands/`** — slash commands: `/spec`, `/next-task`, `/status`, `/commit-task`, `/verify`, `/lint`, `/bench`, `/audit-spec`, `/new-crate`.
- **`agents/`** — subagents: `spec-navigator`, `rust-implementer`, `test-engineer`.

Three top-level docs work together:

- **[`CLAUDE.md`](CLAUDE.md)** — project context loaded on every session.
- **[`AUTONOMY.md`](AUTONOMY.md)** — operating contract for autonomous mode (execution loop, hard rules, stop conditions). **Read before running with `--dangerously-skip-permissions`.**
- **[`ROADMAP.md`](ROADMAP.md)** + **[`docs/phases/`](docs/phases/)** — twelve phases, each with a per-sub-task breakdown (reads, writes, "done when" criteria, pitfalls).

### Quick start

```bash
# Clone, init, verify Phase 0
git clone <url> brain && cd brain
git init && git add -A && git commit -m "Initial: spec + Phase 0 scaffold"
just verify          # cargo build + test + clippy + fmt

# If green, tag Phase 0
git tag phase-0-complete

# Open Claude Code (interactive)
claude

# Or autonomous mode — Claude works through the roadmap unattended
claude --dangerously-skip-permissions
```

### Common slash commands

```
/status              # phase progress, last commit, next sub-task, health
/next-task           # propose the next sub-task with reads/writes/criteria
/spec 05 08          # navigate spec § 05 (storage), file 08 (recovery)
/verify              # run full verify suite
/commit-task 1.3 ... # commit current work with the prescribed message format
/audit-spec brain-protocol  # check implementation against spec
```

### How autonomous mode works

In autonomous mode, Claude executes the loop in [`AUTONOMY.md`](AUTONOMY.md) §1: read state, pick the lowest unfinished sub-task in the active phase doc, implement it, run verify, commit, repeat. On any uncertainty, Claude stops and writes `CONTEXT.md` describing the situation rather than guessing.

The pre-tool-use hooks in `.claude/hooks/` provide a safety net: even with permissions skipped, Claude cannot `rm -rf /`, `git push --force`, `cargo publish`, edit files in `spec/`, or run `sudo`. Edit the hooks to adjust.

## Building and testing

**Linux x86_64 / aarch64, kernel ≥ 5.15.** Brain depends on `io_uring`, `O_DIRECT`, `pwritev2(RWF_DSYNC)`, and a few Linux-only `madvise` / `fallocate` flags — see `spec/01_system_architecture/05_hardware.md` §1.1 for why we chose a single-platform backend over portable shims.

| Crate | Linux | macOS / Windows |
|---|---|---|
| `brain-core`, `brain-protocol`, `brain-cli`, `brain-sdk-rust` | ✓ build + test | ✓ build + test (no I/O / runtime) |
| `brain-storage`, `brain-server`, `brain-workers`, `brain-index` (post-persist), `brain-embed` (post-wiring) | ✓ build + test | ✗ `compile_error!` — use a Linux container |

For non-Linux dev hosts, **see [`DEV_SETUP.md`](DEV_SETUP.md)** for Docker / OrbStack / Colima / Lima / cross-compile recipes. CI (`.github/workflows/ci.yml`) runs everything on `ubuntu-latest` and is the authoritative test gate.

```bash
# Native Linux:
just verify

# Per-crate test (anywhere brain-core/brain-protocol compile):
cargo test -p brain-protocol

# Run the server (Linux only):
cargo run --bin brain-server -- --config config/dev.toml

# CLI (cross-platform):
cargo run --bin brain-cli -- stats
```

## Implementation status

The 17-spec design is **complete**. Phase 0 (workspace scaffold) is provided by the starter template. Phase 1 onward is the implementation work. See [`ROADMAP.md`](ROADMAP.md).

| Phase | Scope | Status |
|---|---|---|
| 0 | Workspace skeleton, CI | Scaffolded — verify with `just verify` |
| 1 | Wire protocol & core types | Not started |
| 2 | Storage: arena + WAL + recovery | Not started |
| 3 | Metadata + redb integration | Not started |
| 4 | HNSW index | Not started |
| 5 | Embedding layer | Not started |
| 6 | Query planner + executor | Not started |
| 7 | Cognitive operations | Not started |
| 8 | Background workers | Not started |
| 9 | Server: end-to-end wire-up | Not started |
| 10 | Rust SDK + CLI | Not started |
| 11 | Observability, benchmarks, acceptance | Not started |

## Documentation

- **For users / operators:** see [`spec/00_master_overview/`](spec/00_master_overview/) and [`spec/14_observability_ops/`](spec/14_observability_ops/).
- **For implementers:** see all 17 spec directories, especially [`spec/01_system_architecture/`](spec/01_system_architecture/).
- **For SDK users:** see [`spec/13_sdk_design/`](spec/13_sdk_design/).
- **For folks evaluating Brain:** see [`spec/16_benchmarks_acceptance/`](spec/16_benchmarks_acceptance/).

## License

TBD.

## Contributing

This is currently a single-developer effort building from spec. Once a working alpha exists, contribution guidelines will be added.
