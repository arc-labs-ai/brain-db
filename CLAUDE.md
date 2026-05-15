# Brain — Claude Code Project Context

This file is loaded automatically by Claude Code at the start of every session. It tells Claude what this project is, what's authoritative, and how to operate.

**For autonomous-mode operating rules** (`claude --dangerously-skip-permissions`), see [`AUTONOMY.md`](AUTONOMY.md). That file defines the execution loop, hard rules, and stop conditions. Read it before doing any work.

---

## 1. What this project is

**Brain** is a cognitive substrate for AI agents — a database where the primitives are cognitive operations (encode, recall, plan, reason, forget) instead of tables/documents/vectors.

v1.0 ships in two layers:

- **Substrate** (spec §00–§16, phases 0–14): vector memory store. WAL, HNSW, wire protocol, cognitive primitives (ENCODE/RECALL/PLAN/REASON/FORGET), HTTP/WS/SSE, observability.
- **Knowledge layer** (spec §17–§31, phases 15–24): typed entities, statements (Fact/Preference/Event), relations, schema DSL, three-tier extractors (pattern → classifier → LLM), hybrid retrieval (semantic + lexical + graph with RRF fusion).

The knowledge layer activates only when a schema is declared via `SCHEMA_UPLOAD`. A deployment that never declares a schema runs as a pure vector substrate (substrate-only mode) — a first-class deployment posture, not a legacy mode. The `v1.0.0` tag lands at the end of Phase 24, after the combined acceptance suite passes.

We are **building the implementation**. The design is already done.

## 2. Single source of truth

The `spec/` directory is **authoritative**. 32 specification sections — 17 substrate (§00–§16) + 15 knowledge layer (§17–§31).

- **The spec is read-only.** Don't edit it. Spec changes go through the user.
- **The spec wins.** If code disagrees with spec, fix the code.
- **The spec is comprehensive.** If a question seems unanswered, look harder before deciding it's missing.

Quick-find:

| Question | Spec file |
|---|---|
| What does ENCODE do? | `spec/09_cognitive_operations/02_encode.md` |
| What does RECALL return? | `spec/09_cognitive_operations/03_recall.md` |
| What's the wire frame format? | `spec/03_wire_protocol/03_frame_header.md` |
| How does WAL recovery work? | `spec/05_storage_arena_wal/08_recovery.md` |
| What's the redb schema? | `spec/07_metadata_graph/02_table_layout.md` |
| Why HNSW M=16? | `spec/06_ann_index/02_parameters.md` |
| What error codes exist? | `spec/03_wire_protocol/10_errors.md` |
| What's the latency target? | `spec/16_benchmarks_acceptance/02_latency_targets.md` |
| What's the three-layer model? | `spec/17_knowledge_model/00_purpose.md` |
| How does entity resolution work? | `spec/18_entities/01_resolution.md` |
| Fact vs Preference vs Event rules? | `spec/17_knowledge_model/02_three_statement_kinds.md` |
| What does the schema DSL look like? | `spec/21_schema_dsl/01_grammar.md` |
| How do the three extractor tiers compose? | `spec/22_extractors/00_purpose.md` |
| How does RRF fusion work? | `spec/23_retrievers/01_rrf_fusion.md` + `spec/24_hybrid_query/00_purpose.md` |
| What knowledge-layer wire opcodes exist? | `spec/28_knowledge_wire_protocol/00_purpose.md` |
| What's the knowledge-layer storage layout? | `spec/26_knowledge_storage/00_purpose.md` |
| What's the combined acceptance gate? | `spec/31_complete_acceptance/00_purpose.md` |

The full directory map is in [`spec/00_master_overview/02_doc_map.md`](spec/00_master_overview/02_doc_map.md).

## 3. How work is structured

The roadmap is in three layers:

1. **[`ROADMAP.md`](ROADMAP.md)** — high-level phase index. One-line per phase.
2. **[`docs/phases/phase-NN-*.md`](docs/phases/)** — detailed sub-task breakdown for each phase. Reads, writes, "done when" criteria.
3. **[`AUTONOMY.md`](AUTONOMY.md)** — operating rules (commit format, stop conditions, scope guards).

When asked "what's next?", the answer is always: the lowest-numbered unfinished sub-task in the active phase doc.

The `/next-task` slash command finds it for you.

## 4. Architecture in one paragraph

