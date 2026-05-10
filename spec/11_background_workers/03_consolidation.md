# 11.03 Consolidation Worker

The consolidation worker promotes groups of related Episodic memories into Consolidated memories — summaries that distill the essence of the group.

## 1. The cognitive metaphor

Human memory has a similar process: short-term experiences (episodic) are gradually consolidated into long-term knowledge (semantic). Brain doesn't claim to model this faithfully — but the abstraction is useful.

In Brain:
- Many small Episodic memories accumulate.
- Periodically, the substrate identifies clusters of related memories.
- It generates a summary (a Consolidated memory) that captures the cluster.
- The original Episodic memories remain (linked to the consolidated via DERIVED_FROM edges).

## 2. Why consolidate

Without consolidation:
- The agent's memory is a flat collection of episodic events.
- RECALL returns relevant episodes one at a time.
- The agent has to re-aggregate insights itself.

With consolidation:
- Higher-level summaries are first-class memories.
- RECALL can return the consolidated memory plus its sources.
- The agent gets pre-aggregated context.

## 3. The cycle

Every 5 minutes (configurable):

1. The worker iterates per-context.
2. For each context, find clusters of memories that warrant consolidation.
3. For each cluster, generate a summary (via an LLM call, see §6).
4. Encode the summary as a Consolidated memory.
5. Add DERIVED_FROM edges to source memories.

## 4. Cluster identification

A cluster is a group of memories that:

- Share a context.
- Are episodic.
- Are temporally close (e.g., within a 24-hour window).
- Are similar in vector space (cosine similarity > 0.6 within the cluster).

Algorithms:

- Pull recent episodic memories from the context.
- Group by vector similarity using a simple density-based clustering (DBSCAN-style).
- A cluster needs at least 5 memories (configurable threshold).

The clustering runs on the metadata of memories (no need to load text yet).

## 5. The threshold trigger

The consolidation worker also has a threshold trigger:

- When a context exceeds 50 episodic memories (configurable), schedule consolidation immediately rather than waiting for the next cycle.
- This prevents large contexts from being unrepresented in semantic memory.

## 6. The summarization

Generating the summary requires an LLM. Brain doesn't ship with an LLM; it integrates with an external service:

```rust
async fn summarize(memories: &[MemoryText]) -> Result<String> {
    let prompt = build_consolidation_prompt(memories);
    let response = llm_service.generate(prompt).await?;
    Ok(response.text)
}
```

The substrate's configuration specifies the LLM service URL and credentials. The substrate doesn't bundle Llama, GPT, or any specific model — it's pluggable.

For deployments without an LLM, consolidation is disabled. The substrate works fine without it; clients just don't get Consolidated memories.

## 7. The summarization prompt

The substrate's default prompt:

```
You are a memory consolidation system. Below are several memories from the same context.
Summarize them into a single, concise paragraph that captures the key information.

Memories:
1. {memory_1.text}
2. {memory_2.text}
...

Summary:
```

The prompt is configurable. Operators can tune it for their use case.

## 8. The encoded consolidated memory

After summarization:

1. The summary text is encoded as a new memory (kind = Consolidated).
2. DERIVED_FROM edges are created from the new memory to each source memory.
3. The source memories' metadata is updated with `consolidated_into = new_memory_id`.

This is a single transactional unit: the new memory, its edges, and the metadata updates all commit atomically.

## 9. The Consolidated memory's properties

A Consolidated memory:

- Has its own MemoryId.
- Has its own vector (from embedding the summary text).
- Has DERIVED_FROM edges to all source memories.
- Has a 90-day half-life (slower decay than Episodic).
- Is searchable like any other memory.

Sources are still searchable too — RECALL might return the Consolidated and the Episodics. The agent can then choose what to use.

## 10. The cost

Per cluster:
- Cluster identification: ~10-50 ms.
- Summarization LLM call: 500-2000 ms (network + LLM latency).
- Encoding the summary: ~5-10 ms.
- Edge creation: ~5 ms.

Total per cluster: ~1-2 seconds dominated by the LLM call.

For a typical context's growth rate, consolidation runs occasionally (every few minutes) and produces a few new Consolidated memories per cycle.

## 11. The "skip" cases

Consolidation skips clusters that:

- Have already been consolidated (memories link to an existing Consolidated).
- Have very high similarity (a near-duplicate; unlikely to add value).
- Have very low coherence (vectors are scattered; not a real cluster).

Skipped clusters may be revisited in future cycles if the membership changes.

## 12. The "consolidation of consolidations"

A Consolidated memory is a memory like any other. It can itself be part of a cluster of related Consolidated memories. The worker can recursively consolidate.

In practice, this is rare — the second-order summaries are at a high abstraction level. The substrate caps recursive consolidation at 2-3 levels.

## 13. The "context boundary"

Consolidation respects context boundaries:
- Episodic memories from one context don't get consolidated with those from another.
- Each context's consolidation is independent.

This matches the agent-data-model expectation: contexts are bounded scopes.

## 14. The "active" filter

Only active (non-tombstoned) memories are consolidated. Tombstoned memories are skipped — they're going away anyway.

If a source memory is FORGOTTEN after its Consolidated derivation:
- The DERIVED_FROM edge points to a tombstoned source.
- The Consolidated memory still exists.
- Eventually, the maintenance worker cleans up the orphaned edge.

The Consolidated memory's text doesn't change — it summarizes what was true at consolidation time.

## 15. The "approval" workflow option

Some applications want human approval of consolidations before they're stored. The substrate supports a "draft" mode:

- The summary is generated and stored as a draft (not yet a memory).
- A client (a UI, presumably) reviews and approves.
- On approval, the substrate encodes the memory.

This is an opt-in mode, configured per-context. By default, consolidation is fully automatic.

## 16. The disabled state

If the LLM service is unavailable or consolidation is disabled, the worker becomes a no-op:

```toml
[workers.consolidation]
enabled = false
```

The substrate works fine. Just no Consolidated memories.

## 17. The consolidation latency vs freshness trade-off

- More frequent consolidation: more LLM calls, fresher summaries.
- Less frequent: fewer LLM calls, staler summaries (memories that should be consolidated wait).

Operators tune the interval based on:
- LLM cost.
- Workload write rate.
- Acceptable consolidation lag.

## 18. The "quality" question

How good are the auto-generated summaries? Depends on the LLM. Brain doesn't validate quality — that's the LLM's job.

If summaries are poor (bad LLM, bad prompt), consolidation can produce noise. Operators may disable consolidation until the quality is acceptable.

For high-quality summaries, the substrate's value-add is significant: the agent gets pre-aggregated context.

---

*Continue to [`04_hnsw_maintenance.md`](04_hnsw_maintenance.md) for HNSW maintenance.*
