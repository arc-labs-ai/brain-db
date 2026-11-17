# 11.04 Extraction Audit

Every extraction call — success, failure, or skip — writes one
`ExtractionAudit` to the `extractor_audit` redb table. Audits
are Brain's source of truth for "what did extractor X
do to memory Y?".

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Audit log" — narrative.
- [`../10_metadata/05_failure_and_audit.md`](../10_metadata/05_failure_and_audit.md)
  §1–§3a — the concrete redb shape for this table, its indexes, the
  per-memory pipeline audit and the spend ledger. That file is the single
  owner of all four; this one defers to it for every key type and field type.

## 1. What the row records

One row is the durable account of one extraction call: what ran, how it went,
what it produced and what it cost. Its field set, types and key encoding are in
[`../10_metadata/05_failure_and_audit.md`](../10_metadata/05_failure_and_audit.md)
§1; what each field is *for* is here.

| Field | What it records |
|---|---|
| `audit_id_bytes` | Identity, and the table's key. |
| `memory_id_bytes`, `extractor_id`, `extractor_version`, `schema_version` | What ran over what, under which versions. The same four values the idempotency contract holds an extractor to ([`./05_idempotency.md`](./05_idempotency.md) §1). |
| `started_at_unix_nanos`, `completed_at_unix_nanos` | How long the call took — the only durable record of an LLM round trip's latency. |
| `status`, `status_reason` | The outcome (§3). `status_reason` is empty on success and carries the operator-actionable text on every other outcome. |
| `outputs` | The entity / statement / relation ids this call produced — the only durable link from an extraction back to the rows it created. Capped at 64 per row. |
| `cost_micro_usd` | What the call was billed. Zero for the pattern and classifier tiers; the LLM tier fills it, including when the reply was billed but unusable. |
| `model_metadata` | Model version, token counts and the cache-hit flag. Empty for non-LLM tiers. |
| `input_hash` | BLAKE3 of the memory text as it was when this call ran. Comparing it against the memory's current text is what detects an **edit** — outputs describing text that no longer exists (§8). |
| `namespace` | The tenant billed. See below. |

`audit_id` is a UUIDv7 so iteration is time-ordered without a separate
timestamp index — the same convention the `entity_resolution_audit` table
uses. The key is the id's bytes rather than an integer, and the owning file
explains why that is what preserves the ordering.

`namespace` names the tenant the row's `cost_micro_usd` is billed to, spelled
exactly as the `llm_spend` ledger spells it (see
[`./01_extractor_tiers.md`](./01_extractor_tiers.md) §22). A namespace's durable
model spend is this field summed across audit rows **plus** that ledger; neither
store subsumes the other. A row the writer did not attribute carries the empty
string and is counted toward no namespace rather than toward a guessed one.

## 2. Where it's stored

In `EXTRACTOR_AUDIT_TABLE`, keyed by audit id, with three `()`-valued
secondary indexes for the per-memory, per-extractor and time-window lookups
§4 exposes. One extraction writes the primary row and all three index entries
in the same write transaction.

**The table definitions, key encodings and field types live in
[`../10_metadata/05_failure_and_audit.md`](../10_metadata/05_failure_and_audit.md)
§1–§2, and only there.** That file owns the redb shape; this one owns what the
rows mean. Transcribing the shape into both is how the two drift apart, and a
reader who needs a key type needs the owning file open anyway.

Two neighbouring tables are worth knowing about, both also defined there:

- `EXTRACTOR_PIPELINE_AUDIT_TABLE` (§3) — the per-**memory** dispatch audit,
  distinct from this one. This table records each individual
  `Extractor::run` invocation; that one records the outcome the worker
  committed for a memory, including the retry counters that decide when a
  failing memory is abandoned (§3.1 there, and
  [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)
  §"Bounded extraction retries").
- `LLM_SPEND_TABLE` (§3a) — the `(namespace, stage)` ledger this row's
  `cost_micro_usd` is summed with to get a tenant's durable model spend.

All of them are substrate redb tables
(see [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md)).

## 3. Status enum

```rust
#[repr(u8)]
pub enum ExtractionStatus {
    Success = 1,
    Failure = 2,
    SkippedFilter = 4,        // trigger condition was false.
    SkippedDuplicate = 5,     // idempotent re-run probe matched.
    SkippedDisabled = 6,      // extractor was disabled at dispatch time.
}
```

