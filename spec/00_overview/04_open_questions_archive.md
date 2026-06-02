# 00.04 Cross-cutting Open Questions Archive

Cross-section open questions deferred to v1.1+ or beyond. These are *known* unknowns: defensible choices were made for v1.0, but they warrant revisiting once operational data accumulates. This archive collects every open question — both cross-cutting and per-section — in one place.

Question IDs (`OQ-V2-N`, `OQ-23-X`, etc.) are stable; once assigned, they don't change so external references continue to resolve.

## OQ-V2-1: Learned vs rule-based query routing

**Current:** rule-based router (5 rules).
**Open:** train a learned router on labeled queries.
**Why deferred:** need real query traffic to label.
**Path:** future versions ships a learned router behind a feature flag; rules remain as fallback.

## OQ-V2-2: SPLADE-style sparse-neural retrieval

**Current:** BM25 only for lexical retrieval (tantivy).
**Open:** add SPLADE as a fourth retriever for sparse-neural matching.
**Why deferred:** inference cost equivalent to dense; gains modest; complexity-to-value is poor for the typed graph.
**Path:** later releases — evaluate against real queries.

## OQ-V2-3: Full bitemporal time

**Current:** valid time only (`valid_from`, `valid_to`).
**Open:** support as-of-transaction-time queries.
**Why deferred:** doubles per-statement storage cost; most users don't need it.
**Path:** future versions if users request; storage cost is the gate.

## OQ-V2-4: Multi-tenant schema isolation

**Current:** one schema per deployment; entities are global within the deployment.
**Open:** per-tenant schemas with isolated entity spaces.
**Why deferred:** affects sharding, query routing, ID spaces; substantial change.
**Path:** future major-version design discussion.

## OQ-V2-5: Statement derivation chains (meta-statements)

**Current:** statements can have statement IDs in their evidence, with depth cap 3. But active derivation rules are not in the typed graph.
**Open:** rule-based derivation engine ("if X reports_to Y, then Y manages X").
**Why deferred:** rule engines invite scope creep; the typed graph keeps extraction LLM-driven.
**Path:** future versions experimental.

## OQ-V2-6: Federated knowledge graphs

**Current:** single node.
**Open:** multi-node Brain with cross-node query.
**Why deferred:** Brain's value proposition is local-first; federation is a different system.
**Path:** future major-version, if a clear use case emerges.

## OQ-V2-7: Vector embeddings for relations

**Current:** entities have embeddings; statements have embeddings; relations don't.
**Open:** embed relations for "find similar relationships" queries.
**Why deferred:** unclear use case; cost of additional HNSW.
**Path:** future versions if requested.

## OQ-V2-8: Schema-as-code language choice

**Current:** custom DSL (the `schema.brain` format).
**Open:** alternative formats — YAML, TOML, or a Rust-embedded eDSL.
**Why deferred:** the custom DSL is readable and parseable; benefits of switching are marginal.
**Path:** community feedback decides.

## OQ-V2-9: Real-time extraction acknowledgment

**Current:** ENCODE returns once memory is written. Extraction happens after. Client doesn't know when extraction is done.
**Open:** option to wait for synchronous extraction completion in ENCODE.
**Why deferred:** synchronous LLM extraction in ENCODE breaks Brain's latency contract.
**Path:** future versions add ENCODE_AWAIT_EXTRACTION opcode that returns after pattern + classifier (skipping LLM).

## OQ-V2-10: External knowledge sources

**Current:** all knowledge derived from memories within Brain.
**Open:** import from external KGs (Wikidata, internal databases) as seed entities.
**Why deferred:** out of scope for Brain; users can ENCODE memories from external sources.
**Path:** future versions if users want first-class external KG bridges.

## OQ-V2-11: Active learning for ambiguous resolutions

**Current:** ambiguous resolutions queue for human review.
**Open:** Brain proposes resolutions when humans review, using an LLM, and learns from human corrections.
**Why deferred:** scope; involves training/feedback loops.
**Path:** future versions.

## OQ-V2-12: Cross-shard graph traversal

**Current:** entities and statements sharded by subject. Graph traversal within a shard is fast; cross-shard is fan-out.
**Open:** denormalized cross-shard adjacency for fast multi-hop.
**Why deferred:** complexity; depends on real workloads.
**Path:** future versions if metrics show cross-shard hops are common.

## OQ-V2-13: Statement merging when contradictions resolve

**Current:** contradictions are surfaced; user/agent resolves.
**Open:** auto-merge when contradictions have an obvious resolution (e.g., one is superseded).
**Why deferred:** auto-resolution risks silent data loss.
**Path:** future versions for high-confidence cases.

## OQ-V2-14: GUI for schema management and audit review

**Current:** wire protocol only.
**Open:** web-based admin UI.
**Why deferred:** out of scope.
**Path:** separate project / community.

## OQ-V2-15: Multi-language support in extractors

**Current:** built-in extractors assume English.
**Open:** multilingual NER, language-detection, per-language extractors.
**Why deferred:** built-in extractors are limited; users can ship their own LLM extractors that handle other languages.
**Path:** community-contributed extractors; future versions bundled.

## OQ-23-A: Streaming query results (`limit > 100`)

**Current:** the hybrid `QUERY` opcode returns a single `QueryResponse` frame, items truncated to `limit`.
**Open:** stream items over the SUBSCRIBE wire path as they pass the limit boundary, per spec §13/05 §"Streaming results".
**Why deferred:** v1 deployments are local-first with modest result-set sizes; the streaming path adds wire-protocol surface (event types) and client iterator plumbing that wasn't worth the complexity at v1.
**Path:** post-v1 — add a `QueryStream` event type on SUBSCRIBE; clients gain a `query()…stream()` pattern returning a `Stream<Item = QueryHit>`.

## OQ-23-B: query + transactional read-your-writes

**Current:** RECALL inside a txn falls back to Brain vector path even when a schema is declared. The hybrid pipeline doesn't see the txn buffer's pending statements / relations.
**Open:** layer the txn buffer's pending writes (entities, statements, relations) on top of the hybrid retriever outputs before fusion + filter.
**Why deferred:** lens layering for Brain's vector recall is bounded scope (one buffer, one corpus). Hybrid + RYW would need parallel lenses for the entity, statement, and relation tables, plus fusion logic that tolerates pending rows missing from secondary indexes (HNSW / tantivy commit cadence).
**Path:** post-v1 — design a per-table `TxnLens` shared by Brain and hybrid paths; phase ordering would put it after the §11 sweepers stabilise.

## OQ-23-C: Filter-only retriever mode (no text, no anchor)

**Current:** the planner rejects requests with neither `text` nor `entity_anchor` as `PlanError::NoSignal`. A filter-only query like "all preferences with confidence ≥ 0.9 in the last week" is not expressible.
**Open:** add an "everything" retriever (or a "filter scan" mode) that emits all candidates matching the pre-filter, then applies the post-fusion filter chain.
**Why deferred:** v1 didn't have a clear use case; "filter-only" is also a query class that benefits from a dedicated index design rather than reusing the hybrid pipeline.
**Path:** post-v1 — likely a new opcode or a planner-side filter-scan retriever; depends on how users land on filter-only patterns in practice.

## OQ-23-D: Learned router on top of the rule-based one

**Current:** rule-based router (5 rules) ships in v1; see also top-level `OQ-V2-1`.
**Open:** train a learned router on labeled query → preferred-retrievers data.
**Why deferred:** need real query traffic + labels. The rules ship as the stable fallback so cold start works.
**Path:** future versions — feature-flag a learned classifier on top; rules stay as fallback. Labels come from click-through, explicit feedback, and synthetic teacher-LLM labels (per §13/05 §"Learned routing").

## OQ-23-E: Cross-shard hybrid result merging

**Current:** the hybrid pipeline runs per-shard. Multi-shard deployments fan RECALL / QUERY out at the connection layer and merge results by score upstream of the hybrid engine.
**Open:** push the cross-shard merge into the query layer — global RRF fusion across shards, with per-shard partial results streamed in.
**Why deferred:** single-shard deployments are the v1 default; multi-shard with cross-shard hybrid fusion adds latency-budget pressure that's better tackled once production telemetry is in.
**Path:** post-v1 — extend the connection layer's fan-out to deliver per-retriever partial result lists, and fuse globally before the filter chain.


---

# Per-section open questions

Open questions scoped to individual sections, collected here from the per-section archives.


## §01_architecture open questions


This file lists architecture-level questions that are unresolved as of the current spec version. Surfacing them is preferable to hiding them. Each question states the issue, the options considered, and a current recommendation.

These differ from per-spec open questions (which appear in each detail spec): the questions here cut across the architecture, often involving multiple subsystems.

---

## OQ-1: TLB pressure on very large arenas

**Issue.** [`../01_architecture/05_hardware_and_targets.md`](../01_architecture/05_hardware_and_targets.md) §3.3 acknowledges that `MADV_HUGEPAGE` does not apply to file-backed mmaps on regular filesystems. For arenas exceeding ~16 GiB, TLB (Translation Lookaside Buffer) pressure becomes measurable: the CPU spends a non-trivial fraction of its time waiting for page-table walks instead of doing useful work.

**Options.**

a) **Accept it as a known overhead.** Document the issue, target shard sizes that stay under the threshold, scale by sharding rather than by growing single shards.

b) **Move the arena to `hugetlbfs`.** A separate filesystem dedicated to huge pages. Operationally complex: must be mounted, capacity must be reserved at boot, and we lose the standard filesystem features (snapshots via reflink, integration with backup tools).

c) **Wait for upstream large-folio readahead** to mature. Modern Linux kernels are improving file-backed huge-page support via "large folios" — the kernel can promote contiguous file pages to huge pages on its own. This is automatic, no application action needed. As of kernel 6.x, this is improving but not yet universal.

**Recommendation.** Defer. Target shard sizes ≤ 10M memories (≤ 15 GiB arena) so TLB pressure is bounded. Re-evaluate when measured TLB pressure becomes a documented bottleneck on real workloads.

---

## OQ-2: Replication

**Issue.** The first version assumes single-replica per shard. Loss of a node's storage means loss of its agents' memories until restored from snapshot. This is acceptable for many use cases (research, internal tools, medium-criticality deployments) but unacceptable for production with high-availability requirements.

**Options.**

a) **Synchronous WAL streaming.** Each WAL record is replicated to one or more peer nodes before the write is acknowledged. Strongest durability; latency cost (replication adds 1–10 ms to every `ENCODE`).

b) **Asynchronous follower replication.** WAL records ship to followers in the background; writes are acknowledged immediately. Eventual durability; followers may lag during high write rates.

c) **Read-replica only.** Replicas exist for read scaling (and disaster recovery) but writes go to a single primary. Simpler than multi-write; doesn't help with primary-loss durability.

**Recommendation.** Defer to a dedicated *Replication* spec (would be document 17 or higher). Slot it after v1 ships. The architecture is replication-friendly (per-shard WAL with LSNs is the right substrate for log shipping); the work is deciding the durability/latency trade-off for the default mode.

---

## OQ-3: Multi-modality

**Issue.** The architecture is non-modal at the storage and index layers, but the embedding layer and operations assume text. Multi-modal agent applications (image search, video memory, audio transcript memory) need image / audio / multi-modal embedding support.

**Options.**

a) **Single multi-modal model.** Replace `bge-small-en-v1.5` with a multi-modal model (CLIP-family, or larger multi-modal LLMs). Storage is unchanged; embedding layer becomes more complex.

b) **Multiple models in one process.** Configure Brain with multiple embedding models, each tagged by modality and identified by fingerprint. Memories carry their modality; cross-modal queries are explicit.

c) **Defer entirely.** Stay text-only in v1; revisit in v2 when the multi-modal embedding landscape stabilizes.

**Recommendation.** Defer entirely. Multi-modality requires more than just swapping the embedding model — modality-aware filtering, modality-specific salience, multi-modal `RECALL` semantics. Treat as a v2 milestone. The architecture is open to it (no v1 design choices preclude multi-modality), but the effort is substantial.

---

## OQ-4: Cross-agent operations

**Issue.** Brain's design assumes agents are isolated. Use cases for cross-agent operations exist (organizational memory, fleet learning), but they cut against sharding.

**Options.**

a) **Allow cross-agent queries.** Add an opcode that queries across multiple agents, federated across shards. Significant complexity in the query planner and execution engine; cross-shard latency on the hot path.

b) **Separate "shared" namespaces.** Agents subscribe to shared namespaces; each has its own shard. Memory written to a shared namespace is queryable by all subscribers.

c) **Application-level federation.** Don't add cross-agent at Brain level; let applications query multiple agents via separate connections.

**Recommendation.** Out of scope. Make cross-agent an explicit non-goal in [`../01_architecture/06_scope_and_comparison.md`](../01_architecture/06_scope_and_comparison.md) (already there). If a clear use case emerges with concrete latency tolerance, revisit.

---

## OQ-5: External vector ingestion

**Issue.** Should Brain accept pre-computed vectors from clients that have their own embedding pipelines (e.g., domain-specific or multi-modal models)? The architecture supports it (the protocol can carry a vector directly), but the operations assume embedding ownership.

**Options.**

a) **Support as a power-user override.** Default is text-in; an alternate `ENCODE_VECTOR_DIRECT` opcode lets advanced users pass vectors. Vectors carry their model fingerprint.

b) **Refuse external vectors.** Brain owns embedding entirely; clients without compatible models can't use Brain.

c) **Support fully.** Treat external vectors as first-class; the embedding layer becomes optional.

**Recommendation.** **Support as a power-user override (option a).** This is already in the spec ([03. Wire Protocol](../04_wire_protocol/00_purpose.md) §7.4). Default is text-in; advanced users have the override. The operations work with any vector that has a known model fingerprint.

---

## OQ-6: Vector quantization

**Issue.** The current spec assumes `f32` vectors (4 bytes per dimension, 1.5 KiB per 384-dim vector). `i8` quantization gives 4× density at modest recall cost. Implementation cost is moderate; benefits are workload-dependent.

**Options.**

a) **Implement `i8` quantization** as an alternate arena format. Per-shard configuration; quantized arenas are 4× denser but pay a small recall hit.

b) **Implement product quantization (PQ)** for even higher compression. Larger recall hit, larger implementation cost.

c) **Stay `f32`-only.** Simpler; recall quality is preserved.

**Recommendation.** Defer; revisit after first benchmark cycle reveals whether storage density is actually a problem.

---

## OQ-7: On-disk format change cadence

**Issue.** Each release pins one on-disk format version per file (arena, WAL, metadata). Bumping a format means every existing deployment runs `brainctl migrate` on upgrade. How often is too often?

**Options.**

a) **Bump aggressively.** Whenever a format change unlocks a meaningful win, take it. Operators absorb migration cost per release.

b) **Bump rarely.** Batch format changes into infrequent releases. Most releases ship without a migration step.

**Recommendation.** (b). Most releases should be migration-free; format bumps land in named "format-bump" releases that operators schedule.

---

## OQ-8: Procedural memory

**Issue.** v1 explicitly excludes procedural memory (skills, policies, executable patterns). Could it be added later?

**Options.**

a) **Treat procedural memory as a kind variant.** Add `Procedural` to the `MemoryKind` enum. Procedural memories store executable content (e.g., agent action templates).

b) **Build a separate substrate for procedural memory.** Different access patterns, different storage characteristics.

c) **Defer indefinitely.** Procedural memory belongs in the LLM's prompt or fine-tuning, not in a memory substrate.

**Recommendation.** Out of scope for the foreseeable future. Procedural memory in modern LLM stacks lives in tool-use schemas and prompt engineering; Brain shape Brain offers (vector + metadata + edges) doesn't naturally fit executable content. If a clear use case emerges, revisit.

---

## OQ-9: Multi-context membership

**Issue.** A memory belongs to exactly one context. Use cases exist for multi-context: a project-related insight that's also relevant to "lessons learned".

**Options.**

a) **Allow multi-context.** Memories carry a list of context IDs; bitmaps must support overlapping membership; recall scoring becomes more complex.

b) **Stay single-context.** Multi-context memories are encoded twice (once per context), accepting storage cost.

c) **Add tags.** A separate concept from contexts: lightweight, many-per-memory, used for soft filtering rather than primary scope.

**Recommendation.** Defer. The single-context model is a v1 simplification; if user feedback indicates contexts-as-tags would significantly help, revisit with option (c) — tags as a separate concept rather than expanding contexts.

---

## OQ-10: User-defined edge types

**Issue.** v1 ships with 8 edge types (`CAUSED`, `FOLLOWED_BY`, etc.). Should clients be able to register custom edge types with their own semantics?

**Options.**

a) **Fixed set in v1, expand in v2.** Lock the v1 set; revisit later.

b) **User-defined types from v1.** Adds complexity to the planner (knowing how to traverse novel edge types) and to consolidation (which edges to derive automatically).

c) **Generic "TAGGED" edge with user-supplied tag.** A fixed type, but the tag is user-defined. Treats user-defined edges as opaque; the planner doesn't reason about their semantics.

**Recommendation.** Ship v1 with fixed types. Add user-defined types in v2 with limited semantic interpretation (option c is the likely path).

---

## OQ-11: The cluster control plane

**Issue.** v1's cluster has a "stateless router". But routers need shard-to-node mappings, which need to be stored and updated as shards rebalance. Where does this state live?

**Options.**

a) **External coordination service.** Use [etcd](https://etcd.io/) or [Consul](https://www.consul.io/) for the shard-to-node mapping. Operationally simple if you already run one of these; adds a dependency if you don't.

b) **Built-in gossip protocol.** Nodes gossip the mapping among themselves; the router pulls from any node. No external dependency; more code to write and test.

c) **Static configuration.** Operator updates a config file on rebalance; routers reload. Simple, slow, manual.

**Recommendation.** This needs a dedicated decision in [12. Sharding + Clustering](../16_sharding/00_purpose.md). The architecture-level position: Brain is independent of the choice; the router needs *some* mapping; the choice is operational.

---

## OQ-12: Observability for operations

**Issue.** Standard observability (latency histograms, error rates, throughput) maps cleanly to operations. But operations have a less-standard observability story: was a `RECALL` "good"? Did the planner make a "good" choice? How would we know?

**Options.**

