# Roadmap

Forward-looking plan for Brain's road to v1.0 and beyond.

This file is the **high-level milestone index**. The per-phase landing record lives in the git history (`git log --oneline`).

---

## Current status

**Pre-release: v0.1.0.** No external users. Wire protocol, redb tables, and schema model are still in flux. Breaking changes land in place without back-compat shims until v1.0 ships.

Implementation coverage is broad — the substrate (vector memory, WAL, HNSW, wire protocol, write/read pipelines, observability) and the typed-graph layer (entities, statements, relations, schema DSL, three-tier extractors, hybrid retrieval) both have code landing on every crate the spec calls for. What remains for **v1.0** is **convergence**, not new feature work: the combined acceptance suite at [`spec/19_benchmarks/06_complete_acceptance.md`](spec/19_benchmarks/06_complete_acceptance.md) must pass end-to-end on reference hardware, several scope-cuts from the implementation phases need closing, and the documentation surface needs a final pass.

---

## v1.0 — the release gate

The `v1.0.0` tag is cut when the combined acceptance suite passes:

- **Functional acceptance** — every wire opcode round-trips; every cognitive verb behaves per spec; schemaless and schema-declared modes both pass their suites.
- **Performance acceptance** — every operation meets the targets in [`spec/19_benchmarks/02_performance_targets.md`](spec/19_benchmarks/02_performance_targets.md) on reference hardware (16-core x86_64, 64 GiB RAM, NVMe SSD).
- **Storage acceptance** — WAL durability under crash/kill; CRC catches corruption; recovery is deterministic.
- **Operational acceptance** — snapshots, restores, admin operations, runbooks all execute cleanly.
- **Documentation acceptance** — every cross-reference resolves; the spec is internally consistent; at least one end-to-end tutorial walks blank deployment → working query.

A deployment that never calls `SCHEMA_UPLOAD` is a valid v1.0 deployment posture — schemaless mode is first-class, not a legacy or minimal mode.

### Convergence work to v1.0

What's pending before the acceptance suite can run green:

| Area | What's needed |
|---|---|
| **Acceptance suite execution** | Wire the harness, run on reference hardware, capture wall-time numbers across all 50+ acceptance checks. Many criterion benches are in place; full-suite integration is the missing piece. |
| **Classifier inference (candle BERT)** | The classifier extractor framework ships with the load path validated; the candle forward pass + linear classifier head is parked behind `BRAIN_NER_MODEL_PATH`. Operator-provided model + runtime weights need to light up the inference path. |
| **Live LLM provider validation** | Anthropic + OpenAI clients are wired through a mock-client integration suite. Live-provider end-to-end runs (real API keys, real cost accounting) need a pass before v1.0. |
| **Production-scale benches** | Per-shard benches run at 10K corpus scale. Full v1.0 acceptance runs at 1M memories per shard with mixed workloads. |
| **Spec consistency final pass** | One last sweep to ensure every cross-reference resolves, all numerical claims are consistent (latency targets, HNSW parameters, slot sizes, grace periods), and any remaining stub sections in §17–§19 are filled. |
| **Tutorial polish** | The end-to-end tutorial walks blank deployment → working query; needs one round of "follow it on a fresh laptop" testing. |

When all of these are green, the `v1.0.0` tag is cut.

---

## v1.x — post-release work that doesn't gate v1.0

Planned improvements that land after v1.0 without changing the wire protocol or storage formats incompatibly.

| Item | Scope |
|---|---|
| **Resolver tier 4 (LLM)** | LLM-assisted entity disambiguation. Tiers 1–3 (exact+alias / fuzzy / embedding) ship in v1; LLM tier wires the resolver to `brain-llm` for ambiguous cases. |
| **Per-statement-kind retention** | Today retention defaults are per-kind via decay; explicit retention policies (e.g., per-namespace TTL) land in v1.1. |
| **`ADMIN_BACKFILL` / `ADMIN_CANCEL` wire opcodes** | Backfill worker is operational via direct enqueue; wire surface for operator-driven control lands in v1.1. |
| **`SCHEMA_DROP` opcode** | In-place schema downgrade (revert is manual per runbook in v1). |
| **Cascade audit rows + soft-cascade revert** | FORGET cascade itself works; the audit log of cascaded writes + revert path is v1.1. |
| **Per-row stale-extraction flag** | Stale-count surfaces via metric in v1; per-row flag (needs `StatementRow.flags` schema bump) is v1.1. |
| **Streaming hybrid query results** | `limit > 100` streams across multiple `QueryResponse` frames; v1 is single-frame. |
| **Hybrid + transactional read-your-writes** | RECALL inside a txn uses the substrate path in v1; lens layering across statements + relations is v1.1+. |
| **Multi-frame cursor pagination** | `ENTITY_LIST`, `STATEMENT_LIST`, `STATEMENT_HISTORY`, `RELATION_LIST_FROM` — all single-frame snapshots in v1. |
| **Wire-protocol conformance corpus** | A language-agnostic round-trip corpus (recorded request/response frames with CBOR payloads) that any client implementation can replay to verify §04 conformance. Brain ships no first-party SDK; the corpus is the drift guard for third-party clients. |
| **`ADMIN_TANTIVY_REBUILD` wire op** | Hot tantivy rebuild from the admin CLI; v1 rebuild is startup-only. |
| **Schema migration plan computation** | The `keep` / `re-extract` / `tombstone` action vocabulary is specified in `spec/03_schema/05_versioning.md`; computation + execution lands post-v1. |
| **Hot rebuild while writer running** | v1 tantivy rebuild requires shard restart. |
| **Partial WAL replay on tantivy recovery** | v1 rebuilds from scratch on `NeedsRebuild` status; partial replay using indexer cursors is post-v1. |
| **Cross-shard hybrid result merging** | v1 is per-shard; router-level fan-out and merge for cross-shard agents is v1.1. |
| **Live-registry sync on `SCHEMA_UPLOAD`** | Uploaded extractors are observable via `EXTRACTOR_LIST` but the dispatching registry rebuilds only at shard spawn in v1. |