Linux server. Connection layer (Tokio) accepts TCP; each request dispatches to one of N **shards**. Each shard runs a **Glommio** executor (thread-per-core, io_uring) and owns its data: a memory-mapped **arena** for vectors, a **WAL** with O_DIRECT + `pwritev2(RWF_DSYNC)` group commit, a **redb** B-tree for metadata, an **HNSW** index in RAM. **Single-writer-per-shard**: writes don't lock; reads use **ArcSwap** + **crossbeam-epoch**. Twelve **background workers** handle decay, consolidation, HNSW maintenance, GC, etc. The substrate **owns the embedding model** (BGE-small via candle, 384-dim); clients send text.

**Knowledge layer (activates when a schema is declared):** the same shard additionally owns an **entity HNSW**, a **statement HNSW**, two **tantivy** indexes (memory text + statement text), and an **LLM extractor cache** (separate redb). Background workers run pattern → classifier → LLM extractors on ENCODE and write entities/statements/relations into knowledge-layer redb tables. The **query router** routes a structured query through three retrievers (semantic / lexical / graph) and fuses ranks via **RRF** (`k=60`), then applies filters (type / temporal / confidence). `RECALL` transparently uses the hybrid path when a schema is declared; clients see the same response shape with extra metadata.

> **Note on slot size:** the arena slot is 1600 bytes (1536 vector capacity + 64 metadata/padding) for forward compatibility with larger embedding models. BGE-small uses 384 dims = 1536 bytes; the rest is reserved. Confirm in spec/05/02 before laying out.

## 5. Core invariants — DO NOT violate

These are non-negotiable. Code that violates them is wrong, regardless of test results.

1. **WAL-before-acknowledge.** No operation returns success until its WAL record is fsynced.
2. **Single writer per shard.** No locks needed; the discipline enforces it.
3. **CRC everywhere.** Every WAL record, every arena slot. Reads verify; mismatches halt.
4. **Slot version on `MemoryId`.** Encoded in the ID. Stale references → `NotFound`.
5. **Idempotency by RequestId.** 24h TTL. Same params → cached response. Different params → `Conflict`.
6. **Tombstone grace before reclamation.** Default 7 days. Hard FORGET zeroes immediately.
7. **No silent corruption.** Fail-stop and alert. Never return wrong data.

Tested per `spec/16_benchmarks_acceptance/06_durability_criteria.md`.

## 6. Tech stack — exact crates

Approved set. New deps require justification + commit message rationale.

| Component | Crate |
|---|---|
| Async runtime (shards) | `glommio` |
| Async runtime (connection layer) | `tokio` |
| Wire encoding | `rkyv` + `bytemuck` |
| Metadata | `redb` |
| HNSW | `hnsw_rs` |
| Embedding | `candle-core` + `candle-nn` + `candle-transformers` + `tokenizers` |
| SIMD math | `matrixmultiply` + `wide` |
| Lock-free swap | `arc-swap` |
| Epoch GC | `crossbeam-epoch` |
| CRC | `crc32c` |
| UUIDs | `uuid` (v7 feature) |
| Hashing | `blake3` |
| Errors | `thiserror` (libs), `anyhow` (binaries) |
| Logging | `tracing` + `tracing-subscriber` |
| Tracing | `opentelemetry` |
| Testing | `proptest`, `criterion`, `tempfile` |

Added without justification → reject.

## 7. Code conventions

- **Edition:** Rust 2021. **MSRV:** stable, latest minus one.
- **Errors:** `thiserror` for libs; `anyhow` for bins. Stable error taxonomy per spec §03/10.
- **No `unwrap()` outside tests.** Use `expect("invariant: <reason>")` for unreachable.
- **Public APIs:** rustdoc + at least one example for non-trivial.
- **No `unsafe` outside `crates/brain-storage`.** That crate needs it for mmap. Every `unsafe` block: `// SAFETY:` comment, smallest scope.
- **Formatting:** rustfmt defaults.
- **Lints:** clippy default warnings as errors in CI. Pedantic is aspirational; not enforced on stubs.
- **Naming:** snake_case items, CamelCase types — Rust standard.

## 8. Workspace structure