a) **Quality metrics on benchmark dataset.** Brain runs a periodic self-test against a fixed benchmark, reports recall quality, calibration error, etc. Useful for regression detection.

b) **Sampled traces with rich attributes.** Every Nth `RECALL` is fully traced, including planner decisions and intermediate scores. Useful for debugging specific issues.

c) **Quality signal from clients.** Clients optionally provide feedback (was this the right memory?). Substrate uses signals for calibration. Adds protocol surface; clients must instrument.

**Recommendation.** Need all three eventually. Specify benchmarks in [16. Benchmarks + Acceptance Criteria](../19_benchmarks/00_purpose.md); specify tracing in [14. Observability + Operations](../17_observability/00_purpose.md); defer client feedback signal to v2.

---

## OQ-13: Backup format vs runtime format

**Issue.** Snapshots today are reflinks of the runtime arena and metadata files. This makes restore fast (reflink back) but couples backup format to the runtime format — a backup taken on version N may not restore on version M.

**Options.**

a) **Status quo.** Snapshots are runtime-format-coupled. Cross-version restore requires re-reading and re-writing.

b) **Logical backup format.** A separate format optimized for portability and compression. Slower to take (full read of the source); restorable across versions.

c) **Both.** Reflink-based snapshots for fast same-version restore; logical-format export for cross-version transfer.

**Recommendation.** Status quo for v1. Add logical format in v2 if cross-version migration becomes a frequent operation.

---

## OQ-14: Cognitive primitives are right?

**Issue.** Brain commits to five cognitive primitives. Are these the right five? Are they too many? Too few?

**Options.**

a) **Trust the design.** The five chosen are well-grounded in cognitive science and we have implementation paths for each.

b) **Add more.** Possible candidates: `EVALUATE` (assign value/utility to a memory), `ASSOCIATE` (explicit edge construction without other operations), `CHRONICLE` (sequential episode demarcation).

c) **Reduce.** `REASON` and `PLAN` overlap significantly; could be unified.

**Recommendation.** Trust the design for v1. The primitives map onto distinct operational profiles (read-mostly, write, search, inference, deletion) and to distinct operations. If specific applications struggle to express what they need, revisit.

---

## OQ-15: Operational footprint

**Issue.** Brain depends on Glommio, candle, redb, hnsw_rs, and several smaller crates. Each is a maintenance dependency. Some are mature (redb, candle); some are smaller communities (Glommio, hnsw_rs).

**Options.**

a) **Trust the dependencies.** Each was chosen carefully; the alternatives are worse.

b) **Vendor critical pieces.** Fork Glommio and HNSW into our tree; absorb maintenance.

c) **Aggressively narrow.** Re-implement from first principles where the dependency is risky.

**Recommendation.** Trust the dependencies in v1. Track each one's health (maintenance activity, issue resolution) and revisit if any becomes a stagnant hazard. Vendoring is a fallback if a critical dependency stalls.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for further reading.*


## §02_data_model open questions


Data-model-level questions unresolved as of this spec version. These are narrower than the architectural open questions in [01.10](../00_overview/04_open_questions_archive.md); they concern the entities and their relationships specifically.

---

## DM-OQ-1: Multi-context membership

**Issue.** A memory belongs to exactly one context. Some applications would benefit from memories belonging to multiple contexts simultaneously (a project insight that's also relevant to "lessons learned").

**Options.**

a) **Stay single-context.** Multi-context memories are encoded twice. Storage cost is doubled; consistency is the application's problem.

b) **Allow multi-context.** Each memory carries a list of context IDs. Storage cost grows with context-count overlap. Filter evaluation is slightly more expensive but tractable.

c) **Add tags as a separate concept.** Tags are lightweight, multi-attach, used for soft filtering. Contexts remain single-attach for primary scope.

**Recommendation.** Defer. If user feedback indicates contexts-as-tags would significantly help, revisit with option (c) — tags as a separate concept rather than expanding contexts.

---

## DM-OQ-2: Soft contexts (boolean expressions over contexts)

**Issue.** `RECALL` accepts a context filter as a single id or a small set. Some applications want richer expressions: "in context A, but exclude memories also in context B" or "in any of contexts {A, B, C} but not D".

**Options.**

a) **Allow boolean expressions.** Define a small filter language. Adds complexity to the planner and to wire encoding.

b) **Stay with set membership.** Applications express complex filters at the application layer (multiple queries, post-filtering).

**Recommendation.** Stay with set membership in v1. Revisit if real workloads consistently need boolean expressions.

---

## DM-OQ-3: Memory provenance richness

**Issue.** A memory carries `source_request_id` (which encode created it) and `embedding_model_fp` (which model embedded it). These are minimal provenance. Some applications want richer trails: tool calls that produced the text, prior memory ids that informed the encode, etc.

**Options.**

a) **Add a generic `provenance: Map<String, Bytes>` field** allowing arbitrary key-value annotations.

b) **Add specific fields** like `produced_by_tool: Option<String>`, `informed_by: Vec<MemoryId>` — typed but not extensible.

c) **Use edges.** A memory's provenance is captured by `DERIVED_FROM` and `REFERENCES` edges; agents express provenance through edges.

**Recommendation.** Use option (c). Edges are the right abstraction for "what informed this memory". Adding provenance fields multiplies schema; using edges keeps the model uniform.

---

## DM-OQ-4: Vector storage precision

**Issue.** Vectors are stored as `f32`. Some workloads could tolerate `f16` (half precision; 2 bytes per element instead of 4) for 2× density at modest recall cost. `i8` quantization gives 4× density at larger recall cost.

**Options.**

a) **Single precision (f32) only.** Simplest; current spec.

b) **Optional `f16`.** Per-shard configuration; quantized arenas use 2 bytes per element.

c) **Optional `i8`.** Per-shard; 4× density.

d) **Product quantization (PQ).** More complex; better compression at higher cost.

**Recommendation.** Defer. Revisit after benchmarks reveal whether storage density is actually a problem for typical workloads.

---

## DM-OQ-5: Composite memories

**Issue.** A memory has one text and one vector. What if the agent wants to encode a structured observation: "user said X via channel Y at time Z while in state S"?

**Options.**

a) **Application encodes structured data as text.** Text is just JSON or natural language; agent parses it back.

b) **Add structured metadata fields.** A few well-defined fields beyond text: channel, modality, etc.

c) **Composite memories.** A memory has multiple "facets" — different texts, different vectors, different views.

**Recommendation.** Stay with single text + vector. Structured data is the application's problem; Brain operates on text.

---

## DM-OQ-6: Agent inheritance / hierarchy

**Issue.** Some applications would benefit from agents inheriting context: a "team agent" with shared memory, plus per-user sub-agents with private memory plus access to the team's.

**Options.**

a) **Out of scope.** Each agent's memory is fully isolated; sharing is the application's problem.

b) **Read-only links.** An agent can subscribe to read-only access to another agent's memories under specific contexts.

c) **Hierarchical agents.** First-class parent-child relationships.

**Recommendation.** Out of scope; this is application-layer composition. If a clear cross-agent sharing pattern emerges, revisit.

---

## DM-OQ-7: Soft delete recovery window

**Issue.** Soft-forgotten memories are recoverable until the slot is reclaimed. There's no explicit recovery window, no "undo" within X seconds. Should there be?

**Options.**

a) **Stay implicit.** Recovery is best-effort, depends on slot pressure.

b) **Add an explicit window.** Forgotten memories are guaranteed recoverable for N minutes; only after that does the slot become eligible for reuse.

c) **Add an explicit `UNDO_FORGET` operation** that restores a recently-forgotten memory.

**Recommendation.** Defer. Most agents don't need undo; the rare cases can be handled with snapshots.

---

## DM-OQ-8: Edge weight calibration

**Issue.** Edge weights are in [0, 1] but Brain doesn't calibrate them. A `CAUSED` edge with weight 0.7 from one agent and 0.7 from another mean different things if the agents' calibrations differ.

**Options.**

a) **Stay uncalibrated.** Weights are agent-specific; cross-agent comparison is the application's problem.

b) **Calibrate weights via observation.** If Brain sees that "agent A's 0.7 typically corresponds to outcomes similar to agent B's 0.5", apply a per-agent transformation.

**Recommendation.** Stay uncalibrated. Calibration would require ground-truth signal Brain does not have.

---

## DM-OQ-9: Memory text and vector coupling

**Issue.** A memory's vector is the embedding of its text. They're coupled. If text is updated (a typo correction), the vector should be re-computed.

**Options.**

a) **No text update.** Memories are immutable except for forgetting. Corrections are new memories with `REFERENCES` edges to the original.

b) **Text update with re-embed.** A new operation `UPDATE_TEXT` re-embeds and updates.

c) **Text update without re-embed.** Update text but leave vector. Corrupts the coupling invariant.

**Recommendation.** Option (a). Memories are immutable. Corrections are explicitly new memories. This is consistent with the immutability principle and avoids the temptation to mutate stored history.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for further reading.*

## Knowledge-model open questions

Decisions deferred at the typed-graph conceptual level.


## OQ-1.1: Should statements be sharded by subject entity or by extractor?

**Status:** Tentatively decided: shard by subject.

**Detail:** Brain shards memories by BLAKE3(agent_id). For statements, two options:

- Shard by subject EntityId: all statements about Priya are in one shard. Subject-filtered queries are local. Cross-subject queries (rare) fan out.
- Shard by extractor / source memory's shard: extractions are local to where the source memory lives. But entity-scoped queries fan out (common).

Decision: shard by subject. Aligns with the dominant query pattern. Cross-subject queries are rare and acceptable to fan out.

**To verify:** workload analysis once we have one. Easy to switch in future versions if wrong.

## OQ-1.2: What's the upper limit on evidence list length per statement?

**Status:** Tentatively decided: 64.

If a Fact has 200 supporting memories, Brain does not need to store all of them on the row. The query rarely needs more than ~10. Store top-K by recency or confidence; spill the rest to a side table (`statement_evidence_overflow`).

K = 64 default. Configurable per deployment.

**To verify:** real workload. If users routinely have hundreds of supporting memories, the overflow design must be fast.

## OQ-1.3: Are statements derivable from other statements?

**Status:** Yes, but limited.

A "meta-statement" can have other statements in its evidence list (`evidence_statements: Vec<StatementId>`). Use case: a rule-based extractor concludes "Priya manages Bob" from "Bob reports to Priya" + "reports_to is transitive."

**Constraint:** the evidence-statement chain has a maximum depth (default 3). Beyond that we refuse to derive, to prevent runaway transitivity.

**To verify:** is this needed in the typed graph, or can it wait until future versions? Lean toward future versions.

## OQ-1.4: Entity merge — what happens to existing statements?

When the user (or a resolver worker) decides that `entity_42` and `entity_77` are the same person:

- Choose a survivor (say `entity_42`).
- All statements with `subject = entity_77` are updated to `subject = entity_42`.
- All statements with `object = entity_77` are updated to `object = entity_42`.
- All relations with `from = entity_77` or `to = entity_77` are updated.
- `entity_77` is marked merged-into-`entity_42`; its row is kept as a redirect.

The mechanics are clear. Open question: do we *always* require the user to confirm merges, or can a high-confidence resolver merge autonomously?

**Tentative answer:** autonomous merging is allowed only if confidence ≥ 0.95 *and* the merge is reversible within a grace period (7 days). Lower confidence flags for human review.

## OQ-1.5: Entity split — is it supported?

If `entity_42` was incorrectly created as one entity but is actually two different people:

This is hard. Statements about `entity_42` need to be redistributed between the two new entities. Brain cannot do this autonomously — it requires per-statement disambiguation.

**Tentative answer:** entity split is a user-driven operation. The user invokes `SPLIT_ENTITY` with rules (which statements go where, perhaps by source-memory range). Brain executes the split as an atomic operation. Not high priority for the typed graph.

## OQ-1.6: How does a Memory's `agent_id` interact with Entity identity?

Memories belong to agents. Entities are referenced *by* agents in their memories. If agent A says "Priya is the manager" and agent B says "Priya is a junior engineer," are these the same Priya?

**Tentative answer:** Entities are global within a deployment (not per-agent). Two agents referring to "Priya" by name will resolve to the same Entity unless they explicitly disambiguate. This may cause cross-agent leakage and is a feature for the memory database use case (agents share an entity space). For deployments where per-agent isolation is required, use separate Brain deployments or wait for future versions multi-tenancy.

## OQ-1.7: Statement object types — what's the union?

Object types currently proposed:

```rust
enum StatementObject {
    Entity(EntityId),
    Value(serde_json::Value),  // typed literal (string, number, bool, struct)
    Memory(MemoryId),          // "evidenced by" type relations
    Statement(StatementId),    // meta-statements
}
```

Open: do Brain requires a `List(Vec<StatementObject>)` variant for "Priya likes [tea, coffee, espresso]"? Or do users express this as three statements?

**Tentative answer:** three statements. Single-object simplicity. If users need list semantics often, we revisit in future versions.

## Entity open questions

### E-OQ-1: Cross-shard merge coordination

Entities on different shards (multi-shard deployment) need a coordinated commit for merge. Two strategies:

(a) **Two-phase commit** across the affected shards. Holds locks across network hops; sensitive to partial failure.
(b) **Authoritative shard for the survivor** — survivor's shard runs the merge; the merged entity's shard is notified async and updates its row + redirects. Eventually consistent; queries through the merged id during the window may not yet see the redirect.

**Recommendation.** (b) for the v1.0 simplicity argument; (a) if benchmarks show the consistency window is unacceptable.

### E-OQ-2: Multi-hop unmerge ordering

For a chain `merged → survivor → other`: should unmerge of `merged` be allowed if `survivor` is itself merged?

Current spec: reject unless upstream is unmerged first. Operators do nested unmerges.

Alternative: allow, and re-route any statements that `survivor → other` took from the `merged → survivor` step. Complex; error-prone.

**Recommendation.** Keep the rejection; force operator-driven ordering.

### E-OQ-3: Cross-type merge

Should merging entities of different `entity_type` be allowed? Current v1.0 stance: forbidden (`ENTITY_TYPE_MISMATCH`).

Use case: extraction misclassified an entity. The fix is currently: `ENTITY_TOMBSTONE` the wrong one, `ENTITY_CREATE` a fresh one in the correct type, re-extract or manually re-author statements. Painful.

Alternative: allow cross-type merge with attribute drop (attributes valid in source type but not destination type are dropped).

**Recommendation.** Defer past v1.0.

### E-OQ-4: Reverse-merge semantics

After `merged → survivor` was merged and then unmerged, can the operator re-merge them?

Currently: yes. Each merge is a fresh audit row.

Alternative: track repeated cycles; refuse after N (defaults: maybe 3) to prevent thrashing.

**Recommendation.** Stay permissive; trust the operator.

### E-OQ-5: `mention_count` truthfulness

GC eligibility recomputes `mention_count` because the stored counter may be stale (e.g. memories were forgotten without decrementing).

Should the stored `mention_count` be **maintained eagerly** (every `STATEMENT_CREATE` / `STATEMENT_TOMBSTONE` / `FORGET_MEMORY` updates it) or **lazily** (recomputed periodically)?

- Eager: simpler queries that need an accurate count, but write amplification on every memory / statement op.
- Lazy: cheap writes, but consumers must treat the stored count as approximate.

**Recommendation.** Lazy with a periodic reconcile worker.

### E-OQ-6: Embedding refresh policy

When an entity's `canonical_name` changes (rename or merge contribution), `embedding_version` bumps and the embedding worker eventually re-embeds. **When** does the worker run?

Options:

(a) On a schedule (e.g. every minute, scan for entities with `embedding_version > current_embedded_version`).
(b) Event-driven (subscribe to entity events, re-embed reactively).
(c) On read (lazy — re-embed when the resolver actually needs the embedding for this entity).

**Recommendation.** (a) for predictability; (c) is too query-time-expensive.

### E-OQ-7: Attribute conflict policies per attribute vs per type

