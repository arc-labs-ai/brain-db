# 20.02 Symmetric Relations

Storage + read semantics for relations whose `RelationType` declares
`symmetric = true`. Symmetric relations express edges where the
direction doesn't carry meaning (`discussed_with`, `co_authored`,
`married_to`).

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Symmetric relations".
- [`./03_storage.md`](./03_storage.md) §1 — index layout symmetric
  relations are projected through.
- [`./01_cardinality.md`](./01_cardinality.md) §4 — cardinality
  interaction with canonicalisation.

## 1. The problem

Without symmetry, "A discussed_with B" and "B discussed_with A"
would store two relations for the same conceptual edge. That's:

- Wasted storage.
- A consistency hazard (one tombstoned, the other not).
- Ambiguous for cardinality (`OneToOne` would gate on both rows).

Symmetric relations resolve this by storing **once** in canonical
form, and indexing such that reads work regardless of which side
the caller queries.

## 2. Canonical form

The canonical direction is `from < to` byte-wise on `EntityId`.

```text
canonical_from = min(caller_from, caller_to)
canonical_to   = max(caller_from, caller_to)
```

`relation_create` for a symmetric relation:

1. Reads `relation_type.is_symmetric`.
2. If symmetric and `caller_from > caller_to`: swap before insert.
3. Persists `(canonical_from, canonical_to)` in the primary row +
   both directional indexes.

The original caller-supplied direction is lost. Wire responses
report the canonical direction (clients aware of symmetry don't
care about which side was "from").

## 3. Indexing

The relation appears in **both** directional indexes:

```text
RELATIONS_BY_FROM_TABLE.insert(
    (canonical_from_bytes, relation_type_id, is_current),
    relation_id_bytes,
)
RELATIONS_BY_TO_TABLE.insert(
    (canonical_to_bytes, relation_type_id, is_current),
    relation_id_bytes,
)
```

For asymmetric relations, only `RELATIONS_BY_FROM` carries the
`from` side and only `RELATIONS_BY_TO` carries the `to` side. For
symmetric relations, **both** sides participate in both indexes (the
same relation_id is indexed under both endpoints).

This means `relation_list_from(entity, type, current_only)` returns
all symmetric relations involving `entity` even when `entity` is
the canonical `to`. Same for `list_to`. The dual-index population
is what makes "find all `discussed_with` involving Priya" a single
lookup regardless of which side Priya was on.

## 4. Reading both sides

For a query that explicitly wants "all symmetric relations of type
T involving entity X", the planner unions:

```text
results = []
results += relation_list_from(rtxn, X, T, current_only)
// If T is symmetric, results already contains both directions;
// no second index call needed. Per §3 the relation is in BOTH
// directional indexes for its canonical endpoints, and X matches
// either canonical_from or canonical_to.
//
// If T is asymmetric, list_from only returns relations where
// X == from. Callers wanting both sides separately call
// list_to as well.

if !relation_type.is_symmetric:
    results += relation_list_to(rtxn, X, T, current_only)
results = dedupe(results)  // for symmetric, dedupe is required
                            // because list_from already returned
                            // the relation; list_to would too.
```

Phase 18.4 implementation: `relation_list_from / _to` handle the
union + dedup internally when the `RelationType` is loaded and
`is_symmetric` is true, so callers don't need to know.

## 5. Cardinality interaction

§01 §4 covers this — the cardinality lookups operate on the
canonical direction. A `OneToOne + symmetric` relation type means
the canonical_from and canonical_to BOTH can only have one current
relation of this type.

## 6. Asymmetry verifier

A worker-time invariant (phase 21+ sweep) verifies:

```text
For each symmetric relation R in RELATIONS_TABLE:
    assert R.from_entity < R.to_entity  // canonical order
    assert R appears in BOTH RELATIONS_BY_FROM and RELATIONS_BY_TO
        at (R.from, R.type, R.is_current) and
        (R.to, R.type, R.is_current).
```

Violations indicate a write-path bug. v1.0 phase 18 ships the write
path; phase 21+ adds the sweeper.

## 7. Wire-layer semantics

`RelationView` (§28/07 §2.4) reports the **canonical** direction
in `from / to` plus a `flags & 1 == 1` bit indicating symmetry. SDK
projections handle this transparently — `RelationHandle::other_side(known_endpoint)`
returns the opposite end without the caller caring about
canonicalisation.

## 8. Cross-shard considerations

Symmetric relations are sharded by `canonical_from`. The reverse
index entry on `RELATIONS_BY_TO` lives on the canonical_to's shard
when from / to are on different shards — cross-shard write, same
mechanic as §28/06 §11 for statements. Phase 18 ships same-shard
only; cross-shard reverse-index population lands in phase 23.

## 9. Open questions

See [`./06_open_questions.md`](./06_open_questions.md). Notably:

- Q3 — Should symmetric relations with the same canonical
  `(from, to, type)` be deduplicated on create? Currently no — two
  `discussed_with(A, B)` rows with different `topic` properties
  coexist. Matches ManyToMany semantics.

## 10. Tests

Phase 18.4 verifies:

- Symmetric create with `caller_from > caller_to` canonicalises
  internally; row stored with `from < to`.
- Symmetric ManyToMany: query from either side returns the
  relation; counts dedupe.
- Symmetric OneToOne: canonical-side cardinality enforced
  consistently.
- Asymmetric relations stored verbatim; per-direction index queries
  return only matching side.
