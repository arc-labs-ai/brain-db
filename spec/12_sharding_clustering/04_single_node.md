# 12.04 Single-Node Deployment

The v1 deployment model: all shards in one process on one machine.

## 1. The model

```
┌──────────────────────────────────────────┐
│  Brain Substrate Process                 │
│                                          │
│  ┌────────┐ ┌────────┐ ... ┌────────┐    │
│  │Shard 0 │ │Shard 1 │     │Shard N │    │
│  │CPU 0   │ │CPU 1   │     │CPU N   │    │
│  └────────┘ └────────┘     └────────┘    │
│                                          │
│  ┌────────────────────────────────────┐  │
│  │ Connection Layer (TCP listener)    │  │
│  └────────────────────────────────────┘  │
└──────────────────────────────────────────┘
            │
            ▼
       ┌────────┐
       │ Disk   │ — arena, WAL, metadata per shard
       └────────┘
```

One process. N shards (typically one per CPU core). Connection layer dispatches to shards.

## 2. Process startup

```
1. Read config.
2. For each configured shard:
   a. Spawn an OS thread.
   b. Pin the thread to the assigned CPU.
   c. Create a Glommio executor on the thread.
   d. Spawn the shard's tasks (writer, workers, etc.).
3. Open the TCP listener.
4. Accept connections; dispatch frames to shards.
```

Startup time: dominated by per-shard recovery (WAL replay, HNSW restore). Typically 5-30 seconds for a substrate with ~1M memories.

## 3. Process shutdown

On `SIGTERM`:

```
1. Close the TCP listener (no new connections).
2. Drain in-flight requests (with timeout, e.g., 30s).
3. Send shutdown signal to each shard's tasks.
4. Each shard:
   a. Stops accepting new requests.
   b. Drains the writer queue.
   c. Triggers a checkpoint.
   d. Stops workers.
   e. Closes storage files.
5. Process exits.
```

Clean shutdown takes a few seconds. On `SIGKILL` or crash, recovery on next startup uses WAL.

## 4. Resource provisioning

Per-machine recommendations:

| Resource | Recommendation |
|---|---|
| CPU cores | 1 per shard, plus 1-2 for connection layer + system |
| RAM | 4-32 GB per shard (data-size dependent) |
| Disk | NVMe SSD; ~10× the working data size for headroom |
| Network | 1-10 Gbps |

For a 16-core machine with 1M memories per shard, ~64-128 GB RAM is reasonable.

## 5. The connection layer

A separate component:

```
┌──────────────────────────────────────────┐
│  Connection Layer                         │
│  ┌────────────┐                          │
│  │ TCP Accept │                          │
│  └────────────┘                          │
│       │                                  │
│       ▼                                  │
│  ┌────────────┐                          │
│  │ Frame Parse│                          │
│  └────────────┘                          │
│       │                                  │
│       ▼                                  │
│  ┌────────────────────────────────────┐  │
│  │ Router → dispatch to shard         │  │
│  └────────────────────────────────────┘  │
└──────────────────────────────────────────┘
```

The connection layer handles TCP, TLS, framing. It doesn't have its own data; it dispatches frames to the appropriate shard.

For load balancing across cores, the connection layer can use Linux's SO_REUSEPORT (multiple accept threads).

## 6. The "shared-nothing" property

Despite being in one process, shards share **nothing** mutable:

- Each shard's storage files are private.
- Each shard's in-memory state (HNSW, caches) is private.
- Cross-shard "calls" are method calls but use only immutable references (the request/response shape).

This is enforced by Rust's type system: shard state is not Send/Sync, can't be moved between threads.

## 7. The cross-shard dispatch

For a request that needs cross-shard data:

```rust
async fn cross_shard_recall(req: RecallRequest) -> Vec<RecallResult> {
    let shards = self.router.shards_for_agent(&req.agent_id);
    let mut futures = Vec::new();
    for shard_id in shards {
        let shard_handle = self.shards.get(shard_id).clone();
        futures.push(shard_handle.recall(req.clone()));
    }
    futures::future::join_all(futures).await
}
```

Each `shard_handle.recall(...)` is a message to that shard's executor. The result is awaited from the originating shard's executor.