Merge currently lists 5 conflict policies (`survivor_wins`, `merged_wins`, `newest_wins`, `concat_text`, `reject_merge`). The policy is **per entity type** (declared in the type's schema definition).

Should it be **per attribute**? E.g. for `Person`: `name = survivor_wins`, `bio = concat_text`, `email = reject_merge`.

**Recommendation.** Per-attribute granularity in schema DSL.

### E-OQ-8: Resolver tier 4 (LLM) integration with merge

The resolver's tier 4 is a stub today. When the LLM tier is wired up, it may suggest merges with confidence in the auto-merge band. Should auto-merge from tier 4 be:

(a) Allowed (auto-merge if confidence ≥ 0.95).
(b) Always queued for human review regardless of confidence (LLM-sourced merges are inherently lower-trust).

**Recommendation.** (b) for safety; reconsider after empirical accuracy data.

### E-OQ-9: GC-driven entity hard-delete and privacy

The wire protocol intentionally has no immediate hard-delete opcode. Privacy / GDPR / "right to erasure" requests are operator-driven offline operations.

Is this sufficient? Or should there be an `ENTITY_RETRACT` opcode (analogous to `STATEMENT_RETRACT`) that immediately tombstones and queues for accelerated reclamation?

**Recommendation.** Defer past v1.0; discussion may move to a compliance section once one exists.

### E-OQ-10: Statement / relation re-routing sweep

Statement / relation re-routing during merge needs a **retroactive sweep** to re-route any rows that referenced now-merged entities and update the `entity_merge_log` audit overflow lists.

- Does the sweep run **eagerly at startup** (one-time pass over all existing merge_log rows), or **lazily** (audit overflow lists populated on next read)?
- If eager: who owns the sweep — a one-shot migration helper, or the regular consolidation worker?
- What happens if a merge issued before the rerouting code exists is unmerged after but before the sweep runs? The unmerge code path must handle a partially-populated audit gracefully.

**Recommendation.** Eager one-shot sweep at first startup, gated by a schema-version sentinel in the metadata.

### E-OQ-11: Concurrent merge and re-route race

Two operators issue `ENTITY_MERGE(A, B)` and `ENTITY_MERGE(B, C)` concurrently. The first commits then the second's pre-condition `merged.merged_into.is_none()` fails. Operator B's merge returns `ENTITY_MERGE_CONFLICT`.

Is this the right UX? Alternative: silently chain the merge (`(A → B → C)` becomes `(A → C)` directly). Current spec: reject; operator retries with `(A, C)`.

**Recommendation.** Keep the rejection; the cleaner failure mode beats the silent chain.

### E-OQ-12: Re-merging after grace expires

After the merge's grace period expires (`finalized = 1`), can the same `(survivor, merged)` pair be re-merged?

The merged entity's `merged_into` is still set, so the merge attempt would fail the `merged.merged_into.is_none()` pre-condition. Two options:

(a) Reject — operator must manually create a new entity to merge with.
(b) Allow — treat post-grace as a clean state; new merge audit row written.

**Recommendation.** (a) — keep merges idempotent against their audit row.

## Statement open questions

### S-OQ-1: Discrete `STATEMENT_CONTRADICTED` event

Contradictory Facts emit only the per-create `STATEMENT_CREATED` event. Consumers that want contradiction notifications must run their own check after each create.

Should Brain emit a discrete `STATEMENT_CONTRADICTED` event when `statement_create` detects a new contradiction? V1.0 doesn't include this — keeps the event surface minimal — but query / monitoring use-cases would benefit.

**Recommendation.** Add the event.

### S-OQ-2: `STATEMENT_LIST` `only_consistent` filter

Should `STATEMENT_LIST` support a filter that excludes subjects with contradictory Facts? V1.0 ships without.

Use case: "show me Priya's consistent facts; flag the contradictory ones separately."

**Recommendation.** Add as part of the query DSL rather than a `STATEMENT_LIST` filter.

### S-OQ-3: Confidence recomputation at read time

Stored confidence is a snapshot at last touch. Pure age-based decay is **not** applied at read.

Should reads optionally recompute? A `recompute_at_read: bool` flag on `STATEMENT_LIST` and `QUERY` would let cost-sensitive callers stay with snapshots while accuracy-sensitive callers get fresh confidence.

**Trade-off.** Read latency vs accuracy.

### S-OQ-4: Lazy confidence sweep worker

A periodic worker may recompute confidence for aging statements. Open: which trigger?

- (a) **Time-based:** every N hours, scan statements with `confidence > 0.1` and `last_touched_at_unix_nanos < now - 30 days`, recompute.
- (b) **Read-driven:** on each `STATEMENT_GET` of a statement older than threshold, queue it for recomputation; worker drains the queue.

**Recommendation.** (a) — simpler ops story.

### S-OQ-5: `STATEMENT_ADD_EVIDENCE` opcode

Evidence is set at create / supersede time only in v1.0. A separate op to append evidence (without superseding) would be useful for extractor pipelines that observe the same claim repeatedly.

**Recommendation.** Add as `STATEMENT_ADD_EVIDENCE` and route through `statement_ops`.

### S-OQ-6: Multi-chunk evidence overflow

V1.0 supports a single overflow row per statement (up to 1000 evidence entries). Statements with > 1000 evidence need multi-chunk overflow.

Use cases: aggregated long-lived Preferences ("Priya prefers async meetings" with 10,000 supporting conversations).

**Recommendation.** Chained `EvidenceOverflow` rows with `next_chunk_id`.

### S-OQ-7: Contradiction down-weighting

Should contradictory Facts down-weight each other? Current spec: each carries its own confidence independently; contradictions are surface signal, not confidence input.

Alternative: divide each contradicting Fact's confidence by the number of contradicting peers. Models "agreement reduces credibility."

**Recommendation.** Stay independent; consumers weight at query time.

### S-OQ-8: Per-predicate decay overrides

Kind-level decay defaults only in v1.0. Per-predicate overrides would let `manages` (slowly changing) decay slower than `prefers_format` (fast-changing) within the Preference kind.

**Recommendation.** Add as `decay_half_life_seconds` predicate metadata in the schema DSL.

### S-OQ-9: Auto-tombstone on critical evidence loss

When FORGET cascade reduces a statement's evidence to a single low-confidence entry, should Brain auto-tombstone with `SourceMemoryForgotten`?

**Trade-off.** Aggressive cleanup (tidy graph) vs preserving evidence for audit (history matters).

**Recommendation.** Make it configurable via deployment config; default conservative.

### S-OQ-10: Cross-shard `statements_by_object_entity` consistency

A statement with cross-shard `subject` and `object` writes its `by_object_entity` index entry to the object's shard. The two writes (primary on subject's shard, reverse-index on object's shard) must coordinate.

V1.0 ships best-effort: primary writes succeed; reverse index updates asynchronously. Brief inconsistency window during which "what statements have X as object?" may miss a just-created statement.

**Recommendation.** Acceptable for v1.0; document the window.

### S-OQ-11: `STATEMENT_LIST` cursor pagination

V1.0 ships single-frame snapshot with limit cap 1000. Cursor pagination deferred.

### S-OQ-12: Statement-on-statement (meta-statements)

`StatementObject::Statement(StatementId)` is allowed by the type system — meta-statements like "Statement S1 was authored by Priya."

Open: when meta-statements form cycles (S1 references S2 which references S1), how do reads handle? V1.0 stores them as-is; cycle detection is a query concern.

**Recommendation.** Detect and reject cycles at create time in a future hardening pass; until then, query consumers must guard.

### S-OQ-13: Preference de-duplication

When `statement_create` for a Preference matches an existing current Preference exactly (same subject, predicate, object, evidence): is it a duplicate (return existing) or a supersession (chain extends with the new)?

V1.0 implementation: **supersession with new evidence merged**. This is consistent with re-affirmation as a confidence boost.

Alternative: **silent duplicate suppression** — return existing without new chain entry.

**Recommendation.** Supersession (re-affirmation is real signal).

## Relation open questions

### R-OQ-1: Discrete `RELATION_CARDINALITY_CONFLICT` event

`OneToOne` two-sided conflict errors at `relation_create`. The error surfaces to the caller but is otherwise invisible to monitoring.

Should Brain emit a discrete `RelationCardinalityConflict` event distinct from the standard error path? V1.0 doesn't — errors are sufficient. Monitoring use cases that need event-stream visibility into cardinality violations would benefit.

**Recommendation.** Add the event.

### R-OQ-2: Bulk-mode cardinality skip

Cardinality is checked at every `relation_create`. For bulk extractor backfills running millions of creates, the per-create lookup is the dominant cost.

Should the wire request carry a `skip_cardinality_check: bool` flag for bulk imports? Caller takes responsibility for cardinality correctness; a post-import sweep verifies.

**Recommendation.** Add as `RelationCreateRequest.skip_cardinality_check`, requires admin permission.

### R-OQ-3: Symmetric deduplication on create

Two `discussed_with(A, B)` rows with different `topic` properties currently coexist. Some operators expect symmetric ManyToMany to dedupe by `(canonical_from, canonical_to)`.

**Recommendation.** Predicate-level config: `dedup_by_endpoints: bool`. Default false (preserve property diversity).

### R-OQ-4: Path-edge vs terminal-set TRAVERSE response

V1.0 returns full path metadata (relation_id, from, to, type at each step). Some queries ("set of entities reachable within 3 hops") only need the terminal set; returning paths is wasteful.

**Recommendation.** Add `RelationTraverseRequest.return_paths: bool`. Default true.

### R-OQ-5: `RELATION_RETRACT` opcode

V1.0 doesn't ship a hard-delete path for relations. Operators wanting to permanently remove a relation (privacy compliance, mis-extraction cleanup) must tombstone + wait for the (non-existent) GC sweeper.

**Recommendation.** Add `RELATION_RETRACT` mirroring `STATEMENT_RETRACT`; GC worker handles physical reclamation.

### R-OQ-6: Cross-shard TRAVERSE coordination

Deep traversal across shards needs an inter-shard coordination mechanism. V1.0 ships same-shard only; queries that need to cross shard boundaries either fan-out via the planner or return early.

**Recommendation.** Planner spawns per-shard sub-traversals and unions results client-side or in the router.

### R-OQ-7: Weight-aware shortest-path traversal

Relations carry `confidence`. A natural extension is "shortest path weighted by `1 / confidence`" — find the most-supported route between two entities. V1.0 BFS treats every edge as weight 1.

**Recommendation.** Optional `RelationTraverseRequest.weight_by: ConfidenceMetric` enum in a future hardening pass.

### R-OQ-8: FORGET-cascade auto-tombstone configurability

When FORGET removes a relation's last evidence, the v1.0 default tombstones the relation. Operators wanting "preserve as low-confidence" need a config knob.

**Recommendation.** Deployment-level config `brain.relation.cascade.tombstone_on_zero_evidence: bool`. Default true.

### R-OQ-9: Entity-merge relation re-routing

When entity A is merged into B (`ENTITY_MERGE`), relations citing A as `from` or `to` should logically re-route to B.

V1.0 ships without the re-route path — relations citing the merged-away entity become orphaned (the entity is tombstoned, the relation still references its tombstone).

**Recommendation.** Add `relation_reroute_on_merge` in a worker that subscribes to `EntityMerged` events.

### R-OQ-10: `relations_by_type` index

The per-type index is deferred. Admin queries "list all current `manages` relations" currently scan `RELATIONS_TABLE` filtered by type, O(N).

**Recommendation.** Add when an admin opcode needs the scan to be O(log N).


## §03_schema open questions


Schema-DSL-specific open questions. Wire-shape questions live in
[`..../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

## Active

### Q1 — `Schema.parent_version` for diff computation

[`./02_ast.md`](../03_schema/02_ast.md) §1: `Schema` doesn't carry a
parent-version pointer. Adding one would let the migration
planner (post-v1) diff against the previous version
cleanly.

**Target:** post-v1. **Status:** deferred per v1 no-migration
scope.

---

### Q2 — Multi-document schemas per namespace

A user might prefer to split a 200-definition schema across
multiple files (one per concern: people, projects, events). v1
requires single-document uploads. `use other_namespace;` imports
or multi-file `SCHEMA_UPLOAD` payloads land post-v1.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
add a `Vec<SchemaDocument>` to `SchemaUploadRequest` with merge
semantics.

---

### Q3 — Migration plan computation

[`./05_versioning.md`](../03_schema/05_versioning.md) §3: v1 validator runs
**structural** checks only. It does NOT compute deltas between
schema versions, doesn't reject removals of types with live
entities, doesn't enumerate which statements need re-extraction.

**Target:** v1.1+ (post-first-deployment). **Status:** explicitly
deferred per project scope. The v1 deployment has no existing
data to migrate; introducing migration semantics now would be
speculative and over-fit to imagined needs.

When this lands:

- Validator gets a `parent: &ValidatedSchema` arg and emits
  `MigrationStep` items for each affected resource.
- `MigrationPlan` is computed at validate time and surfaced to
  the operator via `SCHEMA_UPLOAD.dry_run = true`.
- A migration worker executes the plan against live
  data.

---

### Q4 — Warnings vs errors split

[`./03_validator.md`](../03_schema/03_validator.md) §4: v1 treats every
validation issue as an error. Some checks (e.g., "this attribute
should probably be a relation") are advisory; gating uploads on
them is overzealous.

**Target:** **Status:** open. **Likely outcome:** add
`ValidationWarning` distinct from `ValidationError`; uploads
succeed with warnings.

---

### Q5 — Custom validation rules / plugins

Some deployments may want domain-specific rules ("our predicates
must follow `noun_verb_object` naming"). v1 ships fixed rules.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
declarative rule extension via a `validation_rules` section in
the schema document.

---

### Q6 — Cross-namespace references in schema documents

[`./04_namespaces.md`](../03_schema/04_namespaces.md) §2: v1 forbids
qualified references like `from: crm:Person` from inside a
schema document. This rules out shared-type patterns.

**Target:** **Status:** open. **Likely outcome:** add
`use crm.Person;` imports with explicit version pinning.

---

### Q7 — Cross-namespace traversal filter syntax

[`./04_namespaces.md`](../03_schema/04_namespaces.md) §5: `RELATION_TRAVERSE
.relation_types: Vec<String>` requires the caller to enumerate
relation types if they want only one namespace's. A
`namespace:*` wildcard would help.

**Target:** **Status:** open.

---

### Q8 — Namespace renaming

Operators may want to rename a namespace post-deployment (e.g.,
`acme` → `acme_corp`). v1 doesn't support this — namespace is
part of every type id's qname and is on disk.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
admin opcode that rewrites all rows under the old namespace,
heavyweight but doable.

---

### Q9 — Schema deletion / rollback

Can a deployment "delete" a namespace, or "roll back" to an
earlier version? v1 says no — schema versions are append-only
audit history. Operators wanting cleanup must do so manually via
direct redb access.

**Target:** post-v1. **Status:** deferred. **Likely outcome:**
hard-delete admin opcode with require-confirmation flag.

---

### Q10 — Validator-version evolution

[`./05_versioning.md`](../03_schema/05_versioning.md) §2.1: `SchemaVersionRow`
carries a `validator_version: u32`. When the validator's rules
change (e.g., add a new check), previously-uploaded schemas may
now fail re-validation.

**Target:** post-v1. **Status:** open. **Likely outcome:**
running schemas under their original validator version (read-only)
+ requiring re-upload through the new validator to upgrade.

---

### Q11 — Binary-bootstrap migration for system schema

[`./06_system_schema.md`](../03_schema/06_system_schema.md) §2: when a new
binary ships with a changed system schema (added type / changed
description), the deployment doesn't auto-upgrade. Existing
`brain` namespace at version 1 stays.

**Target:** v1.1+. **Status:** deferred. **Likely outcome:** add
a binary-bootstrap migration path that detects the diff at
`MetadataDb::open` and emits a "system schema mismatch — run
`brain admin migrate-system-schema`" warning.

---

### Q12 — Should system schema be queryable via SCHEMA_GET?

[`./06_system_schema.md`](../03_schema/06_system_schema.md) §6: yes — the
read path doesn't distinguish `brain` from user namespaces; only
upload is gated.

**Status:** resolved by §06 §6.

---

### Q13 — Derive macros + their generated schema contributions

An earlier [client interface](../06_sdk/00_purpose.md) sketch
listed `#[derive(BrainEntity)] / BrainFact /
BrainRelation` macros. These auto-generate trait
impls + a static schema fragment per type.

**Status:** open. **Risk:** proc macros are a large new surface.

---

### Q14 — Pest vs hand-rolled parser

[`./01_grammar.md`](../03_schema/01_grammar.md) §"Parser implementation
choice" prefers `pest`. Brain uses pest 2.7. Alternative:
hand-rolled recursive-descent, smaller dep tree.

**Status:** provisional pest.

---

### Q15 — §02/11 relation wire spec vs §03/05 truth

[`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md)
was authored before this section's no-migration directive landed.
Several §02/11 relation fields encode migration semantics that v1 doesn't
implement:

| §02/11 relation field | v1 behaviour |
|---|---|
| `SchemaUploadRequest.allow_breaking` | Accepted and ignored. |
| `SchemaUploadResponse.migration_summary` | Replaced with `migration_summary_blob: Vec<u8>` (empty). |
| `SchemaGetRequest.version_id` (no namespace) | Extended with a `namespace: String` field. |
| `SchemaUpdatedEvent` | Carries `namespace: String`. |
| `SchemaListRequest.cursor` | Accepted and ignored (single-frame). |
| Admin authorization (§02/11 relation §8) | Open access — admin auth lands separately. |

When §02/11 relation is next edited, fold these resolutions back. None of
the spec divergences alter the wire opcode table.

**Status:** resolved.

## Resolved

- Q12 — System schema queryable via `SCHEMA_GET` (§06 §6).
- **Extractor fan-out from `SchemaItem::Extractor`** — user-declared
  `define extractor` blocks register into the `EXTRACTORS_TABLE` at
  schema-apply time. Built-in extractors load via the same path
  through the system schema. Spec: [`../11_extractors/`](../11_extractors/00_purpose.md).


## §04_wire_protocol open questions


Wire-protocol-level questions unresolved as of this spec version.

---

## OQ-WP-1: HTTP/2 or QUIC mapping

**Issue.** The protocol is currently TCP-only with custom framing. Wrapping or co-implementing it over HTTP/2 (or QUIC for HTTP/3) would help with deployment in environments that need HTTP-style routing, and could leverage HTTP-aware load balancers.

**Options.**

a) **Stay TCP.** Operators wanting HTTP routing put a TCP-aware proxy in front (Envoy with TCP routing rules, HAProxy in TCP mode).

b) **Add an HTTP/2 frame mapping.** Each Brain frame becomes an HTTP/2 stream message. Adds protocol surface area and dependencies.

c) **Run alongside.** TCP for primary deployment; HTTP/2 mapping for environments that require it. Both supported.

**Recommendation.** Stay TCP for v1. Revisit if user demand for HTTP/2 emerges. The TCP design is intentional and the latency floor benefits from not paying HTTP/2 framing overhead.

---

## OQ-WP-2: Compression

**Issue.** Frame payloads are uncompressed. Some payloads (long text in ENCODE, large RECALL responses with full content) could benefit from compression. Trade-off: CPU cost vs network bandwidth.

**Options.**

a) **No compression.** Simplest. Bandwidth cost grows linearly with corpus size for large RECALL responses.

b) **Per-frame zstd.** Each frame independently compressed. Negotiated via feature flag at handshake.

c) **Stream-level compression.** Long-running streams (SUBSCRIBE) get a dedicated compression context that improves ratios over time.

**Recommendation.** Defer. For typical agent traffic (small frames, frequent), the wire-format overhead is small and compression's per-frame setup cost is comparable to the savings. If we observe a deployment where frames are large enough to benefit, add a feature flag for per-frame zstd.

---

## OQ-WP-3: Server-initiated push beyond SUBSCRIBE

**Issue.** Currently the server only initiates frames on existing client streams (responses) or on subscribed streams. Use cases for unsolicited server push exist:

- "The model has been migrated; please re-issue your last query."
- "A long-running PLAN is making progress; here's a status update."
- "Server is shutting down; please disconnect gracefully."

**Options.**

a) **Even-numbered stream IDs for server push.** Reserve them now for v2 use; document them as forbidden in v1.

b) **Use existing PING/PONG for liveness; add server-push only as needed.** Simpler; revisit when use cases solidify.

c) **Use SUBSCRIBE for everything.** Define a "server-events" subscription that delivers operational notifications.

**Recommendation.** Reserve even-numbered stream IDs (option a). Don't implement server-push in v1; the reservation is cheap and enables future addition without a wire-version bump if the additions fit in the existing frame format.

