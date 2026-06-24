# Roadmap

Forward-looking plan for Brain's road to v1.0 and beyond.

This file is the **high-level milestone index**. The per-phase landing record lives in the git history (`git log --oneline`).

---

## Current status

**Pre-release: v0.1.0.** No external users. Wire protocol, redb tables, and schema model are still in flux. Breaking changes land in place without back-compat shims until v1.0 ships.

Implementation coverage is broad — the substrate (vector memory, WAL, HNSW, wire protocol, write/read pipelines, observability) and the typed-graph layer (entities, statements, relations, schema DSL, three-tier extractors, retrieval) both have code landing on every crate the spec calls for. What remains for **v1.0** is **convergence**, not new feature work: the combined acceptance suite at [`spec/19_benchmarks/06_complete_acceptance.md`](spec/19_benchmarks/06_complete_acceptance.md) must pass end-to-end on reference hardware, the test suite's pre-existing red baseline must be brought green, and the documentation surface needs a final pass.

Recent convergence closures: an operator readiness probe (`GET /readyz` — `503` until every shard serves), the superseded-statement reclamation worker wired into shard spawn (off by default), and backfill re-extraction made functional (it re-enqueues onto the durable extraction queue rather than failing). The test suite was also pruned of redundant/dead coverage (~340–400 functions removed, all with named survivors) and given a documented naming convention (see [`CONTRIBUTING.md`](CONTRIBUTING.md)).

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
| **Green test baseline** | The first full `nextest` run to build to completion surfaced a pre-existing red baseline (~40 failures) dominated by the recall-test-harness family — `recall`/`txn`/`encode` unit + e2e tests that return zero hits *in the test harness* while recall works in the live server and the eval rig. These (and a couple of resolver/e2e cases) must be diagnosed and made green before the acceptance log can be green. Not introduced by recent work — verified against a clean baseline. |
| **Acceptance suite execution** | The end-to-end harness now lives in the `brain-eval` rig (`brain-eval acceptance --scale 1m` / `soak`) — latency / throughput / recall@K / system scenarios / restart-recovery, gated. What remains is running it on quiet reference hardware and capturing wall-time numbers across all 50+ checks. |
| **Classifier inference latency** | The candle forward pass is **implemented and functionally validated** — the real GLiNER pipeline (DeBERTa-v3 backbone → projection → label MLP → BiLSTM → markerV0 span head → einsum scoring → sigmoid decode) runs end-to-end and is dispatched live (ENCODE → near-foreground ExtractorWorker → resolver → persisted entities/statements). The `#[ignore]`d real-weight tests pass against `urchade/gliner_small-v2.1`. What remains is **latency/throughput characterization on reference hardware**: on the dev box (aarch64, opt-level=2) a short memory takes ~60–80 ms, over the §11/01 p99 15 ms budget. Because classification is enqueued (ENCODE does not block on it), this is a worker-throughput ceiling, not an ENCODE-p99 blocker — but the reference-hardware number (x86_64, opt-level=3 + LTO, optionally the `mkl` candle feature) must be captured, and the `mkl`/F16 perf paths are still optional. |
| **Live LLM provider validation** | Anthropic + OpenAI clients are wired through a mock-client integration suite. Live-provider end-to-end runs (real API keys, real cost accounting) need a pass before v1.0. |
| **Production-scale benches** | In-crate criterion benches run at 10K corpus scale in CI; the 1M-per-shard mixed-workload acceptance run is driven by the `brain-eval` rig on reference hardware. |
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
| **Streaming retrieval query results** | `limit > 100` streams across multiple `QueryResponse` frames; v1 is single-frame. |
| **Retrieval + transactional read-your-writes** | RECALL inside a txn runs the same retrieval pipeline over committed data with the txn's pending writes overlaid (read-your-writes); richer lens layering across statements + relations is v1.1+. |
| **Multi-frame cursor pagination** | `ENTITY_LIST`, `STATEMENT_LIST`, `STATEMENT_HISTORY`, `RELATION_LIST_FROM` — all single-frame snapshots in v1. |
| **Wire-protocol conformance corpus** | A language-agnostic round-trip corpus (recorded request/response frames with CBOR payloads) that any client implementation can replay to verify §04 conformance. Brain ships no first-party SDK; the corpus is the drift guard for third-party clients. |
| **`ADMIN_TANTIVY_REBUILD` wire op** | Hot tantivy rebuild from the admin CLI; v1 rebuild is startup-only. |
| **Schema migration plan computation** | The `keep` / `re-extract` / `tombstone` action vocabulary is specified in `spec/03_schema/05_versioning.md`; computation + execution lands post-v1. |
| **Hot rebuild while writer running** | v1 tantivy rebuild requires shard restart. |
| **Partial WAL replay on tantivy recovery** | v1 rebuilds from scratch on `NeedsRebuild` status; partial replay using indexer cursors is post-v1. |
| **Cross-shard retrieval result merging** | v1 is per-shard; router-level fan-out and merge for cross-shard agents is v1.1. |
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
- **Single embedding model per deployment.** A deployment is pinned to the embedding model it was created with. There is no in-place model migration in v1 — changing the model means standing up a fresh deployment and re-ingesting against it. (An offline `ADMIN_MIGRATE_EMBEDDINGS` re-embed path is deferred to a future version; the opcode is reserved but not implemented, and v1 ships no register-model / retire-fingerprint admin surface.)
- **No query language.** Wire protocol is typed RPC; a SQL-like text language is v2 at earliest.
- **Secure-by-deployment, not secure-by-default.** v1's default posture is permissive (any client may claim any `agent_id`; the admin HTTP listener is unauthenticated and loopback-bound). This is intentional for a single-tenant box behind a trusted boundary. Exposing a server beyond `localhost`/a trusted LAN requires walking the hardening runbook in [`SECURITY.md`](SECURITY.md#production-deployment-hardening) (scoped API keys, wire TLS, loopback/reverse-proxied admin). The server logs a loud startup `WARN` while permissive.
- **Fine-grained access control out of scope.** Brain has authentication and shard-level authorization; per-memory ACLs, field-level security, and time-bounded permissions are deferred to a future version.