Latency: ~10-100 µs per shard message (includes channel send + executor wakeup + receive).

## 8. Disk layout

```
/var/lib/brain/
├── config.toml
├── data/
│   ├── <shard_uuid_1>/
│   │   ├── arena.bin
│   │   ├── wal/
│   │   └── metadata.redb
│   ├── <shard_uuid_2>/
│   │   └── ...
│   └── ...
├── snapshots/                  (optional)
│   └── <snapshot_uuid>/
└── logs/
```

The data directory contains all shards. Shards are physical isolation but on the same filesystem.

For HA via filesystem replication (DRBD, etc.), the entire `data/` directory is replicated.

## 9. Filesystem considerations

**Recommended filesystem**: ext4 or xfs.

- xfs: better for very large files (multi-TB arenas).
- ext4: ubiquitous; good general-purpose.

Brain's WAL uses O_DIRECT, which both filesystems support. The arena uses mmap; both support that.

Per [05.10 Snapshots](../05_storage_arena_wal/10_snapshots.md), reflink copies require xfs (with `-m reflink=1`) or btrfs. ext4 doesn't support reflink.

For deployments using snapshots without reflink, the substrate falls back to whole-file copies (slower but works on any filesystem).

## 10. NUMA considerations

For multi-socket systems, NUMA matters:

- Each shard's memory should be on the same NUMA node as its CPU.
- Glommio supports NUMA-aware allocation.
- The substrate's config can specify NUMA placement per shard.

For single-socket systems, NUMA is N/A. Brain works fine without NUMA configuration on smaller machines.

## 11. The "monolithic" preference for v1

Single-node, single-process is the v1 deployment model because:

- Simpler operationally — one process to start, monitor, shutdown.
- Fewer failure modes (no network between shards).
- Lower latency for cross-shard calls.
- Adequate for the workload sizes Brain targets (up to ~hundreds of millions of memories per machine).

For larger workloads, v2 will introduce clustering. Until then, single-node is the only mode.

## 12. Vertical scaling

To scale a single-node deployment:

- Add more cores → more shards.
- Add more RAM → larger HNSW caches, more in-memory data.
- Add more disk → more memories.

Vertical scaling has limits (max core count per machine, max RAM, etc.) but for v1's target workloads, those limits are far away.

For workloads exceeding single-machine capacity, horizontal scaling (clustering) is the v2 answer.

## 13. The "binary distribution"

Brain ships as a single static binary:

- One Rust executable.
- Plus configuration files.
- Plus a data directory.

No external dependencies (no separate Postgres, no Redis, no Kafka). The binary contains everything.

This makes deployment simple: copy binary, write config, start. No orchestration of multiple services.

## 14. Configuration management

Configuration is a single TOML file, plus per-environment overrides:

```
config/
├── default.toml          # Base config
├── production.toml       # Production overrides
└── staging.toml          # Staging overrides
```

The substrate reads `default.toml` then layers overrides based on `BRAIN_ENV`.

## 15. Logging

Structured logs to stdout (or a file via redirection):

```json
{"ts":"2026-05-07T12:00:00Z","level":"info","shard":"<uuid>","operation":"encode","memory_id":"...","latency_ms":8}
```

Standard tools (jq, Grafana Loki, etc.) consume the logs.

For SIEM/audit, the substrate emits a separate audit stream (configurable).

## 16. Metrics export

Prometheus-style metrics on `/metrics` endpoint:

```
brain_request_total{shard="...",op="encode",status="success"} 12345
brain_memory_count{shard="..."} 1000000
```

Plus Glommio runtime metrics, Rust process metrics. Standard Prometheus + Grafana setup gives full observability.

## 17. The HA story (or lack thereof)

In v1 single-node, there's no built-in HA:

- One process: process crash = shard outage.
- One machine: hardware failure = all shards down.

For HA, v1 deployments use external mechanisms:

- Process supervisor (systemd, etc.) for auto-restart.
- Filesystem replication (DRBD) or block-storage replication for disk.
- Snapshot-and-restore for backup.
- A standby machine that can take over via configuration changes.

These are operational concerns, not Brain features. v2 will integrate replication.

---

*Continue to [`05_clustered.md`](05_clustered.md) for clustered deployment.*