---

## OQ-WP-4: Out-of-order frame delivery for streaming responses

**Issue.** Streaming responses (RECALL, PLAN, REASON) currently emit frames in order. For PLAN and REASON, results may be discovered out of order (different search branches complete at different times). The server currently serializes them; a parallel-emission mode could deliver faster.

**Options.**

a) **Strictly ordered.** Status quo. Simpler; results arrive as they're computed and serialized.

b) **Optional out-of-order.** A flag at request time enables out-of-order delivery; each result frame carries a sequence number for client-side reordering.

c) **Multiple parallel streams.** Use stream multiplexing — the operation opens multiple stream IDs, one per branch. Adds complexity for marginal gain.

**Recommendation.** Defer. Streaming order is rarely the bottleneck; embedding and search latency dominate. If profiling shows ordering is meaningfully delaying responses, revisit option (b).

---

## OQ-WP-5: Wire-level encryption beyond TLS

**Issue.** TLS protects in-flight bytes but doesn't protect against compromise of the server itself or from a malicious operator. Some deployments may want client-side encryption of sensitive memory content.

**Options.**

a) **Application-level encryption.** Clients encrypt the text before ENCODE; decrypt after RECALL. Brain stores ciphertext. Brain's embedding model produces vectors from the ciphertext (which is meaningless), so similarity search doesn't work.

b) **Searchable encryption schemes.** Cryptographic protocols (homomorphic encryption, structured encryption) that allow similarity search over encrypted vectors. Significant performance cost; complex; immature.

c) **Server-side enclave.** Run the embedding and search inside a trusted execution environment (Intel SGX, AWS Nitro Enclaves). Hardware-bound.

d) **Don't address it at the wire level.** Leave encryption to the application; document that Brain operators see plaintext.

**Recommendation.** Option (d) for v1. Encryption beyond TLS is a hard problem that requires deeper architectural support; the wire protocol is the wrong layer to solve it.

---

## OQ-WP-6: Bidirectional flow control

**Issue.** TCP provides byte-level flow control via the receive window. Brain's protocol doesn't have application-level flow control beyond that. For high-throughput streaming responses, the client may want to signal "slow down, I'm a slow consumer".

**Options.**

a) **Rely on TCP flow control.** Status quo. The client doesn't read; TCP window narrows; server stalls.

b) **Application-level credits.** Each stream has a credit count; the client grants credits as it processes; the server pauses when out of credits.

c) **Backpressure-aware streaming.** Server emits a frame, waits for ACK, emits next.

**Recommendation.** Option (a) for v1; TCP flow control is sufficient for the workloads we expect. If we observe streams where consumer-side latency varies wildly and the server's emission rate matters, revisit option (b).

---

## OQ-WP-7: Client identity in multi-tenant deployments

**Issue.** The handshake authenticates a session to an `agent_id`. Some deployments may have multiple agents per session (an admin tool that operates across agents). The current protocol requires one agent per connection.

**Options.**

a) **One agent per connection.** Status quo. Admin tools open multiple connections.

b) **Multi-agent sessions.** AUTH establishes a "principal" who can speak as multiple `agent_id`s. Each operation specifies the agent. Validation checks the principal's authorization for that agent.

c) **Principal + impersonation.** A principal authenticates; subsequent operations may include an "impersonate" agent_id, which is checked at the authorization layer.

**Recommendation.** Status quo for v1. The added complexity isn't justified for the workloads we're targeting. Operations admin tools that span agents should use multiple connections — the cost is small.

---

## OQ-WP-8: ALPN identifier

**Issue.** When TLS-wrapped, Brain doesn't currently advertise an ALPN identifier. Some load balancers and routers can use ALPN to decide where to send a connection.

**Options.**

a) **Define an ALPN string.** Suggest `"brain/1"`. Servers advertise it; clients can include it in their ALPN list.

b) **Don't bother.** Brain's TCP port is dedicated; ALPN is unnecessary.

**Recommendation.** Option (a). Define `"brain/1"` as the ALPN string for wire version 1; future versions get `"brain/2"` etc. Cost is negligible; benefit is meaningful for operators using ALPN-aware infrastructure.

---

## OQ-WP-9: Stream cancellation finer than frame-level

**Issue.** Currently, stream cancellation is a frame-level operation (the client sends a CANCEL frame). For very long-running operations (PLAN, REASON), the cancellation latency depends on the server's ability to interrupt the running task.

**Options.**

a) **Status quo.** CANCEL is best-effort; the server interrupts at convenient checkpoints.

b) **Hard cancellation.** CANCEL forces a kill; partial state is discarded.

c) **Cancellation tokens.** Operations periodically check a per-stream cancellation token; CANCEL flips the token; operations notice within a bounded time.

**Recommendation.** Option (c) is the right shape. Detail it in [10. Concurrency + Epoch Model](../14_concurrency/00_purpose.md) — it's a runtime concern, not a wire concern. The wire protocol's CANCEL frame stays as-is; the server's response to it gets richer.

---

## Typed-graph open questions

Open items specific to the `0x01xx` (typed-graph) opcodes. Each has a rough-target phase and a status. Resolved items move to the bottom.

### OQ-WP-K1 — Strategy A vs B for typed-graph error-code wire shape

The errors file ([`../04_wire_protocol/07_error_handling.md`](../04_wire_protocol/07_error_handling.md) §3.10) describes two strategies for surfacing typed-graph error codes in Brain ERROR frame:

- **Strategy A** — extend substrate `ErrorCodeWire` with new variants. Long-term plan.
- **Strategy B** — interim fallback that maps typed-graph errors onto closest existing substrate codes (in code at _(later)_).

Question: when does `ErrorCodeWire` get the typed-graph variants?

**Status:** open.

### OQ-WP-K2 — Schema fan-out coordination

