# 01.10 Open Questions

This file lists architecture-level questions that are unresolved as of the current spec version. Surfacing them is preferable to hiding them. Each question states the issue, the options considered, and a current recommendation.

These differ from per-spec open questions (which appear in each detail spec): the questions here cut across the architecture, often involving multiple subsystems.

---

## OQ-1: TLB pressure on very large arenas

**Issue.** [`05_hardware.md`](05_hardware.md) §3.3 acknowledges that `MADV_HUGEPAGE` does not apply to file-backed mmaps on regular filesystems. For arenas exceeding ~16 GiB, TLB (Translation Lookaside Buffer) pressure becomes measurable: the CPU spends a non-trivial fraction of its time waiting for page-table walks instead of doing useful work.

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

**Issue.** The architecture is non-modal at the storage and index layers, but the embedding layer and cognitive operations assume text. Multi-modal agent applications (image search, video memory, audio transcript memory) need image / audio / multi-modal embedding support.

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

c) **Application-level federation.** Don't add cross-agent at the substrate level; let applications query multiple agents via separate connections.

**Recommendation.** Out of scope. Make cross-agent an explicit non-goal in [`07_non_goals.md`](07_non_goals.md) (already there). If a clear use case emerges with concrete latency tolerance, revisit.

---

## OQ-5: External vector ingestion

**Issue.** Should Brain accept pre-computed vectors from clients that have their own embedding pipelines (e.g., domain-specific or multi-modal models)? The architecture supports it (the protocol can carry a vector directly), but the cognitive operations assume embedding ownership.

**Options.**

a) **Support as a power-user override.** Default is text-in; an alternate `ENCODE_VECTOR_DIRECT` opcode lets advanced users pass vectors. Vectors carry their model fingerprint.

b) **Refuse external vectors.** Brain owns embedding entirely; clients without compatible models can't use Brain.

c) **Support fully.** Treat external vectors as first-class; the embedding layer becomes optional.

**Recommendation.** **Support as a power-user override (option a).** This is already in the spec ([03. Wire Protocol](../03_wire_protocol/) §7.4). Default is text-in; advanced users have the override. The cognitive operations work with any vector that has a known model fingerprint.

---

## OQ-6: Vector quantization

**Issue.** The current spec assumes `f32` vectors (4 bytes per dimension, 1.5 KiB per 384-dim vector). `i8` quantization gives 4× density at modest recall cost. Implementation cost is moderate; benefits are workload-dependent.

**Options.**

a) **Implement `i8` quantization** as an alternate arena format. Per-shard configuration; quantized arenas are 4× denser but pay a small recall hit.

b) **Implement product quantization (PQ)** for even higher compression. Larger recall hit, larger implementation cost.

c) **Stay `f32`-only.** Simpler; recall quality is preserved.

**Recommendation.** Defer; revisit after first benchmark cycle reveals whether storage density is actually a problem.

---

## OQ-7: Schema evolution beyond two versions

**Issue.** The spec commits to format version N supporting N-1 reading. Some operators may want longer support windows (running version N for years across version-N+1, N+2, N+3 deployments).

**Options.**

a) **Expand to two versions** (N supports N-2). Doubles the test matrix and the maintenance burden.

b) **Stay at one version** (N supports N-1). Simpler; rolling upgrades work; long-term frozen deployments must upgrade.

c) **Define a long-term-stable subset.** A subset of formats committed to indefinitely; new formats opt-in via a feature flag.

**Recommendation.** Stay at one version. If user demand justifies the maintenance cost, revisit. Most real deployments roll forward; very long compatibility windows are a small fraction of users at high cost.

---

## OQ-8: Procedural memory

**Issue.** v1 explicitly excludes procedural memory (skills, policies, executable patterns). Could it be added later?

**Options.**

a) **Treat procedural memory as a kind variant.** Add `Procedural` to the `MemoryKind` enum. Procedural memories store executable content (e.g., agent action templates).

b) **Build a separate substrate for procedural memory.** Different access patterns, different storage characteristics.

c) **Defer indefinitely.** Procedural memory belongs in the LLM's prompt or fine-tuning, not in a memory substrate.

**Recommendation.** Out of scope for the foreseeable future. Procedural memory in modern LLM stacks lives in tool-use schemas and prompt engineering; the substrate shape Brain offers (vector + metadata + edges) doesn't naturally fit executable content. If a clear use case emerges, revisit.

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

**Recommendation.** This needs a dedicated decision in [12. Sharding + Clustering](../12_sharding_clustering/). The architecture-level position: the substrate is independent of the choice; the router needs *some* mapping; the choice is operational.

---

## OQ-12: Observability for cognitive operations

**Issue.** Standard observability (latency histograms, error rates, throughput) maps cleanly to operations. But cognitive operations have a less-standard observability story: was a `RECALL` "good"? Did the planner make a "good" choice? How would we know?

**Options.**

a) **Quality metrics on benchmark dataset.** The substrate runs a periodic self-test against a fixed benchmark, reports recall quality, calibration error, etc. Useful for regression detection.

b) **Sampled traces with rich attributes.** Every Nth `RECALL` is fully traced, including planner decisions and intermediate scores. Useful for debugging specific issues.

c) **Quality signal from clients.** Clients optionally provide feedback (was this the right memory?). Substrate uses signals for calibration. Adds protocol surface; clients must instrument.

**Recommendation.** Need all three eventually. Specify benchmarks in [16. Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/); specify tracing in [14. Observability + Operations](../14_observability_ops/); defer client feedback signal to v2.

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

**Recommendation.** Trust the design for v1. The primitives map onto distinct operational profiles (read-mostly, write, search, inference, deletion) and to distinct cognitive operations. If specific applications struggle to express what they need, revisit.

---

## OQ-15: Operational footprint

**Issue.** Brain depends on Glommio, candle, redb, hnsw_rs, and several smaller crates. Each is a maintenance dependency. Some are mature (redb, candle); some are smaller communities (Glommio, hnsw_rs).

**Options.**

a) **Trust the dependencies.** Each was chosen carefully; the alternatives are worse.

b) **Vendor critical pieces.** Fork Glommio and HNSW into our tree; absorb maintenance.

c) **Aggressively narrow.** Re-implement from first principles where the dependency is risky.

**Recommendation.** Trust the dependencies in v1. Track each one's health (maintenance activity, issue resolution) and revisit if any becomes a stagnant hazard. Vendoring is a fallback if a critical dependency stalls.

---

*Continue to [`11_references.md`](11_references.md) for further reading.*
