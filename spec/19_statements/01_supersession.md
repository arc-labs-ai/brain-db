# 19.01 Statement Supersession

How a statement is replaced by a new version while preserving the audit chain. The mechanism backs `STATEMENT_SUPERSEDE` (`0x0142`) wire op + the auto-supersession that fires on `STATEMENT_CREATE` for Preferences.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Kind-specific contracts" — which kinds allow supersession.
- [`../25_provenance_versioning/00_purpose.md`](../25_provenance_versioning/00_purpose.md) — broader provenance + versioning model.
- [`../28_knowledge_wire_protocol/06_statement_frames.md`](../28_knowledge_wire_protocol/06_statement_frames.md) §5 — `STATEMENT_SUPERSEDE` wire shape.

## 1. The data model

Each `Statement` carries three supersession-related fields:

```rust
struct Statement {
    // ...
    version: u32,                          // 1 for chain root; +1 per supersession
    superseded_by: Option<StatementId>,    // forward link
    supersedes: Option<StatementId>,       // back-pointer
    // ...
}
```

Plus a derived `chain_root`: the `StatementId` of the original (first) statement in the chain. **Stored as a separate index entry** in `STATEMENT_CHAIN_TABLE` keyed by `(chain_root, version) → StatementId.to_bytes()` (see [`./03_storage.md`](./03_storage.md)).

The chain is therefore queryable two ways:

- **By starting from any statement in the chain:** walk forward via `superseded_by` or backward via `supersedes`.
- **By chain root:** range-scan `STATEMENT_CHAIN_TABLE` at prefix `(chain_root, *)` — returns the full chain in version order.

The second form is what `STATEMENT_HISTORY` exposes.

## 2. Which kinds support supersession

| Kind | Supersession allowed | Trigger |
|---|---|---|
| Fact | Explicit only (rare; "this Fact was wrong") | `STATEMENT_SUPERSEDE` opcode |
| Preference | Yes (the common case) | Auto-fires on `STATEMENT_CREATE` of a Preference with same `(subject, predicate)` |
| Event | **No** — Events are point-in-time; corrections are new Events with provenance notes | (would be rejected) |

The kind-specific rule lives at the validation layer in `statement_ops::statement_create` and `::statement_supersede`. Wire shape (§28/06 §5) doesn't enforce this; the handler does.

### 2.1 Why Preferences auto-supersede

A Preference like `(Priya, prefers, async_meetings)` represents a **current belief**. When a new Preference with the same `(subject, predicate)` arrives, the previous one is no longer current — it's history. The substrate's job is to **keep the chain intact**, not pick winners.

So on `STATEMENT_CREATE` for kind=Preference:

1. Look up the current Preference with same `(subject, predicate)` via `statements_by_subject` (filter: `kind=Preference, superseded_by IS NULL, tombstoned=false, valid_to_in_range(now)`).
2. If one exists: supersede it atomically inside the same redb txn that creates the new one.
3. If none: just create.

This auto-step keeps callers from having to issue two opcodes for the common case.

### 2.2 Why Facts don't auto-supersede

A new Fact with same `(subject, predicate)` but **different object** is a **contradiction**, not a supersession — both are stored. See [`./02_contradiction.md`](./02_contradiction.md). The resolver / human decides which is right.

A new Fact with same `(subject, predicate)` and **same object** is a duplicate. The wire-side `request_id` idempotency layer typically dedupes; if it falls through, the handler returns `Conflict` rather than auto-superseding (no signal that the new is "better" than the old).

Explicit `STATEMENT_SUPERSEDE` on a Fact says "I'm replacing this; here's the new statement" — caller takes responsibility.

### 2.3 Why Events never supersede

Events are point-in-time records. "Priya scheduled the planning meeting at 14:00" is a fact about a moment; if it was wrong, you don't *replace* it, you author a new Event ("correction: scheduled at 15:00, prior record was wrong") and the original stays as a record of what was thought at that time.

`STATEMENT_SUPERSEDE` on an Event returns `INVALID_ARGUMENT`.

## 3. Mechanics — `statement_supersede(old_id, new_statement, now)`

Single redb write transaction. All steps atomic:

