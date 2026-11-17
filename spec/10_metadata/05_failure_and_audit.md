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

**Failure mode.** The metadata file's storage format version is not the one this binary writes.

**Detection.** On open, Brain reads the single version row from `__schema_meta` and compares it to its own (see [`02_table_layout.md`](02_table_layout.md) §6).

**Response.** Brain refuses to open, in either direction, with a distinct error for each:
- A **newer** file: the binary is older than the data it is being pointed at. The message names both versions and says to upgrade the substrate or restore from a compatible backup.
- An **older** file: the message names both versions and says to delete the data directory and restart for a fresh database.

Refusal is the whole design. There is no migration registry and no migration tool, so an older file has nowhere to go; opening it anyway is worse than refusing, because the failures it produces do not announce themselves as version problems. A `memory_artifacts` bundle that does not parse resolves to the default, so the inspect view of every stored memory comes back blank with nothing logged, and an rkyv row that does not resolve panics the reader — which the admin audit query and the audit sweeper both are.

**Operator action.** For a newer file, upgrade Brain or point it at a file this binary wrote. For an older file, start from an empty data directory; there is nothing to migrate and nothing to recover in place.

## 7. Open-time index construction failure

**Failure mode.** The `entity_by_type` listing index or the id-ordered statement pagination indexes cannot be built on open (e.g. disk error while writing index rows).

**Detection.** The error propagates out of the open path before the transaction commits.

**Response.**
- The open-time write transaction aborts, so the version stamp, the table materialization and the index rows all roll back together. The file is byte-identical to what it was before the open attempt.
- Brain refuses to start.

These indexes are derived data, rebuilt from the primary rows exactly as the in-RAM indexes are, and the rebuild is idempotent — a no-op once the index holds a row. So a retry is free and a partial build is not a state the file can be left in.

**Operator action.** Address the underlying error (disk space, permissions) and restart.

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
- For each edge in `edges`, verify both endpoints exist.
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

## Audit Tables

redb table layout for the audit logs (extraction, resolution,
merge) and for the spend they are summed with. Brain ships
`EXTRACTOR_AUDIT_TABLE` + its three indexes, the per-memory
`EXTRACTOR_PIPELINE_AUDIT_TABLE`, the `LLM_SPEND_TABLE` ledger, and
the `ENTITY_RESOLUTION_AUDIT` and `MERGE_LOG` tables.

This file holds the concrete redb shape for all of them. The narrative
is in `00_purpose.md` and the extractor-facing contract is in
`../11_extractors/04_audit.md`; both defer here for key types,
field sets and the stable byte values.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"The audit log" — narrative.
- [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) —
  extractor audit row spec.

## 1. `EXTRACTOR_AUDIT_TABLE`

Primary record. One row per extraction call.

```rust
pub const EXTRACTOR_AUDIT_TABLE:
    TableDefinition<'static, [u8; 16], ExtractionAudit> =
    TableDefinition::new("extractor_audit");

#[derive(Archive, Serialize, Deserialize, ...)]
pub struct ExtractionAudit {
    pub audit_id_bytes: [u8; 16],       // UUIDv7
    pub memory_id_bytes: [u8; 16],
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status: u8,                     // extraction_status::*
    pub status_reason: String,          // empty on SUCCESS
    pub outputs: Vec<OutputRef>,        // ≤ OUTPUTS_CAP (64); overflow → follow-on row (post-v1)
    pub cost_micro_usd: u64,            // 0 for pattern + classifier; the LLM tier fills it
    pub model_metadata: Vec<u8>,        // rkyv-archived blob; empty for non-LLM
    pub input_hash: [u8; 32],           // BLAKE3 of memory.text
    pub namespace: String,              // the tenant this extraction is billed to
}

pub struct OutputRef {
    pub kind: u8,                       // 1=Entity, 2=Statement, 3=Relation, 4=EntityMention
    pub id: [u8; 16],                   // EntityMention outputs are transient: all-zero id
}
```

