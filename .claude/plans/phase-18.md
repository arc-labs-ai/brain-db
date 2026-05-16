# Phase 18 — Relation Layer

Implements the third pillar of the knowledge layer: typed edges
between entities (`Relation`), declared relation types with
cardinality + symmetric flags, supersession (cardinality-driven),
1–3 hop traversal with cycle detection, and the wire / SDK surface.

## Prerequisites

- Phase 17 complete (`phase-17-complete` at `10ad1cf`).
- Branch off `dev` (currently at the post-merge tip).

## Branch

`feature/phase-18-relation-layer` (created off `dev`).

## Scope already-prepared by earlier phases

- `Relation` rkyv row + `RELATIONS_TABLE` declared in
  `crates/brain-metadata/src/tables/knowledge/relation.rs` (15.1).
- `RelationId` / `RelationTypeId` types in `brain-core` (15.1).
- `Cardinality` enum + relation event variants (`RelationCreated`,
  `RelationSuperseded`) in `brain-protocol` (16.7.4).
- §28/07 relation frames spec brought to §03-depth in phase 16
  Sitting B (~7 opcodes, full request/response shapes).

## Spec-first discipline — §20 backfill required first

Per [[spec-first-workflow]]: `§20 relations` is a 1-file stub (~7 KB).
§03-substrate depth is 16 files. §19 backfill in 17.1 was 7 files;
§20 will follow the same shape:

**§20 backfill files (sub-task 18.1):**

```
spec/20_relations/
├── 00_purpose.md                    (live — schema, types, indexes)
├── 01_cardinality.md                (new — supersession mechanics per cardinality)
├── 02_symmetric.md                  (new — canonical from/to ordering + dual-index reads)
├── 03_storage.md                    (new — redb table layout matching 15.1 scaffolding)
├── 04_traversal.md                  (new — BFS/DFS algorithm, depth cap, cycle
│                                       detection, branching factor cap)
├── 05_evidence.md                   (new — evidence vec, FORGET cascade)
├── 06_open_questions.md             (new — graph-engine boundary, deep traversals)
└── 07_references.md                 (new — cross-links to §17, §18, §19, §28/07, §25)
```

Bundled spec edits:

- §16/02 §2.4 — add relation-layer perf rows (`RELATION_CREATE`,
  `_GET`, `_LIST_FROM/_TO`, `_TRAVERSE` depth 1 / 2 / 3).
- §29/00 phase-scope table — flip relation helpers from "phase 18.x"
  to "this phase".

## Sub-tasks

### 18.1 — §20 backfill + bundled spec edits

**Reads:** §20/00 + §28/07 + §17/02 + §25/00 + §19/01 (supersession
precedent).
**Writes:** §20/01–07 (new, ~7 files), §16/02 §2.4 rows, §29/00
update.
**Done when:** §20 mirrors §19's depth; perf targets reflect relation
ops.

### 18.2 — `Relation` value types in brain-core

**Reads:** §20/00 (schema), §20/02 (symmetric ordering).
**Writes:** `crates/brain-core/src/knowledge/relation.rs`.
**Done when:** `Relation` struct + `RelationType` + `Cardinality`
already exists; this sub-task ensures the value-type shape matches
§20/00, with helpers `is_current`, `canonical_from_to` (deterministic
ordering for symmetric).
**Pitfalls:** `Cardinality` enum already lives in `kinds.rs` from
15.1. Just need `Relation` value type — likely mostly there from
prep but verify against §20/00.

### 18.3 — Relation-type registry + interning

**Reads:** §20/00 §"Relation type declaration".
**Writes:** `crates/brain-metadata/src/relation_type_ops.rs` +
extend `tables/knowledge/relation_type.rs` if needed.
**Done when:**
- `relation_type_intern(wtxn, namespace, name, from_type, to_type,
  cardinality, symmetric)` → `RelationTypeId`.
