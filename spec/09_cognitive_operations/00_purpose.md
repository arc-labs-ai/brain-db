# 09.00 Purpose

This document specifies what each cognitive operation **means** — not how the substrate implements it, but what the agent gets when it calls one.

## What this document covers

- The semantic contract of each primitive.
- The shape of inputs and outputs.
- The consistency guarantees.
- The error conditions.
- The relationships between primitives (e.g., ENCODE then RECALL).

## What this document does not cover

- **Wire-level details.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **Implementation.** Defined in [08. Query Planner](../08_query_planner/) and the storage docs.
- **SDK ergonomics.** Defined in [13. SDK Design](../13_sdk_design/).

## 1. The "five primitives" framing

Brain's value proposition centers on five cognitive primitives:

- **ENCODE** — write memory.
- **RECALL** — read by similarity.
- **PLAN** — connect via the graph from a state to a goal.
- **REASON** — find evidence for and against.
- **FORGET** — delete.

These map roughly onto cognitive science vocabulary, though Brain doesn't claim to model human cognition. They're useful abstractions for agents.

In addition to the five core primitives, Brain provides:

- **LINK / UNLINK** — direct edge manipulation.
- **TXN_*** — transactional brackets across multiple operations.
- **SUBSCRIBE** — change streams.
- **ADMIN_*** — administrative operations.

## 2. The "declarative" framing

Each primitive is declarative: the agent says **what** it wants, the substrate decides **how**. The agent doesn't need to know about HNSW parameters, ef_search values, or write-ahead logs.

This is similar to SQL vs. low-level B-tree manipulation. The substrate provides primitives at the level of intent.

## 3. The "five-line agent" goal

A primitive should be callable in a single line of agent code. For example:

```
memory_id = brain.encode(text="The user said hi", context="conversation_42")
results = brain.recall("user greeting")
brain.forget(memory_id)
```

No setup, no configuration, no parameters that the agent cares about. The substrate's defaults are sensible for typical agent workloads.

## 4. The agent perspective

From an agent's perspective:

- **ENCODE returns immediately** with a stable identifier.
- **RECALL returns within milliseconds** with similarity-scored results.
- **PLAN and REASON take a bit longer** but are still interactive.
- **FORGET returns quickly** even if the actual reclamation is delayed.
- **LINK is fast** and fits into the same write transactions as ENCODE.
- **TXN_* brackets** are for agents that need atomic groups of operations.
- **SUBSCRIBE** is for agents that want to react to memory changes.

## 5. The substrate's promises

For each operation, the substrate promises:

- **Atomicity** — within a single operation, all sub-steps either complete or none do.
- **Durability** — after acknowledgment, the operation survives crashes.
- **Idempotency** — for state-mutating ops, the same RequestId returns the same result.
- **Eventual consistency for reads** — by default, reads may not see writes from the last few milliseconds.
- **Read-after-write on demand** — reads can be marked to wait for the most recent writes.

## 6. The semantic boundaries

Each primitive has clear semantic boundaries. The substrate doesn't:

- Magically infer what the agent meant beyond what was specified.
- Apply context-aware modifications (e.g., automatic translation).
- Filter results based on undocumented criteria.

Determinism and explicitness are preferred over cleverness.

## 7. The consistency story

Brain provides:

- **Per-shard linearizability for writes.** Within a shard, writes happen in a clear order.
- **Eventual consistency for reads by default.** Reads may not see the latest writes; bounded by ~10 ms.
- **Read-after-write on demand.** A read can be tagged to wait for in-flight writes.

For cross-shard operations, ordering is per-shard; cross-shard ordering isn't enforced (would require global timestamps).

## 8. The transactions story

A transaction (TXN_BEGIN/COMMIT) groups operations atomically:

- All operations succeed together, or none do.
- Reads within the transaction see a consistent snapshot.
- Other clients don't see intermediate states.

Limitations:

- Single-shard only. Cross-shard transactions aren't supported.
- Bounded duration (default 30 sec). Long-held transactions are aborted.

## 9. The subscribe story

SUBSCRIBE delivers changes:

- Each change is a record (memory created/updated/forgotten, edge added/removed).
- Records arrive in WAL order.
- Filters can narrow the stream (per agent, per context, etc.).

Useful for: live agent dashboards, downstream consumers, audit logs.

## 10. The admin story

Admin operations are typically:

- Single-shot (return a result; no streaming).
- Privileged (require admin auth).
- Rare (called by operators, not agents).

Examples: `ADMIN_STATS`, `ADMIN_REBUILD_ANN`, `ADMIN_CONTEXT_DELETE`.

## 11. The compositional story

Agents typically compose primitives:

```
memory = brain.encode("...")
related = brain.recall("...")
brain.link(memory, EdgeKind::SIMILAR_TO, related[0].id)
```

The substrate doesn't have higher-level operations like "encode-and-link". The composition is at the agent level. This keeps the primitive set small.

For very common compositions, ENCODE accepts an inline `edges` parameter. This is a small ergonomic win that doesn't expand the primitive set.

## 12. The "non-magical" rule

Brain's primitives don't:

- Call external LLMs unsolicited.
- Pre-fetch related data the agent didn't ask for.
- Auto-categorize memories beyond what the agent specifies.
- Suggest related operations.

The agent is in control. Brain just remembers.

This is intentional — agents may have constraints (cost, privacy, quality) that Brain shouldn't second-guess. Adding a feature that Brain auto-LLM-summarizes memories crosses a line into agent-policy territory.

## 13. The trade-off space

Each primitive has design trade-offs:

| Primitive | Trade-off |
|---|---|
| ENCODE | Latency vs throughput (group commit) |
| RECALL | Recall vs latency (ef_search) |
| PLAN | Depth vs cost (max_depth) |
| REASON | Coverage vs latency (evidence count) |
| FORGET | Soft vs hard (privacy vs storage) |

Brain exposes the trade-offs as parameters with sensible defaults.

## 14. The "small primitive set" virtue

Brain has ~10 primitives. Each is documented in this spec.

A small set:

- Is easy to learn.
- Composes well.
- Has clear semantics.
- Is stable across versions.

We resist the urge to add primitives. New use cases should be expressible as compositions of existing primitives. If a use case really needs a new primitive, that's a major version event.

---

*Continue to [`01_semantics_overview.md`](01_semantics_overview.md) for the big-picture semantics.*
