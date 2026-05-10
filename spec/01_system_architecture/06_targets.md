# 01.06 Capacity Targets and Scaling Envelope

This file states the numbers Brain commits to. They are the contract between the architecture and the operator: given the hardware envelope from [`05_hardware.md`](05_hardware.md), here's what the system delivers.

These targets are validated by the benchmark suite specified in [16. Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/). When a target says "p99 ≤ 25 ms", that's a number the benchmark suite must measure and pass before a release ships.

## 1. Per-shard targets

A shard is the unit of internal scaling within a node. Targets are per-shard, assuming a typical hardware tier (commodity NVMe, 16-core CPU, 64 GiB RAM).

### 1.1 Capacity

| Metric | Target | Notes |
|---|---|---|
| Memories per shard | 10⁶ – 10⁷ | Sweet spot is 1–10 million. Beyond 10M, TLB pressure on the arena starts to matter (see [`05_hardware.md`](05_hardware.md) §3.3). |
| Active connections per shard | up to 1000 | Each connection costs 2–4 KiB resident. 1000 connections per shard is a soft limit; spread across shards if higher. |
| Concurrent in-flight operations per shard | 10–100 | Bound by the shard's writer task. Reads scale much higher. |

### 1.2 Latency

CPU embeddings (no GPU):

| Metric | p50 | p99 | p99.9 |
|---|---|---|---|
| `ENCODE` | ≤ 12 ms | ≤ 25 ms | ≤ 50 ms |
| `RECALL` | ≤ 8 ms | ≤ 20 ms | ≤ 40 ms |
| `FORGET` | ≤ 3 ms | ≤ 10 ms | ≤ 25 ms |
| `PLAN` (simple) | ≤ 50 ms | ≤ 200 ms | ≤ 500 ms |
| `PLAN` (complex, budget-bound) | depends on budget | depends on budget | depends on budget |
| `REASON` | ≤ 100 ms | ≤ 500 ms | ≤ 2000 ms |

GPU embeddings (CUDA available):

| Metric | p50 | p99 | p99.9 |
|---|---|---|---|
| `ENCODE` | ≤ 3 ms | ≤ 8 ms | ≤ 20 ms |
| `RECALL` | ≤ 2 ms | ≤ 5 ms | ≤ 12 ms |

Cache-hit latency (cue text already embedded recently):

| Metric | p50 | p99 |
|---|---|---|
| `RECALL` (cue cache hit) | ≤ 1.5 ms | ≤ 4 ms |

Latency targets are measured at the protocol layer — from receipt of the request frame at the server to the moment of writing the first response byte. Network transit is excluded; SDK overhead is included.

### 1.3 Throughput

Per-shard throughput targets:

| Operation | Target sustained | Notes |
|---|---|---|
| `ENCODE` (CPU embedding) | 100–200/s | Dominated by embedding inference. |
| `ENCODE` (GPU embedding) | 1K–5K/s | GPU batching makes a big difference. |
| `ENCODE` (storage-only, vector pre-supplied) | 200K/s | Bypasses embedding; storage layer's max. |
| `RECALL` (CPU, cache-cold) | 100–200/s | Embedding-bound. |
| `RECALL` (CPU, cache-warm) | 5K–10K/s | HNSW search is fast when embedding is cached. |
| `RECALL` (GPU) | 1K–5K/s | GPU embedding amortized across batch. |
| `FORGET` | 5K/s | No embedding; just a small write. |

Throughput is sustained, not burst. Burst capacity is higher (the system has buffering), but sustained is what matters for capacity planning.

### 1.4 Recovery

| Metric | Target |
|---|---|
| Recovery time per GiB of WAL | ≤ 30 s |
| Recovery time per shard (typical, post-checkpoint) | ≤ 5 s |
| Recovery time per shard (worst case, full WAL) | ≤ 60 s per million memories |

Recovery is parallel across shards: a node with 10 shards recovers them all at once, so total recovery time is the slowest shard, not the sum.