```text
1. Load old statement.
2. Pre-conditions:
   - old must exist                                    → STATEMENT_NOT_FOUND
   - old.tombstoned must be false                      → INVALID_ARGUMENT
   - old.superseded_by must be None                    → INVALID_ARGUMENT (already superseded)
   - old.kind must not be Event                        → INVALID_ARGUMENT
   - new_statement.kind must equal old.kind            → INVALID_ARGUMENT
   - new_statement.subject must equal old.subject      → INVALID_ARGUMENT
   - new_statement.predicate must equal old.predicate  → INVALID_ARGUMENT
   - new_statement.id must not exist yet               → IdempotencyConflict
3. Allocate new statement_id (UUIDv7).
4. Compute chain_root:
   - if old.supersedes.is_none():   chain_root = old.id
   - else:                          chain_root = STATEMENT_CHAIN_TABLE.lookup(old.id).chain_root
5. Compute version:
   - version = old.version + 1
6. Set fields:
   - new.version = version
   - new.supersedes = Some(old.id)
   - new.chain_root = chain_root
   - new.superseded_by = None
7. Insert new into STATEMENTS_TABLE + all secondary indexes (per
   spec/19_statements/03_storage.md).
8. Update old in place:
   - old.superseded_by = Some(new.id)
   - if old has valid_to_unix_nanos field (Fact / Preference):
       old.valid_to = new.extracted_at
   - re-index old in STATEMENTS_BY_SUBJECT_TABLE since the
     `is_current` bit (key column 4) flipped from 1 to 0.
9. Insert (chain_root, version) → new.id into STATEMENT_CHAIN_TABLE.
10. Commit.
11. Post-commit: emit STATEMENT_SUPERSEDED event on the SUBSCRIBE
    channel (§28/02 §3.2) with old_id, new_id, chain_root.
```

### 3.1 The `is_current` bit in `STATEMENTS_BY_SUBJECT_TABLE`

The key shape is `(subject, kind, predicate_id, is_current)` where `is_current` is `1` iff `superseded_by.is_none() && !tombstoned && valid_at(now)`. The bit lets the "current state" query be a point-lookup at `(subject, kind, predicate_id, 1)` rather than a scan-and-filter.

When `statement_supersede` flips `old.superseded_by` to `Some(_)`, it must also flip `old`'s entry in this index from `is_current=1` to `is_current=0`. That's a remove + insert in the same redb txn.

### 3.2 valid_to inheritance

For Fact / Preference, `valid_to` defaults to `None` (open-ended). When `old` is superseded by `new`, the substrate sets `old.valid_to = new.extracted_at_unix_nanos` (i.e. "old was valid up until new arrived").

If `old` had an explicit `valid_to_unix_nanos != 0` set by the caller (e.g. "this fact was only valid through end of 2026"), the substrate **preserves** that value rather than overwriting — the explicit constraint wins.

The supersede logic:

```text
if old.valid_to_unix_nanos == 0 and old.kind != Event:
    old.valid_to_unix_nanos = new.extracted_at_unix_nanos
```

Events have `valid_to_unix_nanos = 0` permanently (events are point-in-time per §00); the rule above is gated on kind for that reason.

## 4. Chain traversal

`STATEMENT_HISTORY` (opcode `0x0145`) walks the chain in version order:

```text
For chain_root = anchor_id_or_followed_chain_root:
    range-scan STATEMENT_CHAIN_TABLE at prefix (chain_root, *)
    for each (chain_root, version) -> statement_id:
        load Statement
        emit
```

Returns the full chain ordered by `version` ascending. Spec §28/06 §8 details the wire-side shape.

### 4.1 Anchor flexibility

`STATEMENT_HISTORY` accepts **either** a chain root id **or** any statement id in the chain:

```text
if anchor exists in STATEMENT_CHAIN_TABLE as key.0 (i.e. it's a chain_root):
    use anchor as chain_root directly.
else:
    load Statement(anchor); use anchor.chain_root.
```

This lets callers pass `superseded_by`-chained ids without having to find the root first.

## 5. Lookup performance

The `(chain_root, version)` index gives:

- Full-chain history: 1 prefix scan, O(version count) seeks.
- Current statement (version = max): 1 reverse prefix scan, O(1) effective.
- N-th version: 1 point lookup.

The substrate's redb b-tree has predictable per-seek cost (~µs). Even 100-version chains traverse in under 1 ms.

## 6. Versioning invariants

For any chain:

- Versions are dense `1..=N` (no gaps).
- Exactly one statement per chain has `superseded_by.is_none() && !tombstoned` (the "current" entry). This is the one returned by `is_current=1` index lookups.
- After tombstoning the current entry, *no* statement in the chain has `is_current=1`. Queries by `(subject, predicate)` return empty.
- After unretiring (a hypothetical future op — not in v1.0): re-flips `is_current=1`. Tracked in [`./06_open_questions.md`](./06_open_questions.md).

These invariants are enforced by `statement_ops` and verified by §17/02 §21.1 unit tests.

## 7. Cross-shard supersession

Statements are sharded by `subject` EntityId. Supersession typically stays within one shard because subject doesn't change.

Edge case: `statement_supersede` is called with `old_id` whose subject differs from `new_statement.subject` — rejected per §3 step 2 ("`new_statement.subject` must equal `old.subject`"). Cross-shard chains are not possible by construction.

## 8. Audit trail

Every supersession writes an audit record to `entity_resolution_audit` (re-used as a generic audit table — kind discriminator `STATEMENT_SUPERSEDED`). Tracks who superseded whom, when, why. Retained indefinitely per §25.

## 9. Open questions

See [`./06_open_questions.md`](./06_open_questions.md).