```
crates/
├── brain-core/          Shared types: MemoryId, EdgeKind, Error, EntityId, StatementId, ...
├── brain-protocol/      Wire protocol: frame, opcodes, codec, schema DSL parser
├── brain-storage/       Arena + WAL + recovery (substrate + knowledge frame types)
├── brain-metadata/      redb wrapper: substrate tables + knowledge tables (entities, statements, relations, predicates, audits)
├── brain-index/         HNSW (memory + entity + statement); tantivy integration (phase 22)
├── brain-embed/         BGE embedding service
├── brain-planner/       Query planner + executor (substrate + hybrid query router, phase 23)
├── brain-ops/           Cognitive operations
├── brain-workers/       Background workers (substrate + extractors + sweepers)
├── brain-extractors/    Pattern + classifier extractors (introduced phase 20)
├── brain-llm/           LLM client + cache + budget (introduced phase 21)
├── brain-http/          HTTP/WS/SSE transport
├── brain-server/        Server binary, wires it all together
├── brain-sdk-rust/      Rust SDK (substrate + typed knowledge SDK with derive macros)
└── brain-cli/           Admin CLI
```

Each maps to one or more spec sections; see the relevant phase doc. `brain-extractors` and `brain-llm` are introduced in phases 20 and 21 respectively — earlier phases must not depend on them.

## 9. Anti-patterns

- **Don't add Tokio inside a shard.** Shards use Glommio. Mixing blocks the executor.
- **Don't hold a lock across `.await`.**
- **Don't allocate in the hot path** (encode/recall serving). Use object pools.
- **Don't add `Send + Sync`** to per-shard types. They're explicitly `!Send`.
- **Don't use `tokio::fs`** in shard code. Use Glommio's I/O.
- **Don't introduce a thread pool** for parallel work. Sharding is the parallelism.
- **Don't trust user input.** All wire input is untrusted; validate.
- **Don't `panic!` on user-input errors.** Return a structured error.

## 10. Testing strategy

- Unit tests colocated.
- Integration tests in `tests/` per crate.
- Property tests with `proptest` for parsers, allocators, recovery.
- Fuzz with `cargo-fuzz` for the wire protocol.
- Loom for concurrency-critical paths.
- Miri for `crates/brain-storage`'s unsafe.
- Chaos tests for recovery (kill-during-operation).
- Benchmarks with `criterion` per phase.

The spec is the test plan. New behavior → new test. Spec change → corresponding test change.

## 11. Common commands

```bash
just verify            # full verify suite
just build             # workspace build
just test              # all tests
just clippy            # lints with -D warnings
just fmt               # format
just run-server        # cargo run --bin brain-server
just bench <crate>     # criterion bench
just doc               # docs

# Slash commands inside Claude Code
/spec <num> [file]     # navigate the spec
/next-task             # find next sub-task
/verify                # run verify suite
/status                # show phase progress
/audit-spec <crate>    # check implementation against spec
```

## 12. When the spec is silent

Roughly 5% of behavior isn't fully nailed down — see each spec's `*_open_questions.md`.

Process when you hit ambiguity:

1. Re-read the relevant spec section.
2. Check that spec's `*_open_questions.md`.
3. If still unclear: STOP and surface (per [`AUTONOMY.md`](AUTONOMY.md) §3).

Don't invent. The user has spent significant time on the design.

## 13. Style of working

The user has explicitly preferred:

- **Don't ask permission for routine work.** Proceed and report.
- **Decisions inside the spec; flag deviations.**
- **Be honest about uncertainty.** "I don't know" beats confidently wrong.
- **Generate continuously without interruption** when implementing a spec'd feature.
- **Verify after writing.** `cargo check` and `cargo test` — actually run them.
- **Single final version.** No "v1 / v2 of this implementation."

Autonomous mode is enabled. The autonomy contract ([`AUTONOMY.md`](AUTONOMY.md)) operationalizes the trust.

## 14. Initial setup checklist

If running for the first time on a fresh checkout:

1. `git status` — confirm clean.
2. `just verify` — confirm Phase 0 passes.
3. If green: `git tag phase-0-complete` (if not already tagged).
4. Read [`AUTONOMY.md`](AUTONOMY.md) end-to-end.
5. Read the relevant phase doc (`docs/phases/phase-NN-*.md`) end-to-end.
6. Begin sub-task 1.1 (or current).

## 15. When in doubt

The spec wins. The user is the tiebreaker. Don't invent. Stop and surface — see [`AUTONOMY.md`](AUTONOMY.md) §3.
