# 15.04 Data Loss Scenarios

When data loss is possible in Brain, and what bounds it.

## 1. The data-loss promise

Brain's nominal contract:

- An operation that received a "success" response is durable.
- The data survives crash, restart, ordinary failures.

Loss happens only in specific scenarios. This file enumerates them.

## 2. Scenario 1: Operation in-flight at crash

The classic case: the substrate crashes between accepting a request and committing it to WAL.

- Client may or may not have received a response.
- The WAL doesn't have the record.
- After recovery, the operation didn't happen.

Loss: bounded by in-flight operations (typically a few microseconds' worth).

Mitigation: client retries (with the same RequestId) succeed if the request is re-submitted.

## 3. Scenario 2: WAL fsync failed

The WAL append succeeded but the OS hasn't synced to disk yet, then a crash happens.

Brain's WAL uses `RWF_DSYNC` per write, which forces sync as part of the write call. So this scenario shouldn't happen with correct operation.

If the OS or disk lies about sync completion (some cheap consumer drives do), data could be lost.

Mitigation: use enterprise-grade SSDs with reliable sync. Verify with `fsync` benchmarking.

## 4. Scenario 3: Single-region disaster

The data center is lost (fire, power, etc.). All on-site state is gone.

Loss: everything since the last off-site backup.

Mitigation:
- Regular snapshots shipped off-site.
- Cross-region replication (v2).

For deployments without these, the loss is catastrophic.

## 5. Scenario 4: Operator deletes data

`agent delete` removes an agent. Confirmed; intended to delete.

Loss: all of that agent's data.

Recovery:
- Restore from snapshot if available (RPO bounded by snapshot age).
- Without snapshot: data is gone.

This is the operator's responsibility. The substrate provides the safeguards (confirmation, audit log).

## 6. Scenario 5: Soft-FORGET grace period elapses

A FORGET is reversible during the grace period (default 7 days). After that, slot reclamation fully removes the data.

Loss: as intended; the FORGET was a delete operation.

Recovery: not possible after grace period unless backup taken before reclamation.

## 7. Scenario 6: Hard FORGET with `force_reclaim_now`

Hard FORGET zeroes vector and text immediately; with `force_reclaim_now`, the slot is reclaimed before the grace period.

Loss: as intended.

Recovery: not possible.

## 8. Scenario 7: Backup not yet shipped

A snapshot is taken; before it's shipped off-site, the data center is lost.

Loss: the snapshot is lost along with everything else.

Mitigation: ship snapshots quickly; redundant snapshot destinations.

## 9. Scenario 8: WAL retention deletes uncheckpointed data

A bug or misconfigured retention deletes WAL segments that haven't been fully checkpointed.

Loss: the unchecked WAL data.

Detection: post-restart, recovery sees a gap and refuses.

Recovery: restore from snapshot.

## 10. Scenario 9: Memory corruption that affects metadata

A memory corruption (RAM or hardware) corrupts metadata before checkpoint. The corruption is then written to disk during the checkpoint.

Loss: depends on what was corrupted.

Detection: invariants may catch; the audit log shows pre-corruption state.

Recovery: restore from previous snapshot.

## 11. Scenario 10: Substrate bug causes data corruption

A bug in the substrate writes wrong data, marks it as committed.

Loss: the wrong data is "durable" but actually wrong.

Detection: depends on the bug. May surface as wrong query results, invariant violations, or data inconsistency.

Recovery: restore from before the bug; investigate; patch.

This is why testing matters.

## 12. Scenario 11: Adversarial corruption

An attacker compromises the substrate and tampers with data.

Loss: tampered data is durable.

Detection: audit logs (if not tampered) show suspicious activity. Hash-chained audit logs detect tampering of audit itself.

Recovery: restore from a backup pre-compromise. Investigate breach.

## 13. The bounds

Data loss is bounded by:

| Scenario | Bound |
|---|---|
| Crash with in-flight ops | ~ms (in-flight at crash time) |
| Disaster, regular off-site snapshots | RPO = snapshot interval |
| Operator deletion | Deliberate; bounded by intent |
| Forget grace + reclamation | Deliberate; bounded by grace period |
| Substrate bug | All data after bug introduction (until detection) |
| Adversarial | All data within attacker's window |

The substrate's design pushes the envelope of "no loss" as far as engineering allows. Beyond that, operational practices matter.

## 14. The "what we don't lose"

Despite the above, Brain protects against:

- **Power loss**: WAL fsync ensures durability.
- **Process crash**: WAL replay restores.
- **Disk failure** (with redundancy / RAID): the underlying storage handles.
- **Network failure**: doesn't affect persisted data.
- **Memory pressure**: shed load doesn't lose committed data.

These common failures don't cause loss.

## 15. The "consistent restore" guarantee

When restoring from a snapshot, the snapshot represents a consistent point-in-time:

- Either a memory exists or it doesn't.
- Either an edge exists or it doesn't.
- No half-committed states.

The restore brings back this consistent point.

## 16. The "data is the bottom of the stack" principle

Brain stores data; clients write to Brain. Data loss has cascading effects:

- Lost memories = lost agent context = degraded user experience.
- Lost edges = broken reasoning chains.

Operators understand this. Backups, monitoring, and careful operations are essential.

## 17. The "no surprises" goal

Brain's failure modes are documented. No "and sometimes data just disappears" cases.

If this document doesn't list a scenario, it's because:
- The substrate handles it (no loss).
- It's a bug to fix.
- It's a known external limitation (e.g., the operator's responsibility).

If you find an unlisted scenario in production, it's a bug or doc gap. Either way, file an issue.

## 18. The recovery time tradeoff

Faster recovery = less downtime, but may require more frequent backups.

Common settings:

| Backup cadence | RPO | Backup cost |
|---|---|---|
| Continuous (replication) | ~0 | High (network, RAM) |
| Hourly | 1h | Moderate |
| Daily | 24h | Low |

Operators choose based on the deployment's tolerance.

## 19. The "data warmup" recovery semantics

After a recovery from snapshot:

- Caches are cold.
- HNSW may need rebuild.
- Embedder cache empty.

First few hours have higher latency. Then it recovers to steady state.

This is expected; not data loss.

## 20. The "audit trail is durable too"

Audit logs follow the same durability rules:

- Each audit entry is fsynced.
- Logs persist across restart.

For audit-critical deployments, the audit trail itself may be replicated separately, ensuring it survives even substrate-level disasters.

---

*Continue to [`05_partial_failures.md`](05_partial_failures.md) for partial failure handling.*
