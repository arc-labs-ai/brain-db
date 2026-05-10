# 12. Sharding + Clustering

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Operators; cluster-mode implementers |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_system_architecture/), [05. Storage](../05_storage_arena_wal/), [10. Concurrency](../10_concurrency_epochs/) |
| Referenced by | [13. SDK Design](../13_sdk_design/), [14. Observability + Operations](../14_observability_ops/) |

## What this spec defines

How Brain partitions data across shards and (in future versions) across nodes in a cluster. The single-node sharding model is fully specified for v1; clustered deployments are sketched and tracked as future work.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_shard_model.md`](01_shard_model.md) | The shard as a unit |
| [`02_routing.md`](02_routing.md) | Routing operations to shards |
| [`03_shard_assignment.md`](03_shard_assignment.md) | How agents map to shards |
| [`04_single_node.md`](04_single_node.md) | Single-node deployment |
| [`05_clustered.md`](05_clustered.md) | Clustered deployment (future) |
| [`06_rebalancing.md`](06_rebalancing.md) | Moving shards between nodes |
| [`07_replication.md`](07_replication.md) | Replication (future) |
| [`08_failure_modes.md`](08_failure_modes.md) | Failure modes |
| [`09_open_questions.md`](09_open_questions.md) | Unresolved questions |
| [`10_references.md`](10_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
