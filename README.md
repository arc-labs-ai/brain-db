<p align="center">
  <img src="assets/banner.svg" alt="brain — a memory database for AI agents" width="100%">
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
  <a href="#status"><img src="https://img.shields.io/badge/status-pre--release%20v0.1.0-orange.svg" alt="Status"></a>
  <a href="#tech-stack"><img src="https://img.shields.io/badge/language-Rust-orange.svg" alt="Rust"></a>
  <a href="#platform-support"><img src="https://img.shields.io/badge/platform-Linux-lightgrey.svg" alt="Linux"></a>
  <a href="spec/"><img src="https://img.shields.io/badge/spec-153%20files-blue.svg" alt="Spec"></a>
</p>

# Brain

> **A memory database for AI agents.** Stores four record types — Memory, Entity, Statement, Relation — with explicit provenance, confidence, and bi-temporal validity. Hybrid retrieval (semantic + lexical + entity-graph + temporal) fused with weighted rank fusion. One Rust core, one wire protocol, one schema. Apache 2.0.

```text
$ brain-server --config config/dev.toml
─────────────────────────────────────────────────────────────────────────────
  ◉ brain-server  v0.1.0  ·  listening 127.0.0.1:9090 (wire) · :9092 (admin)
  ◉ p99 recall 4.2ms  ·  WAL synced  ·  HNSW warm
  ◉ schema: brain:core + acme:sales (2 namespaces, 14 types)
─────────────────────────────────────────────────────────────────────────────

# Brain ships no client. Any language speaks the binary wire protocol (§04)
# directly — CBOR payloads, documented per-opcode field schemas. Conceptually:

ENCODE  "Had a difficult conversation with Alex about the project"
  → ENCODED                                          LSN 1 · s1/m1/v1 ·  9 ms

RECALL  "conflicts with Alex"  top_k=5
  # → ranked by semantic similarity, edge proximity, recency, and salience
  #   — not just vector distance.

# With a schema declared, RECALL routes through the hybrid path:
# semantic + lexical + entity-graph, fused via RRF.
RECALL  "what's Priya working on?"  include_graph=true
```

---

## Table of contents