### 1.5 Memory overhead

| Metric | Target |
|---|---|
| Per-memory metadata overhead (in-memory, working set) | ≤ 100 bytes |
| Per-memory disk overhead (excluding vector) | ≤ 200 bytes |
| Per-shard fixed overhead | ≤ 50 MiB |
| Per-connection overhead | ≤ 4 KiB |

These exclude the vector itself (1.5 KiB per memory at 384-dim `f32`) and the HNSW edges (~150 bytes per memory at typical M=16 settings).

---

## 2. Per-node targets

A node is a single Brain process. Targets are per-node, assuming the recommended hardware tier (16 cores, 64 GiB RAM).

### 2.1 Aggregate capacity

| Metric | Target |
|---|---|
| Shards per node | 1–100 |
| Total memories per node | 10⁷ – 10⁹ |
| Active connections per node | up to 50,000 |
| Aggregate `RECALL` QPS | 50K – 500K (CPU), workload-dependent |
| Aggregate `ENCODE` QPS | 10K (CPU), 50K+ (GPU) |

The 100-shard upper bound is soft; it reflects when a single node's working set, background work, and connection state start to compete.

### 2.2 Resource utilization

| Metric | Target at warm steady state |
|---|---|
| Resident memory | ≤ 25% of provisioned RAM |
| CPU utilization (request-serving cores) | ≤ 70% sustained |
| Disk I/O | bound by NVMe device |
| Network | bound by NIC |

Headroom matters: a node at 70% CPU has bursts to 100% during load spikes; a node at 95% CPU is operating in queueing-theory's bad regime where any spike causes queue pile-up.

### 2.3 Background work

Background workers run on cores reserved away from the request-serving pool. Targets:

| Worker | Resource use |
|---|---|
| Decay sweep | ≤ 5% of one core, average |
| Consolidation | ≤ 10% of one core, average; bursts during sweeps |
| HNSW maintenance | ≤ 20% of one core, only during active rebuilds |
| Snapshot | bursts of disk I/O during the snapshot operation |

For a 16-core node with 12 request-serving cores and 4 background cores, the budget is comfortable.

---

## 3. Cluster targets

A cluster is the collection of all nodes serving a single Brain deployment.

### 3.1 Scale

| Metric | Target |
|---|---|
| Nodes per cluster | 1–1000 (no built-in upper limit) |
| Shards per cluster | up to 65,535 (16-bit shard ID space) |
| Total memories per cluster | up to 10¹³ (theoretical, far beyond expected production) |
| Cross-node hot-path queries | none |

The lack of an upper limit on nodes is a design choice: each node is independent, the router is stateless, and there's no global coordination on the hot path. Cluster-level operations (rebalancing, gossip, etc.) scale at most O(N log N) in node count.

### 3.2 Cross-node latency

The router adds latency between client and shard owner:

| Metric | Target |
|---|---|
| Router added latency | ≤ 200 µs |
| Cross-node bandwidth | bound by network |
| Failover time on shard owner crash | ≤ 30 s (single-replica; manual restoration) |

The router's 200 µs target assumes:

- Stateless dispatch (no lookups, single hashmap consult).
- Short-lived connections to shard owners (no per-request connection setup).
- Same data center, sub-millisecond network RTT.

### 3.3 Rebalancing

When shards are moved between nodes:

| Metric | Target |
|---|---|
| Shard rebalancing time | ≤ 5 minutes per GiB of shard data |
| Cluster availability during rebalance | 100% for unaffected shards; brief unavailability per rebalanced shard |
| Rebalance throughput | bound by network and source/destination disk |

Rebalancing is performed during off-peak windows by default. Emergency rebalancing during peak load is supported but increases the rebalance time and may affect tail latency on the source node.

---

## 4. Quality targets

These aren't latency or throughput; they're correctness-flavored quality bars.

### 4.1 Recall quality (ANN)