Ids are carried as their 16 big-endian bytes, not as a `u128`. The key is the `audit_id`'s bytes, so redb's byte ordering is the UUIDv7 ordering: insertion order ≈ time order, and an iteration over the table is roughly newest-last. A `u128` key would order by native integer comparison over a value nothing else in the store holds that way.

`status` is one of five stable bytes — `SUCCESS` 1, `FAILURE` 2, `SKIPPED_FILTER` 4, `SKIPPED_DUPLICATE` 5, `SKIPPED_DISABLED` 6. **Byte 3 is unassigned and stays unassigned**: these bytes are positional on disk, so giving an unassigned byte a new meaning would silently give that meaning to every row already carrying it.

`namespace` is the tenant name the extraction is billed to, written with the same spelling the `llm_spend` ledger (§3a) uses for that tenant. A namespace's durable model spend is this field's `cost_micro_usd` summed with that ledger, so the two must name the tenant identically or the sum is wrong. The field is empty when the writer did not attribute the row; an unattributed row counts toward no namespace rather than toward a guessed one, and the empty namespace is the one name that owns nothing.

## 2. Indexes

Three secondary indexes, each `()`-valued (the row data lives in
the primary table; indexes only support lookups).

```rust
pub const EXTRACTOR_AUDIT_BY_MEMORY_TABLE:
    TableDefinition<'static, ([u8; 16], [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_memory");
// Key: (memory_id_bytes, audit_id_bytes) — iterating
// `(mem_id, [0; 16])..=(mem_id, [0xff; 16])` returns all audits for
// one memory in audit-id order.

pub const EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE:
    TableDefinition<'static, (u32, [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_extractor");
// Key: (extractor_id, audit_id_bytes) — per-extractor history.

pub const EXTRACTOR_AUDIT_BY_TIME_TABLE:
    TableDefinition<'static, (u64, [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_time");
// Key: (started_at_unix_nanos, audit_id_bytes) — global time-window scans.
```

Triple-write per extraction: primary + three indexes, all in the
same wtxn.

## 3. `EXTRACTOR_PIPELINE_AUDIT_TABLE`

The per-**memory** dispatch audit, distinct from §1. §1 records each individual `Extractor::run` invocation; this one records the per-memory outcome the worker committed — which tiers fired, what the apply pass kept, what the LLM tier cost, and how many times the memory has been attempted.

```rust
pub const EXTRACTOR_PIPELINE_AUDIT_TABLE:
    TableDefinition<'static, [u8; 16], ExtractorPipelineAuditEntry> =
    TableDefinition::new("extractor_pipeline_audit");

pub struct ExtractorPipelineAuditEntry {
    pub memory_id_bytes: [u8; 16],
    pub extracted_at_unix_nanos: u64,
    pub status: u8,                       // pipeline_status::*
    pub status_reason: String,            // empty on SUCCESS; else the first tier's error
    pub tier_pattern: u8,                 // tier_status::*
    pub tier_classifier: u8,              // tier_status::*
    pub tier_llm: u8,                     // tier_status::*
    pub item_counts: ExtractorItemCounts,
    pub llm_micro_usd_spent: u64,         // 0 when the LLM tier didn't run
    pub attempts: u8,                     // starts at 1
    pub failure_class: u8,                // failure_class::*
    pub item_attributable_failures: u8,
}

pub struct ExtractorItemCounts {
    pub entities: u32,
    pub statements: u32,
    pub relations: u32,
    pub mention_edges: u32,
    pub dropped: u32,                     // candidates the apply pass discarded
    pub contradictions: u32,              // Facts committed into a Fact-vs-Fact conflict
    pub entities_undeclared: u32,         // entities whose type no schema declares
    pub had_active_schema: bool,          // the denominator for entities_undeclared
}
```