### Deferred capabilities (reserved, not in v1)

Each of these is a *conscious* deferral with a safe disposition — the code either errors loudly or is a dormant no-op, never a silent wrong answer. Listed so the boundary is explicit:

- **Entity garbage collection.** `EntityGc` ships as a dormant no-op (its inbound-reference count returns "always referenced") and is not spawned. Entity rows are append-mostly in v1; orphaned entities are not reclaimed. No path depends on its output.
- **Subscribe by similarity.** A `SUBSCRIBE` with a `similar_to` vector filter is rejected with a structured `NotYetImplemented` error. v1 subscriptions filter by agent / context / kind only.
- **Hot on-demand tantivy reindex.** The lexical (tantivy) index rebuilds automatically from authoritative redb at startup whenever `open` reports corruption / schema-mismatch (an operator forces it by removing the index dir and restarting). A *live* reindex-without-restart admin call is deferred — it needs the writer quiesced, since the rebuild swaps the index directory. The vector (HNSW) index *does* have an on-demand rebuild (`POST /v1/rebuild-ann`).
- **Scheduled full-shard snapshots.** Operator-triggered full backup + restore over HTTP (`/v1/snapshots`) is the v1 backup story. The periodic background snapshot worker captures the HNSW graph only; automatic periodic *full* bundles are deferred.
- **Statement-level semantic retrieval lane.** The statement-text embedding index is populated (`StatementEmbed` worker) but is not yet wired as a retrieval lane; statement retrieval in v1 is lexical + graph. Deferred per the spec's retrieval roadmap.
- **Consolidation by vector clustering.** The consolidation worker uses window-based clustering in v1; vector-DBSCAN consolidation is shipped but unwired, deferred.
- **Per-row stale-extraction flags.** Stale (schema-version-behind) statements are counted via metrics; a durable per-row stale flag (needs a row-schema bump) is deferred. Re-extraction itself is handled by the schema-migration worker.
- **Slot-version free-list reclamation.** The `SlotAllocator` (free-list + version-bump-on-realloc) is implemented and exercised by recovery, but the writer mints slots via a `next_slot` atomic and live occupancy is read from redb — so the allocator's free-list reclamation is not on the live path (its unit tests are `#[ignore]`'d). The spec-mandated **slot version** is still enforced via the `MemoryId` encoding; only physical slot *reuse* is deferred.

These aren't bugs — they're scope boundaries. Don't accidentally implement them.

---

## Implementation history

`git log --oneline` shows the full landing history — what each phase delivered, deferrals, and bench results are captured in the commit log.

---

## How v1.0 gets cut

1. Run the in-repo acceptance gates (`.devcontainer/acceptance.sh`) plus the end-to-end scale-run + soak in the `brain-eval` rig (`brain-eval acceptance --scale 1m` / `soak`) on reference hardware.
2. Capture wall-time numbers for every operation against the targets in `spec/19_benchmarks/02_performance_targets.md`.
3. Execute the schema-toggle and disaster-recovery runbooks against a chaos scenario.
4. One final spec consistency pass — every cross-reference resolves, every numerical claim is harmonized.
5. Cut the `v1.0.0` tag from the same commit that contains the green acceptance log.

Once `v1.0.0` is out, the v1.x work above can land incrementally without wire/storage breaks; v2 work requires a new major version.
