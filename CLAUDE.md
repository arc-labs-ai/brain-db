# Brain — Claude Code Project Context

This file is loaded automatically by Claude Code at the start of every session. It tells Claude what this project is, what's authoritative, and how to operate.

**For autonomous-mode operating rules** (`claude --dangerously-skip-permissions`), see [`AUTONOMY.md`](AUTONOMY.md). That file defines the execution loop, hard rules, and stop conditions. Read it before doing any work.

---

## 1. What this project is

**Brain** is a memory database for AI agents. It stores typed memories — facts, preferences, events, entities, relations — with explicit provenance, confidence, and bi-temporal validity. Retrieval is hybrid (semantic + lexical + entity-graph + temporal) fused with weighted rank fusion. One Rust core, one wire protocol, one schema. Apache 2.0.

Brain is **one system, one write path**. There is one `Write { phases: Vec<Phase> }` model, one writer (`submit(Write)`), one apply layer dispatching every phase variant. Capabilities differ by which state they touch (memories, entities, statements, relations, edges, schema, audit, slots) — not by which "layer" they belong to. The spec drops the old substrate / knowledge-layer split entirely.

Capabilities that touch typed-graph state (typed-entity wire ops, statement/relation graph, procedural-memory materialization, VSA algebra, plugins, bi-temporal record-time) check **per-type** against the active schema set: a write referencing a declared type is accepted; one referencing an undeclared type is rejected or dropped (depending on whether the write is explicit or extractor-driven). The seeded `brain:` system namespace is always active; there is no "schema not declared" mode any more. Retrieval, extraction, and the index pipeline run on every shard regardless.

## Status

Brain is **pre-release (v0.1.0)**. No external users. The wire protocol, redb tables, and schema model are still in flux. The v1.0 release is the next milestone; the gate is the combined acceptance suite in `spec/19_benchmarks/06_complete_acceptance.md`. Until v1.0 ships, breaking changes are made in place without back-compat shims.

We are **building the implementation**. The design is already done.

## 2. Single source of truth

The `spec/` directory is **authoritative**. 20 specification sections (§00–§19), unified after the May 2026 consolidation and tightened by the §00..§19 hygiene sweep. Each section bundles all concerns at its layer: memory-record handling, WAL, HNSW, wire frames, workers, schema, extractors, retrievers, and metadata live in the section appropriate to their layer (e.g., wire opcodes in §04, workers in §15, storage in §08, metadata in §10, retrieval in §13). Typed-graph wire ops write rows only when the corresponding type / predicate exists in some active schema namespace — without declared types, the rows are dropped silently (extractor best-effort) or rejected loudly (explicit `ENTITY_CREATE` / `STATEMENT_CREATE` / `RELATION_CREATE` with an undeclared type). The seeded `brain:` system namespace is active from byte zero, so every shard has at least one schema present; user `SCHEMA_UPLOAD` calls merge into it.

- **The spec is read-only.** Don't edit it. Spec changes go through the user.
- **The spec wins.** If code disagrees with spec, fix the code.
- **The spec is comprehensive.** If a question seems unanswered, look harder before deciding it's missing.

Quick-find:

| Question | Spec file |
|---|---|
| What does ENCODE do? | `spec/05_operations/02_write_pipeline.md` |
| What does RECALL return? | `spec/05_operations/03_read_pipeline.md` |
| What's the wire frame format? | `spec/04_wire_protocol/02_wire_format.md` |
| How does WAL recovery work? | `spec/08_storage/04_recovery.md` |
| What's the redb schema? | `spec/10_metadata/02_table_layout.md` |
| Why HNSW M=16? | `spec/09_indexing/01_hnsw_basics.md` |
| What error codes exist? | `spec/04_wire_protocol/07_error_handling.md` |
| What's the latency target? | `spec/19_benchmarks/02_performance_targets.md` |
| What records does Brain store? | `spec/02_data_model/00_purpose.md` |
| How does entity resolution work? | `spec/11_extractors/03_resolver.md` |
| Fact vs Preference vs Event rules? | `spec/02_data_model/07_statement.md` |
| What does the schema DSL look like? | `spec/03_schema/01_grammar.md` |
| How do the three extractor tiers compose? | `spec/11_extractors/00_purpose.md` |
| How does rank fusion work? | `spec/13_retrievers/01_rrf_fusion.md` + `spec/13_retrievers/05_hybrid_query.md` |
| What typed-graph wire opcodes exist? | `spec/04_wire_protocol/03_opcodes.md` |
| What's the typed-graph storage layout? | `spec/10_metadata/02_table_layout.md` |
| What's the combined acceptance gate? | `spec/19_benchmarks/06_complete_acceptance.md` |
| What's the procedural-memory opcode? | `spec/04_wire_protocol/03_opcodes.md` (`MATERIALIZE_PROCEDURAL` 0x0164) |
| What are the design wedges? | `spec/01_architecture/07_wedges_and_roadmap.md` |