The key is the memory id's big-endian bytes, one row per memory, which is what makes the row an idempotency probe: the worker checks it before re-running extraction, so a queue replay on worker restart cannot double-write entities, statements or mention edges.

`status` is one of four stable bytes — `SUCCESS` 1, `PARTIAL_FAILURE` 2, `FAILURE` 3, `SKIPPED` 4. A partial failure is still audited: the worker committed what the surviving tiers produced, and re-running would duplicate it. `tier_status` is likewise stable at `RAN` 1, `SKIPPED` 2, `FAILED` 3, `ABSENT` 4 — `ABSENT` distinguishes "this deployment has no LLM extractor configured" from "it ran and declined".

### 3.1 The two retry counters

`attempts` and `item_attributable_failures` are the durable half of the retry decision, and they are two counters rather than one because they bound different failures.

`failure_class` says whether a failed LLM tier is worth retrying at all: `UNCLASSIFIED` 0, `TRANSIENT` 1, `PERMANENT` 2. Unclassified is treated as retryable — a transient blip is the safe default, since retry-with-backoff is bounded and cheap while dropping grounding on an unclassified failure is not. `PERMANENT` (absent key, no balance, malformed request, schema mismatch) is terminal immediately so an operator sees it instead of a silent retry loop.

A transient failure is retried on widening backoff, `min(1 h, 1 s × 2^(attempts − 1))`, and is terminal once **either** budget is spent:

| Counter | Budget | What it catches |
|---|---|---|
| `item_attributable_failures` | 5 | A memory failing on its own content. Only incremented when the provider was demonstrably reachable — another LLM-tier run on the shard succeeded inside the worker's reachability window. |
| `attempts` | 48 | A provider that never comes back. Derived: the two-day outage window the budget covers, divided by the capped one-hour retry interval. |

Splitting them is what keeps a provider outage from spending the per-item budget on memories that are perfectly fine. An attempt not attributable to the memory is charged to the provider and counts only toward `attempts`, so an outage cannot convert a shard's whole queue into permanent derivation loss.

Two reads gate the queue, in that order. The first asks whether the memory is already extracted: a `SUCCESS` or `SKIPPED` row is, and so is a failure whose retry budget is spent — both are terminal and leave the queue. The second asks whether a memory that survived the first is *due* now: a memory with no row is, and an unspent retryable transient failure only once its backoff has elapsed since `extracted_at_unix_nanos`.

## 3a. `LLM_SPEND_TABLE`

Durable spend for the model-backed stages that produce **no** extraction audit row.

```rust
pub const LLM_SPEND_TABLE:
    TableDefinition<'static, (&str, u8), u64> =
    TableDefinition::new("llm_spend");

pub mod llm_spend_stage {
    pub const HYPE_GENERATE: u8 = 1;
    pub const HYPE_REFRESH: u8 = 2;
    pub const ENTITY_DISAMBIGUATION: u8 = 3;
}
```

Keyed `(namespace, stage)` to a running micro-USD total (1e-6 USD). The three stage bytes are stable and never reassigned.

A namespace's durable model spend is this ledger **plus** the `cost_micro_usd` on its §1 audit rows. Summing only the ledger misses every extraction; adding extraction costs into the ledger double counts them against the audit rows that already hold them. The per-tenant total is computed by summing both sides on the row's own `namespace` field — no join through each memory's space is involved.

Two stores rather than one, because neither subsumes the other. An audit row answers "what did extracting this memory cost", which a `(namespace, stage)` counter cannot express. The ledger answers "what has this tenant spent on this stage", which the per-call audit rows do not aggregate. The stages in this ledger have no per-call record to hang a cost on, so a counter is the whole of what is durable for them.

Summing both sides scans the extraction audit table, so it is a reporting read, not a request-path one.

## 4. `ENTITY_RESOLUTION_AUDIT_TABLE`

Lives alongside the extraction audit in
`crates/brain-metadata/src/tables/audit.rs`.
Same indexing pattern (by-entity + by-time).

