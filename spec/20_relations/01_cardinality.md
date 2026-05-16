# 20.01 Cardinality and Supersession

How a relation type's `cardinality` declaration drives the
auto-supersession behaviour of `RELATION_CREATE`. Mirrors §19/01's
treatment of Preference auto-supersession; relations generalise the
pattern over four cardinality variants.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Relation type declaration"
  + §"Cardinality and supersession" — variants + intent.
- [`./03_storage.md`](./03_storage.md) §3 — index updates per
  supersession.
- [`../19_statements/01_supersession.md`](../19_statements/01_supersession.md)
  — value-side supersession precedent.

## 1. The four variants

```rust
#[repr(u8)]
pub enum Cardinality {
    OneToOne = 0,
    OneToMany = 1,
    ManyToOne = 2,
    ManyToMany = 3,
}
```

Cardinality is declared on the `RelationType`, not on individual
relations. All `Relation` rows of a given type share the cardinality
of their type.

| Variant | Constraint | Example |
|---|---|---|
| `OneToOne` | At most one current relation of this type touching either `from` OR `to`. | `married_to` (symmetric); `holds_seat_X` (asymmetric, X is unique). |
| `OneToMany` | At most one current relation of this type touching the `to` side. Many `from` allowed. | `employed_by` (a Person can have at most one employer per period, but a company can employ many). |
| `ManyToOne` | At most one current relation of this type touching the `from` side. Many `to` allowed. | `reports_to` (each Person reports to at most one Person; one Person can have many reports). |
| `ManyToMany` | No cardinality constraint. | `discussed_with`, `attended`. |

"Touching" means the entity appears as either `from` or `to`,
considering the canonical ordering for symmetric relations (see
[`./02_symmetric.md`](./02_symmetric.md) §3).

## 2. Auto-supersession rules

`relation_create(wtxn, &Relation, now)` runs this check **before**
the insert. The check is read-only first; if it finds a conflicting
current relation, it delegates to `relation_supersede` inside the
same redb txn.

```text
For new relation N with relation_type T, cardinality C:
    matches = []

    if C in {OneToMany, ManyToMany}:
        // No constraint on the from side.
    if C in {OneToOne, ManyToOne}:
        // Constraint: at most one current relation from N.from.
        matches += relation_lookup_current_from(rtxn, N.from, T)

    if C in {OneToMany, OneToOne}:
        // Constraint: at most one current relation to N.to.
        matches += relation_lookup_current_to(rtxn, N.to, T)

    matches = dedupe(matches)

    if matches.is_empty():
        // No prior current — just insert N.
        insert_new_relation(wtxn, N)
        return Ok(N.id)

    if matches.len() == 1:
        // Auto-supersede the single prior current.
        old = matches[0]
        return relation_supersede(wtxn, old.id, N, now)

    // matches.len() > 1: cardinality is somehow already violated on
    // disk. This is a caller / extractor bug; surface to operator
    // via the same `Conflict` path as a manual supersede.
    return Err(StorageInvariantViolated)
```

Symmetric relations canonicalise `from / to` before the lookup (see
§02 §2); the cardinality check therefore considers the canonical
direction only.

## 3. Per-variant write paths

### 3.1 `ManyToMany`

Common case. No lookup; new relation inserts cleanly. Two existing
`discussed_with(A, B)` relations with different `topic` properties
coexist as concurrent current relations.

### 3.2 `ManyToOne`

Common case for "Person → Person" hierarchies (`reports_to`,
`managed_by`). Auto-supersedes the prior `(from, type)` current
relation. Old's `superseded_by` and `valid_to` set per §19/01 §3.2.

### 3.3 `OneToMany`

Symmetric variant of `ManyToOne` (constraint flipped to the `to`
side). Less common; example: an `employed_by(Person, Company)` where
a Person can be employed by at most one Company at a time.

### 3.4 `OneToOne`

Most restrictive. Both directions hold simultaneously. If either
`(from, type)` or `(to, type)` is currently held by an existing
relation, supersede happens. If **both** are held by different
relations (e.g., A married_to B and C married_to D, now asserting
A married_to D), the create errors with `INVALID_ARGUMENT` —
two-sided supersession is intentionally not auto-resolved (it
suggests the schema is mis-modeled or the extractor is confused).

## 4. Cardinality and symmetric relations

When `symmetric = true`, the cardinality applies after
canonicalisation. For `OneToOne + symmetric` (the `married_to`
shape):

- Canonical relation stores `(canonical_from, canonical_to)` with
  `from < to`.
- The lookup "is canonical_from already in a current marriage?"
  queries `RELATIONS_BY_FROM` AND `RELATIONS_BY_TO` at
  `(canonical_from, type, 1)`.
- Same for `canonical_to`.

If either lookup finds a current relation, that's the one to
supersede. The query unions the two indexes per [`./02_symmetric.md`](./02_symmetric.md)
§4.

## 5. Explicit supersession

`RELATION_SUPERSEDE` (opcode `0x0152`) handles the case where the
caller explicitly chains a new relation onto a known prior. Same
chain mechanics as §19/01 §3 (version, supersedes back-pointer,
`valid_to` inheritance). The cardinality check is **not** re-run on
explicit supersede — the caller has named the prior id explicitly,
so the substrate trusts the call.

## 6. Tombstone and cardinality

Tombstoning a relation flips its `is_current` bit to 0. The slot it
was "occupying" for cardinality purposes becomes free. A subsequent
`RELATION_CREATE` of the same `(from, type)` no longer triggers
auto-supersede.

## 7. Open questions

See [`./06_open_questions.md`](./06_open_questions.md). Notably:

- Q1 — Should `OneToOne` two-sided conflict trigger a discrete
  `RELATION_CARDINALITY_CONFLICT` event for monitoring?
- Q2 — Should the cardinality check be skippable via a wire-level
  flag for bulk extractor backfills?

## 8. Tests

Phase 18.4 unit tests cover:

- ManyToMany: two creates → both current.
- ManyToOne: second create auto-supersedes first; chain length 2.
- OneToMany: same but inverted.
- OneToOne: same-side supersede works; two-sided conflict errors.
- Symmetric ManyToMany: canonicalisation kicks in; both index sides
  see the relation.
- Symmetric OneToOne: marriage scenario.
- Tombstone frees the slot.
- Retombstone is a no-op.
