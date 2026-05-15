# 19.02 Contradiction Handling

When two active Facts have the same `(subject, predicate)` but different `object`, they **contradict**. The substrate **never auto-resolves**; it surfaces the conflict and lets the caller / human decide.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Kind-specific contracts" — Fact-only rule.
- [`./01_supersession.md`](./01_supersession.md) §2.2 — why Facts don't auto-supersede.

## 1. The detection rule

A pair of statements `S1`, `S2` contradicts iff **all** of:

1. `S1.kind == StatementKind::Fact && S2.kind == StatementKind::Fact`.
2. `S1.subject == S2.subject`.
3. `S1.predicate_id == S2.predicate_id`.
4. `S1.object != S2.object` (tagged-union inequality — different variant or different inner value).
5. Both are **active**: not tombstoned, not superseded.
6. Their validity intervals overlap (`valid_from..=valid_to` ranges intersect with `now` or with each other for as-of queries).

Preference and Event are explicitly excluded:

- Preferences supersede each other (§01 §2.1) — no contradiction.
- Events are point-in-time — two events at different times aren't a contradiction; two events at the same time about the same subject + predicate are recorded as-is (the substrate trusts the source).

## 2. The non-action

When `statement_create` is called and would produce a Fact that contradicts an existing active Fact, the substrate **stores it anyway**. Both Facts coexist. Neither is "right"; the substrate has no authority to decide.

Wire-side: `STATEMENT_CREATE` returns success; the response carries the new `StatementId` like any other create. The contradiction is **not** signalled in the success path — clients that care must explicitly query.

### 2.1 Why not error on contradiction?

Two reasons:

1. **The substrate doesn't know what's true.** It receives claims from extractors / agents / humans. Refusing the conflicting claim would silently drop information.
2. **Contradiction is signal.** Two contradictory claims means the upstream source has an inconsistency. The right response is to surface it, not hide it.

The trade-off: query consumers must be aware that contradictions exist. The default query (`STATEMENT_LIST where_subject(x) of_kind(Fact)` returning current Facts) returns **all** non-superseded non-tombstoned active Facts, which may include contradictory pairs. Consumers post-process or rank.

## 3. The surface op

`statements_contradicting(rtxn, subject, predicate) -> Vec<Statement>` (in `brain-metadata::statement_ops`):

```text
results = STATEMENTS_BY_SUBJECT_TABLE.range(
    (subject, StatementKind::Fact as u8, predicate_id, 1)..=
    (subject, StatementKind::Fact as u8, predicate_id, 1),
)
contradicting = []
for s in results:
    if s is active and overlaps_validity(s, now):
        contradicting.push(s)
if contradicting.iter().map(|s| &s.object).collect::<HashSet>().len() <= 1:
    return []   // single object => no contradiction
return contradicting
```

Returns:
- Empty if no active Facts with that `(subject, predicate)`.
- Empty if all active Facts agree (`object` equal across all).
- The set of disagreeing Facts otherwise — caller decides how to surface.

### 3.1 Wire / SDK surface

In v1.0 contradiction inspection is internal to the substrate (used by query routing in phase 23) and by the admin op `ADMIN_LIST_PENDING_RESOLUTIONS` ([§28/14](../28_knowledge_wire_protocol/14_admin_frames.md) §4). There is **no** `STATEMENT_LIST_CONTRADICTIONS` wire opcode in v1.

Clients that want contradictions:

- Call `STATEMENT_LIST` with `subject + predicate + only_current=true`.
- Inspect the returned set; if more than one distinct `object`, the set is contradictory.

Phase 23's hybrid query router exposes contradictions in `QUERY_TRACE` debug output. Production callers route there.

## 4. Resolving contradictions

Three operator-facing options for resolving:

### 4.1 Tombstone the wrong one

`STATEMENT_TOMBSTONE` on the incorrect Fact. The other remains active; the chain stays intact for audit.

### 4.2 Supersede both

`STATEMENT_SUPERSEDE` on **each** Fact with a single new Fact that authoritatively settles the dispute. Each gets a different chain — they were independent original Facts. The new Fact must reference both via its `evidence` field (or just inherit evidence from one).

