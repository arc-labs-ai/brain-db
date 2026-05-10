# 09.12 Open Questions

Cognitive-operations questions unresolved as of this spec version.

---

## OQ-CO-1: Salience-weighted ranking in RECALL

**Issue.** RECALL currently ranks purely by similarity. Salience (importance) doesn't influence ranking — only filtering.

**Options.**

a) **Score-only (status quo).** Ranking by cosine similarity.

b) **Blended ranking.** `final_score = alpha * similarity + (1 - alpha) * salience`.

c) **Per-request control.** A `ranking` parameter the client sets.

**Recommendation.** Defer. Most agents either don't care about salience or do their own re-ranking. The substrate's pure-similarity is predictable.

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

c) **Saga pattern in SDK.** Application-level compensating actions.

**Recommendation.** (c). The SDK provides saga helpers; the substrate stays simple.

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

**Issue.** All queries see the current state. There's no way to see "what did the substrate look like an hour ago?"

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

**Recommendation.** Stay with (a). Brain isn't a graph database; we don't want to compete with Neo4j etc. on graph-query expressiveness.

---

*Continue to [`13_references.md`](13_references.md) for references.*
