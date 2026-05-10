# 09.05 REASON

The REASON primitive: find supporting and contradicting memories for a query.

## 1. Semantic contract

```
REASON(query_text, agent_id, max_supporting, max_contradicting, ...) → ReasonResponse
```

The substrate:

1. Embeds the query text.
2. Finds memories near the query (the "base set").
3. From the base set, follows SUPPORTS / DERIVED_FROM edges to find supporting evidence.
4. From the base set, follows CONTRADICTS edges to find opposing evidence.
5. Aggregates and returns evidence with scores and confidence.

## 2. The arguments

### query_text

The claim or question. The substrate doesn't parse it as a logical proposition — it's just text to embed and lookup.

### agent_id

The owning agent. Reasoning is scoped to this agent's memories.

### max_supporting

How many supporting items. Default 5; max 50.

### max_contradicting

How many contradicting items. Default 5; max 50.

### include_text

Whether to return memory text in the response. Default true (REASON is meant to be interpretable).

### confidence_min

Optional. Filter out evidence with low individual confidence (similarity score below threshold).

## 3. The response

```rust
struct ReasonResponse {
    supporting: Vec<EvidenceItem>,
    contradicting: Vec<EvidenceItem>,
    confidence: f32,                 // Aggregate; balance of evidence
    base_memories: Vec<MemoryId>,    // The seed memories
}

struct EvidenceItem {
    memory_id: MemoryId,
    text: Option<String>,
    score: f32,                      // Individual confidence (0..1)
    edge_path: Vec<EdgeKind>,        // How this connects to the query
    distance: usize,                 // Graph distance from base set
}
```

## 4. The "supporting" semantics

A memory is "supporting" if:

- It's directly similar (high score) to the query.
- AND/OR it's reached from the base set via SUPPORTS or DERIVED_FROM edges.

Both kinds of evidence are returned. Similarity-only support is weaker (just thematic relevance). Edge-traversed support is stronger (explicit assertion).

## 5. The "contradicting" semantics

A memory is "contradicting" if:

- It's reached from the base set via CONTRADICTS edges.
- OR it's similar in topic but with significantly different content (this is harder to detect; see § 11).

For v1, the substrate primarily uses CONTRADICTS edges. Vector-distance-based contradiction is research-grade and not reliable enough.

## 6. The aggregate confidence

The aggregate `confidence` is roughly:

```
support_strength = sum(supporting.score)
contradict_strength = sum(contradicting.score)

confidence = (support_strength - contradict_strength) / (support_strength + contradict_strength)
```

Range: -1 (all contradicting) to +1 (all supporting). 0 means balanced.

This is a heuristic. Agents shouldn't use confidence as a hard truth value — it's a hint about the balance of evidence.

## 7. The "base memories" output

The response includes which memories were the seeds:

- `base_memories`: top similar memories to the query.
- These are the starting points for evidence traversal.

Agents can use this to verify the substrate is reasoning about the right topic.

## 8. The "edge_path" output

For each evidence item, the response shows how it relates to the base:

- `[]`: directly similar (no edge traversal).
- `[SUPPORTS]`: one hop through a SUPPORTS edge.
- `[DERIVED_FROM, SUPPORTS]`: two hops.

Up to depth 2 by default. Longer paths are weaker evidence.

## 9. Latency

For typical REASON:

- p50: ~30 ms.
- p99: ~70 ms.

The latency is similar to PLAN but typically faster because depth is smaller (default 2 vs 4) and edge types are fewer.

## 10. The "no contradicting evidence" case

Often, REASON finds support but no contradictions. The response has `contradicting: []`. This indicates the agent's memory is consistent with the query.

If the agent's memory is biased (only one perspective is encoded), REASON's responses will be biased too. The substrate doesn't fact-check the memory.

## 11. The "no support, no contradiction" case

If the query is about something the agent has no memory of:

- `supporting: []`
- `contradicting: []`
- `confidence: 0.0`
- `base_memories: []` (no similar memories found).

The agent should interpret this as "I don't know — I have no memory about this".

## 12. The vector-distance contradiction question

We've considered using vector distance to flag potential contradictions:

- Memory M is similar to the query in topic (mid-range score).
- But its content vector points in a noticeably different direction.

This is research-grade. It tends to flag false positives (similar topic, different angle, but not actually contradicting).

For v1, we don't do this. CONTRADICTS edges (explicitly created by the agent or by a downstream LLM) are the contradiction signal.

A future enhancement: integrate with an LLM-based contradiction detector. The substrate would generate candidate pairs (query + memory) and let an external LLM judge contradiction. Out of scope for v1.

## 13. The "explain" option

With `explain=true`, the response includes:

- Why each evidence item was selected.
- Which edges were traversed.
- Per-edge confidence.

Useful for showing reasoning chains to humans or other systems.

## 14. The "different from PLAN" semantic

PLAN: "how do I get from A to B?" — finds connections.
REASON: "what supports/contradicts X?" — finds evidence.

Different goals, similar mechanics (both traverse the graph). The edge sets are different:

- PLAN: forward edges (CAUSED, FOLLOWED_BY).
- REASON: associative edges (SUPPORTS, CONTRADICTS, DERIVED_FROM).

## 15. The "REASON about a memory" pattern

A common pattern: the agent has a specific memory and wants evidence for or against it. Two approaches:

1. Use the memory's text as the query to REASON.
2. Use REASON-by-id (a future addition; not in v1).

Currently, the agent passes the memory's text. The substrate embeds it and reasons; results may include the memory itself in the base set.

## 16. The "confidence is a hint" warning

The aggregate confidence is a rough indicator. It's not:

- A probability of truth.
- A measure of the substrate's certainty about the world.
- A score the agent should use as a hard cutoff.

It reflects the balance of stored memories. If the memory is wrong, biased, or incomplete, the confidence is too.

Agents should treat confidence as one input among many, not the final word.

## 17. The "edge weight" effect

Edges have weights. REASON uses them in scoring:

```
evidence_strength = base_similarity × product(edge.weight along path)
```

A high-weight SUPPORTS edge contributes more than a low-weight one. Agents that create edges with calibrated weights get better REASON results.

## 18. The "REASON without memories" case

If the agent has zero memories matching the query:

- `base_memories: []`.
- `supporting: []`, `contradicting: []`.
- `confidence: 0.0`.

The substrate isn't generating evidence — only retrieving. No memories means no reasoning.

## 19. The "small evidence" case

For agents with few memories on a topic, REASON returns weak evidence:

- A handful of items with low scores.
- Confidence near zero either way.

The agent can use this as a signal to seek more information (encode more memories from external sources, do web searches, etc.).

---

*Continue to [`06_forget.md`](06_forget.md) for FORGET.*