### 4.3 Retract one

`STATEMENT_RETRACT` (hard delete) on the wrong Fact. Removes the row after the grace period. Used when the wrong Fact was authored in error and shouldn't persist in audit.

The substrate **doesn't pick** — operators / agents pick via these explicit opcodes.

## 5. Detection during create

`statement_create` for a Fact runs the contradiction check inside the same redb txn that inserts. The check is **read-only** — it surfaces the conflict to the caller-emitted event (§5.1) but does **not** block the insert.

```text
At step "validate" in statement_create (after subject/predicate check):
    if kind == Fact:
        check = statements_contradicting(rtxn, subject, predicate_id)
        if !check.is_empty():
            // Add the new statement to the check set; if the new
            // object differs from all existing, this is a fresh
            // contradiction.
            contradicting_now = check + [new]
            distinct = contradicting_now.iter().map(|s| &s.object).collect::<HashSet>().len()
            if distinct > 1:
                contradiction_audit_record(rtxn, subject, predicate_id, contradicting_now)
                // No error — insert proceeds.
```

`contradiction_audit_record` writes a row to `entity_resolution_audit` (re-used) so operators can find unresolved contradictions via `ADMIN_LIST_PENDING_RESOLUTIONS`.

### 5.1 Event emission

The post-commit event is **still** `STATEMENT_CREATED` (not a special "contradicting" event). Consumers that subscribe to statement events get the create event and can run their own contradiction check if they care.

A future phase may add `STATEMENT_CONTRADICTED` as a discrete event; v1.0 keeps the surface minimal. Tracked in [`./06_open_questions.md`](./06_open_questions.md).

## 6. Time-bounded contradictions

Two Facts with non-overlapping `valid_from..=valid_to` intervals don't contradict (they describe different periods). E.g.:

- F1: `(Priya, role, "engineer")` valid 2025-01-01 → 2025-06-30.
- F2: `(Priya, role, "manager")` valid 2025-07-01 → ongoing.

These are sequential, not contradictory. The detection rule in §1 step 6 handles this — non-overlapping intervals fail the overlap test.

Edge case: implicit `valid_to = None` means "still valid". F1 with `valid_to=None` plus F2 with `valid_from=2025-07-01` do overlap (F1 extends through 2025-07-01+). The substrate treats this as a contradiction unless the operator explicitly closed F1 (which is what `STATEMENT_SUPERSEDE` does — sets `old.valid_to = new.extracted_at`).

## 7. Confidence and contradictions

The substrate **ranks** contradicting Facts by confidence in the default query path. Two Facts with confidences 0.95 and 0.42 — the higher one sorts first.

Consumers can ignore the lower-confidence claim if confidence is below their tolerance threshold. The substrate's job is to surface the disagreement; the consumer's job is to weight.

The confidence is **not** updated by the contradiction. Each Fact's confidence reflects its own evidence (§04). The contradiction itself doesn't mean either is wrong — it means the evidence disagrees.

## 8. Tests (phase 17 acceptance)

- Contradictory pair created: both stored, both queryable, both `is_current=1`.
- `statements_contradicting()` returns both; ordering by confidence.
- Tombstone one: contradiction set returns just the remaining one (no contradiction now).
- Supersede one with a new Fact: chain intact; new Fact contradicts the other if object still differs.
- Preference with same `(subject, predicate)` but different `object`: auto-supersedes the prior, **no** contradiction recorded.
- Event with same `(subject, predicate)` repeated: both stored, **no** contradiction (Events don't contradict).

Test file: `crates/brain-server/tests/knowledge_statement_contradiction.rs` (lands 17.10).

## 9. Open questions

See [`./06_open_questions.md`](./06_open_questions.md). Notably:

- Should the substrate emit a discrete `STATEMENT_CONTRADICTED` event distinct from `STATEMENT_CREATED`?
- Should `STATEMENT_LIST` have an `only_consistent` filter that skips contradictory subjects?
- How do confidence + decay interact for as-of queries against contradictory pairs?
