# 01.05 Hardware and Capacity Targets

> **TL;DR.** Hardware envelope Brain is calibrated for (Linux kernel ≥5.15, x86_64/aarch64 with AVX2/NEON, NVMe SSD, 64 GiB RAM target) and the capacity / latency / throughput numbers Brain commits to given that envelope. Per-shard targets feed the per-node and per-cluster envelopes; all numbers are validated by the benchmark suite in §19.

This file documents the deployment-target assumptions and the capacity / latency / throughput targets Brain commits to given that envelope. Capacity targets appear in [§7 Capacity Targets](#capacity-targets) below.

## 1. Operating system

**Linux**, kernel ≥ 5.15.

The 5.15 kernel is the LTS series with mature `io_uring`. Earlier kernels have `io_uring`, but its surface evolved rapidly through 5.6–5.15; 5.15+ has the API stability we depend on. Kernel 6.x is recommended for newer features (faster io_uring fixed-buffer support, better large-folio readahead).

### 1.1 Why Linux-only

Brain depends heavily on Linux-specific I/O facilities:

- **`io_uring`** — Linux-only. macOS uses kqueue, Windows uses IOCP; both have similar capabilities but different APIs and different latency characteristics. Glommio is built directly on `io_uring` and has no portable backend.
- **`O_DIRECT`** — Linux's interpretation differs significantly from FreeBSD's; macOS doesn't have it; Windows has unbuffered I/O via `FILE_FLAG_NO_BUFFERING` with different semantics.
- **The specific `madvise` flags** Brain uses (`MADV_RANDOM`, `MADV_DONTDUMP`) — non-Linux equivalents exist but aren't byte-compatible.
- **`fallocate` with `FALLOC_FL_KEEP_SIZE`** — Linux-specific behavior.

Brain considered abstracting these. The conclusion: the abstraction would be either leaky (revealing platform differences in tail latency) or bloated (multiple I/O backends). For a system whose value proposition is latency, a single optimized backend is better than a portable one.

### 1.2 Other operating systems

Out of scope. Other OS targets would be a separate project.

For local development on macOS or Windows, run Brain in a Linux container under Docker Desktop, OrbStack, or Lima. Tail latency in the container is not representative of native performance, but functional correctness and basic throughput are unaffected.

### 1.3 Container runtimes

Brain runs fine inside Docker, Kubernetes, or any OCI-compliant runtime. Operational considerations:

- **`io_uring` access** — must be permitted by the seccomp profile. Default Docker seccomp blocks some `io_uring` operations; the container needs `--security-opt seccomp=unconfined` or a custom profile that permits `io_uring_setup`, `io_uring_enter`, `io_uring_register`.
- **`memlock` rlimit** — Glommio requires at least 512 KiB of locked memory for `io_uring` to work, per its [README](https://github.com/DataDog/glommio). The container needs `--ulimit memlock=-1` or equivalent.
- **`fsync` semantics** — must be honored end-to-end. Some container storage drivers (overlayfs, fuse-based) introduce subtle fsync semantics. NVMe-backed bind mounts or volumes with proper fsync are strongly recommended.

---

## 2. CPU

**x86_64** with SSE 4.2, **OR** **ARM64** with the CRC32 extension.

### 2.1 SIMD requirements

Both SIMD requirements are widely available:

- **SSE 4.2 on x86_64** — Intel Nehalem (2008) and AMD Bulldozer (2011) and later. Provides hardware-accelerated CRC32C used for WAL record checksums.
- **ARMv8.0+ on ARM64** — the optional CRC32 extension is mandatory in ARMv8.1 (released 2014). Modern AArch64 server CPUs (AWS Graviton, Apple Silicon, Ampere Altra) all support it.

Brain uses SIMD for vector dot products: AVX2 on x86 (256-bit, 8 floats per instruction), NEON on ARM64 (128-bit, 4 floats per instruction), with portable fallbacks (`std::simd` or [`wide`](https://github.com/Lokathor/wide)) for cores lacking either.

AVX-512 is detected at runtime and used opportunistically when available, but not required. AVX-512 doubles the SIMD width but isn't broadly available outside server-grade Intel CPUs.

### 2.2 Core count

| Tier | Cores | Use |
|---|---|---|
| Minimum | 4 | Development, single-tenant agents. Fewer cores leave no headroom for background workers. |
| Recommended | 8–32 | Production workload per node. Each core can serve a meaningful fraction of a shard's load. |
| Maximum useful | ~64 | Beyond ~64 cores per node, NUMA effects begin to dominate. Run multiple processes pinned to NUMA domains rather than a single oversized process. |

The thread-per-core model means every core matters: 8 cores at 50% utilization handles less load than 4 cores at 100% utilization, because each core serves disjoint shards.

### 2.3 Hyperthreading / SMT

Brain benefits from SMT (Hyperthreading on Intel, equivalent on AMD/ARM) for the embedding workload (which has memory-bandwidth bottlenecks that SMT can hide). It does not benefit much for the storage / index hot path, which is already CPU-bound.

Our recommendation: enable SMT, treat each logical CPU as its own core in Brain's configuration, and let the scheduler exploit the SMT pairs.

### 2.4 NUMA

For multi-socket servers (typically ≥ 32 cores), NUMA awareness matters. Memory accesses across NUMA boundaries are 2–3× slower than local accesses. Brain's thread-per-core model interacts with NUMA in a specific way:

- We pin each Glommio executor to a specific physical core.
- Memory allocations for a shard come from the node-local memory of the shard's home core.
- The arena's mmap'd region is opened by the home core, and the OS naturally faults pages in node-local memory for that core's accesses.

For very large servers, the recommendation is to run **one Brain process per NUMA node** rather than one process spanning all NUMA nodes. This eliminates cross-NUMA traffic at the cost of slightly higher operational complexity (multiple processes to manage). Sharding across NUMA processes is the same as sharding across nodes.

---

## 3. Memory

The arena is mmap'd; resident memory tracks the working set, not the total stored data.

### 3.1 Sizing exercise

A typical sizing exercise for a single shard:

- 1 million memories per shard at 1600 bytes per slot (vector + flags + padding) = ~1.5 GiB on disk.
- HNSW index overhead ≈ 30% of arena size = ~500 MiB on disk.
- Working set, with ~10% of memories hot, ≈ 200 MiB resident per shard.
- Per-connection state ≈ 2–4 KiB (one Glommio task plus session buffers).

For a node serving 100 shards (100M total memories), expect ~20 GiB resident memory at warm steady state. This sizing puts a solid commodity server (64 GiB RAM, 32 cores) in the comfortable zone for ~100 shards with substantial headroom.

### 3.2 The page cache is your friend

mmap delegates working-set management to the OS page cache. This works well for our access pattern:

- HNSW search jumps to candidate slots, which are read into the page cache on first access.
- Repeated accesses (hot memories) stay resident.
- Cold memories age out under memory pressure.

The OS makes this decision better than any application-level cache we could build, because it has visibility into the system-wide memory situation. Our job is to give it good hints (`madvise`) and not interfere.

### 3.3 No huge pages on the arena

An earlier draft of this spec proposed using `MADV_HUGEPAGE` on the arena. We've corrected this based on the [Linux kernel transparent hugepage documentation](https://github.com/torvalds/linux/blob/master/Documentation/admin-guide/mm/transhuge.rst):

> "Currently THP only works for anonymous memory mappings and tmpfs/shmem. But in the future it can expand to other filesystems."

Our arena lives on a regular filesystem (ext4/xfs/btrfs), so `MADV_HUGEPAGE` would have no effect. Brain uses 4 KiB pages for the arena.

This means TLB pressure for very large arenas (>16 GiB) is a real concern. The realistic mitigations:

- **Large-folio readahead** — kernel-managed and automatic in newer kernels (6.x+). No user-space action required, just upgrade the kernel.
- **`hugetlbfs`** — a separate filesystem dedicated to huge pages. Operationally complex (must be mounted, capacity must be reserved at boot). Not used in the default deployment.
- **Multiple smaller shards per node** — each shard's arena stays under the TLB pressure threshold.

The recommendation: target shards of 1–10M memories (1.5–15 GiB arena), and add nodes rather than growing shards beyond that.

### 3.4 Swap

Brain works fine on systems without swap. Working-set management is via the page cache, which doesn't need swap.

If swap is configured, set `vm.swappiness` to a low value (10 or below). Brain doesn't allocate large amounts of anonymous memory on the hot path; aggressive swapping would only hurt latency.

### 3.5 Memory headroom

Reserve at least **25% of system memory** for the OS, page cache headroom for non-hot pages, and bursts. Provisioning Brain to use 100% of system memory means the OS is constantly under pressure, page cache evictions cascade, and tail latency suffers.

---

## 4. Storage

**NVMe SSD** is required.

### 4.1 NVMe specifications

Minimums:

- Sequential write throughput ≥ 1 GB/s (for WAL writes under load).
- Random read throughput ≥ 500K IOPS (for cold-arena slot reads).
- Latency p99 ≤ 200 µs for 4 KiB writes.

These specs are met by all modern NVMe SSDs (consumer-grade and enterprise). Use enterprise-grade SSDs in production for the better p99.9 latency, sustained throughput, and endurance.

### 4.2 What's out of scope

**Spinning disks (HDDs)** are out of scope. Random access latencies (5–10 ms) make HDDs unusable for the hot path.

**Network-attached storage (EBS, Persistent Disks, NFS)** is acceptable but the latency floor rises by 1–2 ms per round trip. For deployments using NAS, expect `ENCODE` p99 in the 30–50 ms range rather than the ~25 ms target. The capacity numbers in [§7 Capacity Targets](#capacity-targets) below assume local NVMe.

**Optane / PMEM** is technically a fit (better tail latency than NVMe) but increasingly unavailable. Brain does not optimize for it specifically.

### 4.3 Filesystem requirements

The filesystem MUST be one of:

| Filesystem | Reflink (instant snapshots) | Recommended for |
|---|:-:|---|
| ext4 | No | Acceptable; snapshots use full file copies (slower, more disk during snapshot) |
| xfs (with `mkfs.xfs -m reflink=1`) | Yes | Recommended for production |
| btrfs | Yes (intrinsic) | Acceptable; understand btrfs operational characteristics first |
| zfs (Linux) | Yes (via dataset clone) | Acceptable; ZFS-specific tuning matters |

**ext4** is the default Linux filesystem and works fine. Snapshots fall back to full file copies, which take longer during the snapshot operation and use 2× disk briefly.

**xfs with reflink** is the recommended production choice. Reflink-based snapshots are near-instant. xfs handles very large files well and has mature `O_DIRECT` semantics.

**btrfs** supports reflink intrinsically per [the btrfs documentation](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/btrfs.rst): the filesystem itself is copy-on-write. Suitable for development and small deployments. Production use requires understanding btrfs operational characteristics (rebalancing, snapshot management, free-space behavior).

### 4.4 Filesystem mount options

Recommended mount options for the data directory:

- **`noatime`** — disables access-time updates, reducing metadata writes on every file read.
- **`nodiratime`** — same for directory access times.

Avoid `data=writeback` (ext4) or equivalents that defer data integrity. We trade a small amount of throughput for the durability guarantees our WAL needs.

### 4.5 Disk capacity planning

Per shard, disk usage is approximately:

```
disk = arena_size + active_wal_size + checkpointed_wal_size + snapshots

For 1M memories at 1600 bytes/slot:
arena_size              ≈ 1.6 GiB
active_wal_size         ≈ 256 MiB (default segment size)
HNSW index              ≈ 0.5 GiB
metadata (redb)         ≈ 0.1 GiB
snapshots (configurable retention) ≈ 1× current state per snapshot generation

Total per shard, working: ~2.5 GiB
With 7 daily snapshots:    ~20 GiB
```

For sizing: budget 20 GiB per million memories at production retention.

---

## 5. Network

### 5.1 TCP

The protocol runs over **TCP only**. UDP is not a fit for the structured request/response pattern with backpressure.

For high-QPS deployments, TCP keepalive and connection reuse are critical. The client is responsible for connection pooling; the server accepts long-lived connections and multiplexes streams over them per the protocol specification.

### 5.2 Bandwidth requirements

Brain is moderately bandwidth-intensive:

- An `ENCODE` over the wire is ~1–2 KiB (text + framing).
- A `RECALL` request is ~200 bytes; a response with 10 results and full content is ~5–20 KiB.
- A `SUBSCRIBE` event is ~200 bytes.

For a node serving 5K QPS with average 5 KiB per request: ~25 MB/s = 200 Mbps. Fits comfortably in 1 Gbps; 10 Gbps NICs are recommended for headroom and for fast snapshot replication.

### 5.3 Latency to clients

The protocol assumes sub-millisecond network latency to typical clients (same data center, same availability zone). Wide-area latency (cross-region) makes the latency floor much higher; agents run in the same region as their Brain shard.

### 5.4 TLS

Production deployments SHOULD wrap connections in TLS via [`rustls`](https://github.com/rustls/rustls), pure-Rust and integrating cleanly with `glommio`. TLS 1.3 only; older versions MUST be refused.

The TLS handshake adds ~1 ms of latency on first connection. For high-QPS workloads with persistent connections, this is a one-time cost — connection reuse amortizes it to zero.

For internal-only deployments (Brain on a private network with no untrusted access), TLS is optional. Internet-facing deployments SHOULD use TLS unless wrapped by a trusted reverse proxy.

---

## 6. Optional: GPU

The embedding layer supports CUDA via candle's CUDA backend.

### 6.1 When GPU helps

With a single A100 or H100 GPU, batched embedding throughput exceeds 10K items/second versus ~100–200 items/second per CPU core.

For deployments with consistently high embedding load (>1K embeddings/second per node), GPU is the right answer. The cost: a GPU is expensive, and inference workloads waste it most of the time (waiting for the next batch).

For low-QPS deployments (<500 embeddings/second per node), CPU-only is simpler and sufficient.

### 6.2 GPU selection

| GPU | Throughput (batched) | Notes |
|---|---|---|
| A100 (80 GB) | 50K+ items/s | High-end; underutilized for Brain alone |
| H100 | 100K+ items/s | Even more underutilized |
| L4 | 10K items/s | Cost-effective for inference |
| T4 | 5K items/s | Older but adequate; widely available |
| RTX 4090 (consumer) | 30K items/s | Best $/throughput; not always supported by cloud providers |

For most deployments, an L4 or T4 in the inference role is the right balance. A100/H100 makes sense only when the GPU is shared with other workloads (e.g., the LLM inference itself).

### 6.3 GPU is optional, not required

Brain runs fully on CPU. The GPU path is opt-in via configuration. The architecture supports both modes; the embedding layer's design accommodates batching for GPU while remaining single-item-friendly for CPU.

---

## 7. Time

The system clock matters more than usual. Memories are timestamped with `unix_nanoseconds`, salience decay is time-driven, idempotency is bounded by clock-based windows.

Recommendations:

- **NTP / chrony** — keep time within ±10 ms of true time.
- **Monotonic clocks** for measuring durations within a process; wall-clock for memory timestamps.
- **No wall-clock time travel** — large clock jumps confuse the decay worker. If the operator must adjust system time significantly (>1 minute), pause the decay worker first and restart it after.

Brain does not currently support cross-shard wall-clock ordering of memories. If you need a global LSN ordering across shards, you need a coordination service (out of scope for v1).

---

## 8. The hardware envelope summary

A reasonable production target node:

- **CPU:** 16-core x86_64 or ARM64, AVX2 or NEON.
- **RAM:** 64 GiB.
- **Storage:** 1 TiB NVMe with ext4 or xfs; xfs with reflink recommended.
- **Network:** 10 Gbps NIC.
- **OS:** Linux 6.x with reasonable defaults; `memlock` rlimit raised; `noatime` mount option.
- **TLS:** rustls, TLS 1.3.

This node sustains ~100 shards (100M total memories) with comfortable headroom for bursts and background work.

A development laptop target:

- **CPU:** any modern x86_64 or ARM64 with SSE 4.2 / NEON.
- **RAM:** 8 GiB sufficient for development.
- **Storage:** any local SSD.
- **OS:** Linux 5.15+; container on macOS/Windows is fine for non-perf development.

---

## Capacity Targets {#capacity-targets}

The numbers Brain commits to. They are the contract between the architecture and the operator: given the hardware envelope above, here's what the system delivers.

These targets are validated by the benchmark suite specified in [19. Benchmarks](../19_benchmarks/00_purpose.md). When a target says "p99 ≤ 25 ms", that's a number the benchmark suite must measure and pass before a release ships.

## 1. Per-shard targets

A shard is the unit of internal scaling within a node. Targets are per-shard, assuming a typical hardware tier (commodity NVMe, 16-core CPU, 64 GiB RAM).

### 1.1 Capacity

| Metric | Target | Notes |
|---|---|---|
| Memories per shard | 10⁶ – 10⁷ | Sweet spot is 1–10 million. Beyond 10M, TLB pressure on the arena starts to matter (see §3.3 above). |
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

Latency targets are measured at the protocol layer — from receipt of the request frame at the server to the moment of writing the first response byte. Network transit is excluded; client overhead is included.

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
| index maintenance | ≤ 20% of one core, only during active rebuilds |
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

Measured on standardized benchmarks. The HNSW configuration parameters (M, ef_construction, ef_search) are tuned to hit these targets. See [09. Indexing](../09_indexing/00_purpose.md) §5 for the tuning methodology.

### 4.2 Confidence calibration

Brain emits a `confidence` value in [0, 1] for `RECALL` results. Calibration target:

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

## 5. What Brain is not optimizing for

The targets above are conscious choices. The following are *not* optimization goals:

### 5.1 Sub-microsecond hot-path

Brain accepts multi-millisecond latency for embedding inference; that's the floor. Optimizing the rest of the system to sub-microsecond would not move the user-perceived metric.

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

The benchmark suite ([19. Benchmarks](../19_benchmarks/00_purpose.md)) contains tests for each target. The release criteria require all targets to pass on the reference hardware.

Targets that don't pass don't quietly slip — they either get fixed before release or get explicitly downgraded with a documented reason. The point of having them in writing is to keep that conversation honest.

Targets are reviewed and potentially updated each major version. They are not promises forever; they're promises for this version.

---

*Continue to [`06_scope_and_comparison.md`](06_scope_and_comparison.md) for explicit non-goals.*