The full directory map is in [`spec/00_overview/02_doc_map.md`](spec/00_overview/02_doc_map.md).

## 3. How work is structured

The implementation phases are complete; what remains for v1.0 is convergence (see [`ROADMAP.md`](ROADMAP.md)). Work is tracked in two places:

1. **[`ROADMAP.md`](ROADMAP.md)** — high-level milestone index and the v1.0 cut criteria.
2. **[`AUTONOMY.md`](AUTONOMY.md)** — operating rules (commit format, stop conditions, scope guards).

The per-phase landing record (what each phase delivered, deferrals, bench results) lives in the git history — `git log --oneline`. New plans are drafted under `.claude/plans/`.

## 4. Architecture in one paragraph

Linux server. Connection layer (Tokio) accepts TCP; each request dispatches to one of N **shards**. Each shard runs a **Glommio** executor (thread-per-core, io_uring) and owns the full per-shard state from byte zero: a memory-mapped **arena** for vectors, a **WAL** with O_DIRECT + `pwritev2(RWF_DSYNC)` group commit, a **redb** B-tree for metadata, three **HNSW** indexes in RAM (memory, entity, statement), two **tantivy** indexes (memory text + statement text), and the **LLM extractor cache** (separate redb). **Single-writer-per-shard**: writes don't lock; reads use **ArcSwap** + **crossbeam-epoch**. Per-shard background workers handle decay, consolidation, HNSW maintenance, idempotency sweep, slot reclamation, WAL retention, snapshot, pattern/classifier/LLM extraction, entity resolution, tantivy indexing, FORGET cascade, supersession sweep, schema migration, and entity GC — all of these run on every shard, not gated on schema upload. Brain **owns the embedding model** (BGE-small via candle, 384-dim); clients send text.

**One read pipeline:** `RECALL` and `QUERY` fan out to the three always-wired retrievers (semantic / lexical / graph) and fuse ranks via **RRF** (`k=60`), then run an always-on cross-encoder rerank, then apply the filter chain (type / temporal / confidence / tombstone / supersession). Rerank is first-class: it runs on every read whenever the cross-encoder is loaded — there is no request flag. The only control is the deploy-time `config.rerank.enabled` load gate; when the operator opts out, no model loads and the pipeline returns RRF-only ordering (no error).

**Schema is always on, not a mode.** The seeded `brain:` system namespace is active from byte zero. User `SCHEMA_UPLOAD` calls merge new declarations into the active state additively (associative merge with byte-equal idempotency and all-or-nothing conflict semantics; see [`spec/03_schema/05_versioning.md`](spec/03_schema/05_versioning.md) §1a). Declarations narrow what `STATEMENT_CREATE` / `RELATION_CREATE` / `ENTITY_CREATE` accept and which extracted candidates persist — they do not gate any retrieval, extraction, or index path. `SCHEMA_REPLACE` (`0x0127`, admin-only, requires `force_drop_existing: true`) is the destructive escape hatch when a declaration must be removed or narrowed.