- `relation_type_get(rtxn, id)` / `_lookup_by_qname`.
- Built-ins: `brain:related_to` (any→any, many-to-many, symmetric=false)
  auto-registered at `MetadataDb::open`.
- Identifier validation mirrors predicate naming.

### 18.4 — `relation_ops` module

**Reads:** §20/01 (cardinality), §20/02 (symmetric), §20/03 (storage),
§20/05 (evidence).
**Writes:** `crates/brain-metadata/src/relation_ops.rs`.

Free functions:
- `relation_create(wtxn, &Relation, now)` — validates from/to types
  against relation type's constraints; runs cardinality check
  (many-to-one: auto-supersede prior current relation from same
  `from`; one-to-one: supersede in both directions); canonicalises
  from/to ordering for symmetric; writes primary + 3 secondary
  indexes (`by_from`, `by_to`, `by_type`) + evidence reverse index.
- `relation_get(rtxn, id)`.
- `relation_supersede(wtxn, old_id, &new_relation, now)`.
- `relation_tombstone(wtxn, id, now)`.
- `relation_list_from(rtxn, entity, type_filter, current_only)`.
- `relation_list_to(rtxn, entity, type_filter, current_only)`.
- `relation_history(rtxn, anchor)` — walk chain (mirrors statement_ops).
- `relations_with_evidence(rtxn, memory_id)` — reverse for FORGET
  cascade.

### 18.5 — 1–3 hop traversal

**Reads:** §20/04 (algorithm), §20/00 §"Graph queries".
**Writes:** `crates/brain-metadata/src/relation_traversal.rs`.
**Done when:**
- `traverse(rtxn, start, type_filter, direction, max_depth, max_branching_factor)`
  → `Vec<TraversalPath>` (path = list of (relation_id, entity) pairs).
- Cycle detection via `HashSet<EntityId>` visited set.
- Default depth = 3, cap = 5.
- Default branching cap = 1000 per level.
- Direction: `Outgoing` / `Incoming` / `Both`.

**Pitfalls:** Symmetric relations must be visited in either direction;
the traversal treats canonicalised storage transparently.

### 18.6 — Wire opcodes 0x0150-0x0156

**Reads:** §28/07 (already detailed; ~300 lines).
**Writes:**
- `crates/brain-protocol/src/knowledge/relation_req.rs`.
- `crates/brain-protocol/src/knowledge/relation_resp.rs`.
- Extend `Opcode` enum + `RequestBody` / `ResponseBody`.

Opcodes:
- `RELATION_CREATE` (0x0150) / Resp (0x01D0).
- `RELATION_GET` (0x0151) / Resp.
- `RELATION_SUPERSEDE` (0x0152) / Resp.
- `RELATION_TOMBSTONE` (0x0153) / Resp.
- `RELATION_LIST_FROM` (0x0154) / Resp.
- `RELATION_LIST_TO` (0x0155) / Resp.
- `RELATION_TRAVERSE` (0x0156) / Resp (streaming-ish; single-frame
  snapshot in v1 mirroring entity/statement LIST).

Round-trip tests for each.

### 18.7 — Handlers + event emission

**Reads:** §28/02 §3.3 relation events.
**Writes:** `crates/brain-ops/src/ops/knowledge_relation.rs` +
dispatch wire-up.
**Done when:** 7 handlers wrap `relation_ops` + traversal calls.
Emit `RelationCreated` / `RelationSuperseded` /
`RelationTombstoned` events post-commit.

Phase scope: `RELATION_LIST_*` + `RELATION_TRAVERSE` ship as
single-frame snapshot (streaming + cursor pagination → phase 23).

### 18.8 — SDK relation builders

**Reads:** §29/00 §"Typed relation API".
**Writes:** `crates/brain-sdk-rust/src/knowledge/relation.rs` +
extend `Client` with `relation::<T>()` entry point.

Following the entity SDK pattern (typed `RelationHandle<T>`):
- `client.relation::<Manages>().create().from(a).to(b).send()` —
  uses the `T: BrainRelationType` trait (built-in implementations
  for the seeded `brain:related_to`).
