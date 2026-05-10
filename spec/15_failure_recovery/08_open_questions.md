# 15.08 Open Questions

Failure-recovery questions unresolved as of this spec version.

---

## OQ-FR-1: Built-in off-site backup

**Issue.** Brain doesn't ship cloud-storage integrations for snapshot upload. Operators use external tools.

**Options.**

a) **External (status quo).** Standard Unix tools (S3 sync, etc.) work.

b) **Built-in connectors.** S3, GCS, Azure Blob.

**Recommendation.** Stay with (a) for v1. Reduces substrate's surface area. v1.x may add (b) for convenience.

---

## OQ-FR-2: Continuous backup (write-ahead replication)

**Issue.** Snapshots are point-in-time. WAL records between snapshots are at risk.

**Options.**

a) **Snapshots only (status quo).**

b) **Stream WAL records to off-site immediately.**

c) **Replicate to a remote substrate (v2 feature).**

**Recommendation.** (b) is a v1.x enhancement; (c) is the v2 long-term direction.

---

## OQ-FR-3: Automated failover

**Issue.** v1 has no automatic failover. v2 will (with replication). What's the failover policy?

**Options.**

a) **Manual (v1 default).**

b) **Auto-promote replica.** Detected via heartbeat; consensus protocol promotes.

**Recommendation.** (b) for v2, with operator-tunable thresholds.

---

## OQ-FR-4: Online recovery from corruption

**Issue.** Corruption recovery currently requires offline restoration. Could be online (substrate continues serving while recovering)?

**Options.**

a) **Offline (status quo).** Stop, restore, start.

b) **Online via shadow shard.** Restore to a shadow shard; switch when ready.

**Recommendation.** (b) is appealing but complex. v2.

---

## OQ-FR-5: Subset recovery

**Issue.** Currently, recovery is per-shard. Could it be per-agent (recover one agent's data without affecting others)?

**Options.**

a) **Per-shard (status quo).**

b) **Per-agent recovery.** Restore one agent's memories from snapshot.

**Recommendation.** (b) is a v1.x feature. Requires agent-aware snapshots.

---

## OQ-FR-6: Backup verification

**Issue.** Snapshots could silently be corrupt. The first you'd know is when you try to restore.

**Options.**

a) **Manual verification (status quo).** Operator periodically tests restore.

b) **Built-in verification.** The substrate validates backups by attempting restore in a sandbox.

**Recommendation.** (b). Implement as a periodic verification job.

---

## OQ-FR-7: Time-travel queries

**Issue.** With WAL, you could query the state as of any past LSN. Could be useful for debugging.

**Options.**

a) **Not exposed (status quo).**

b) **Read-as-of API.** Query the historical state.

**Recommendation.** (b) is interesting but expensive. Defer; consider for v2.

---

## OQ-FR-8: Data scrubbing

**Issue.** A periodic background scan could verify all stored data integrity.

**Options.**

a) **No scrub (status quo).** Issues found at read time.

b) **Background scrub worker.** Reads everything periodically; verifies CRCs.

**Recommendation.** (b) as opt-in. For deployments wanting proactive corruption detection. Resource-heavy.

---

## OQ-FR-9: Self-healing for partial corruption

**Issue.** If one slot is corrupt, can the substrate auto-repair it? Currently it's marked corrupt and ignored.

**Options.**

a) **No auto-repair (status quo).**

b) **Auto-rebuild from WAL.** If the original ENCODE record exists in WAL, replay it to a new slot.

**Recommendation.** (b) is feasible if the WAL is intact for that operation. Implementation has edge cases (multiple updates, etc.). v1.x exploration.

---

## OQ-FR-10: Multi-region DR (v2)

**Issue.** v2 will have replication. Multi-region DR has special concerns (latency, data sovereignty).

**Options.**

a) **Single-region only.**

b) **Async multi-region replication.**

c) **Sync multi-region (high latency).**

**Recommendation.** (b) for v2 initial. (c) is for special cases.

---

*Continue to [`09_references.md`](09_references.md) for references.*