- [Why Brain](#why-brain)
- [What Brain stores](#what-brain-stores)
- [Schemaless vs schema-declared](#schemaless-vs-schema-declared)
- [Quickstart](#quickstart)
- [Cognitive operations](#cognitive-operations)
- [Architecture in 30 seconds](#architecture-in-30-seconds)
- [Performance targets](#performance-targets)
- [Status](#status)
- [Documentation](#documentation)
- [Repository layout](#repository-layout)
- [Tech stack](#tech-stack)
- [Platform support](#platform-support)
- [Contributing](#contributing)
- [License](#license)

---

## Why Brain

Today's agent stacks duct-tape four or five storage systems: a vector database for similarity, a graph database for relationships, a full-text store for keyword matching, an LLM extraction pipeline, plus an orchestration layer that pretends to keep them consistent. Half of that orchestration is reinventing transaction semantics across systems that don't agree on what "committed" means.

Brain collapses the stack into one Rust core with one wire protocol and one schema:

- **Cognitive verbs, not CRUD.** `encode` / `recall` / `plan` / `reason` / `forget` are the primitive operations.
- **Hybrid retrieval out of the box.** Three retrievers (semantic / lexical / graph) fused via weighted RRF — not "top-k by cosine."
- **Provenance is first-class.** Every typed claim carries an evidence list back to source memories, plus four bi-temporal timestamps.
- **Predictable tail latency.** Thread-per-core (Glommio + `io_uring`), single-writer-per-shard, lock-free reads, group-commit WAL.
- **Apache 2.0, end to end.** No premium edition, no SaaS lock-in. The typed graph, the extractor pipeline, the reranker, and the schema DSL are all in the open repo.

The architectural justification, the five design wedges, and the comparison with adjacent systems (Pinecone, Qdrant, Neo4j, Mem0, Letta, Zep) are in [`spec/01_architecture/`](spec/01_architecture/00_purpose.md).

---

## What Brain stores

Four record types, one database:

| Record | What it is | Example |
|---|---|---|
| **Memory** | Raw experience — text + 384-dim embedding + salience + edges + provenance | `"Alex pushed the deadline to next Friday"` |
| **Entity** | Canonical noun with a stable UUIDv7 identity, alias list, and typed attributes | `Person(canonical_name="Alex Chen", aliases=["Alex"])` |
| **Statement** | Typed claim about entities — `Fact` / `Preference` / `Event` — with confidence and bi-temporal validity | `Event(subject=alex, predicate=pushed, object="deadline to Friday", valid_from=t0, confidence=0.92)` |
| **Relation** | Typed binary edge between entities, with cardinality and evidence | `reports_to(alex, priya)` |

Entities, Statements, and Relations are derived from Memories by a three-tier extractor pipeline (pattern → GLiNER classifier → LLM with prompt cache) when a schema is declared.

The full data model is in [`spec/02_data_model/`](spec/02_data_model/00_purpose.md).

---

## Schemaless vs schema-declared

The same Brain binary serves both modes; the runtime gate is whether the per-shard `SCHEMA_ACTIVE_VERSIONS_TABLE` is empty.

| Mode | What's active | Use it when |
|---|---|---|
| **Schemaless** (default) | Memory record only. `ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`, `SUBSCRIBE`, `TXN_*` over vector memory. | Prototyping; semantic memory without typed-graph overhead; small agents. |
| **Schema-declared** | All of the above + typed extraction + entity/statement/relation writes + hybrid retrieval over the typed graph. | Production agents that need provenance, temporal reasoning, supersession, or entity-anchored queries. |

A deployment can move in either direction. Declaring a schema after months of schemaless use kicks off a backfill. Schema is not a legacy or minimal mode — both postures are first-class.

The DSL is documented in [`spec/03_schema/`](spec/03_schema/00_purpose.md). Example:

```text
namespace acme

define entity_type Person {
    attributes {
        email:    text optional unique
        team:     text optional
        timezone: text optional
    }
}

define predicate prefers {
    kind:   Preference
    object: Value<text>
}

define relation_type reports_to {
    from:        Person
    to:          Person
    cardinality: many-to-one
}

define extractor preferences {
    kind:                 llm
    target:               statement Preference
    trigger:              on encode where memory.kind = episodic
    model:                "gpt-4o-mini"
    confidence_threshold: 0.7
    cache:                enabled
}
```

---

## Quickstart

**Requires:** Docker, [`@devcontainers/cli`](https://github.com/devcontainers/cli) (`npm install -g @devcontainers/cli`).

```bash
git clone https://github.com/brain-db-io/brain-db
cd brain-db
just docker-up            # builds image, starts container, runs post-create
just docker-shell         # bash inside the dev container
```

Inside the container:

```bash
just verify                                            # fmt + build + clippy + test
cargo run --bin brain-server -- --config config/dev.toml   # the database
curl -s http://127.0.0.1:9092/v1/stats                 # admin via curl (loopback)
```

One binary:

- **`brain-server`** — the database. Binary wire protocol on the data port; a loopback HTTP admin listener (stats, snapshots, audit, worker control) reachable with `curl`. Brain ships no client/SDK/CLI — speak the wire protocol from any language; a `brainctl` migration tool is future work.

A persistent agent identity is opt-in; a client supplies its `agent_id` at handshake, and by default the server mints an ephemeral one per connection if none is given.

**Memory is isolated per agent.** `RECALL` returns only the calling agent's own memories by default — one tenant never sees another's. Cross-agent reads are explicit: a request scopes to a named agent set, or opts into the shared view. Each hit carries its owning `agent_id` so provenance is always legible.

---

## Cognitive operations

The verbs that drive Brain. Full semantics are in [`spec/05_operations/`](spec/05_operations/00_purpose.md).

| Verb | What it does |
|---|---|
| **ENCODE** | Store an experience. Embeds the text, picks a slot, writes the WAL record, updates metadata, inserts into HNSW. With a schema declared, queues extractors. |
| **RECALL** | Find memories relevant to a cue. Blends similarity with salience, recency, and edge proximity. Routes through the hybrid retriever when a schema is declared. |
| **PLAN** | Construct a path from one cognitive state to another. Pull-based executor with budgets (steps, wall time, branches). |
| **REASON** | Multi-hop traversal explaining why X is connected to Y. Returns the path, evidence memories, and confidence. |
| **FORGET** | Soft (mark + grace period) or hard (zero the slot) tombstoning. Cascades to derived typed-graph records when a schema is active. |
| **LINK** / **UNLINK** | Manually assert / retract a typed edge between two memories. |
| **SUBSCRIBE** | Stream events: memory created, statement created, extractor failed, schema updated, etc. |
| **TXN_BEGIN** / **TXN_COMMIT** / **TXN_ABORT** | Group multiple operations into one atomic unit. |

One-shot mode (each invocation runs a single verb and exits):

```bash
brain encode "Alex pushed the deadline to next Friday"
brain recall "when did Alex change the deadline?" --top-k 5 --include-text
brain plan "current sprint state" "feature shipped" --max-steps 8
brain forget s1/m18/v1 --mode soft
```

Or the same inside the REPL — no `brain` prefix:

```text
brain> encode "Alex pushed the deadline to next Friday"
brain> recall "when did Alex change the deadline?" --top-k 5
brain> reason "Alex changed the deadline" --depth 3
brain> subscribe --kind episodic --collect 10
```

Encoding the same content twice is a no-op by default — pass `--allow-duplicate` to write a fresh copy.

---

## Architecture in 30 seconds

```
┌─────────────────────────────────────────────────────────────────────────────┐
│            CLIENTS (any language — speak the wire protocol directly)         │
└────────────────────────────────────┬────────────────────────────────────────┘
                                     │ custom binary protocol over TCP
                                     │ CBOR structured payloads + raw LE-f32 vectors
┌────────────────────────────────────▼────────────────────────────────────────┐
│  CONNECTION LAYER · Tokio · accept · TLS · frame validate · shard dispatch  │
└────────────────────────────────────┬────────────────────────────────────────┘
                                     │ message channels, one per shard
                ┌────────────────────┼────────────────────┐
                ▼                    ▼                    ▼
        ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
        │  Shard 0     │     │  Shard 1     │     │  Shard N     │
        │  Glommio +   │     │  Glommio +   │     │  Glommio +   │
        │  io_uring    │     │  io_uring    │     │  io_uring    │
        │              │     │              │     │              │
        │  ┌────────┐  │     │  ┌────────┐  │     │  ┌────────┐  │
        │  │ arena  │  │     │  │ arena  │  │     │  │ arena  │  │
        │  │ WAL    │  │     │  │ WAL    │  │     │  │ WAL    │  │
        │  │ redb   │  │     │  │ redb   │  │     │  │ redb   │  │
        │  │ HNSW×3 │  │     │  │ HNSW×3 │  │     │  │ HNSW×3 │  │
        │  │ tantvy │  │     │  │ tantvy │  │     │  │ tantvy │  │
        │  └────────┘  │     │  └────────┘  │     │  └────────┘  │
        │              │     │              │     │              │
        │ Single writer per shard. Lock-free reads via ArcSwap + crossbeam.   │
        └──────┬───────┘     └──────┬───────┘     └──────┬───────┘
               │                    │                    │
               └────────────────────┼────────────────────┘
                                    ▼
                    BACKGROUND WORKERS (per-shard, dedicated cores)
                    decay · consolidation · HNSW maintenance · GC
                    schema-declared: extractors · text indexer · sweepers
```

**Two runtimes, one host.** Connection layer on Tokio (many tasks, accept TCP, decode 32-byte frame, dispatch). Shard layer on Glommio (thread-per-core, `io_uring`, single writer per shard). The two communicate via channels carrying messages — per-shard data never crosses the boundary.

**Six data structures per shard:**

| Structure | Role | Spec |
|---|---|---|
| Arena | mmap'd file of 1600-byte slots (1536 vector + 64 metadata/padding) | [`spec/08_storage/01_arena.md`](spec/08_storage/01_arena.md) |
| WAL | Per-shard append-only log; O_DIRECT + `pwritev2(RWF_DSYNC)` group commit | [`spec/08_storage/02_wal.md`](spec/08_storage/02_wal.md) |
| redb | Embedded ACID B-tree for metadata + typed-graph tables | [`spec/10_metadata/02_table_layout.md`](spec/10_metadata/02_table_layout.md) |
| HNSW × 3 | Memory `M=16, ef_c=200, ef_s=64`; Entity `M=16, ef_c=100, ef_s=64`; Statement `M=32, ef_c=200, ef_s=128` | [`spec/09_indexing/01_hnsw_basics.md`](spec/09_indexing/01_hnsw_basics.md) |
| tantivy × 2 | BM25 over memory text + statement text | [`spec/10_metadata/06_tantivy_layout.md`](spec/10_metadata/06_tantivy_layout.md) |
| LLM cache | Separate redb for extractor responses with TTL | [`spec/11_extractors/06_prompt_caching.md`](spec/11_extractors/06_prompt_caching.md) |

**Seven non-negotiable invariants** (from [`spec/08_storage/00_purpose.md`](spec/08_storage/00_purpose.md)):

1. **WAL-before-acknowledge** — no operation returns success until its WAL record is fsynced.
2. **Single writer per shard** — no locks needed; the discipline enforces it.
3. **CRC everywhere** — every WAL record, every arena slot. Reads verify; mismatches halt.
4. **Slot version on `MemoryId`** — encoded in the ID; stale references → `NotFound`.
5. **Idempotency by `RequestId`** — same params → cached response; different params → `Conflict`.
6. **Tombstone grace before reclamation** — default 7 days. Hard FORGET zeroes immediately.
7. **No silent corruption** — fail-stop and alert. Never return wrong data.

Tested per [`spec/19_benchmarks/01_correctness_and_durability.md`](spec/19_benchmarks/01_correctness_and_durability.md).

For the layered architecture diagram (seven internal layers from L1 connection through L7 sharding) and the full design wedges, see [`spec/01_architecture/04_layers.md`](spec/01_architecture/04_layers.md) and [`spec/01_architecture/07_wedges_and_roadmap.md`](spec/01_architecture/07_wedges_and_roadmap.md).

---

## Performance targets

Hard targets from [`spec/01_architecture/05_hardware_and_targets.md`](spec/01_architecture/05_hardware_and_targets.md) §7 and [`spec/19_benchmarks/02_performance_targets.md`](spec/19_benchmarks/02_performance_targets.md). Single shard, warm, reference hardware (16-core x86_64 / 64 GB RAM / NVMe SSD):

| Operation | p50 | p99 |
|---|---|---|
| `ENCODE` (text, CPU embedding) | ≤ 12 ms | ≤ 25 ms |
| `ENCODE` (text, GPU embedding) | ≤ 3 ms | ≤ 8 ms |
| `ENCODE_VECTOR_DIRECT` (pre-supplied vector) | ≤ 1 ms | ≤ 5 ms |
| `RECALL` (top-k = 10, schemaless) | ≤ 8 ms | ≤ 20 ms |
| `RECALL` (top-k = 10, hybrid path) | ≤ 10 ms | ≤ 50 ms |
| `FORGET` | ≤ 3 ms | ≤ 10 ms |
| `PLAN` (simple) | ≤ 50 ms | ≤ 200 ms |
| `REASON` | ≤ 100 ms | ≤ 500 ms |

Brain optimizes for predictable tails, not minimum averages. The combined acceptance suite at [`spec/19_benchmarks/06_complete_acceptance.md`](spec/19_benchmarks/06_complete_acceptance.md) is the v1.0 release gate.

---

## Status

**Pre-release (v0.1.0).** No external users. The wire protocol, redb tables, and schema model are still in flux. Until v1.0 ships, breaking changes happen in place without back-compat shims.

The v1.0 release ships when the combined acceptance suite passes — functional, performance, storage, operational, and schemaless mode tests, end-to-end.

For the high-level milestone index, see [`ROADMAP.md`](ROADMAP.md); the per-phase landing record is in the git history.

---

## Documentation

| Topic | Location |
|---|---|
| **Specification** (153 files, 20 sections, normative) | [`spec/`](spec/) |
| Spec entry point + glossary + doc map | [`spec/00_overview/`](spec/00_overview/00_index.md) |
| System architecture + design wedges | [`spec/01_architecture/`](spec/01_architecture/00_purpose.md) |
| Data model (Memory / Entity / Statement / Relation) | [`spec/02_data_model/`](spec/02_data_model/00_purpose.md) |
| Wire protocol (frames + opcodes + handshake) | [`spec/04_wire_protocol/`](spec/04_wire_protocol/00_purpose.md) |
| Schema DSL grammar | [`spec/03_schema/`](spec/03_schema/00_purpose.md) |
| Acceptance gate for v1.0 | [`spec/19_benchmarks/06_complete_acceptance.md`](spec/19_benchmarks/06_complete_acceptance.md) |
| Roadmap (milestone index) | [`ROADMAP.md`](ROADMAP.md) |

---

## Repository layout

```
brain/
├── crates/
│   ├── brain-core/         Shared types: MemoryId, EdgeKind, Error, EntityId, ...
│   ├── brain-protocol/     Wire protocol: frame, opcodes, codec, schema DSL parser
│   ├── brain-storage/      Arena + WAL + recovery
│   ├── brain-metadata/     redb wrapper: memory + entity + statement + relation tables
│   ├── brain-index/        HNSW × 3 + tantivy
│   ├── brain-embed/        BGE embedding service
│   ├── brain-planner/      Query planner + executor
│   ├── brain-ops/          One write path + retrievers + extractor writes
│   ├── brain-workers/      Background workers (decay, consolidation, extractors, …)
│   ├── brain-extractors/   Pattern + classifier extractors
│   ├── brain-llm/          LLM client + cache + budget
│   ├── brain-http/         HTTP transport for the admin listener
│   └── brain-server/       Server binary
├── spec/                   The 153-file specification (authoritative)
└── ROADMAP.md              Milestone index
```

---

## Tech stack

Pinned in the workspace `Cargo.toml`. New dependencies require commit-message justification.

| Component | Crate |
|---|---|
| Async runtime (shards) | [`glommio`](https://github.com/DataDog/glommio) — thread-per-core, `io_uring` |
| Async runtime (connection layer) | [`tokio`](https://tokio.rs) |
| Wire encoding | [`ciborium`](https://github.com/enarx/ciborium) (CBOR) + raw little-endian `f32` vectors |
| Internal storage encoding | [`rkyv`](https://github.com/rkyv/rkyv) + [`bytemuck`](https://github.com/Lokathor/bytemuck) |
| Metadata store | [`redb`](https://github.com/cberner/redb) |
| ANN index | [`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs) |
| Lexical index | [`tantivy`](https://github.com/quickwit-oss/tantivy) |
| Embedding inference | [`candle`](https://github.com/huggingface/candle) + [`tokenizers`](https://github.com/huggingface/tokenizers) |
| SIMD math | [`matrixmultiply`](https://github.com/bluss/matrixmultiply) + [`wide`](https://github.com/Lokathor/wide) |
| Lock-free swap | [`arc-swap`](https://github.com/vorner/arc-swap) |
| Epoch GC | [`crossbeam-epoch`](https://docs.rs/crossbeam-epoch) |
| CRC | [`crc32c`](https://docs.rs/crc32c) |
| UUIDs (v7) | [`uuid`](https://docs.rs/uuid) |
| Errors | [`thiserror`](https://docs.rs/thiserror) + [`anyhow`](https://docs.rs/anyhow) |
| Telemetry | [`tracing`](https://docs.rs/tracing) + [`opentelemetry`](https://opentelemetry.io) |

---

## Platform support

**Linux only.** Kernel ≥ 5.15 (for stable `io_uring`). macOS and Windows are not supported; use the supplied dev container for local development on those platforms.

Brain depends on Linux-specific I/O facilities: `io_uring`, `O_DIRECT`, `madvise(MADV_RANDOM | MADV_DONTDUMP)`, `fallocate(FALLOC_FL_KEEP_SIZE)`. Abstracting these would either leak platform differences in tail latency or bloat the codebase with multiple backends. For a system whose value proposition is latency, one optimized backend wins.

CPU: x86_64 with SSE 4.2 **or** ARM64 with the CRC32 extension. AVX2 / NEON used opportunistically. Full hardware envelope in [`spec/01_architecture/05_hardware_and_targets.md`](spec/01_architecture/05_hardware_and_targets.md).

---

## Contributing

Brain is pre-release. The wire protocol, on-disk formats, and schema model still change without back-compat shims. Until v1.0:

- Spec changes go through the project owner. Code disagreements with the spec are fixed by changing the code.
- The seven invariants in [`spec/08_storage/00_purpose.md`](spec/08_storage/00_purpose.md) are non-negotiable.
- New dependencies require commit-message justification; the approved set is in [`Cargo.toml`](Cargo.toml).

CI (`.github/workflows/ci.yml`) is the authoritative test gate. Run `just verify` locally before pushing — it does `fmt + build + clippy -D warnings + test`.

By submitting a pull request, you agree your contribution is licensed under the Apache-2.0 terms (per Apache-2.0 §5).

---

## License

[Apache-2.0](LICENSE). Source code, spec, and documentation are all under the same license.

Repository: <https://github.com/brain-db-io/brain-db>