Discriminants are stable — never reassigned. New variants append. Discriminant
`3` is unassigned: no status means "refused on cost", because nothing in Brain
refuses an LLM call on cost (§22 of
[`./01_extractor_tiers.md`](./01_extractor_tiers.md)). The storage layer holds
the same values as named byte constants rather than as this enum, since a row
on disk carries a byte; the two representations are required to agree, and the
byte side is listed in
[`../10_metadata/05_failure_and_audit.md`](../10_metadata/05_failure_and_audit.md)
§1. This enum is the extractor-facing form — it is what an extractor returns,
and it converts to and from the byte at the storage boundary.

## 4. Audit query API

```rust
pub fn audit_get(
    rtxn: &ReadTransaction,
    audit_id: AuditId,
) -> Result<Option<ExtractionAudit>, AuditOpError>;

pub fn audit_by_memory(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;

pub fn audit_by_extractor(
    rtxn: &ReadTransaction,
    extractor_id: u32,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;

pub fn audit_recent_failures(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;
```

Every listing query takes a `limit` and returns **newest-first**, which the
UUIDv7 key gives for free: the index range is collected, sorted and reversed,
so recency ordering needs no timestamp index.

`audit_recent_failures` has no status-indexed table behind it. It fans out over
the time index, filters, and stops at `limit`, so in the worst case it scans
more rows than it returns.

The operator surface for these queries is `GET /v1/audit` on the admin
plane (bulk: `GET /v1/audit/export`); the API
internally only.

## 5. Retention

Default 90 days, configurable per deployment. A periodic worker
(`audit_log_sweeper`, see
[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md))
iterates `EXTRACTOR_AUDIT_BY_TIME_TABLE` and deletes rows older than the
cutoff plus their index entries.

## 6. Atomicity

Audit row writes share the `wtxn` that produced the extracted
outputs:

```text
wtxn = db.begin_write()
  extractor produces ExtractedItem[]
  for each item: entity_put / statement_create / relation_create
  write_audit_row(wtxn, ExtractionAudit { outputs, ... })
wtxn.commit()
```

If any output write fails, the wtxn rolls back — both the outputs
AND the audit row disappear together. There's no "audit says
success but no output" or vice versa.

## 7. Performance budget

Per [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md):

| Operation | p50 | p99 |
|---|---|---|
| `write_audit_row` (single wtxn cost) | 200 µs | 1 ms |

Three index inserts + one primary insert. Verified by the
audit-table criterion bench.

## 8. What stops repeated work

Two gates stop repeated work, and neither reads this table.

**Queue replay, per memory.** With `[workers.extractor] skip_already_extracted`
on (the default), the worker consults the per-memory pipeline audit before
running anything: a memory whose extraction is **terminal** is dropped from the
queue, and one carrying an unspent transient-failure budget is re-run once its
backoff elapses. That probe reads `EXTRACTOR_PIPELINE_AUDIT_TABLE`, not the
per-call rows here, because the question it asks — "is this memory done with?" —
is about the memory, not about one extractor's invocation. Turning the flag off
is what lets a backfill drive a second pass over an already-audited memory.

**The LLM call, per input.** The LLM tier's own cache is keyed
`(input_hash, extractor_id, extractor_version, model_id, credential_scope)`
and is what makes a repeated extraction free rather than merely harmless
(§20 of [`./01_extractor_tiers.md`](./01_extractor_tiers.md)).

`input_hash` on this row serves a narrower purpose: it is BLAKE3 over the
memory text, so comparing it against the memory's current text detects an
**edit** — a case where the stored outputs describe text that no longer
exists. That is a diagnostic, not a gate.

Nothing re-emits an audit row's `outputs` in place of re-running an extractor.
When a retry does re-run the pipeline, duplication is prevented at the apply
layer instead; [`./05_idempotency.md`](./05_idempotency.md) §4 has the
mechanism.

## 9. Open questions

- `outputs` could be unbounded; v1 caps at 64 entries per row,
  overflow written to a follow-on row.
- `cost_micro_usd` is 0 for pattern and classifier extractors;
  the LLM tier fills it.