**Capability hard-fail.** Capability config (`rerank.enabled`, `extractors.pattern.enabled`, `extractors.classifier.enabled`, `extractors.llm.enabled`) gates individual tiers via explicit enable flags. A **disabled** tier is skipped silently — the operator chose to opt out. An **enabled** tier that fails to load at shard spawn is a hard spawn failure (`ShardError::TantivyInitFailed` / `ShardError::CrossEncoderInitFailed` / `ShardError::ExtractorInitFailed`); the shard refuses to start rather than running with a quietly-missing capability. Clients call `GET_CAPABILITIES` (`0x0032` / `0x00B2`) after `WELCOME` to introspect which capabilities are live on the connected shard.

> **Note on slot size:** the arena slot is 1600 bytes (1536 vector capacity + 64 metadata/padding) for forward compatibility with larger embedding models. BGE-small uses 384 dims = 1536 bytes; the rest is reserved. Confirm in `spec/08_storage/01_arena.md` before laying out.

## 5. Core invariants — DO NOT violate

These are non-negotiable. Code that violates them is wrong, regardless of test results.

1. **WAL-before-acknowledge.** No operation returns success until its WAL record is fsynced.
2. **Single writer per shard.** No locks needed; the discipline enforces it.
3. **CRC everywhere.** Every WAL record, every arena slot. Reads verify; mismatches halt.
4. **Slot version on `MemoryId`.** Encoded in the ID. Stale references → `NotFound`.
5. **Idempotency by RequestId.** 24h TTL. Same params → cached response. Different params → `Conflict`.
6. **Tombstone grace before reclamation.** Default 7 days. Hard FORGET zeroes immediately.
7. **No silent corruption.** Fail-stop and alert. Never return wrong data.

Tested per `spec/19_benchmarks/01_correctness_and_durability.md`.

## 6. Tech stack — exact crates

Approved set. New deps require justification + commit message rationale.

| Component | Crate |
|---|---|
| Async runtime (shards) | `glommio` |
| Async runtime (connection layer) | `tokio` |
| Wire encoding | `ciborium` (CBOR) + little-endian `f32` vectors |
| Internal storage encoding | `rkyv` + `bytemuck` |
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
- **Errors:** `thiserror` for libs; `anyhow` for bins. Stable error taxonomy per `spec/02_data_model/07_statement.md`.
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
├── brain-storage/       Arena + WAL + recovery (all frame types)
├── brain-metadata/      redb wrapper: memory + entity + statement + relation + predicate + audit tables
├── brain-index/         HNSW (memory + entity + statement); tantivy integration (phase 22)
├── brain-embed/         BGE embedding service
├── brain-planner/       Query planner + executor (memory recall + hybrid query router)
├── brain-ops/           One write path (`handlers/` per opcode → `apply/` per table → `writer/submit`) + `index/` retrievers + `extractor_writes`
├── brain-workers/       Background workers (auto-edge, temporal-edge, extractor, decay, …)
├── brain-extractors/    Pattern + classifier extractors (introduced phase 20)
├── brain-llm/           LLM client + cache + budget (introduced phase 21)
├── brain-http/          HTTP transport for the operator admin listener
└── brain-server/        Server binary, wires it all together
```

Brain ships **no first-party client, SDK, or CLI**. Clients speak the §04 wire protocol directly (portable binary frame + CBOR payloads); operators administer via `curl` against the admin HTTP listener. A `brainctl` offline migration/admin tool is future work, not yet built.

Each crate maps to one or more spec sections. `brain-extractors` and `brain-llm` are the extraction tiers; the rest of the workspace must not depend on them at the wire/storage layer.

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
/verify                # run verify suite
/audit-spec <crate>    # check implementation against spec
```

## 12. When the spec is silent

Roughly 5% of behavior isn't fully nailed down — see each spec's `*../00_overview/04_open_questions_archive.md`.

Process when you hit ambiguity:

1. Re-read the relevant spec section.
2. Check that spec's `*../00_overview/04_open_questions_archive.md`.
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
2. `just verify` — confirm the workspace builds and the suite is green.
3. Read [`AUTONOMY.md`](AUTONOMY.md) end-to-end.
4. Read [`ROADMAP.md`](ROADMAP.md) for the v1.0 convergence work that remains.

## 15. When in doubt

The spec wins. The user is the tiebreaker. Don't invent. Stop and surface — see [`AUTONOMY.md`](AUTONOMY.md) §3.
