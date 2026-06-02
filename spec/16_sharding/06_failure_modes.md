# 16.06 Sharding & Clustering Failure Modes

What can go wrong with sharding and (in a future major version) clustering, and how Brain responds.

## 1. Single-shard process crash

**Failure mode.** Brain process crashes; all shards go offline.

**Detection.** Process exits; supervisor (systemd, etc.) detects.

**Response.** Process supervisor restarts. Each shard recovers via WAL replay.

**Operator action.** Investigate the crash via logs. If recurrent, fix the cause.

## 2. Disk failure on shard's data files

**Failure mode.** The disk holding a shard's files fails (read errors, corruption).

**Detection.** I/O errors during shard operations.

**Response.** The shard is marked offline. Other shards continue.

**Operator action.** Replace the disk; restore from backup. If using filesystem replication or block replication, fail over.

## 3. Wrong-shard request

**Failure mode.** A request reaches a shard that doesn't own the data.

**Detection.** The shard checks its agents/memories table; doesn't find the data.

**Response.** Returns `WrongShard` error with the correct runtime `shard_id`.

**Operator action.** None (transient). The client refreshes routing and retries.

## 4. Stale routing table

**Failure mode.** A client's routing table is out of date; it routes incorrectly.

**Detection.** `WrongShard` errors at the shard.

**Response.**
- Client retries with corrected routing.
- Some clients refresh their routing table proactively (e.g., periodically).

**Operator action.** None. Clients handle this.

## 5. Shard count mismatch

**Failure mode.** Brain is configured with N shards but the data directory has fewer / more.

**Detection.** Startup mismatch.

**Response.** Brain refuses to start. Logs an error.

**Operator action.** Resolve the mismatch — either fix the config or move shard directories.

## 6. Agent assignment to a non-existent shard

**Failure mode.** An override map references a shard that doesn't exist.

**Detection.** The stateless router resolves to an unknown shard.

**Response.** The request fails with `ShardNotFound`. Logs an error.

**Operator action.** Fix the override map.

## 7. Cross-shard fan-out partial failure

**Failure mode.** A multi-shard query (e.g., for a multi-shard agent) succeeds on some shards and fails on others.

**Detection.** The fan-out collector sees mixed results.

**Response.** Returns a partial response (with a `partial: true` flag) plus errors for the failed shards.

**Operator action.** None (transient). For sustained failures, investigate the failing shard.

## 8. Shard with corrupted data

**Failure mode.** A shard's data files are corrupted (bad CRCs, schema violations).

**Detection.** Recovery or operations report corruption.

**Response.** The shard refuses to come online. Other shards continue.

**Operator action.** Restore from snapshot. May need to rebuild metadata from WAL.

## 9. Imbalanced shard load

**Failure mode.** One shard has much more load than others (hot agent, etc.).

**Detection.** Per-shard latency / queue-depth metrics.

**Response.**
- The hot shard's request latency rises.
- Backpressure (`Overloaded` errors) on extreme overload.

**Operator action.** Re-distribute load via override map; split the shard if needed.

## 10. Shard runs out of disk

**Failure mode.** A shard's volume is full; writes fail.

**Detection.** Storage layer returns `NoSpace` errors.

**Response.**
- Writes to that shard fail.
- Reads continue.
- Other shards (on different volumes) continue.

**Operator action.** Free space (delete old data, expand volume) or migrate the shard to a larger volume.

## 11. Shard's process can't bind to its CPU

**Failure mode.** The CPU pinning configured for a shard isn't available (e.g., taskset constraints).

**Detection.** Glommio fails to start the shard's executor.

**Response.** Shard fails to start. Brain may continue with other shards or fail entirely.

**Operator action.** Adjust CPU pinning configuration.

## 12. Cross-node call timeout (future)

**Failure mode.** A call from one node to another times out (network, dest node overloaded).

**Detection.** Timeout fires.

**Response.**
- The call returns an error.
- The originating node tries an alternative (replica, if any).

**Operator action.** Investigate the destination node's health.

## 13. Network partition (future)

**Failure mode.** Network split between subsets of nodes.

**Detection.** Heartbeats fail across the partition.

**Response.** Depends on consensus / membership protocol:
- Smaller side: gives up writes (to avoid split-brain).
- Larger side: continues with majority.

**Operator action.** Investigate the network. Once partition is resolved, the smaller side rejoins.

## 14. Promotion failure (future)

**Failure mode.** A primary fails; promoting a replica fails (data inconsistency, network issue).

**Detection.** Promotion procedure errors out.

**Response.** Shard remains unavailable. Operator intervention needed.

**Operator action.** Investigate which replica is most current; manually promote.

## 15. Replica lag too high (future)

**Failure mode.** A replica falls too far behind the primary.

**Detection.** Lag metric exceeds threshold.

**Response.**
- Replica is marked out-of-sync.
- May be removed from read pool.
- Re-sync via snapshot transfer.

**Operator action.** Investigate why; may need to scale up the replica's resources.

## 16. Membership corruption (future)

**Failure mode.** The membership table (in the control plane's Raft log) becomes inconsistent.

**Detection.** Nodes disagree on shard assignments.

**Response.**
- The control plane's Raft protocol resolves via leader election and log truncation.
- If the inconsistency is in committed records: a critical bug.

**Operator action.** Address the bug. May need to manually reset membership.

## 17. The "phantom shard" (future)

**Failure mode.** A shard's files exist on a node but the membership table thinks it's elsewhere.

**Detection.** Node startup sees orphaned shard files.

**Response.** Logs warnings; doesn't auto-claim. Operator decides.

**Operator action.** Either delete the orphan or update membership.

## 18. The "single-node to clustered upgrade" failure

**Failure mode.** Upgrading from single-node v1 to a future clustered release hits issues.

**Detection.** Migration scripts fail.

**Response.** Stay on the current version; address issues; retry.

**Operator action.** Backup before upgrade. Test on staging first.

## 19. The "rebalance stuck" case

**Failure mode.** A rebalance starts but doesn't complete (e.g., source node fails mid-transfer).

**Detection.** Rebalance status is "in progress" indefinitely.

**Response.**
- Operator can manually abort the rebalance.
- Brain keeps the source's data (the destination's partial copy is discarded).

**Operator action.** Investigate; restart the rebalance after fixing the issue.

## 20. The client failure modes

**Failure modes.**
- Client has stale routing.
- Client can't reach any node.
- Client has bugs in retry logic.

**Detection.** Application-level errors.

**Response.** Clients should:
- Retry on `WrongShard`.
- Refresh routing on persistent failures.
- Fall back to bootstrap nodes if all fails.

**Operator action.** Update the client if buggy. Configure correct bootstrap nodes.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
