---
name: brain-redb-schema
description: Audit redb metadata-store schema changes — table layout matches spec §07/02, migrations require version bump, idempotency table consulted on writes.
when-to-use: |
  Triggers:
    - Diff in crates/brain-metadata/**/*.rs
    - Adding or renaming a redb table
    - Changing an existing table's key or value type
    - Touching the idempotency / dedupe table
    - User says "metadata schema" / "redb migration"
trigger-files:
  - crates/brain-metadata/**/*.rs
spec-refs:
  - spec/07_metadata_graph/02_table_layout.md
  - spec/07_metadata_graph/06_idempotency.md
  - spec/07_metadata_graph/08_transactions.md
---

# redb Schema Audit

## When to use

Any change to the redb metadata store: tables, keys/values, indexes, transactions, idempotency, or migrations.

## What this enforces

### Spec contracts

- **Table layout per §07/02.** Every table named in the spec exists; types match the documented key/value pairs.
- **Idempotency table per §07/06.** Every write op consults a `RequestId → response` table before doing work; same params → cached response; different params with same id → `Conflict`.
- **Transactions per §07/08.** Multi-table writes go through a single redb write transaction; commits are atomic.
- **Schema versioning.** Adding a table or changing a key/value type is a schema-version bump. Migration code must handle the prior version (or document "no support for old data").

## Workflow

1. **Identify the diff shape.** New table? Renamed table? Key/value type change? Migration code? Each has a different audit path.
2. **Cross-check against spec §07/02 (table layout).** Every spec'd table is present. Every present table is spec'd. Drift either direction → STOP and surface.
3. **Idempotency.** New write op? Confirm the handler:
   - Hashes the canonical request payload (deterministic).
   - Reads `idempotency_table[request_id]`.
   - On miss: do the work, cache the response.
   - On hit with matching hash: return cached response.
   - On hit with mismatched hash: return `Conflict`.
4. **Transactional grouping.** A single op that touches multiple tables (e.g., encode → memory table + edge table + context counter) opens one redb write transaction and commits at the end. No partial commits.
5. **Migration.** Schema bump (new table, changed type) requires:
   - Bumped schema-version constant.
   - Migration function from prior version.
   - Test that loads a fixture from the prior version and validates post-migration state.

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| New table without spec entry | Drift | STOP and surface; spec §07/02 must update first |
| Idempotency table not consulted on write | Duplicate effects on retry | Read first, write last |
| Multiple `write_txn().commit()` for one op | Partial-commit risk | Single transaction, single commit |
| Schema version unchanged on type change | Migration bug | Bump version + add migration |
| `expect("table exists")` on first start | Missing setup | Create-if-missing in init |

## Test coverage

- **Round-trip per table:** insert → read → assert.
- **Idempotency:** retry same op with same RequestId → no duplicate; same response.
- **Idempotency conflict:** retry with different params and same RequestId → `Conflict`.
- **Transaction atomicity:** kill mid-op (chaos test) → either all writes visible or none.
- **Migration:** load fixture from prior schema → assert post-migration state matches expected.

## Cross-references

- `brain-invariants` — invariant #5 (idempotency by RequestId).
- `brain-chaos-test` — for the kill-during-transaction tests.
- spec §07.

## Source / Adaptations

Project-local. Operationalizes spec §07.
