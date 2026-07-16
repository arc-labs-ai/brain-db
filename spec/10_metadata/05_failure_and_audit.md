# 10.05 Failure Modes and Audit

> **TL;DR.** Metadata-store failure modes (redb file corruption, transaction abort, lock contention, schema-version mismatch) and the audit-table layout (extraction audit, resolution audit, merge log) that records every operationally-significant event.

## Failure Modes

What can go wrong with the metadata store and how Brain responds.


## 1. redb file corruption

**Failure mode.** The metadata.redb file is corrupted (bad bytes from a hardware fault, partial write during crash, etc.).

**Detection.** redb checks page checksums; corruption is detected on read. Brain gets a `redb::Error::Corrupted`.

**Response.**
- Brain logs the error and refuses to start (or marks the shard offline if running).
- An operator must restore from backup or rebuild from WAL.

**Operator action.** Restore the latest known-good snapshot ([05.10 Snapshots](../08_storage/06_snapshots.md)). After restore, replay WAL records to bring the metadata current.

## 2. Failed commit

**Failure mode.** A write transaction's `commit()` returns an error (disk full, permission lost, hardware error).

**Detection.** redb returns the error.

**Response.**
- Brain aborts the in-progress operation.
- The WAL record was already written (it's the durability barrier); the metadata commit failure means the operation is partially-applied (WAL durable, redb not).
- Brain retries the commit a few times.
- If retries fail, Brain marks the shard degraded and stops accepting writes.

**Operator action.** Investigate disk health. Free space if full. Restart the shard, which will re-apply the WAL record on recovery.

## 3. Disk full

**Failure mode.** The disk has no free space; redb commits fail.

**Detection.** Commit returns `IoError(NoSpace)`.

**Response.**
- Brain stops accepting writes.
- Reads continue to work.
- An alert is raised.

**Operator action.** Free disk space (delete old WAL segments, prune snapshots, expand the volume). After space is available, the shard resumes writes.

## 4. Inconsistency between WAL and metadata

**Failure mode.** After a crash, recovery finds that the metadata doesn't match what the WAL says it should.

**Possible causes:**
- redb commit succeeded but Brain crashed before acknowledging.
- A bug in the recovery code.

**Detection.** Recovery validates: every WAL record after the metadata's checkpoint LSN should successfully apply. If applying a record finds the change is already present (idempotent re-apply), that's fine. If it finds a conflicting state, that's a bug.

**Response.**
- Idempotent re-apply: the recovery proceeds.
- Conflict: recovery logs the bug, attempts to resolve (typically by trusting the WAL), and proceeds. Brain raises an alert.

**Operator action.** Report the bug. Brain is usually still functional after a recovery anomaly.

## 5. Failed read

**Failure mode.** A read returns an error (disk read fault, redb internal error).

**Detection.** redb returns an error from `get()` or `range()`.

**Response.**
- The request handler returns an error to the client.
- Brain logs the failure with details.
- Repeated failures suggest a hardware problem; the shard may be marked unhealthy.

**Operator action.** Investigate disk. May need to restore from backup if the corruption is widespread.

## 6. Schema version mismatch

**Failure mode.** The metadata store's table format version is newer or older than Brain expects.

**Detection.** On open, Brain reads the format version and compares to its expected version.

**Response.**
- If the file is older: run the registered migrations to bring it up to current.
- If the file is newer: Brain refuses to open (Brain is too old).

**Operator action.** For older files: nothing; migrations run automatically. For newer files: upgrade Brain.

## 7. Migration failure

**Failure mode.** A schema migration fails partway through (e.g., disk error during a migration that rewrites all rows).

**Detection.** The migration's transaction commit fails.

**Response.**
- The migration's transaction aborts; the database is in the pre-migration state.
- Brain logs the error and refuses to start.
- The migration can be retried; partial migrations don't leave the database in a half-state.

**Operator action.** Address the underlying error (disk space, etc.) and restart. The migration will retry.

## 8. Concurrent write failure (impossible by design)

**Failure mode.** Two writer tasks try to commit on the same shard.

**Detection.** redb's API ensures at most one write transaction at a time; second `begin_write()` blocks.

**Response.**
- This shouldn't happen due to single-writer-per-shard discipline.
- If it does (a bug), the second writer waits.
- An assertion in the writer task catches this case in debug builds.

**Operator action.** Report the bug.

## 9. Long-running read holds resources

**Failure mode.** A long-running read transaction (a misbehaving SUBSCRIBE client, a stuck maintenance worker) holds the redb snapshot, preventing page reclamation.

**Detection.** Disk usage grows beyond expected; Brain exposes a metric `read_txn_oldest_age`.

**Response.**
- Brain kills read transactions older than the configured max (default 1 hour).
- An alert is raised when transactions exceed the warning threshold (default 30 min).

**Operator action.** Investigate which client or worker is holding the long transaction. May indicate a stuck process.

## 10. Idempotency table grows unbounded

**Failure mode.** The TTL pruning worker fails or stalls; the idempotency table grows beyond expected size.

**Detection.** Table size metric exceeds threshold.

**Response.** Manual or scheduled retry of the pruning worker.

**Operator action.** Investigate why the worker stalled. Manually trigger pruning if needed.

## 11. Edge orphan

**Failure mode.** An edge references a memory that no longer exists (the memory was deleted but the edge wasn't).

**Detection.** Edge listing finds the orphan; verification queries flag it.

**Response.** A maintenance worker periodically scrubs orphan edges:
- For each edge in `edges_out`, verify both endpoints exist.
- If an endpoint is missing, delete the edge.

**Operator action.** None; the worker handles it.

## 12. Counter drift

**Failure mode.** The denormalized counters (edges_out_count, edges_in_count, memory_count) drift from the actual counts.

**Detection.** Periodic reconciliation by maintenance workers.

**Response.** Workers recompute the actual count and update the denormalized field.

**Operator action.** None.

## 13. Stale slot version

**Failure mode.** A MemoryId's slot_version doesn't match the slot's current version (the slot was reclaimed without the MemoryId being cleaned up elsewhere).

**Detection.** When following a MemoryId reference, Brain compares its slot_version to the slot's current version.

**Response.**
- The reference is treated as dangling; the operation fails or returns "not found".
- A maintenance worker scrubs stale references in the HNSW.

**Operator action.** None; Brain handles it.

## 14. Write transaction starvation

**Failure mode.** The writer task can't make progress because every commit hits a disk error or because cooperative yields don't return control.

**Detection.** The writer's `last_commit_at` doesn't advance; the WAL hasn't been written either (since they're paired).

**Response.**
- Latency rises sharply.
- Health checks mark the shard unhealthy.

**Operator action.** Investigate disk health, OS load, the writer task itself.

## 15. The "metadata-only" recovery

**Failure mode.** The arena is intact but the metadata store is corrupted/lost.

**Detection.** Arena exists, metadata.redb is missing or corrupt.

**Response.**
- Recovery rebuilds the metadata from the WAL.
- The arena is the source of vectors; the WAL is the source of metadata operations.
- Rebuild iterates the WAL from the start (or from the last known-good metadata snapshot).

**Operator action.** Trigger metadata rebuild via `ADMIN_RECOVER_METADATA`. The shard is offline during rebuild.

## 16. The "WAL-only" recovery

**Failure mode.** The metadata is intact but the WAL is partial/corrupt.

**Detection.** WAL records can't be read past a certain LSN.

**Response.**
- Recovery uses the metadata as it stands at the last consistent LSN.
- Records past the corruption are lost; this is a partial data loss event.

**Operator action.** Restore from backup if the data loss is unacceptable.

## 17. Total shard loss

**Failure mode.** All shard files (arena, WAL, metadata) are lost (e.g., disk failure with no replication).

**Detection.** Files don't exist on startup.

**Response.**
- The shard can't be opened.
- If the deployment uses replication or snapshots, the operator restores.
- Otherwise: data is lost.

**Operator action.** Restore from snapshot. Brain doesn't have built-in replication in v1.

## 18. The error-reporting discipline

For every failure mode above, Brain:

- Logs the failure with structured fields (shard, operation, error code).
- Increments a counter metric.
- Raises an event for severity > Warning.

Operators monitor these signals; alerts fire on severity > Error.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*

---

## Audit Tables

redb table layout for the audit logs (extraction, resolution,
merge). Brain ships `EXTRACTOR_AUDIT_TABLE` + its three indexes
alongside the `ENTITY_RESOLUTION_AUDIT` and `MERGE_LOG` tables.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"The audit log" — narrative.
- [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) —
  extractor audit row spec.

## 1. `EXTRACTOR_AUDIT_TABLE`

Primary record. One row per extraction call.

```rust
pub const EXTRACTOR_AUDIT_TABLE:
    TableDefinition<'static, u128, ExtractionAuditRow> =
    TableDefinition::new("extractor_audit");

#[derive(Archive, Serialize, Deserialize, ...)]
pub struct ExtractionAuditRow {
    pub audit_id: u128,                 // UUIDv7
    pub memory_id: u128,
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status: u8,
    pub status_reason: String,
    pub outputs: Vec<OutputRefRow>,     // ≤ 64; overflow → follow-on row (post-v1)
    pub cost_micro_usd: u64,            // 0 here
    pub model_metadata: Vec<u8>,        // rkyv-archived blob; empty for non-LLM
    pub input_hash: [u8; 32],           // BLAKE3
}

pub struct OutputRefRow {
    pub kind: u8,                       // 1=Entity, 2=Statement, 3=Relation, 4=EntityMention
    pub id: u128,
}
```

Primary key is `audit_id`; UUIDv7 means insertion order ≈ time
order, so an iteration over the table is roughly newest-last.

## 2. Indexes

Three secondary indexes, each `()`-valued (the row data lives in
the primary table; indexes only support lookups).

```rust
pub const EXTRACTOR_AUDIT_BY_MEMORY:
    TableDefinition<'static, (u128, u128), ()> =
    TableDefinition::new("extractor_audit_by_memory");
// Key: (memory_id, audit_id) — iterating `(mem_id, 0)..(mem_id+1, 0)`
// returns all audits for one memory in time order.

pub const EXTRACTOR_AUDIT_BY_EXTRACTOR:
    TableDefinition<'static, (u32, u128), ()> =
    TableDefinition::new("extractor_audit_by_extractor");
// Key: (extractor_id, audit_id) — per-extractor history.

pub const EXTRACTOR_AUDIT_BY_TIME:
    TableDefinition<'static, (u64, u128), ()> =
    TableDefinition::new("extractor_audit_by_time");
// Key: (started_at_unix_nanos, audit_id) — global time-window scans.
```

Triple-write per extraction: primary + three indexes, all in the
same wtxn.

## 3. `EXTRACTOR_AUDIT_TABLE` row shape

The table holds the §1 shape — primary row plus three indexes.

## 4. `ENTITY_RESOLUTION_AUDIT_TABLE`

Lives in `crates/brain-metadata/src/tables/knowledge/audit.rs`.
Same indexing pattern (by-entity + by-time).

## 5. `MERGE_LOG_TABLE`

Survivor-entity merge log.

## 6. Retention

Per [`./00_purpose.md`](./00_purpose.md) §"Retention":

| Table | Default retention |
|---|---|
| `EXTRACTOR_AUDIT_TABLE` + indexes | 90 days |
| `ENTITY_RESOLUTION_AUDIT_TABLE` | 90 days |
| `MERGE_LOG_TABLE` | Forever |

The sweeper that deletes expired audit rows + their index entries
is tracked as an open question in
[`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

## 7. Performance

Per the performance budgets in
[`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| Audit-row write (primary + 3 indexes, single wtxn) | 200 µs | 1 ms |
| `audit_by_memory(mem_id, limit=100)` | 500 µs | 2 ms |
| `audit_by_extractor(ext_id, limit=100)` | 500 µs | 2 ms |
| `audit_recent_failures(since, limit=100)` | 1 ms | 5 ms |

Verified by the audit-table criterion bench.

## 8. Sample query

```rust
// "Show me extractor X's most recent failures."
let mut failures = audit_by_extractor(&rtxn, ext_id, 1000)?
    .into_iter()
    .filter(|r| r.status == ExtractionStatus::Failure as u8)
    .take(20)
    .collect::<Vec<_>>();
failures.sort_by_key(|r| std::cmp::Reverse(r.started_at_unix_nanos));
```

A future admin op `ADMIN_GET_EXTRACTION_AUDIT` will wrap this with
proper filtering + pagination over the wire.

## 9. Atomicity invariants

- Extractor outputs (entity / statement / relation rows) and the
  audit row share one wtxn. Either both commit or neither does.
- Index inserts share the same wtxn as the primary insert.
- The audit row's `outputs: Vec<OutputRefRow>` is captured AFTER
  the output writes but BEFORE the wtxn commit, so the IDs in
  `outputs` are guaranteed durable.

## 10. Caller provenance under `act_as` — dual principal

Every audit row that attributes an operation to an actor records **who
acted**. Ordinarily that is a single identity: the connection's
key-derived `(namespace, agent)` plus the non-authoritative `user` tag
([`../17_observability/04_admin_ops.md`](../17_observability/04_admin_ops.md)
§12.1). Under the per-request `act_as` field — a trusted service
principal running an op on behalf of a tenant agent, defined in
[`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md)
§"Per-request identity (`act_as`)" — one identity is **not enough**.

The audit provenance MUST record **both** principals:

- the **acting service principal** — the connection's own key-derived
  identity (the `can_act_as` holder), taken from the authenticated
  session, never from the request payload; and
- the **effective identity** — the `(namespace, agent_id)` named in
  `act_as`, under which the op actually ran.

This is delegation for audit (RFC 8693): impersonation collapses the two
identities for authorization and isolation, but the acting party is
**never erased** from the record. An `act_as` op is therefore always
traceable to the edge/gateway that issued it *and* the tenant it acted
for — the two facts needed to investigate a compromised service
principal fanning across tenants. When `act_as` is absent, the acting
principal and the effective identity are the same, and the row records
that single identity as today (no dual-principal row for ordinary ops).