Brain uses the authoritative-shard-0 coordination strategy for multi-shard `SCHEMA_UPLOAD` (see [`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md) §"Multi-shard schema state"). The target consistency window is ≤ 100ms.

**Status:** resolved.

### OQ-WP-K3 — Cross-type entity merge

[`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) — should merging entities of different `entity_type_id` be allowed? Default v1.0 stance: forbidden (returns `EntityTypeMismatch`).

If allowed in a later version: how are type-specific attributes resolved? Drop them? Migrate to the survivor type?

**Status:** open. **Likely outcome:** stay forbidden.

### OQ-WP-K4 — Retroactive event emission for pre-existing entities

SUBSCRIBE event-emission wiring (see [`../04_wire_protocol/06_streaming.md`](../04_wire_protocol/06_streaming.md)). Should it retroactively emit `ENTITY_CREATED` for entities created via the `ENTITY_CREATE` opcode before event emission was wired?

Default leaning: **no** — events are forward-only from their introduction. Clients that need a backfill use `ENTITY_LIST`.

**Status:** open. **Likely outcome:** no retroactive emission.

### OQ-WP-K5 — `move_to_alias = false` rename semantics

[`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md): the `move_to_alias` flag is wire-stable but the handler currently rejects `false`. What does a "no-trail" rename mean semantically?

Interpretation: **drop the old name entirely.** New canonical, no alias trail. Loses query-ability by the old name.

**Status:** open.

### OQ-WP-K6 — Aliases and Unicode confusables

[`../04_wire_protocol/07_error_handling.md`](../04_wire_protocol/07_error_handling.md) §4.9: aliases are deduplicated on `normalize_name` (lowercase + whitespace collapse). Should that also apply NFKC normalization to collapse Unicode confusables (e.g. Cyrillic "а" vs Latin "a")?

**Status:** open. **Trade-off:** correctness vs surprise (NFKC can alter visible characters).

### OQ-WP-K7 — `attributes_blob` schema-aware validation on the wire

[`../04_wire_protocol/07_error_handling.md`](../04_wire_protocol/07_error_handling.md) §4.9: `attributes_blob` is opaque at the wire layer. Should Brain push schema validation into the wire layer (decode the blob, check against the entity type's attribute schema, reject malformed before the handler runs)?

**Status:** open. **Trade-off:** cleaner errors at wire-time vs more upfront decode cost on hot paths.

### OQ-WP-K8 — Streaming back-pressure semantics for `ENTITY_LIST` / `STATEMENT_LIST` / `RELATION_LIST_*`

Substrate streaming back-pressure ([`../04_wire_protocol/06_streaming.md`](../04_wire_protocol/06_streaming.md)) applies, but the typed-graph-specific detail: when Brain's per-stream buffer fills, do these list operations:

- (a) **Block** the producer (current substrate convention)?
- (b) **Cancel** the stream with a partial result + cursor for resume (typed-graph-specific)?

Default leaning: (a) until a real workload proves otherwise.

**Status:** open. Revisit when query streaming hits production scale.

### OQ-WP-K9 — Schema removal (`SCHEMA_DROP`)

Once a schema is declared, there's no opcode to revert. Should a `SCHEMA_DROP` opcode exist?

**Trade-off:** symmetric API vs accidental-erasure risk. A deployment with 10M statements losing schema would orphan them entirely.

**Status:** deferred. **Likely outcome:** never as a wire opcode; only as an offline admin action.

### OQ-WP-K10 — `ENTITY_LIST` cursor pagination + multi-frame streaming

Brain ships `ENTITY_LIST` as a **single-frame snapshot** with `limit` capped at 1000 (max ~2 MiB of `EntityView` payload, well within the 16 MiB frame budget). The wire shape (`EntityListResponseFrame`) supports streaming via `items + cumulative_count + is_final`, but only the `is_final = true` single-frame mode is wired.

Deferred behaviors:

- **Cursor resume.** Caller passes a non-empty `cursor` to fetch the next page. Wire-side currently rejects with `InvalidArgument`.
- **Multi-frame streaming within a page.** The 1000-cap is comfortable; sub-1k responses fit in one frame.

The query router already needs streaming infrastructure for `QUERY` (`0x0160`), `RECALL_HYBRID` (`0x0163`), and `STATEMENT_LIST` (`0x0146`); `ENTITY_LIST` pagination would piggyback on that work rather than duplicating it here.

**Status:** open.

### Resolved (typed-graph)

#### R-K1 — Opcode namespace conflict

The pre-rewrite typed-graph opcode table directly collided with Brain opcode assignments (`0x30` was `ENTITY_CREATE` in typed-graph but `SUBSCRIBE_REQ` in substrate). Resolved by widening the wire opcode to `u16` and splitting into namespace bytes (`0x00xx` substrate, `0x01xx` typed-graph). See [`../04_wire_protocol/03_opcodes.md`](../04_wire_protocol/03_opcodes.md).

#### R-K2 — Flags field shrunk to u8

The pre-rewrite header had a 16-bit `flags` field with bits 12-0 reserved. Shrunk to `u8` (only EOS / MPL / CMP bits ever used) and reclaimed the freed byte for the upper half of the new `u16` opcode. See [`../04_wire_protocol/02_wire_format.md`](../04_wire_protocol/02_wire_format.md).

#### R-K3 — `STREAM_START` envelope

Typed-graph opcodes reuse substrate streaming verbatim (sequence of frames sharing a `stream_id`, intermediate frames clear EOS, final frame sets EOS). No `STREAM_START` / `STREAM_ITEM` / `STREAM_END` envelope.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §05_operations open questions


Cognitive-operations questions unresolved as of this spec version.

---

## OQ-CO-1: Salience-weighted ranking in RECALL

**Issue.** RECALL currently ranks purely by similarity. Salience (importance) doesn't influence ranking — only filtering.

**Options.**

a) **Score-only (status quo).** Ranking by cosine similarity.

b) **Blended ranking.** `final_score = alpha * similarity + (1 - alpha) * salience`.

c) **Per-request control.** A `ranking` parameter the client sets.

**Recommendation.** Defer. Most agents either don't care about salience or do their own re-ranking. Brain's pure-similarity is predictable.

---

## OQ-CO-2: Vector-distance contradiction detection

**Issue.** REASON's contradiction signal currently relies on explicit CONTRADICTS edges. Auto-detection of contradictions from vector geometry is research-grade.

**Options.**

a) **Explicit edges only (status quo).**

b) **Heuristic auto-detection.** Memories similar in topic but pointing in different directions in the embedding space → flag as potentially contradicting.

c) **LLM-based detection.** Call an external LLM to judge contradiction. Heavy.

**Recommendation.** Stay with (a). Auto-detection has too many false positives in our experiments.

---

## OQ-CO-3: Cross-shard transactions

**Issue.** Transactions are single-shard. For workflows spanning multiple shards (rare for typical agents), there's no atomicity.

**Options.**

a) **Single-shard only (status quo).**

b) **Two-phase commit across shards.** Heavy; complex.

c) **Saga pattern in the client.** Application-level compensating actions.

**Recommendation.** (c). Clients provide saga helpers; Brain stays simple.

---

## OQ-CO-4: Edge versioning

**Issue.** Edges are last-write-wins. The history of edge changes isn't preserved.

**Options.**

a) **Last-write-wins (status quo).**

b) **Versioned edges.** Each edge has versions; readers can request specific versions.

c) **Edge log.** Edge changes go to a separate log; current state is the latest.

**Recommendation.** Defer. Most use cases don't need history; the storage cost isn't worth it.

---

## OQ-CO-5: Bulk import API

**Issue.** Bulk imports (loading many memories at once) currently use ENCODE_BATCH. For very-large bulk (millions of memories), this is awkward.

**Options.**

a) **ENCODE_BATCH with streaming (status quo).**

b) **Dedicated BULK_IMPORT primitive.** Streams memories and edges via a dedicated stream protocol.

c) **Offline import tool.** A separate tool that writes directly to substrate files.

**Recommendation.** (a) for now; (c) for very-large imports as an offline tool.

---

## OQ-CO-6: Consolidation as a primitive

**Issue.** Consolidation happens in background workers. Agents can't trigger consolidation directly.

**Options.**

a) **Worker-only (status quo).**

b) **Agent-triggered consolidation.** A `CONSOLIDATE(memory_ids)` operation that creates a Consolidated memory from sources.

c) **Scheduling control.** Agents can hint at consolidation priorities.

**Recommendation.** (b) might be useful in v1.x. For now, agents can use ENCODE with `kind: Semantic` and DERIVED_FROM edges to manually create consolidation-like memories.

---

## OQ-CO-7: Time-travel queries

**Issue.** All queries see the current state. There's no way to see "what did Brain look like an hour ago?"

**Options.**

a) **Current-state only (status quo).**

b) **Snapshot-based time travel.** Use admin snapshots to reconstruct historical state.

c) **Time-aware queries.** A `as_of` parameter on RECALL etc.

**Recommendation.** (b) is feasible operationally. (c) would require maintaining historical state — too expensive.

---

## OQ-CO-8: Multi-modal memories

**Issue.** Brain currently stores only text. Image, audio, etc. aren't first-class.

**Options.**

a) **Text-only (status quo).** Multi-modal can be encoded as text descriptions.

b) **Multi-modal with separate embedders.** Different embedders per modality; cross-modal queries.

c) **Multi-modal embedder.** A single model handles text + images.

**Recommendation.** Defer to v2. Would require significant architectural change to the embedding layer.

---

## OQ-CO-9: PLAN with goal probability

**Issue.** PLAN returns paths sorted by score. It doesn't return a "probability of success" for the plan.

**Options.**

a) **Just paths (status quo).**

b) **Add a probability field.** Use Bayesian inference on edge weights.

c) **Multiple-path coverage.** Return diverse paths instead of best paths.

**Recommendation.** (b) is interesting but requires reliable edge-weight calibration, which agents typically don't provide. Defer.

---

## OQ-CO-10: Streaming RECALL responses

**Issue.** For very large K, the RECALL response is one big frame. Streaming as results are computed would let clients start processing earlier.

**Options.**

a) **Single-frame response (status quo).**

b) **Streaming results.** First N results in first frame; more in subsequent frames.

**Recommendation.** Defer. Most clients use small K. Streaming would add wire-protocol complexity.

---

## OQ-CO-11: Per-memory access control

**Issue.** Memories belong to an agent. Within an agent, all memories are equally accessible.

**Options.**

a) **Agent-level only (status quo).**

b) **Per-memory ACLs.** Memories can be tagged with access levels.

c) **Context-level ACLs.** Different contexts have different access requirements.

**Recommendation.** Defer. Adds significant complexity. Multi-agent workflows can use separate agents per access level.

---

## OQ-CO-12: Dedicated graph query language

**Issue.** Graph queries are expressed via PLAN and REASON, plus direct edge enumeration. There's no query language (Cypher, GQL, SPARQL) for arbitrary graph traversals.

**Options.**

a) **Primitive-based (status quo).**

b) **Add a graph query language.** Substantial work.

**Recommendation.** Stay with (a). Brain isn't a graph database; Brain does not want to compete with Neo4j etc. on graph-query expressiveness.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §06_sdk open questions


**Superseded (standalone-DB pivot).** Brain ships no first-party SDK; the questions below are retained as historical record only and are moot.

---

## OQ-SDK-1: SDK in WASM environments

**Issue.** Users want to run the SDK in browsers (WASM) and edge environments (Cloudflare Workers, Deno).

**Options.**

a) **Native only (status quo).** SDKs require Node.js / browser ecosystems for JavaScript variants.

b) **WASM-compatible.** A subset of the SDK works in WASM (TCP not allowed; needs WebSocket fallback).

**Recommendation.** Add a WebSocket transport option in v1.x. Brain's wire protocol over WebSocket is straightforward.

---

## OQ-SDK-2: Static vs dynamic dispatch

**Issue.** In Rust, builder patterns can use static (compile-time) or dynamic dispatch. Dynamic is more flexible; static is faster.

**Options.**

a) **Static (status quo).** Each operation type is its own builder.

b) **Dynamic.** A single builder with operation type as a field.

**Recommendation.** Stay with static. The performance and ergonomics are better.

---

## OQ-SDK-3: Auto-batching

**Issue.** Multiple ENCODE calls in quick succession could be batched. The SDK could auto-detect and batch.

**Options.**

a) **No auto-batching (status quo).** Explicit `encode_batch` for batching.

b) **Auto-batch based on submission rate.** Up to N ms; batch then send.

**Recommendation.** Defer. Auto-batching adds latency and complicates timing semantics. Explicit batching is clearer.

---

## OQ-SDK-4: Client-side caching

**Issue.** Repeated RECALLs with the same cue could hit a client-side cache.

**Options.**

a) **No cache (status quo).** Every call hits Brain.

b) **Optional cache with TTL.** Cache hits avoid network.

**Recommendation.** Stay with no cache. Brain's embedding cache handles most repeated work; client-side caching adds complexity for marginal benefit.

---

## OQ-SDK-5: Stream resumption guarantees

**Issue.** When a SUBSCRIBE stream disconnects and resumes, what's the LSN guarantee?

**Options.**

a) **Best effort (status quo).** Resume from last seen LSN; might miss events in the gap.

b) **Strict no-loss.** Brain retains events for a window; resumption is lossless.

**Recommendation.** Add (b) as opt-in. Substrate's WAL retention permits lossless resumption within bounds.

---

## OQ-SDK-6: Generated code from schema

**Issue.** SDK code is hand-written per language. Could be generated from a wire-protocol schema.

**Options.**

a) **Hand-written (status quo).** Each SDK is its own codebase.

b) **Generated from schema.** Single source of truth; multiple language outputs.

c) **Hybrid: generate types, hand-write logic.**

**Recommendation.** (c). Types are tedious to maintain by hand; logic benefits from manual care.

---

## OQ-SDK-7: Async runtime selection

**Issue.** Rust SDK uses tokio. Some users want async-std or smol.

**Options.**

a) **Tokio only (status quo).**

b) **Runtime-agnostic.** Abstract over the runtime.

c) **Multiple SDKs (one per runtime).**

**Recommendation.** (a). Tokio is dominant; supporting alternatives adds complexity. Users can wrap if needed.

---

## OQ-SDK-8: gRPC alternative

**Issue.** Brain's wire protocol is custom. Some users prefer gRPC for tooling reasons.

**Options.**

a) **Custom only (status quo).**

b) **Add a gRPC gateway.** A separate component that translates gRPC to Brain's wire protocol.

c) **Native gRPC.** Implement gRPC server in Brain.

**Recommendation.** (b) as v2 if there's demand. Brain's wire protocol is more efficient; gRPC's value is mostly tooling.

---

## OQ-SDK-9: SDK observability standardization

**Issue.** OpenTelemetry is the standard. The SDK should integrate naturally.

**Options.**

a) **OTel-friendly (current intent).** SDK exposes spans / metrics in OTel format.

b) **OTel-native.** SDK uses OTel APIs directly.

**Recommendation.** (a). OTel-native means hard dependency; OTel-friendly works without OTel installed.

---

## OQ-SDK-10: Test fixture data

**Issue.** Common test fixtures (sample memories, etc.) save boilerplate but may not match real workloads.

**Options.**

a) **Generic fixtures (current intent).** Simple memory text, agent IDs.

b) **Domain-specific fixtures.** "Chatbot fixtures", "knowledge-base fixtures".

**Recommendation.** (a) for general SDK. (b) as separate libraries for specific domains.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §07_embedding open questions


Embedding-layer questions unresolved as of this spec version.

---

## OQ-EL-1: INT8 quantization

**Issue.** The model runs at FP32 by default. INT8 quantization reduces weights by 4× and inference latency by ~2×, with marginal accuracy loss.

**Options.**

a) **Stay FP32.** Simpler; baseline accuracy preserved.

b) **INT8 quantized weights.** A separate quantized model file (`bge-small-en-v1.5-int8.safetensors`). Configurable per deployment.

c) **Mixed precision.** FP32 for some layers, INT8 for others, optimized for accuracy.

**Recommendation.** Defer until first benchmark cycle reveals whether the latency improvement justifies the accuracy cost. INT8 is well-understood and easy to add.

---

## OQ-EL-2: Multi-modal embeddings

**Issue.** v1 is text-only. Image and audio embeddings would require a different model and possibly different vector dim.

**Options.**

a) **Single multi-modal model.** CLIP-family or similar. Vector dim and storage layout change.

b) **Per-modality model + separate arenas.** Each modality has its own embedding model; vectors are tagged with modality at storage time.

c) **Defer.** Stay text-only in v1.

**Recommendation.** Defer. Multi-modality is mostly a "different layers everywhere" change, not a localized embedding-layer change. Treat it as a v2 milestone.

---

## OQ-EL-3: Cross-language deployments

**Issue.** `bge-small-en-v1.5` is English-only. Non-English deployments need a different model.

**Options.**

a) **Configurable model.** Use `bge-m3` (multilingual) for multilingual deployments. Already supported via the `model_path` configuration.

b) **Multiple active models.** Different agents in the same deployment use different models based on language. Complex; cross-agent queries become difficult.

c) **Translate-then-embed.** Pre-translate non-English content to English. Adds latency and dependency on a translation service.

**Recommendation.** Option (a) — single configured model per deployment. Multilingual deployments use a multilingual model. Cross-language operation within a single deployment is not a goal.

---

## OQ-EL-4: Dynamic batching window

**Issue.** The GPU batching window is configured statically (default 2 ms). Dynamic adjustment based on load could improve performance.

**Options.**

a) **Static window.** Status quo. Operators tune for their workload.

b) **Adaptive.** The window expands under low load (waiting longer for bigger batches) and contracts under high load (better latency). Heuristic-driven.

**Recommendation.** Defer. The static window is simple and good enough for most workloads. If profiling reveals scenarios where adaptation helps significantly, revisit.

---

## OQ-EL-5: Per-agent embedding model

**Issue.** All agents in a deployment share the embedding model. Some applications want different agents with different models (e.g., different specializations).

**Options.**

a) **Single model per deployment.** Status quo. Different specializations require different deployments.

b) **Per-agent model selection.** Each agent's encode and recall use the agent's configured model. Storage layout would need to handle mixed-fingerprint shards as a normal case rather than a migration window.

**Recommendation.** Out of scope. The architecture is designed around a single active model per shard. Cross-model querying within a shard adds complexity without clear benefit; deployments wanting multi-model run separate deployments.

---

## OQ-EL-6: Embedding quality observability

**Issue.** Brain doesn't directly observe embedding quality. If the chosen model produces poor vectors for the deployment's content, queries silently degrade.

**Options.**

a) **No direct observation.** Operators evaluate quality externally (sample queries, compare to expected results).

b) **Built-in quality benchmark.** A small fixed dataset that Brain periodically embeds and queries; reports metrics.

c) **Customer-supplied benchmark.** The deployment uploads its own benchmark; substrate runs it on demand.

**Recommendation.** Specify a built-in benchmark in [16. Benchmarks + Acceptance Criteria](../19_benchmarks/00_purpose.md) that runs at startup and periodically. This catches gross issues. For deployment-specific quality, operators are on their own — too varied to standardize.

---

## OQ-EL-7: Embedding for other operations

**Issue.** Currently, only encode and recall use the embedding layer. Could other operations benefit?

Examples:

- Filter expressions in RECALL could be embedded for semantic-aware filters.
- ADMIN tools might want to find memories matching descriptions semantically.

**Options.**

a) **Stay encode/recall.** Status quo.

b) **Generic embed RPC.** A cognitive operation that just embeds text; clients can use it for arbitrary purposes.

**Recommendation.** Defer. The current usage is sufficient. A generic embed RPC could be added if a clear use case appears.

---

## OQ-EL-8: Streaming embedding for long content

**Issue.** Truncation at 512 tokens loses content. For long content, Brain could embed multiple chunks and combine them (e.g., averaging vectors).

**Options.**

a) **Truncate.** Status quo. The agent is responsible for chunking long content.

b) **Auto-chunk-and-aggregate.** Long inputs are split into 512-token chunks; each is embedded; vectors are averaged or otherwise aggregated.

c) **Allow longer inputs via different model.** A long-context model exists; could be configured.

**Recommendation.** Stay with truncate (a). Auto-aggregation has subtle semantic implications (averaging losing information) and is better addressed at the application layer where the chunking decisions can be informed by context.


## §08_storage open questions


Storage-layer questions unresolved as of this spec version.

---

## OQ-ST-1: Vector compression (PQ, scalar quantization)

**Issue.** Vectors take 1.5 KB each. For very large shards, compression could meaningfully reduce storage and possibly improve cache hit rate (smaller vectors → more in cache).

**Options.**

a) **Stay f32.** Status quo. Simple, fast SIMD, no quality loss.

b) **f16 (half precision).** 2× compression, slight quality loss, requires SIMD support that's spotty.

c) **Scalar quantization (SQ8).** 4× compression. ~5% accuracy loss for retrieval. Used by FAISS and others.

d) **Product quantization (PQ).** 8-32× compression. More accuracy loss, higher implementation complexity.

**Recommendation.** Defer. v1 prioritizes simplicity; if a deployment hits storage walls, revisit. SQ8 is the most natural next step.

---

## OQ-ST-2: Non-blocking checkpoints

**Issue.** Current checkpoint procedure has a brief drain (10–50 ms). For latency-critical deployments, this is a noticeable hiccup.

**Options.**

a) **Status quo.** Brief drain.

b) **Non-blocking via snapshot.** Use the snapshot mechanism: reflink files at a point in time, work with the reflinked copies, no drain. The active files continue accepting writes during the checkpoint.

c) **Online checkpoint.** Use HNSW's online checkpoint protocol (capture state without stopping inserts). Complex; coupling to HNSW internals.

**Recommendation.** Add option (b) as a config flag in v1.1. The reflink-based approach is well-understood and adds complexity in one place.

---

## OQ-ST-3: Per-record sequence number gaps

**Issue.** WAL records have monotonic LSNs. Gaps (e.g., LSN 5 missing while 4 and 6 present) are treated as errors. Some recovery scenarios might benefit from gap-tolerant replay.

**Options.**

a) **Strict.** Status quo. Gap = corruption; refuse to start.

b) **Tolerant on operator request.** A `--allow-wal-gaps` flag for advanced operators who know what they're doing.

c) **Repair via replay from snapshot.** If a snapshot covers the gap, restore from the snapshot.

**Recommendation.** Stay strict (a). Gaps in append-only WAL should not happen; tolerating them silently could mask real bugs. Option (c) is the operator's normal recovery path.

---

## OQ-ST-4: Group-commit window adaptive sizing

**Issue.** The group-commit window is statically configured (default 100 µs). Under varying load, a fixed window may be suboptimal.

**Options.**

a) **Static.** Status quo. Operator tunes for typical load.

b) **Adaptive based on queue depth.** Wider window under low load (more chance for batches), narrower under high load (latency wins).

**Recommendation.** Defer. The static window is good enough for most workloads. Revisit if load profiling reveals scenarios where adaptation helps significantly.

---

## OQ-ST-5: Direct I/O for arena reads

**Issue.** Arena reads go through the page cache. For some workloads (huge arenas, low memory), bypassing the page cache might be better.

**Options.**

a) **mmap (status quo).** Page cache handles working set.

b) **O_DIRECT reads.** Manual buffer management, more deterministic latency, but loses kernel readahead.

c) **Hybrid.** Configurable per shard.

**Recommendation.** Stay with mmap. The page cache works well for our access patterns; manual buffer management would be a significant complexity increase for marginal benefit.

---

## OQ-ST-6: WAL compression

**Issue.** WAL records contain text and vectors that could be compressed. Especially the vector (1.5 KB of f32) may compress well in some cases.

**Options.**

a) **No compression.** Simplest.

b) **Per-record zstd.** Records over a threshold get compressed.

c) **Streaming compression on segment level.** Each segment is gzip/zstd-compressed.

**Recommendation.** Defer. Compression is a meaningful complexity increase; let's see what real workloads look like before committing. Streaming compression at segment level is the most attractive option if we go this way (handles long sequences efficiently).

---

## OQ-ST-7: Multi-shard WAL coalescing

**Issue.** Each shard has its own WAL with its own fsync. A node hosting many shards has many concurrent fsyncs. Could shards on the same node share a WAL device or fsync barrier?

**Options.**

a) **Per-shard WALs.** Status quo. Independent fsyncs.

b) **Shared WAL with shard tagging.** All shards on a node write to one WAL with a shard-id tag per record. Recovery filters per shard.

c) **Coalesced fsync.** Per-shard WALs but a single fsync covers all of them.

**Recommendation.** Stay per-shard (a). Coupling shards' durability is the wrong direction; Brain wants isolation, not coupling. The fsync overhead is bounded by the device's IOPS, which is plenty for our targets.

---

## OQ-ST-8: Snapshot streaming to remote storage

**Issue.** Snapshots produce local files. For backup, the operator typically copies them to S3, GCS, etc. Could Brain stream snapshots directly?

**Options.**

a) **Local-only.** Status quo. Operators run external backup tools.

b) **Direct S3 / S3-compatible.** Brain uploads snapshot files to a configured S3 bucket.

c) **Pluggable backend.** A trait for snapshot destinations; operators configure their preferred backend.

**Recommendation.** Stay local-only. External backup tools (rclone, restic, native AWS CLI) are mature; Brain doesn't need to compete. Document the integration patterns and let operators handle the upload.

---

## OQ-ST-9: WAL fsync coalescing across operations

**Issue.** Each operation that mutates state writes a WAL record. For workloads with many tiny mutations (salience updates dominating), per-record fsync overhead adds up.

**Options.**

a) **Group commit + record coalescing (status quo).** Multiple records → one fsync; salience updates coalesce within a record.

b) **More aggressive coalescing.** Multiple operations from the same agent within a window become a single record.

c) **Delayed fsync for non-critical records.** Salience updates and similar bookkeeping records aren't fsync'd individually; they piggyback on the next fsync.

**Recommendation.** (c) is interesting. Salience updates are not "durable" in the strict sense — losing them in a crash means Brain loses some salience boost, which is annoying but not catastrophic. Treating them as deferred-durability could improve throughput. But the implementation must not delay them indefinitely; a max-delay is needed. Defer to v1.1.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §09_indexing open questions


ANN-index-level questions unresolved as of this spec version.

---

## OQ-AN-1: Pre-filter index for highly-selective filters

**Issue.** Post-search filtering wastes compute when filters are very selective. A pre-filter approach would search only matching candidates.

**Options.**

a) **Status quo.** Post-search filter; expand ef_search if too few results.

b) **Per-filter HNSW indexes.** Maintain separate HNSW indexes per kind, per popular context, etc. Significant memory overhead.

c) **Inline filter awareness.** A patched HNSW that prunes during traversal based on the filter. Requires modifying hnsw_rs.

**Recommendation.** Defer. Post-search works for most filters; more sophisticated approaches add complexity.

---

## OQ-AN-2: Partial rebuild

**Issue.** Full rebuild is heavy for very large shards. A partial rebuild that only repairs degraded regions would be cheaper.

**Options.**

a) **Full rebuild only.** Status quo.

b) **Region-based partial rebuild.** Identify dense-tombstone regions, rebuild just those.

c) **Incremental cleanup during inserts.** Each insert opportunistically cleans up nearby tombstones.

**Recommendation.** Defer. Full rebuild scales OK for our targets (5–30 sec for 1M memories). For 10M+ shards, this becomes important.

---

## OQ-AN-3: Vector compression in HNSW

**Issue.** HNSW stores its own copy of each vector (~1.5 KB each). With compression (PQ, scalar quantization), the in-HNSW copy could be much smaller.

**Options.**

a) **f32 (status quo).** Simple, fast SIMD, full precision.

b) **f16 in HNSW.** 2× memory savings, slightly slower distance.

c) **PQ-compressed HNSW.** 4-16× savings, more accuracy loss.

**Recommendation.** Defer. v1 prioritizes simplicity. Hits storage walls would push toward (b) or (c).

---

## OQ-AN-4: GPU acceleration of search

**Issue.** Search is CPU-bound. GPUs could compute many distances in parallel, especially for very large indexes.

**Options.**

a) **CPU only (status quo).** Simple; portable; sufficient for typical workloads.

b) **GPU search.** Use a GPU-aware ANN library. Different architecture (FAISS-GPU, ScaNN).

**Recommendation.** Stay CPU-only for v1. GPU-ANN libraries are powerful but operationally heavy. Revisit if we have customer workloads where ANN is the bottleneck and CPU isn't enough.

---

## OQ-AN-5: Hybrid index types

**Issue.** Some workloads might benefit from non-HNSW index types (IVF for very large indexes, brute-force for very small).

**Options.**

a) **HNSW only.** Status quo; simpler.

b) **Configurable index type per shard.** Operator chooses based on shard size and access pattern.

c) **Auto-selection.** Substrate picks the index type based on shard size.

**Recommendation.** Stay HNSW-only. Brain's small-shard fast path uses brute force already; truly large shards (>100M) aren't in v1's target.

---

## OQ-AN-6: Multi-query batching

**Issue.** For workloads with many concurrent queries, batching them through HNSW could improve throughput.

**Options.**

a) **Per-query (status quo).** Each query independently.

b) **Batch queries.** Multiple queries gathered in a window, processed together. The HNSW visits some shared nodes once across queries.

**Recommendation.** Defer. The per-query model works well; batching would add complexity for marginal gain.

---

## OQ-AN-7: Continuous incremental cleanup

**Issue.** The maintenance worker's full rebuild is a "stop the world" operation. Continuous cleanup during normal operations would smooth performance.

**Options.**

a) **Periodic full rebuild (status quo).**

b) **Continuous cleanup.** Each insert/search opportunistically cleans up nearby tombstones; full rebuild becomes a fallback.

**Recommendation.** Future enhancement. The mechanics are well-understood (some HNSW variants do this); implementation is non-trivial.

---

## OQ-AN-8: Cross-shard ANN

**Issue.** Cross-shard queries fan out to each shard, run HNSW search on each, merge results. The merge isn't quite right — the K results from each shard may not be the global top-K.

**Options.**

a) **K from each shard (status quo).** Slightly inflated K guards against missing global top-K.

b) **Iterative refinement.** Start with K from each shard; if the merged K-th result has score lower than any shard's K+1-th, re-query that shard with K' > K.

**Recommendation.** Currently just inflate K (Brain uses K * over_factor for cross-shard). Iterative refinement is a possible enhancement.

---

## OQ-AN-9: Approximate top-K rather than top-K

**Issue.** Some applications care more about "diverse results" than "top K by similarity". Brain's current top-K returns highly-similar (often near-duplicate) results.

**Options.**

a) **Top-K (status quo).**

b) **MMR (Maximal Marginal Relevance).** Trade off similarity vs diversity in result selection.

c) **Cluster-then-pick.** Cluster candidates; return one from each cluster.

**Recommendation.** This belongs in the query planner / operations layer, not the ANN layer. The ANN returns top-K; the planner can ask for more candidates and apply post-processing.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §10_metadata open questions


Metadata-store-level questions unresolved as of this spec version.

---

## OQ-MD-1: Per-agent index for memory listing

**Issue.** Listing all memories for an agent currently requires scanning all memories in the shard and filtering by agent_id. For shards with many agents, this is wasteful.

**Options.**

a) **Scan and filter (status quo).** Simple; works fine for shards with one or few agents.

b) **Add a `(AgentId, MemoryId) → ()` index table.** Range scan returns the agent's memories. Costs ~30 bytes per memory in extra storage.

c) **Group shards by agent.** Each shard hosts a single agent (or a small group). Avoids the indexing problem at the routing layer.

**Recommendation.** Add the index table when shards routinely host more than ~10 agents. For v1's typical deployment patterns, status quo is fine.

---

## OQ-MD-2: Per-context index for memory listing

**Issue.** Same as agent index, but for contexts.

**Options.**

a) **Scan and filter (status quo).**

b) **Add a `(ContextId, MemoryId) → ()` index.** ~25 bytes per memory.

c) **Combined `(AgentId, ContextId, MemoryId) → ()` index.** Slightly more compact; serves both agent-listing and context-listing.

**Recommendation.** Add index (c) in v1.1 if usage patterns show frequent context-scoped enumeration.

---

## OQ-MD-3: Soft-delete vs immediate-delete

**Issue.** The metadata table keeps tombstoned rows during the grace period. Live rows and tombstoned rows are mixed; queries always include a "is active" filter.

**Options.**

a) **Mixed table (status quo).** Filter on read.

b) **Two tables.** Active rows in `memories`; tombstoned rows in `memories_tombstoned`. Move on FORGET.

c) **Bloom-filter for active.** Quick check before reading metadata; if not in filter, skip.

**Recommendation.** Status quo. The cost of the active filter is low; the simplicity matters more.

---

## OQ-MD-4: Multi-version edges

**Issue.** A given (source, kind, target) triple can have only one edge. Some applications might want multiple edges with different annotations (a "history" of relationships).

**Options.**

a) **Single edge per triple (status quo).**

b) **Compound key with timestamp:** `(source, kind, target, version)`. Stores multiple instances.

c) **External versioning** at the application level — encode versioned edges as (source_v1, kind, target).

**Recommendation.** Status quo. Use case is rare; complexity added by (b) isn't justified.

---

## OQ-MD-5: Compressed text storage

**Issue.** Text dominates the metadata store's size. Compression (zstd, LZ4) would reduce footprint.

**Options.**

a) **Uncompressed (status quo).** Simple; fast read.

b) **Per-row compression.** Each text is independently compressed. Read decompresses on the fly.

c) **Dictionary compression.** Train a zstd dictionary on a sample of texts; compress with shared dictionary for better ratio.

**Recommendation.** Defer. Disk is cheap; the operational complexity of compression isn't justified unless storage is a bottleneck.

---

## OQ-MD-6: Indexes on flexible attributes

**Issue.** Brain currently indexes only what's hardcoded (memory ID, edges, contexts). Custom indexes (e.g., by tag, by metadata field) aren't supported.

**Options.**

a) **No custom indexes (status quo).** All filtering is post-search.

b) **User-defined indexes.** Operators or agents can request indexes on specific fields.

c) **Schema-driven indexes.** Detect commonly-queried fields and auto-index them.

**Recommendation.** Defer. Brain isn't a SQL database; flexible indexing isn't core to the value proposition.

---

## OQ-MD-7: Partitioned tables for very large shards

**Issue.** redb performance is good for moderately-sized tables but may degrade at very large sizes (hundreds of GB).

**Options.**

a) **Single redb file per shard (status quo).**

b) **Horizontal partition within shard.** E.g., split memories by time range; each range in its own redb file.

c) **Operate at smaller shard sizes.** Encourage sharding before tables grow too large.

**Recommendation.** (c). Brain's sharding is the partition mechanism; if a shard grows too large, split it.

---

## OQ-MD-8: redb backup tooling

**Issue.** Backup/restore is currently file-level (snapshot the entire metadata.redb). For large databases with small ongoing changes, this is wasteful.

**Options.**

a) **File-level backup (status quo).**

b) **Logical backup.** Export rows; import into fresh database.

c) **Incremental backup.** Diff between snapshots; only ship changes.

**Recommendation.** File-level is fine for v1. Incremental backup is a possible v2 enhancement.

---

## OQ-MD-9: redb's WAL mode

**Issue.** redb has its own write-ahead-log (separate from Brain's WAL). Brain currently uses redb's default sync-on-commit. There's a higher-throughput async mode.

**Options.**

a) **Sync-on-commit (status quo).** Each commit fsyncs.

b) **Async commits.** redb buffers commits; periodic group sync. Higher throughput; small durability window for redb's own state (Brain's WAL still ensures actual durability).

**Recommendation.** Stay with sync-on-commit. The overhead is acceptable; durability simplicity matters.

---

## OQ-MD-10: Cross-shard transactions

**Issue.** Currently, Brain doesn't support transactions across shards. An operation that needs cross-shard atomicity isn't expressible.

**Options.**

a) **No cross-shard transactions (status quo).** Caller responsible for handling failures.

b) **Two-phase commit across shards.** Heavy; Brain does not want to be a distributed database.

c) **Saga pattern.** Application-level compensating actions on failure.

**Recommendation.** Stay with (a). For applications needing cross-shard atomicity, clients provide saga helpers.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## Provenance open questions


Provenance / versioning deferrals.

## Active

### Q1 — `ADMIN_GET_EXTRACTION_AUDIT` wire op

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §8 — audit query
is available via `brain-metadata::audit_ops` but
isn't exposed over the wire. Operators have to attach a client
shim. A dedicated wire op is deferred.

**Status:** deferred.

---

### Q2 — Audit-row output overflow

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §1 —
`outputs: Vec<OutputRefRow>` is capped at 64 entries. Overflow
behaviour (follow-on row keyed by `(audit_id, seq)`) deferred to
post-phase-20.

**Target:** post-v1. **Status:** deferred.

---

### Q3 — Audit-log sweeper

[`..../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md)
Q4 tracks the sweeper that deletes expired rows + indexes. Phase
20 ships the write path; the sweeper itself lands post-phase-20.

**Target:** post-v1. **Status:** deferred.

---

### Q4 — Re-extraction worker

[`./00_purpose.md`](../10_metadata/00_purpose.md) §"Re-extraction workflow"
describes the admin-triggered re-extraction. Brain supports
re-running via `ExtractorRunOptions { replay: true }` but only as
a direct API call. A worker that runs across many memories is
deferred.

**Target:** post-v1. **Status:** deferred.

---

### Q5 — Bitemporal as-of-transaction

[`./00_purpose.md`](../10_metadata/00_purpose.md) §"Version visibility" — `as_of`
is valid-time only. Transaction-time queries (return state as it
existed in the system at time T) is post-v1.

**Target:** post-v1. **Status:** deferred.

---

### Q6 — Stale-extraction detection worker

[`./00_purpose.md`](../10_metadata/00_purpose.md) §"Stale extraction detection"
flags older versions. The detector runs as a periodic worker;
Brain does not ship it in v1.0.

**Status:** deferred.

---

### Q7 — `model_metadata` shape

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §1 — `model_metadata:
Vec<u8>` is an rkyv-archived blob. Pattern + classifier tiers
leave it empty; the LLM tier fills it with token counts, cache
hit, and model version.

**Status:** open.

## Resolved

- Audit row layout (primary + 3 indexes) — resolved in
  [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md).
- Atomicity (audit row + outputs in one wtxn) — resolved in
  [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §9.

---

### Q8 — Record co-location for entity bundles (deferred optimization)

**Issue.** redb stores entities, statements, and relations in separate B-trees. Looking up a statement, its subject entity, and its evidence list hits three different B-tree positions — three page-cache misses per query on cold data. Co-locating each entity with its current statements and outgoing relations on the same page would cut that to one. Neo4j's block-format storage engine uses this pattern: related records live on the same page so cache-line and page-cache wins compound, and fewer IOPS land per query.

**Options.**

a) **Status quo.** Separate tables, accept the read amplification. Hot data lives in the page cache anyway.

b) **Custom redb table strategy or a layer on top** that materializes "entity bundles" for hot read paths. Trade: more write amplification (every statement write touches the bundle), less read amplification.