---

## v2 — out of v1 scope

Capability changes that touch the wire protocol, on-disk formats, or cluster architecture.

| Item | Why it's v2 |
|---|---|
| **Multi-node clustering** | v1 is single-node. Distributed coordination, range-based sharding, cross-node query fan-out — each is a multi-month design. |
| **Replication** | v1 is single-replica per shard; node loss means restore-from-snapshot. Synchronous WAL streaming + asynchronous follower replication are both v2 candidates with different trade-offs. |
| **Wire-protocol conformance corpus** | Brain ships no first-party SDK in any language; the public interface is the §04 wire protocol (CBOR payloads). Once the wire freezes, a language-agnostic conformance corpus lets third-party clients verify their implementations against recorded frames. |
| **Tenant offloading / lazy loading** | Cold tenants serialized to object storage; lazy-load on first query. Pattern documented in `spec/01_architecture/07_wedges_and_roadmap.md` Roadmap. |
| **Storage-compute separation with freshness layer** | Blob storage as source of truth + in-memory freshness layer for recent writes. Pinecone-serverless-style architecture. |
| **IVF + PQ on top of HNSW** | For billion-vector scale; today's HNSW is RAM-heavy past 10⁷ vectors per shard. |
| **Range-based sharding with Raft replication** | CockroachDB-style auto-split/auto-merge ranges. Today's `hash(agent_id) % shard_count` caps tenant scale at single-shard throughput. |
| **Decoupled roles (separate processes per concern)** | FoundationDB-style. Coordinator / proxy / log / resolver / storage as separate processes. |
| **Multi-region active-active** | Cross-region writes with replication. v2 at earliest. |
| **Federated knowledge graphs** | Cross-node entity / statement queries. Brain's value proposition is local-first; federation is a different system. |

The patterns above (offloading, storage-compute split, IVF+PQ, range sharding, decoupled roles) are documented as **future-direction candidates** in [`spec/01_architecture/07_wedges_and_roadmap.md`](spec/01_architecture/07_wedges_and_roadmap.md) §Roadmap. None are commitments; each gates on a demonstrated operator need that the current architecture can't satisfy.

---

## Known limitations of v1

Documented up front so the scope is honest:

- **Single-node only.** Multi-node clustering is v2.
- **No replication.** Backups (snapshots) only. v2.
- **No first-party SDK.** Brain is a standalone database; the public interface is the §04 wire protocol (CBOR payloads). Clients are third-party.
- **Linux only.** Glommio + `io_uring` don't run elsewhere.
- **English text only.** `bge-small-en-v1.5` is English; multilingual support requires a different embedding model and re-embedding. v2.
- **Single embedding model per deployment.** Hot-swapping the model requires `ADMIN_MIGRATE_EMBEDDINGS` (offline).
- **No query language.** Wire protocol is typed RPC; a SQL-like text language is v2 at earliest.
- **Fine-grained access control out of scope.** Brain has authentication and shard-level authorization; per-memory ACLs, field-level security, and time-bounded permissions are v2 if at all.

These aren't bugs — they're scope boundaries. Don't accidentally implement them.

---

## Implementation history

`git log --oneline` shows the full landing history — what each phase delivered, deferrals, and bench results are captured in the commit log.

---

## How v1.0 gets cut

1. Run the combined acceptance suite on reference hardware (`scripts/acceptance.sh`).
2. Capture wall-time numbers for every operation against the targets in `spec/19_benchmarks/02_performance_targets.md`.
3. Execute the schema-toggle and disaster-recovery runbooks against a chaos scenario.
4. One final spec consistency pass — every cross-reference resolves, every numerical claim is harmonized.
5. Cut the `v1.0.0` tag from the same commit that contains the green acceptance log.

Once `v1.0.0` is out, the v1.x work above can land incrementally without wire/storage breaks; v2 work requires a new major version.
