# 01.05 Hardware Assumptions

The architecture is calibrated to a specific deployment envelope. Operating outside it is technically possible but means giving up the latency and capacity numbers we promise.

This file documents what we assume about the deployment target. Capacity targets — what we promise *given* this hardware — are in [`06_targets.md`](06_targets.md).

## 1. Operating system

**Linux**, kernel ≥ 5.15.

The 5.15 kernel is the LTS series with mature `io_uring`. Earlier kernels have `io_uring`, but its surface evolved rapidly through 5.6–5.15; 5.15+ has the API stability we depend on. Kernel 6.x is recommended for newer features (faster io_uring fixed-buffer support, better large-folio readahead).

### 1.1 Why Linux-only

Brain depends heavily on Linux-specific I/O facilities:

- **`io_uring`** — Linux-only. macOS uses kqueue, Windows uses IOCP; both have similar capabilities but different APIs and different latency characteristics. Glommio is built directly on `io_uring` and has no portable backend.
- **`O_DIRECT`** — Linux's interpretation differs significantly from FreeBSD's; macOS doesn't have it; Windows has unbuffered I/O via `FILE_FLAG_NO_BUFFERING` with different semantics.
- **The specific `madvise` flags** we use (`MADV_RANDOM`, `MADV_DONTDUMP`) — non-Linux equivalents exist but aren't byte-compatible.
- **`fallocate` with `FALLOC_FL_KEEP_SIZE`** — Linux-specific behavior.

We considered abstracting these. The conclusion: the abstraction would be either leaky (revealing platform differences in tail latency) or bloated (multiple I/O backends). For a system whose value proposition is latency, a single optimized backend is better than a portable one.

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

We use SIMD for vector dot products: AVX2 on x86 (256-bit, 8 floats per instruction), NEON on ARM64 (128-bit, 4 floats per instruction), with portable fallbacks (`std::simd` or [`wide`](https://github.com/Lokathor/wide)) for cores lacking either.

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

Our arena lives on a regular filesystem (ext4/xfs/btrfs), so `MADV_HUGEPAGE` would have no effect. We use 4 KiB pages for the arena.

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

**Network-attached storage (EBS, Persistent Disks, NFS)** is acceptable but the latency floor rises by 1–2 ms per round trip. For deployments using NAS, expect `ENCODE` p99 in the 30–50 ms range rather than the ~25 ms target. The capacity numbers in [`06_targets.md`](06_targets.md) assume local NVMe.

**Optane / PMEM** is technically a fit (better tail latency than NVMe) but increasingly unavailable. We don't optimize for it specifically.

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

For high-QPS deployments, TCP keepalive and connection reuse are critical. The SDK is responsible for connection pooling; the server accepts long-lived connections and multiplexes streams over them per the protocol specification.

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

*Continue to [`06_targets.md`](06_targets.md) for what we promise given this hardware.*