c) **Bench first.** Measure the page-cache miss rate on representative hot read paths before designing.

**Recommendation.** (c). The read-path benefit is unproven for Brain's access patterns. Schedule a measurement pass; only design (b) if the data justifies it. **Target:** post-v1. **Status:** deferred optimization.


## §11_extractors open questions


Extractor-specific deferrals. Wire-shape questions live in
[`..../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

## Active

### Q1 — Bundled NER model licensing + format

Brain ships a built-in `brain.basic_ner` classifier. The
exact CONLL-trained checkpoint, its licence, and its candle
compatibility are open at spec-write time. Fallback: a deterministic
rule-based classifier that satisfies the trait but doesn't beat
the pattern tier on recall.

**Status:** open.

---

### Q2 — Custom feature extractors

[`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §3
defines a `FeatureExtractor::Custom { id }` variant for user-
supplied feature pipelines. Brain implements `Builtin` only.

**Status:** deferred.

---

### Q3 — `OnDemand` / `OnSchemaChange` / `Periodic` triggers

[`../11_extractors/02_triggers.md`](../11_extractors/02_triggers.md) §1 lists five trigger types;
Brain implements two (`OnEncode`, `OnEncodeWhere`). The other
three parse and persist but never fire — they produce `Skipped`
audit rows.

**Status:** deferred.

---

### Q4 — Multi-extractor batching

A classifier model with batch inference could process N memories
at once. Brain dispatches one-at-a-time. Batching is deferred to a
future worker version that buffers near-foreground queue items
into mini-batches.

**Status:** deferred.

---

### Q5 — Ambiguous-entity admin queue

[`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md) §2 — `ResolutionOutcome::
Ambiguous` writes `Skipped(reason: "ambiguous")` audit rows but
doesn't surface them via a dedicated admin op. Operators have to
query audit failures by hand.

**Status:** deferred.

---

### Q6 — Auto-predicate creation

[`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md) §4 — Brain auto-creates
entities but not predicates / relation types. A pattern extractor
that emits a `StatementMention` for an unknown predicate fails
with `UnknownPredicate`. Some users may want lax mode where the
predicate auto-coins under the extractor's namespace.

**Status:** deferred. **Likely outcome:**
new `auto_create_predicates: bool` extractor field.

---

### Q7 — Cross-shard mention dedup

Two ENCODEs of structurally-identical text on different shards
produce two `EntityMention` entries; resolver tier 3 on each shard
may decide differently whether to dedupe. Cross-shard mention
dedup is deferred.

**Status:** deferred.

---

### Q8 — Content-addressed output IDs

[`../11_extractors/05_idempotency.md`](../11_extractors/05_idempotency.md) §4 — output IDs are
UUIDv7; idempotency relies on the audit row's `outputs` cache.
Some output kinds (e.g., `EntityMention`) could be content-addressed
by `BLAKE3(memory_id || extractor_id || span)`, making them truly
deterministic.

**Status:** deferred.

---

### Q9 — Audit row output overflow

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §9 — extreme extractors might
produce ≥64 outputs per memory. Brain caps at 64; overflow goes to a
follow-on `extractor_audit_overflow` row keyed by the original
`audit_id`. The cap is implemented; the overflow mechanism is deferred.

**Status:** deferred.

---

### Q10 — Cost tracking units

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §1 — `cost_micro_usd: u64`. Pattern
and classifier tiers store 0 (zero-cost). The LLM tier uses this
field. Question: should we track in dollar micro-units, token
counts, or both? Currently dollars; a future addition could add
a `tokens: u64` companion.

**Status:** open.

---

### Q11 — Deterministic diamond-dependency ordering

[`../11_extractors/02_triggers.md`](../11_extractors/02_triggers.md) §6 — `depends_on` chains
admit diamonds, but Brain's scheduler dispatches diamond legs
in arbitrary order. Two extractors both depending on a third may
see each other's outputs or not, non-deterministically.

**Status:** deferred. **Likely outcome:**
topological-sort with stable tie-breaking on `ExtractorId`.

---

### Q12 — Resolver tier-4 (LLM-assisted)

[`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md)
mentions a tier-4 LLM-assisted resolver. The shipped resolver stops at tier 3;
tier 4 lands alongside the LLM extractor tier.

**Status:** deferred — Brain ships the LLM extractor but not an LLM-driven resolver. Q12 lands separately once the LLM tier has live-traffic data.

---

### Q-llm-1 — Per-deployment global cost budget

[`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §5 — Brain
ships per-call budget enforcement only. A per-deployment global
budget (daily / weekly cap shared across shards) requires
cross-shard coordination (atomic counter or central registry).

**Status:** deferred.

---

### Q-llm-2 — Adaptive rate-limit retry

[`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §9 — Brain
treats `LlmError::RateLimit { retry_after_ms }` as `Failure` and
moves on. A more sophisticated impl backs off and retries
within the worker queue.

**Status:** deferred.

---

### Q-llm-3 — Proper tokenizer integration for cost estimation

[`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §5 — Brain
uses `chars / 4` as a tokens proxy for the cost estimate. Real
tokenizer integration (tiktoken for OpenAI; anthropic's tokenizer
counter API for Claude) is deferred.

**Status:** open. **Likely outcome:** plug
in `tiktoken-rs` for OpenAI; HTTP probe for Anthropic.

---

### Q-llm-4 — Local LLM backends (llama.cpp / vLLM)

[`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §2 — Brain
ships Anthropic + OpenAI HTTP transports only. Air-gapped
deployments need a local backend.

**Status:** deferred. **Likely shape:** a
new `LocalClient` implementing the `LlmClient` trait against
llama.cpp's HTTP server protocol.

---

### Q-llm-5 — `STATEMENT_ADD_EVIDENCE` for richer confidence

[`../02_data_model/07_statement.md`](../02_data_model/07_statement.md)
documents the evidence model. LLM extractors produce per-call
confidence which would slot into per-evidence weighting via a
`STATEMENT_ADD_EVIDENCE` op.

**Status:** deferred.

## Resolved

- Audit-row vs ERROR-frame split — pattern / classifier failures
  write `Failure` audits and don't surface as wire errors. Resolved
  in [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §3.
- Extractor fan-out from schema_upload — the `SchemaItem::Extractor` arm
  was deferred out of the v1 schema-upload path.


## §12_query_optimizer open questions


Planner & executor questions unresolved as of this spec version.

---

## OQ-PE-1: Plan caching

**Issue.** Repeated identical request shapes invoke the planner each time. Caching plans would save ~50 µs per request.

**Options.**

a) **No caching (status quo).** Plan each request.

b) **Plan cache by request shape.** Hash the structural shape of the request; cache the plan.

c) **Lazy plan compilation.** First call compiles; subsequent calls reuse.

**Recommendation.** Defer. Plan time is small relative to total request latency. Caching is justified only if planning becomes a bottleneck.

---

## OQ-PE-2: Cost-based ef adaptation

**Issue.** The planner's ef_search picking is rule-based. Could it be cost-based, picking the ef that meets a target latency given current load?

**Options.**

a) **Rule-based (status quo).** Predictable; simple.

b) **Cost-based.** Estimate cost as a function of ef; pick ef to hit target latency.

c) **Adaptive.** Track per-shard latency vs ef and learn.

**Recommendation.** Defer. The rules are good enough; cost-based adds complexity without clear gain.

---

## OQ-PE-3: Query rewriting

**Issue.** Some requests can be rewritten for efficiency. Example: a complex filter could be split into multiple simpler queries with merged results.

**Options.**

a) **No rewriting (status quo).** Plan as written.

b) **Rule-based rewriting.** A small library of rewrites.

c) **General optimizer.** Like a SQL query optimizer.

**Recommendation.** Stay with (a). Rewrites are a slippery slope; Brain wants predictable execution.

---

## OQ-PE-4: Streaming RECALL responses

**Issue.** For very large K, the response is sent as one frame. Streaming results as they're computed would let clients start processing earlier.

**Options.**

a) **Single-frame response (status quo).**

b) **Streaming**: send results in batches as they're available.

**Recommendation.** Defer. Most clients use small K. For large-K cases, the wire protocol's stream support is available; we can revisit if a real workload demands it.

---

## OQ-PE-5: Idempotency fail-open vs fail-closed

**Issue.** When the idempotency table is unavailable (corrupt, etc.), should Brain fail open (process without check, risking duplicates) or fail closed (reject the request)?

**Options.**

a) **Fail open (current).** Log warning, proceed.

b) **Fail closed.** Reject with `IdempotencyUnavailable`; client retries.

**Recommendation.** Fail closed — duplicate memories are worse than retry-able errors. Implement before v1 release.

---

## OQ-PE-6: Plan re-execution after partial failure

**Issue.** If a shard fails mid-execution, can we re-route to a healthy shard?

**Options.**

a) **No re-routing (status quo).** Fail with partial results.

b) **Re-routing for read-only operations.** If a shard is unreachable, try a replica.

c) **Full HA failover.** Multi-shard replication with automatic failover.

**Recommendation.** Need replication first. Without replicas, there's nowhere to re-route. v2 priority.

---

## OQ-PE-7: Per-tenant prioritization

**Issue.** All requests are first-come-first-served. Some tenants may need priority over others.

**Options.**

a) **FIFO (status quo).**

b) **Priority queues.** Per-tenant or per-request priority.

c) **Quotas + token-bucket.** Each tenant gets a rate; over-rate requests get deprioritized.

**Recommendation.** (c) is right for multi-tenant deployments. Implement in v1.x.

---

## OQ-PE-8: Compiled plans

**Issue.** Plans are interpreted (the executor matches on plan variants). A compiled plan (executable code) would be faster.

**Options.**

a) **Interpreted (status quo).**

b) **Compiled.** Generate Rust code at startup or runtime.

**Recommendation.** Stay with interpreted. The interpretive overhead is negligible compared to the actual storage operations.

---

## OQ-PE-9: Plan introspection in production

**Issue.** Currently, plans are logged but not visible to clients. Should clients be able to see plans (similar to SQL `EXPLAIN`)?

**Options.**

a) **Operator-only (current).** Plans visible via admin commands; not to typical clients.

b) **Per-request explain flag.** Clients can request the plan in the response.

c) **Always include.** Every response includes the plan.

**Recommendation.** (b) for v1. Explicit opt-in.

---

## OQ-PE-10: Subexpression deduplication

**Issue.** PLAN and REASON involve multiple RECALLs. If two of those RECALLs are similar, work is duplicated.

**Options.**

a) **No deduplication (status quo).**

b) **Detect identical sub-queries within a plan.** Run once; reuse.

**Recommendation.** Defer. Cases where this matters are rare; complexity isn't justified.

---

## OQ-PE-11: Speculative execution

**Issue.** For some queries, the planner could speculatively start work before knowing the full plan. Example: start embedding the cue while finalizing other plan parameters.

**Options.**

a) **Sequential (status quo).** Plan, then execute.

b) **Speculative.** Plan and execute concurrently; cancel if speculation was wrong.

**Recommendation.** Defer. Plan time is < 50 µs; speculation saves at most that.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §13_retrievers open questions


Retriever / fusion deferrals.

## Active

### Q1 — Learned router on top of the rule-based one

Brain ships a rule-based router (5 rules) per §13/05. A learned router on top — trained on labelled `(query → preferred-retrievers)` pairs — would adapt to deployment-specific query shapes the rules can't capture.

**Deferred because:** training data does not exist yet. The rule-based router is the cold-start baseline.

**Path:** post-v1. Feature-flag a learned classifier; rules remain as fallback. Labels come from click-through, explicit feedback, and synthetic teacher-LLM labels.

---

### Q2 — SPLADE-style sparse-neural retrieval

Brain ships BM25 over tantivy for lexical retrieval. SPLADE (sparse-neural) would add a fourth retriever for sparse-neural matching.

**Deferred because:** inference cost equivalent to dense; the quality gain is modest at v1's deployment scale; the complexity-to-value ratio is poor.

**Path:** evaluate against real queries once Brain has them.

---

### Q3 — Streaming query results (`limit > 100`)

The `QUERY` opcode returns a single `QueryResponse` frame with items truncated to `limit`. For large result sets, streaming over SUBSCRIBE as items pass the limit boundary would lower client-side memory pressure.

**Deferred because:** v1 deployments are local-first with modest result-set sizes. The streaming path adds wire-protocol surface (event types) and client iterator plumbing.

**Path:** post-v1. Add a `QueryStream` event type on SUBSCRIBE; clients gain a `query()…stream()` pattern returning a `Stream<Item = QueryHit>`.

---

### Q4 — Query inside a transaction + read-your-writes

`RECALL` inside a txn falls back to memory-only ANN search even when a schema is declared. The hybrid pipeline does not see the txn buffer's pending statements / relations.

**Deferred because:** layering txn-buffer reads onto the hybrid pipeline requires parallel `TxnLens` instances for the entity, statement, and relation tables, plus fusion logic that tolerates pending rows missing from secondary indexes (HNSW / tantivy commit cadence).

**Path:** post-v1. Design a per-table `TxnLens` shared by both the memory-only and hybrid paths.

---

### Q5 — Filter-only retriever mode (no text, no anchor)

The planner rejects requests with neither `text` nor `entity_anchor` as `PlanError::NoSignal`. A filter-only query like "all preferences with confidence ≥ 0.9 in the last week" is currently inexpressible.

**Deferred because:** v1 has no clear use case. Filter-only also benefits from a dedicated index design rather than reusing the hybrid pipeline.

**Path:** post-v1. Likely a new opcode or a planner-side filter-scan retriever.

---

### Q6 — Cross-shard query result merging

The hybrid pipeline runs per-shard. Multi-shard deployments fan `RECALL` / `QUERY` out at the connection layer and merge by score upstream of the hybrid engine.

**Deferred because:** single-shard deployments are the v1 default. Multi-shard cross-shard fusion adds latency-budget pressure that is best tackled once production telemetry exists.

**Path:** post-v1. Extend the connection layer's fan-out to deliver per-retriever partial result lists, and fuse globally before the filter chain.

---

### Q7 — IVF + product quantization for billion-vector deployments

Pure HNSW (per §09) holds every full-precision 384-dim vector in RAM. At billion-vector scale this becomes prohibitive. IVF (inverted file index) + PQ (product quantization) collapses memory ~10× with modest recall loss.

**Deferred because:** v1's target scale is millions, not billions. The added executor mode is non-trivial.

**Path:** post-v1. New executor mode in §09 indexing; HNSW remains default for ≤10M vectors.

**Partially resolved (v1.x):** HNSW + PQ (compressed graph payload, full-precision arena for re-rank) shipped per [§09.07](../09_indexing/07_hnsw_pq.md). Pure IVF (no HNSW graph) remains deferred — HNSW+PQ resolves the immediate memory pressure at Brain's mid-scale target.

---

### Q8 — Cross-encoder rerank model upgrade

The reranker (§13/07) ships with `bge-reranker-base` (110M params). Newer reranker models exist; the choice is a quality/latency trade-off.

**Deferred because:** the v1 reranker baseline needs production usage data to evaluate alternatives.

**Path:** post-v1. Pluggable reranker model selection per query.

---

## Resolved

(none yet)


## §14_concurrency open questions


Concurrency-model questions unresolved as of this spec version.

---

## OQ-CC-1: Multi-writer-per-shard

**Issue.** The single-writer-per-shard discipline is a strong simplification. Could multi-writer give more throughput?

**Options.**

a) **Single-writer (status quo).** Simple; predictable.

b) **Multi-writer with serialization.** Multiple writers, but they serialize commits. Marginal improvement.

c) **Truly parallel writers.** Each commits independently with conflict resolution. Significant complexity.

**Recommendation.** Stay with single-writer. For higher write throughput, scale shards (each with its own writer). The architecture matches the workload pattern.

---

## OQ-CC-2: Adaptive publication interval

**Issue.** The publication interval is fixed (10 ms typical). Could it adapt — slow under heavy write load (better throughput), fast under light load (better read freshness)?

**Options.**

a) **Fixed (status quo).** Simple.

b) **Adaptive.** Adjust based on write rate and read demand.

c) **Per-request control.** Reads with `consistency=ReadAfterWrite` force immediate publication.

**Recommendation.** (c) is already implemented. (b) is a minor optimization; not pursued in v1.

---

## OQ-CC-3: Read priorities

**Issue.** All reads have the same priority. Some applications might want priority for time-sensitive reads.

**Options.**

a) **Equal priority (status quo).**

b) **Per-tenant priority.** Configurable.

c) **Per-request priority.** Set in the request.

**Recommendation.** Defer. For most workloads, equal priority is fine. Per-tenant comes in with multi-tenancy features (v1.x).

---

## OQ-CC-4: Adaptive yield budgets

**Issue.** The yield budget (~100 µs) is fixed. Could it adapt to load — finer granularity under contention, coarser when idle?

**Options.**

a) **Fixed (status quo).**

b) **Load-aware.** Track concurrent task counts; yield more when contended.

**Recommendation.** Defer. Fixed budget is simple and predictable. Adaptive scheduling is hard to get right and easy to misconfigure.

---

## OQ-CC-5: Replacement for crossbeam-epoch

**Issue.** crossbeam-epoch is mature but has rough edges. Hazard pointers (HP) or RCU might be alternatives.

**Options.**

a) **crossbeam-epoch (status quo).**

b) **Hazard pointers.** When a mature Rust impl exists.

c) **Custom epoch protocol.** Tailored to Brain's use cases.

**Recommendation.** Stay with crossbeam-epoch. Alternatives don't have clear advantages for our scale.

---

## OQ-CC-6: Per-shard executor config

**Issue.** All shards use the same Glommio executor configuration. Different shards (e.g., write-heavy vs read-heavy) might benefit from different tuning.

**Options.**

a) **Single config (status quo).**

b) **Per-shard tuning.** Configurable per shard.

**Recommendation.** Defer. The current config works for typical workloads. If specific shards need different config, an operator can add overrides.

---

## OQ-CC-7: Cross-shard transaction isolation

**Issue.** Brain doesn't support cross-shard transactions. Some applications might need them (rare).

**Options.**

a) **No support (status quo).**

b) **Two-phase commit.** Heavy; adds complexity.

c) **Saga pattern via the client.** Application-level compensating actions.

**Recommendation.** Stay with (a). For applications needing cross-shard atomicity, the client's saga pattern is sufficient.

---

## OQ-CC-8: Memory pressure handling

**Issue.** Under memory pressure, Brain may shed load and degrade gracefully. The current behavior is conservative (reject when CPU > 90% sustained for 5 sec). Could it be smarter?

**Options.**

a) **Threshold-based (status quo).**

b) **Predictive.** Detect rising load earlier and pre-emptively shed.

c) **Per-operation cost.** Higher-cost operations shed first.

**Recommendation.** (c) is reasonable; tracked for v1.1.

---

## OQ-CC-9: Interruptible long-running queries

**Issue.** Some queries (very deep PLAN, large RECALL) take seconds. Currently they run to completion.

**Options.**

a) **Run to completion (status quo).** Cancellable on client disconnect.

b) **Periodic check-in.** The client sends keep-alives; missing them cancels.

c) **Server-side time budget.** Cancel after a configured budget.

**Recommendation.** (c) is partially implemented (request timeout). For v1.x, expose finer-grained controls.

---

## OQ-CC-10: Consistency hint per request

**Issue.** Currently `consistency` has two values (Eventual / ReadAfterWrite). More nuanced options might be useful (e.g., "wait for the writer's queue to drain").

**Options.**

a) **Two-value (status quo).**

b) **Multi-value.** With explicit timing semantics.

**Recommendation.** Stay with two values. The current model handles the common cases.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §15_background_workers open questions


Worker-related questions unresolved as of this spec version.

---

## OQ-BW-1: Worker prioritization within the low-priority pool

**Issue.** All workers are low-priority. But some are more important than others (idempotency sweep vs decay).

**Options.**

a) **Equal priority (status quo).**

b) **Sub-priorities within low.** Critical workers (idempotency, WAL retention) get higher within-low priority.

**Recommendation.** Implement (b) in v1.x. The current model treats all workers equally; some workers becoming behind has different consequences.

---

## OQ-BW-2: Adaptive intervals

**Issue.** Worker intervals are fixed. Could they adapt to load and pending work?

**Options.**

a) **Fixed (status quo).**

b) **Adaptive.** Workers run more often when there's pending work, less when idle.

**Recommendation.** Defer. Fixed intervals are simple and predictable. Adaptive scheduling adds complexity.

---

## OQ-BW-3: Cross-shard worker coordination

**Issue.** Each shard's workers operate independently. For deployments wanting global view (e.g., total decay across shards), there's no coordination.

**Options.**

a) **Per-shard only (status quo).**

b) **Global coordination.** A central coordinator schedules across shards.

**Recommendation.** (a). Brain is per-shard by design; cross-shard coordination is a different system.

---

## OQ-BW-4: Worker resource budgets

**Issue.** Workers don't have explicit CPU/memory budgets per worker. They share the low-priority pool.

**Options.**

a) **Shared pool (status quo).**

b) **Per-worker budgets.** Each worker has a CPU% allocation.

**Recommendation.** Defer. The shared pool is fine for typical workloads.

---

## OQ-BW-5: Manual worker triggering

**Issue.** Operators can stop workers but not manually trigger a cycle. Sometimes they want to force a cycle (e.g., immediate decay after a config change).

**Options.**

a) **No manual trigger (status quo).** Wait for next cycle.

b) **Add `ADMIN_WORKER_RUN_NOW <kind>`.**

**Recommendation.** (b) is useful and simple. Implement in v1.

---

## OQ-BW-6: Worker dependency graph

**Issue.** Some workers logically depend on others (e.g., reclamation depends on slot tombstoning happening first). Brain doesn't model this; it just relies on independent timing.

**Options.**

a) **Independent (status quo).**

b) **Explicit dependencies.** A worker only runs after its dependencies have run.

**Recommendation.** (a) is fine. The implicit timing works because workers are idempotent.

---

## OQ-BW-7: Distributed-mode workers

**Issue.** In a distributed deployment, some workers (e.g., consolidation requiring an LLM call) might benefit from being centralized.

**Options.**

a) **Per-shard (status quo).** Each shard runs its own.

b) **Centralized for some workers.** A leader-elected coordinator handles certain tasks.

**Recommendation.** Defer until v2 (clustered deployments).

---

## OQ-BW-8: Worker hot-config reload

**Issue.** Worker configs are read at startup. Changes require restart.

**Options.**

a) **Restart on change (status quo).**

b) **SIGHUP reload.**

c) **Live reload via admin command.**

**Recommendation.** (c) is operator-friendly. Implement in v1.x.

---

## OQ-BW-9: Worker progress checkpointing

**Issue.** Some workers (decay, edge scrub) have cursors that track progress through a full pass. The cursor is in memory; lost on restart.

**Options.**

a) **In-memory cursor (status quo).** Restart resets to start.

b) **Persistent cursor.** Saved to redb.

**Recommendation.** (b) for workers with long passes. The cost is small (a few bytes); the benefit (continuing where left off) is meaningful for very large shards.

---

## OQ-BW-10: Telemetry granularity

**Issue.** Workers emit metrics at the cycle level. For debugging, per-record telemetry might be useful.

**Options.**

a) **Cycle-level (status quo).** Cheap; coarse.

b) **Per-record on demand.** A debug flag enables per-record logging.

**Recommendation.** (b). Implement as opt-in via configuration. Default off (volume would be too high).

---

## typed-graph worker open questions

Worker-specific deferrals for the typed-graph workers introduced in
§00 §14. Wire-shape questions live in
[`..../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md);
extractor-specific in
[`..../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

### Active

#### Q1 — Decay sweeper

[`./00_purpose.md`](../15_background_workers/00_purpose.md) lists a "Supersession sweeper"
running periodically at low priority. Statement / relation decay
under §02/04's noisy-OR aggregation needs a sweeper that recomputes
confidence on long-stale rows. Brain does not implement this.

**Target:** post-v1. **Status:** deferred.

---

#### Q2 — Resolution workers

Tier-2 (alias) and tier-3 (trigram) entity resolution are
synchronous in Brain. Heavy-load deployments may want
near-foreground tiers. Workers would queue mention-resolution work
behind a backpressure gate.

**Target:** post-v1. **Status:** deferred.

---

#### Q3 — FORGET cascade worker

§10/00 §"Cascading effects of FORGET" describes the cascade. Brain
implements substrate-side cascade; the typed-graph cascade
(statements / relations / entity_mentions referencing the forgotten
memory) is deferred to a follow-on worker.

**Target:** post-v1. **Status:** deferred.

---

#### Q4 — Audit log sweeper

[`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) §5
specifies 90-day default retention. The sweeper itself (periodic
worker that deletes rows + index entries older than the cutoff)
is deferred.

**Target:** post-v1. **Status:** deferred.

---

#### Q5 — Adaptive throttling

[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md) §6 —
Brain ships static queue capacities. Adaptive throttling that lowers
the dispatch rate when queue depth crosses a threshold (rather
than dropping) is a possible later improvement.

**Target:** post-v1. **Status:** deferred.

---

#### Q6 — Cross-shard worker coordination

Each shard runs its own worker queues. A noisy classifier on shard
0 doesn't push back against shard 5's load. Some workloads might
want cluster-wide work-stealing.

**Target:** post-v1. **Status:** deferred.

---

#### Q7 — Queue persistence across restart

[`./00_purpose.md`](../15_background_workers/00_purpose.md) §"Graceful shutdown" mentions
persisting queue state to disk on shutdown. Brain does not
implement persistence — restarts lose in-flight items (which then
trigger fresh extraction on the next ENCODE because the audit
probe misses).

**Target:** post-v1. **Status:** deferred.

---

#### Q8 — Schema migration worker

§00 lists a "Schema migration" worker triggered on schema update.
Brain has no migration (explicit scope cut, §03/07 Q3); this
worker stays as a 1-line placeholder.

**Status:** deferred.

#### Q9 — Full content-aware memory text rebuild

[`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md)
§5 specifies rebuild from authoritative redb tables. `MEMORIES_TABLE`
stores `text_size` but not the text itself (text lives only on the
ENCODE wire path + WAL frames), so Brain's `rebuild_memory_text`
produces a valid empty index — operators re-ingest existing memories
from their own source-of-truth. Full content-aware rebuild needs
either a WAL scan or a parallel text-store table.

**Status:** deferred.

---

#### Q10 — Partial WAL replay on shard recovery

[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)
§6 describes WAL-based replay of unflushed writes at startup.
Brain implements only the full-rebuild path on
`IndexStatus::NeedsRebuild`; for `Ready` indexes, the loss
bound is ≤ N-1 writes per indexer at crash (default N=256 per
§02/13 §3). Cursor-tracked partial replay (stamping
`last_indexed_unix_ms` on the tantivy payload, scanning redb
for rows beyond the cursor at startup) is a deferred improvement.

**Status:** deferred.

---

#### Q11 — Hot rebuild while live writer is running

[`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md)
§5's atomic-rename semantics allow in-flight readers to keep
operating against the old index until they re-open. Brain
implements startup-only rebuild — no coordination with a live
`IndexWriter`. Hot rebuild (e.g. admin-triggered without
restarting the shard) requires writer pause + drain coordination
that the current drain loops don't yet support.

**Target:** post-v1. **Status:** deferred.

---

#### Q12 — Segment-merge windowing during low-traffic intervals

[`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md)
§4 calls out tantivy's segment merge as expensive and notes
Brain relies on `LogMergePolicy` running as part of tantivy's
background merger threads (governed by the shard's I/O budget).
Operators that observe latency hits during merges may want to
window merges into low-traffic intervals.

**Target:** post-v1. **Status:** deferred.

---

#### Q13 — Admin rebuild wire op (`ADMIN_TANTIVY_REBUILD`)

Brain lands the rebuild functions but the on-demand admin
trigger (operator-facing wire op or CLI subcommand) is admin-
surface scope.

**Target:** §02/11 relation admin. **Status:** deferred.

### Resolved

- Per-tier dispatch semantics (sync / near-foreground / background)
  — resolved in [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md).
- Worker overflow policy — `Drop + audit Skipped(queue full) +
  metric`, resolved in §02/13 §6.
- Text-indexer overflow policy — `Backpressure on foreground` (not
  drop). Resolved in [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)
  §1 §6 with full justification (lexical recall is correctness, not
  best-effort).

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §16_sharding open questions


Sharding and clustering questions unresolved as of this spec version.

---

## OQ-SC-1: Consistent hashing for routing

**Issue.** The default hash-modulo routing means changing shard count requires re-routing all agents. Consistent hashing minimizes re-routing.

**Options.**

a) **Hash-modulo (status quo).** Simple; doesn't support shard count changes.

b) **Consistent hashing.** Each shard owns a hash range. Adding a shard splits one range. Most agents stay put.

c) **Rendezvous (highest-random-weight) hashing.** Each agent independently picks the shard with the highest hash. Adding a shard reassigns ~1/N of agents.

**Recommendation.** Implement (b) or (c) in v2. (b) is simpler; (c) gives more uniform spread under shard count changes.

---

## OQ-SC-2: Auto-split

**Issue.** When a shard grows too large, splitting is manual. Could Brain auto-split?

**Options.**

a) **Manual split (status quo, v1).**

b) **Auto-detect; alert; manual confirm.**

c) **Fully automatic.**

**Recommendation.** Stay with manual through v1; add (b) in v1.x; consider (c) in v2 with safety limits.

---

## OQ-SC-3: Multi-shard agent strategies

**Issue.** Multi-shard agents have several distribution strategies (round-robin, sticky-by-context, weighted). Which is best?

**Options.** All of the above, with operator choice.

**Recommendation.** Default to sticky-by-context (so a context's data is in one place; queries within a context don't need fan-out). Round-robin as opt-in for finer-grained spreading. Weighted is v2.

---

## OQ-SC-4: Cross-shard transactions

**Issue.** v1 doesn't support transactions across shards. Some applications might need them.

**Options.**

a) **No support (status quo).** Sagas in the application.

b) **Two-phase commit.** Standard but heavy.

c) **Calvin-style determinism.** Order operations globally; execute deterministically across shards.

**Recommendation.** (a). Cross-shard transactions are complex; Brain's other primitives are usually sufficient.

---

## OQ-SC-5: Replication consistency

**Issue.** When v2 introduces replication, what's the default consistency level?

**Options.**

a) **Async (high throughput, risk of data loss on failure).**

b) **Sync (strong consistency, high latency).**

c) **Quorum (balanced).**

**Recommendation.** (c) for the default. Configurable for tuning.

---

## OQ-SC-6: Replication topology

**Issue.** How replicas are placed (same DC? cross-DC? cross-region?).

**Options.**

a) **Operator config.** Operator specifies topology.

b) **Auto-spread.** Substrate places replicas across failure domains.

**Recommendation.** Both. Operators specify zones; Brain spreads within them.

---

## OQ-SC-7: Cluster size limits

**Issue.** What's the upper bound on cluster size? 10 nodes? 100? 1000?

**Options.**

a) **Small (3-10).** Tractable; well-tested.

b) **Medium (10-100).** Needs more care with gossip / membership.

c) **Large (100+).** Hard; may need different architecture.

**Recommendation.** Target small in v2 initial release; medium in v2.x. Large is a different system.

---

## OQ-SC-8: Failover automation

**Issue.** When a primary fails, who decides the new primary?

**Options.**

a) **Manual (operator).** Slow but safe.

b) **Auto via consensus protocol.** Fast but adds complexity.

**Recommendation.** (b) for v2, using a Raft-based control plane.

---

## OQ-SC-9: Geo-replication

**Issue.** v2 may want to support geo-replication for data residency or latency.

**Options.**

a) **No geo features (just naive cross-DC).**

b) **Geo-aware routing.** Read from the nearest replica.

c) **Geo-pinned shards.** Specific shards always live in specific regions.

**Recommendation.** Defer. Single-region v2 is the priority.

---

## OQ-SC-10: Capacity-based placement

**Issue.** When creating new shards in a cluster, Brain could pick less-loaded nodes automatically.

**Options.**

a) **Operator-specified.** Operator chooses.

b) **Auto-select.** Substrate picks based on metrics.

**Recommendation.** Both, with auto as default.

---

## OQ-SC-11: Online schema migration in clusters

**Issue.** Schema migrations across many shards / nodes need coordination.

**Options.**

a) **Quiesce-and-migrate.** Downtime during migration.

b) **Rolling migration.** Migrate shards one at a time; cluster runs without writes during the window.

**Recommendation.** (a) for v2's first iteration; (b) once the per-shard migration step is fast enough that single-shard quiesces are tolerable.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §17_observability open questions


Observability and operations questions unresolved as of this spec version.

---

## OQ-OO-1: Per-tenant metrics

**Issue.** Brain's metrics are per-shard, not per-agent. For multi-tenant deployments, per-tenant visibility is wanted but cardinality is a concern.

**Options.**

a) **No per-tenant metrics (status quo).** Use logs / audit instead.

b) **Top-N tenants only.** Emit metrics for top consumers; aggregate the long tail.

c) **Configurable per-tenant.** Operators specify which tenants get metrics.

**Recommendation.** (b) for v1.x. Tracks the heavy hitters without exploding cardinality.

---

## OQ-OO-2: Built-in alerting

**Issue.** Brain emits metrics; operators configure alerts in Alertmanager. Some users want built-in alerting (no external dependency).

**Options.**

a) **External (status quo).** Standard Prometheus + Alertmanager.

b) **Built-in alerting.** Brain evaluates alert rules and fires notifications directly.

**Recommendation.** Stay with (a). Prometheus + Alertmanager is mature; reinventing it is unnecessary.

---

## OQ-OO-3: Distributed tracing for cross-shard ops (v2)

**Issue.** Cross-node calls in v2 need careful trace propagation.

**Options.** Standard OpenTelemetry propagation.

**Recommendation.** Implement when v2 clustering arrives. Standard tooling.

---

## OQ-OO-4: Adaptive sampling in tracing

**Issue.** Fixed-rate sampling (e.g., 1%) doesn't capture rare-but-interesting traces. Adaptive sampling captures more errors and slow requests.

**Options.**

a) **Fixed rate (status quo).** Simple.

b) **Tail-based sampling.** Sample after the request, based on outcome.

c) **Adaptive.** Higher rate for errors and outliers; lower for normal.

**Recommendation.** (b) via tracing collector (Tempo, Honeycomb support it). (c) is a possible substrate-side enhancement.

---

## OQ-OO-5: Metric persistence across restarts

**Issue.** Counters reset on restart. Tools (Prometheus) handle this, but some metrics (e.g., total memories ever encoded) are lost.

**Options.**

a) **Reset (status quo).** Counters reset; cumulative views via PromQL.

b) **Persist.** Save counter state to disk; restore on startup.

**Recommendation.** Stay with (a). PromQL handles resets; persistence adds complexity.

---

## OQ-OO-6: Health-check details

**Issue.** Health endpoint returns "healthy" or not. More granular info might help orchestrators.

**Options.**

a) **Binary (status quo).**

b) **Detailed.** Returns per-component health (storage, embedder, workers).

**Recommendation.** Add (b) as `/healthz/detailed`. Keeps `/healthz` simple but offers depth.

---

## OQ-OO-7: Self-healing automation

**Issue.** Some issues have known fixes (HNSW rebuild, restart worker). Brain could auto-fix.

**Options.**

a) **Manual fix (status quo).**

b) **Auto-rebuild.** Substrate auto-rebuilds when threshold hit (already implemented).

c) **Auto-restart workers.** Already implemented for crashed workers.

d) **More automation.** E.g., auto-shed load on memory pressure.

**Recommendation.** (b) and (c) are done. More aggressive automation should be opt-in (operators may not want surprise behavior).

---

## OQ-OO-8: Cost metrics

**Issue.** Operators want per-operation cost (CPU, network, storage). Calculating this is non-trivial.

**Options.**

a) **No cost metrics (status quo).** Operators derive externally.

b) **Approximate cost metrics.** Brain emits resource usage per operation; cost calculated externally.

c) **Cost metrics with pluggable cost models.**

**Recommendation.** (b) is reasonable; tracked for v1.x.

---

## OQ-OO-9: Anomaly detection

**Issue.** Threshold alerts catch known patterns. Anomaly detection (statistical or ML-based) might catch unknowns.

**Options.**

a) **Threshold alerts only (status quo).**

b) **Built-in anomaly detection.**

c) **External tools.**

**Recommendation.** (a) and (c). Brain's metrics work with anomaly detection tools (Datadog Watchdog, etc.). No need for built-in.

---

## OQ-OO-10: Log-to-metric conversion

**Issue.** Some signals are in logs, not metrics. For alerting, metrics are easier.

**Options.**

a) **Manual via log aggregator.** (Loki / Splunk can derive metrics from logs.)

b) **Brain emits the metric directly.**

**Recommendation.** Both. For high-value signals, Brain emits metrics. For everything else, use the log aggregator.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §18_failure_recovery open questions


Failure-recovery questions unresolved as of this spec version.

---

## OQ-FR-1: Built-in off-site backup

**Issue.** Brain doesn't ship cloud-storage integrations for snapshot upload. Operators use external tools.

**Options.**

a) **External (status quo).** Standard Unix tools (S3 sync, etc.) work.

b) **Built-in connectors.** S3, GCS, Azure Blob.

**Recommendation.** Stay with (a) for v1. Reduces Brain's surface area. v1.x may add (b) for convenience.

---

## OQ-FR-2: Continuous backup (write-ahead replication)

**Issue.** Snapshots are point-in-time. WAL records between snapshots are at risk.

**Options.**

a) **Snapshots only (status quo).**

b) **Stream WAL records to off-site immediately.**

c) **Replicate to a remote substrate (v2 feature).**

**Recommendation.** (b) is a v1.x enhancement; (c) is the v2 long-term direction.

---

## OQ-FR-3: Automated failover

**Issue.** v1 has no automatic failover. v2 will (with replication). What's the failover policy?

**Options.**

a) **Manual (v1 default).**

b) **Auto-promote replica.** Detected via heartbeat; consensus protocol promotes.

**Recommendation.** (b) for v2, with operator-tunable thresholds.

---

## OQ-FR-4: Online recovery from corruption

**Issue.** Corruption recovery currently requires offline restoration. Could be online (substrate continues serving while recovering)?

**Options.**

a) **Offline (status quo).** Stop, restore, start.

b) **Online via shadow shard.** Restore to a shadow shard; switch when ready.

**Recommendation.** (b) is appealing but complex. v2.

---

## OQ-FR-5: Subset recovery

**Issue.** Currently, recovery is per-shard. Could it be per-agent (recover one agent's data without affecting others)?

**Options.**

a) **Per-shard (status quo).**

b) **Per-agent recovery.** Restore one agent's memories from snapshot.

**Recommendation.** (b) is a v1.x feature. Requires agent-aware snapshots.

---

## OQ-FR-6: Backup verification

**Issue.** Snapshots could silently be corrupt. The first you'd know is when you try to restore.

**Options.**

a) **Manual verification (status quo).** Operator periodically tests restore.

b) **Built-in verification.** Brain validates backups by attempting restore in a sandbox.

**Recommendation.** (b). Implement as a periodic verification job.

---

## OQ-FR-7: Time-travel queries

**Issue.** With WAL, you could query the state as of any past LSN. Could be useful for debugging.

**Options.**

a) **Not exposed (status quo).**

b) **Read-as-of API.** Query the historical state.

**Recommendation.** (b) is interesting but expensive. Defer; consider for v2.

---

## OQ-FR-8: Data scrubbing

**Issue.** A periodic background scan could verify all stored data integrity.

**Options.**

a) **No scrub (status quo).** Issues found at read time.

b) **Background scrub worker.** Reads everything periodically; verifies CRCs.

**Recommendation.** (b) as opt-in. For deployments wanting proactive corruption detection. Resource-heavy.

---

## OQ-FR-9: Self-healing for partial corruption

**Issue.** If one slot is corrupt, can Brain auto-repair it? Currently it's marked corrupt and ignored.

**Options.**

a) **No auto-repair (status quo).**

b) **Auto-rebuild from WAL.** If the original ENCODE record exists in WAL, replay it to a new slot.

**Recommendation.** (b) is feasible if the WAL is intact for that operation. Implementation has edge cases (multiple updates, etc.). v1.x exploration.

---

## OQ-FR-10: Multi-region DR (v2)

**Issue.** v2 will have replication. Multi-region DR has special concerns (latency, data sovereignty).

**Options.**

a) **Single-region only.**

b) **Async multi-region replication.**

c) **Sync multi-region (high latency).**

**Recommendation.** (b) for v2 initial. (c) is for special cases.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*


## §19_benchmarks open questions


Acceptance and benchmarking questions unresolved as of this spec version.

---

## OQ-BA-1: Targets for non-reference hardware

**Issue.** Targets are for reference hardware. How should they translate to other hardware (smaller, larger, different generation)?

**Options.**

a) **Linear scaling.** Document expected scaling curves.

b) **Per-class targets.** Targets for "small", "standard", "large" hardware.

c) **Per-deployment.** Operators measure their own.

**Recommendation.** (a) plus (c). Document expected scaling; deployments verify their own targets.

---

## OQ-BA-2: SLO vs SLI vs hard target

**Issue.** Latency targets are stated as p99 ≤ 25 ms. Is this a hard contract, or a target with some flexibility?

**Options.**

a) **Hard contract.** Substrate must always meet; failure indicates bug.

b) **SLO target.** Substrate aims to meet 99% of the time; occasional misses are normal.

**Recommendation.** (b). Realistic target with explicit SLO framing. p99 is itself an aggregate over time, so some flexibility is built-in.

---

## OQ-BA-3: Workload realism

**Issue.** The reference workload (70/25/5) may not match all real workloads. Some are write-heavy, others read-heavy.

**Options.**

a) **One reference workload.** Other workloads tested separately.

b) **Multiple reference workloads.** "Read-heavy", "write-heavy", "balanced".

**Recommendation.** (b). Several workloads to characterize Brain's behavior across regimes.

---

## OQ-BA-4: Recall benchmark dataset

**Issue.** Recall depends on the dataset. Synthetic data may not reflect real behavior.

**Options.**

a) **Synthetic only (status quo).**

b) **Real-world datasets.** E.g., publicly available embedding datasets.

c) **Both.**

**Recommendation.** (c). Synthetic for control, real for validation.

---

## OQ-BA-5: Latency targets for slow operations

**Issue.** Some operations (REASON depth-10, full PLAN with text) are slow by design. Targets must be realistic.

**Options.**

a) **Single target per operation type.**

b) **Targets parameterized by inputs.** E.g., REASON depth=N has target f(N).

**Recommendation.** (b). Specific operations have specific targets based on input characteristics.

---

## OQ-BA-6: Cold-start performance

**Issue.** Cold-start is slow (recovery + warm-up). Should this be a target?

**Options.**

a) **No cold-start target.** Excluded from primary targets.

b) **Recovery time target.** "Recover in < N seconds for a 1M-memory shard."

**Recommendation.** (b). Explicit recovery time target (10-30 sec for 1M memories).

---

## OQ-BA-7: Tail behavior under sustained overload

**Issue.** When sustained load exceeds capacity, what's the spec? Latency unbounded? Errors? Drops?

**Options.**

a) **Backpressure.** Return Overloaded errors.

b) **Bounded queues.** Drop after queue fills.

c) **Slow down.** Accept everything but with degraded latency.

**Recommendation.** (a). Already specified. The acceptance tests verify Overloaded is returned cleanly.

---

## OQ-BA-8: Comparison benchmarks

**Issue.** Comparing to other systems is fraught — they have different APIs, models, capabilities.

**Options.**

a) **No comparisons.** Each system stands alone.

b) **Apples-to-apples.** Where APIs match (vector search), compare. Note differences.

c) **Workload-based.** "For this workload, system A: X ops/sec; Brain: Y ops/sec; pgvector: Z ops/sec."

**Recommendation.** (b) and (c). Both useful; honest about limits.

---

## OQ-BA-9: Continuous benchmarking

**Issue.** Tracking benchmark trends over time helps catch slow regressions.

**Options.**

a) **Manual.** Run benchmarks at release time.

b) **CI-integrated.** Run continuously; chart over time.

c) **Public benchmark site.** Like AS-SAFE-Bench or similar.

**Recommendation.** (b) for v1; (c) is aspirational.

---

## OQ-BA-10: Acceptance criteria for "edge" deployments

**Issue.** Brain may run on edge devices (resource-constrained). Different targets apply.

**Options.**

a) **Not supported.** Brain is for servers.

b) **Edge profile.** A subset of features with relaxed targets for edge.

**Recommendation.** (a) for v1; (b) for future. Edge has different constraints; revisit.

---

## OQ-BA-11: Deterministic simulation testing (VOPR-style)

**Issue.** Brain has chaos tests that kill processes mid-WAL-replay. These exercise real recovery paths but run slowly — one fault per test invocation. A deterministic simulator that runs the full write path with mock clock, mock I/O, deterministic random, and injected faults would shake out concurrency and recovery bugs orders of magnitude faster. TigerBeetle's VOPR simulator demonstrates the approach: a Viewstamped Replication state machine fast-forwards weeks of operations with injected faults in minutes.

**Options.**

a) **Status quo.** Chaos tests + property tests + loom for concurrency-critical paths. Acceptable bug-finding velocity.

b) **Add a `brain-simulator` crate.** Mock clock, mock I/O, deterministic random, replay traces. The single-writer-per-shard invariant plus Glommio's cooperative scheduling make Brain a decent candidate — no thread-interleaving non-determinism inside a shard.

c) **Borrow tooling.** Wrap an existing simulator framework rather than write one.

**Recommendation.** (a) for v1. Revisit once chaos-test runtime becomes the limiting factor on recovery-bug velocity. Testing-infrastructure investment of this size needs concrete payoff signal. **Target:** post-v1. **Status:** deferred testing infrastructure.

---

*Continue to [`../00_overview/05_external_references.md`](../00_overview/05_external_references.md) for references.*