| Metric | Target |
|---|---|
| Recall@10 vs brute force | ≥ 0.95 |
| Recall@100 vs brute force | ≥ 0.98 |

Measured on standardized benchmarks. The HNSW configuration parameters (M, ef_construction, ef_search) are tuned to hit these targets. See [06. ANN Index](../06_ann_index/) §5 for the tuning methodology.

### 4.2 Confidence calibration

The substrate emits a `confidence` value in [0, 1] for `RECALL` results. Calibration target:

| Metric | Target |
|---|---|
| Calibration error (Expected Calibration Error) | ≤ 0.10 on benchmark dataset |

A confidence of 0.8 should mean "80% chance this is the correct/relevant memory", measured against ground-truth labels.

### 4.3 Durability

| Metric | Target |
|---|---|
| Acknowledged write durability | 100% (after WAL fsync) |
| Lost-write rate | 0 in normal operation |
| Window of vulnerability after WAL fsync | 0 |
| Window of vulnerability before WAL fsync | bounded by group commit interval (≤ 200 µs) |

A write that the client sees acknowledged is durable: it survives an immediate process crash or host crash. A write that the client has not yet seen acknowledged may be lost; idempotent retry recovers.

### 4.4 Consistency

| Metric | Target |
|---|---|
| Per-shard linearizability | guaranteed |
| Cross-shard linearizability | NOT guaranteed |
| Read-after-write within a session | guaranteed |
| Read-after-write across sessions to the same shard | guaranteed |

A session that just `ENCODE`d a memory can immediately `RECALL` it. Two different sessions writing to the same shard observe each other's writes after they commit. Cross-shard, the order of writes is undefined.

---

## 5. What we are not optimizing for

The targets above are conscious choices. The following are *not* optimization goals:

### 5.1 Sub-microsecond hot-path

We accept multi-millisecond latency for embedding inference; that's the floor. Optimizing the rest of the system to sub-microsecond would not move the user-perceived metric.

For deployments that *can* bypass embedding (using `ENCODE_VECTOR_DIRECT` with pre-computed vectors), sub-millisecond `ENCODE` is achievable. But the typical-user latency target reflects the typical-user code path.

### 5.2 Petabyte-scale single-shard

A single shard tops out at ~10⁷ memories. Very-large-scale agents shard at the application level (e.g., one Brain shard per agent's project, or per time window). The architecture doesn't support a single shard at petabyte scale, and we're not planning to add it.

### 5.3 Multi-region active-active

The cluster is single-region. Cross-region replication is supported as a disaster-recovery / read-replica feature (out of scope for v1), but multi-region active-active — where writes go to any region and replicate everywhere — is not on the roadmap.

### 5.4 Strong cross-shard consistency

Each shard is internally linearizable. Cross-shard operations (the rare admin migrations) are eventually consistent. Brain doesn't aim to be a cross-shard transaction system.

### 5.5 Broad multi-tenancy isolation

Each agent is isolated by shard, but Brain doesn't enforce strict resource quotas across tenants on shared infrastructure. A heavy-load agent on shard A doesn't directly affect agent B on shard B (different cores, different storage), but they share NIC bandwidth, page cache, and disk capacity. For hard isolation, run separate Brain clusters.

### 5.6 Browser-side / client-side embedded use

Brain is a server. There is no embedded mode that runs in a browser or mobile process. The architecture (mmap'd files, glommio, io_uring) is fundamentally server-side.

---

## 6. How these targets are validated

The benchmark suite ([16. Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/)) contains tests for each target. The release criteria require all targets to pass on the reference hardware.

Targets that don't pass don't quietly slip — they either get fixed before release or get explicitly downgraded with a documented reason. The point of having them in writing is to keep that conversation honest.

Targets are reviewed and potentially updated each major version. They are not promises forever; they're promises for this version.

---

*Continue to [`07_non_goals.md`](07_non_goals.md) for explicit non-goals.*