- `client.relations::<Manages>().list_from(a).await`.
- `client.relations().traverse(start).types(&[T]).depth(2).execute()`.

Phase 19 derive macro `#[derive(BrainRelation)]` generalises.

### 18.9 — Integration tests + perf bench + phase exit

**Writes:**
- `crates/brain-server/tests/knowledge_relation_wire.rs` — per-op
  smoke + cardinality error paths.
- `crates/brain-server/tests/knowledge_relations_phase_exit.rs` —
  full lifecycle: create (asymmetric + symmetric) → list_from →
  list_to → cardinality supersede → traverse 1-hop / 2-hop → cycle
  test → tombstone.
- `crates/brain-sdk-rust/tests/knowledge_relation.rs` — SDK builder
  integration.
- `crates/brain-metadata/benches/relation_ops.rs` — criterion bench
  against §16/02 §2.4 perf rows.

**Phase exit checklist:**

- All sub-task tests pass.
- Asymmetric + symmetric relations work end-to-end via wire + SDK.
- Cardinality enforcement (one-to-one / many-to-one / one-to-many /
  many-to-many) verified.
- Traversal terminates on cycles + respects depth + branching cap.
- Update ROADMAP. Tag `phase-18-complete` — user authorises.

## Suggested commit cadence

~10 commits mirroring phase 17:

1. `18.1` — §20 backfill (single commit; doc-only).
2. `18.2` — `Relation` value types + symmetric ordering helpers.
3. `18.3` — relation-type registry + built-in registration.
4. `18.4` — `relation_ops` module.
5. `18.5` — traversal (with cycle + branching cap).
6. `18.6` — wire structs + dispatch.
7. `18.7` — handlers + event emission.
8. `18.8` — SDK builders.
9. `18.9a` — integration tests + lifecycle.
10. `18.9b` — bench + ROADMAP + phase exit + tag.

## Risks

- **Cardinality enforcement is the hardest correctness gate.**
  Auto-supersede on many-to-one means relation_create must look up
  + flip the prior current. Tests must cover both directions for
  one-to-one, both sides for symmetric.
- **Symmetric canonicalisation** chooses `from < to` byte-wise.
  Reads must query both indexes and dedupe. Easy to get wrong; tests
  with symmetric relation where caller passes `to < from` order
  verify the canonicalisation kicks in.
- **Traversal cycle detection** is straightforward but easy to miss
  edge cases (self-loop, symmetric back-edge). Property test with
  random small graphs.
- **`Relation.evidence`** is a flat `Vec<MemoryId>` per spec — not
  the richer `EvidenceRef::Inline / Overflow` structure used by
  statements. Documented in 17.x as the open question; phase 18
  ships the flat shape per spec §20/00 and defers overflow to phase
  22 alongside the statement ADD_EVIDENCE op.
- **Statement / relation re-routing on entity merge** (deferred from
  phase 16) is a phase-18 follow-up if scope allows — otherwise
  punt to phase 23. Document in 18.9 phase-exit notes.

## Out of scope (this phase)

- `#[derive(BrainRelation)]` macro — phase 19.
- Streaming TRAVERSE + cursor pagination — phase 23.
- Deep traversal optimisations (> 3 hops, custom path scoring) —
  phase 23.
- Cross-shard traversal — phase 23.
- Entity-merge re-routing of relations — phase 18 if scope allows,
  else phase 23.
- Relation extraction by phase-22 extractors — phase 22.

## Verification gate (per sub-task)

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-core -p brain-protocol -p brain-sdk-rust
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

## After phase 18

Phase 19 — Schema DSL. Brings derive macros (`BrainEntity`,
`BrainFact`, `BrainRelation`), `SCHEMA_UPLOAD` opcode, and the typed
schema builder that user deployments use to declare their domain
model. Hard prerequisite for the extractor phases (20–22).