## 5. `MERGE_LOG_TABLE`

Entity-merge history, and the record unmerge replays in reverse.

```rust
pub const MERGE_LOG_TABLE:
    TableDefinition<'static, (u64, [u8; 16]), MergeRecord> =
    TableDefinition::new("merge_log");

pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    pub reason: String,
    pub actor_kind: u8,                                  // actor_kind::*
    pub actor_space_bytes: [u8; 16],
    pub aliases_added: Vec<String>,                      // what the merged entity contributed
    pub trigrams_added: Vec<[u8; 3]>,
    pub attribute_conflicts: Vec<AttributeConflictRecord>,
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
    pub mention_count_added: u32,
    pub rerouted_statements: Vec<StatementReroute>,
    pub rerouted_relations: Vec<RelationReroute>,
    pub survivor_attributes_before: Vec<u8>,
    pub finalized: u8,
    pub unmerged_at_unix_nanos: u64,
    pub unmerged_by_actor_kind: u8,
    pub unmerged_by_space_bytes: [u8; 16],
}
```

The key is `(timestamp_unix_nanos, merge_id_bytes)`, so the table traverses in merge order and a grace-period unmerge can find the merges it may still reverse by range rather than by scan.

The row is not a summary. It carries the **exact per-row diff** a merge applied, because unmerge replays it rather than recomputing it: which aliases and trigrams the merged entity contributed, the survivor's attribute blob as it stood before, and per re-routed record the old and new version plus the chain root, so a statement's version-keyed chain row can be rewritten back to exactly where it was. Recomputing instead would have to re-derive symmetric relation canonicalisation and re-guess which of two coexisting values the merge had chosen.

`actor_kind` is `SYSTEM` 0 (the resolver or a background worker) or `SPACE` 1 (an operator acting over the wire), with `actor_space_bytes` naming the space in the latter case. `AttributeConflictRecord` records each conflicting attribute with the `policy` that decided it — `SURVIVOR_WINS` 1, `MERGED_WINS` 2, `NEWEST_WINS` 3, `CONCAT_TEXT` 4, `REJECT_MERGE` 5 — and the `outcome` that followed: `KEPT_SURVIVOR` 1, `REPLACED_WITH_MERGED` 2, `CONCATENATED` 3. Both byte sets are stable.

### 5.1 `ENTITY_MERGE_AUDIT_OVERFLOW`

```rust
pub const ENTITY_MERGE_AUDIT_OVERFLOW:
    TableDefinition<'static, ([u8; 16], u32), MergeAuditOverflow> =
    TableDefinition::new("entity_merge_audit_overflow");

pub struct MergeAuditOverflow {
    pub rerouted_statement_ids: Vec<[u8; 16]>,
    pub rerouted_relation_ids: Vec<[u8; 16]>,
}
```

Keyed `(merge_id_bytes, chunk)`. A merge of two heavily-referenced entities re-routes more ids than belong inline in one `MergeRecord`, so the id lists spill here in chunks and the primary row stays a bounded read for every merge that did not need them.

## 5a. `MERGE_REVIEW_QUEUE_TABLE`

Entity-merge proposals in the `[0.7, 0.95)` confidence band — the ones the resolver is neither confident enough to apply nor willing to discard.

```rust
pub const MERGE_REVIEW_QUEUE_TABLE:
    TableDefinition<'static, [u8; 16], MergeReviewProposal> =
    TableDefinition::new("merge_review_queue");

pub const MERGE_REVIEW_BY_STATUS_TABLE:
    TableDefinition<'static, (u8, [u8; 16]), ()> =
    TableDefinition::new("merge_review_by_status");

pub struct MergeReviewProposal {
    pub proposal_id: [u8; 16],
    pub source_entity: [u8; 16],         // the entity the resolver minted for this surface form
    pub candidate_entity: [u8; 16],      // the close-but-not-confident existing entity
    pub confidence: f32,
    pub tier_that_proposed: u8,          // proposal_tier::*
    pub proposed_at_unix_nanos: u64,
    pub status: u8,                      // proposal_status::*
    pub resolved_at_unix_nanos: u64,
    pub last_recheck_confidence: f32,
    pub last_recheck_unix_nanos: u64,
}
```

Above the band the resolver auto-merges; below it, the candidate would be dropped and the signal lost. Parking the proposal takes the third option: the write proceeds under a freshly minted entity so nothing blocks, and a periodic worker re-checks the proposal as the entity HNSW grows. The two `last_recheck_*` fields are what make that re-check a decision rather than a guess — they say what the cosine was the last time anyone looked, and when.

`status` is stable at `PENDING` 0, `AUTO_APPLIED` 1, `APPROVED` 2, `REJECTED` 3, `EXPIRED` 4, and `proposal_tier` at `EXACT` 1, `ALIAS` 2, `EMBEDDING` 3, `LLM` 4. Recording the proposing tier is what lets an operator read the signal's strength rather than only its score.

The secondary index keys `(status_byte, proposal_id)` directly on the status byte, so the worker's "find pending" and an admin's "list pending" are the same range scan over `(PENDING, _)` instead of a read of every proposal.

## 6. Retention

Per [`./00_purpose.md`](./00_purpose.md) §"Retention":

| Table | Default retention |
|---|---|
| `EXTRACTOR_AUDIT_TABLE` + indexes | 90 days |
| `ENTITY_RESOLUTION_AUDIT_TABLE` | 90 days |
| `MERGE_LOG_TABLE` | Forever |
| `EXTRACTOR_PIPELINE_AUDIT_TABLE` | Forever |
| `LLM_SPEND_TABLE` | Forever |

The last two are not retention candidates, for different reasons.
A pipeline audit row (§3) is load-bearing state, not a log: it is the
idempotency probe, so deleting it makes the memory eligible for
extraction again and re-runs every tier against text already grounded.
The spend ledger (§3a) is a running total with no per-row timestamp to
expire against — there is nothing in a row to age out, and dropping one
would silently reduce a tenant's durable spend.

The `audit_log_sweeper` background worker enforces this window. It
hard-deletes `EXTRACTOR_AUDIT_TABLE` rows past the retention cutoff
along with their by-memory / by-extractor / by-time index entries, and
expired `ENTITY_RESOLUTION_AUDIT_TABLE` rows. Merge / unmerge rows live
on a separate table and are never swept.

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
- The audit row's `outputs: Vec<OutputRef>` is captured AFTER
  the output writes but BEFORE the wtxn commit, so the IDs in
  `outputs` are guaranteed durable.

## 10. Caller provenance under `act_as` — dual principal

Every audit row that attributes an operation to an actor records **who
acted**. Ordinarily that is a single identity: the connection's
key-derived `(namespace, space)` plus the non-authoritative `user` tag
([`../17_observability/04_admin_ops.md`](../17_observability/04_admin_ops.md)
§12.1). Under the per-request `act_as` field — a trusted service
principal running an op on behalf of a tenant agent, defined in
[`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md)
§"Per-request identity (`act_as`)" — one identity is **not enough**.

The audit provenance MUST record **both** principals:

- the **acting service principal** — the connection's own key-derived
  identity (the `can_act_as` holder), taken from the authenticated
  session, never from the request payload; and
- the **effective identity** — the `(namespace, space_id)` named in
  `act_as`, under which the op actually ran.

This is delegation for audit (RFC 8693): impersonation collapses the two
identities for authorization and isolation, but the acting party is
**never erased** from the record. An `act_as` op is therefore always
traceable to the edge/gateway that issued it *and* the tenant it acted
for — the two facts needed to investigate a compromised service
principal fanning across tenants. When `act_as` is absent, the acting
principal and the effective identity are the same, and the row records
that single identity as today (no dual-principal row for ordinary ops).
