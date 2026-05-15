# Phase 17 — Statement Layer

Implements the second pillar of the knowledge layer: typed claims about entities (Fact / Preference / Event), with supersession chains, contradiction surfacing, evidence-driven confidence aggregation, and a per-shard statement HNSW for semantic retrieval.

## Prerequisites

- Phase 16 complete (`phase-16-complete` at `546a34c`). Branch off `dev` (currently at the post-merge tip).
- Substrate phases 0-14 (vector substrate scaffolding) — already in place.

## Branch

`feature/phase-17-statement-layer` (created off `dev`).

## Scope already-prepared by phase 16

Phase 16's 15.x prep landed scaffolding that 17 builds on:

- redb tables (8 of them) declared in `crates/brain-metadata/src/tables/knowledge/statement.rs` — `STATEMENTS_TABLE`, `STATEMENTS_BY_SUBJECT_TABLE`, `_BY_PREDICATE_TABLE`, `_BY_OBJECT_ENTITY_TABLE`, `_BY_EVENT_TIME_TABLE`, `_BY_EVIDENCE_TABLE`, `STATEMENT_CHAIN_TABLE`, `EVIDENCE_OVERFLOW_TABLE`.
- `StatementMetadata` rkyv row (live).
- Wire shape spec §28/06 detailed enough to implement against (334 lines, all 7 statement opcodes specified).
- `StatementKind` enum, `PredicateId` u32 type, `StatementId` UUIDv7 type in `brain-core::knowledge`.
- Knowledge SUBSCRIBE event variants (`StatementCreated`, `StatementSuperseded`, `StatementTombstoned`) in `brain-protocol::knowledge::events` from phase 16.7.4.
- Knowledge wire error code mapping (Strategy B) extended in 16.7.4 for `STATEMENT_NOT_FOUND`, `STATEMENT_OBJECT_TYPE_MISMATCH`, `STATEMENT_CONTRADICTS_EXISTING`.

## Spec-first discipline — §19 backfill required first

Per [[spec-first-workflow]] memory: `§19 statements` is a 1-file stub (246 lines). §03 depth is 16 files. Before any code lands, §19 must reach implementable depth.

**§19 backfill files (sub-task 17.1):**

```
spec/19_statements/
├── 00_purpose.md                      (live — schema, kinds, indexes)
├── 01_supersession.md                 (new — version / chain_root / valid_to mechanics)
├── 02_contradiction.md                (new — Fact contradiction surface; never auto-resolve)
├── 03_storage.md                      (new — redb table layout, written against the
│                                          tables already declared in code)
├── 04_confidence.md                   (new — 1-Π(1-c_i·decay) formula, per-kind decay,
│                                          recomputation triggers)
├── 05_evidence.md                     (new — inline vs overflow, FORGET cascade,
│                                          evidence integrity)
├── 06_open_questions.md               (new — known gaps deferred to phases 18+)
└── 07_references.md                   (new — cross-links to §17, §28/06, §25, §26)
```

Also small bundled spec edits:

- §16/02 §2.2 — add statement-layer perf rows (STATEMENT_CREATE, GET, LIST, SUPERSEDE, etc.).
- §29/00 phase-scope table — flip statement helpers from "phase 17.x" status to "this phase".

## Sub-tasks

### 17.1 — §19 backfill + bundled spec edits

**Reads:** §19/00 + §28/06 + §17/02 + §25/00 (existing).
**Writes:** §19/01-07 (new, ~7 files), §16/02 §2.2 rows, §29/00 phase-scope update.
**Done when:** §19 mirrors §18's depth; perf targets reflect statement ops.

### 17.2 — `Statement` value type in brain-core

**Reads:** §19/00, §19/05 (new from 17.1).
**Writes:** `crates/brain-core/src/knowledge/statement.rs`.
**Done when:** `Statement`, `StatementObject` (tagged union: Entity / Value / Memory / Statement), `EvidenceRef` (Inline / Overflow), `SubjectRef` (Entity / Pending), `TombstoneReason` enum compile; non-rkyv value types (`brain-core` doesn't carry rkyv on value types — it's the wire-side that derives Archive).
**Pitfalls:** tagged-union variants need stable representations for the wire types (which live in brain-protocol::knowledge::statement_req — landing in 17.6).

### 17.3 — Predicate registry + interning

**Reads:** §19/00 §"Predicate vocabulary".
**Writes:** `crates/brain-metadata/src/predicate_ops.rs` + extend `tables/knowledge/predicate.rs` if needed.
**Done when:**
- `predicate_intern(wtxn, namespace, name, kind_constraint, object_type_constraint)` → returns existing `PredicateId` or allocates a new one.
- `predicate_lookup(rtxn, namespace, name)` → `Option<Predicate>`.
- Built-in predicates auto-registered at `MetadataDb::open` time: `brain:is_a`, `brain:has_name`, `brain:mentions`, `brain:related_to`.
- Reject `"namespace:name"` with invalid chars (alphanumeric + underscore + `:`).

### 17.4 — `statement_ops` module

**Reads:** §19/00, §19/01 (supersession), §19/02 (contradiction).
**Writes:** `crates/brain-metadata/src/statement_ops.rs`.

Free functions over `WriteTransaction` mirroring `entity_ops` precedent:

- `statement_create(wtxn, &Statement, now) -> Result<StatementId, StatementOpError>`
  - Validates against predicate definition (kind / object-type).
  - Validates subject EntityId exists (`brain-metadata::entity_ops::entity_get`).
  - For `Preference`: auto-supersedes prior current statement with same `(subject, predicate)`.
  - Writes to `STATEMENTS_TABLE` + all 6 secondary indexes + `STATEMENT_CHAIN_TABLE` + `STATEMENTS_BY_EVIDENCE_TABLE` for each evidence MemoryId.
  - All in one redb txn.
- `statement_get(rtxn, id) -> Result<Option<Statement>, StatementOpError>`.
- `statement_supersede(wtxn, old_id, new_statement, now) -> Result<StatementId, StatementOpError>`:
  - Atomic two-step inside one txn — create new, link `old.superseded_by = new`, `new.supersedes = old`, compute `chain_root` per §19/01.
  - Set old's `valid_to = new.extracted_at`.
- `statement_tombstone(wtxn, id, reason, now) -> Result<u64, StatementOpError>` (soft).
- `statement_retract(wtxn, id, reason, now) -> Result<RetractRecord, StatementOpError>` (hard — sets tombstone + zero-out timer).
- `statement_history(rtxn, anchor) -> Result<Vec<Statement>, StatementOpError>` — walks `STATEMENT_CHAIN_TABLE` by chain_root.
- `statement_list(rtxn, filter) -> Result<Vec<Statement>, StatementOpError>` — uses appropriate compound index per filter shape.
- `statements_contradicting(rtxn, subject, predicate) -> Result<Vec<Statement>, StatementOpError>` — surfaces active Facts with same `(subject, predicate)` but different `object`.

### 17.5 — Statement HNSW per shard

**Reads:** §26/00 (storage catalog), `spec/06_ann_index/` for substrate HNSW conventions; entity HNSW (`crates/brain-index/src/entity_hnsw.rs`) as a pattern reference.
**Writes:** `crates/brain-index/src/statement_hnsw.rs`.
**Done when:** insert / search / tombstone API matches the entity HNSW; statements get a 384-dim embedding (computed from `predicate + object_text + subject_canonical_name` — done by the embedding worker in phase 21; the HNSW itself just stores precomputed vectors here).
**Phase scope:** the HNSW is **declared and write-able**; the embedding worker that populates it lives in phase 21. 17.5 wires the table without a populator.

### 17.6 — Wire opcodes 0x0140-0x0146

**Reads:** §28/06 (already detailed; 334 lines).
**Writes:**
- `crates/brain-protocol/src/knowledge/statement_req.rs` (request structs).
- `crates/brain-protocol/src/knowledge/statement_resp.rs` (response structs incl. `StatementView`).
- Extend `Opcode` enum with `StatementCreateReq=0x0140` etc.
- Extend `RequestBody` / `ResponseBody` with 7 new variants.

Opcodes:
- `STATEMENT_CREATE` (0x0140)
- `STATEMENT_GET` (0x0141)
- `STATEMENT_SUPERSEDE` (0x0142)
- `STATEMENT_TOMBSTONE` (0x0143)
- `STATEMENT_RETRACT` (0x0144)
- `STATEMENT_HISTORY` (0x0145)
- `STATEMENT_LIST` (0x0146)

Round-trip tests for each request and response.

### 17.7 — Handlers + event emission

**Reads:** §28/02 (subscribe events §3.2 statement variants).
**Writes:** `crates/brain-ops/src/ops/knowledge_statement.rs` + dispatch wire-up.
**Done when:** 7 handler functions wrap `statement_ops` calls inside the dispatch, emit `StatementCreated` / `StatementSuperseded` / `StatementTombstoned` events.

Phase scope: `STATEMENT_LIST` ships as single-frame snapshot (same convention as `ENTITY_LIST` — true streaming + cursor in phase 23).

### 17.8 — SDK Fact / Preference / Event builders

**Reads:** §29/00 (statement API section).
**Writes:** `crates/brain-sdk-rust/src/knowledge/statement.rs` + extend `Client` with `fact()`, `preference()`, `event()`, `statements()` entry points.
**Done when:**
- `client.fact().subject(id).predicate("role").object_value("Eng").evidence(...).confidence(0.9).send()`
- `client.preference()...send()`
- `client.event().event_at(t)...send()`
- `client.statements().where_subject(id).of_kind(Fact).current_only().list()`
- `client.statements().history(chain_root).list()`
- `client.statement().tombstone(id, reason)` / `.retract(...)`

Per phase 16.8 precedent: hand-written for v1; derive macro `#[derive(BrainFact)]` is phase 19.

### 17.9 — Confidence aggregation

**Reads:** §19/04 (new from 17.1).
**Writes:** `crates/brain-core/src/knowledge/confidence.rs`.
**Done when:**
- `aggregate_confidence(evidence: &[EvidenceEntry], now: u64, decay_fn) -> f32` computes `1 - Π(1 - c_i · decay(age_i))`.
- Per-kind decay function selectable (`StatementKind::Fact` = slow decay; `Preference` = faster; `Event` = none — event_at is a moment, not a window).
- Empty evidence → 0.0.
- Re-computed by `statement_create` and `statement_supersede`.

### 17.10 — Integration tests + perf bench + phase exit

**Writes:**
- `crates/brain-server/tests/knowledge_statements_phase_exit.rs` — full lifecycle: create Fact / Preference / Event → supersede Preference → list current vs history → contradict Fact (two with same subject+predicate, different object) → tombstone → retract.
- `crates/brain-server/tests/knowledge_statement_wire.rs` — per-op smoke (mirror `knowledge_entity_wire.rs`).
- `crates/brain-sdk-rust/tests/knowledge_statement.rs` — SDK builder integration tests (mock-server).
- `crates/brain-metadata/benches/statement_ops.rs` — criterion bench against §16/02 §2.3 (new perf rows for STATEMENT_CREATE / GET / LIST / SUPERSEDE).

**Phase exit checklist:**

- All sub-task tests pass.
- All three statement kinds work end-to-end via wire + SDK.
- Supersession chains traverse correctly.
- Contradictions surface (no auto-resolve).
- Evidence aggregation produces correct confidence.
- Statement HNSW writes / reads stay coherent (workers populate it in phase 21).
- Update ROADMAP. Tag `phase-17-complete` — user authorises tag op.

## Suggested commit cadence

Mirroring phase 16's 16-commit cadence:

1. `17.1` — §19 backfill (single commit; large doc-only).
2. `17.2` — `Statement` value types.
3. `17.3` — Predicate registry + built-in registration.
4. `17.4` — `statement_ops` module.
5. `17.5` — Statement HNSW (declared, populator deferred to phase 21).
6. `17.6` — Wire structs + dispatch (mirror 16.6c structure).
7. `17.7` — Handlers + event emission.
8. `17.8` — SDK builders.
9. `17.9` — Confidence aggregation.
10. `17.10a` — Integration tests + lifecycle test.
11. `17.10b` — Bench + ROADMAP + Cargo.lock + phase exit.

~11 commits. Each compiles + tests independently. No `--no-verify`, no Co-Authored-By Claude trailer, plan-first per sub-task.

## Risks

- **Contradiction surface vs auto-resolve** is a spec call (§19/02 says "never auto-resolve, surface to caller"). Easy to accidentally pick a winner. 17.4's tests must verify both contradictory Facts coexist.
- **Supersession chain_root computation** has an inherit-from-old subtlety. §19/01 spells it out; the test must cover both "first supersession" (root = old.id) and "Nth supersession" (root inherited).
- **Statement HNSW without populator.** Phase 17 writes to the HNSW only on explicit `STATEMENT_CREATE` callers passing a pre-computed vector. Production population is phase 21's embedding worker. Test gates only cover write/read on the HNSW; semantic correctness comes phase 21.
- **Predicate-name validation** has spec gaps (§19/00 doesn't specify allowed chars). 17.3's spec edit fills this in along with the §19 backfill.
- **EVIDENCE_OVERFLOW** for very-long evidence lists exists as a table; phase 17 ships the write path. Tests cover the inline → overflow tier transition.

## Out of scope (this phase)

- Extractors (pattern / classifier / LLM) — phases 20-21.
- Statement HNSW embedding worker — phase 21.
- Hybrid query routing for statements — phase 23.
- `#[derive(BrainFact)]` macro — phase 19.
- Cursor pagination on `STATEMENT_LIST` — phase 23 (same deferral as `ENTITY_LIST`).
- Statement supersession sweeper worker — phase 23.
- Cross-shard fan-out for `STATEMENT_LIST` without a subject filter — phase 23.

## Spec-first discipline check

- §19 expanded in 17.1 (before any code).
- §16/02 perf targets added in 17.1.
- §29/00 phase-scope updated in 17.1.
- All per-sub-task plans land in `.claude/plans/phase-17-task-NN.md` before that sub-task's first commit.

## Verification gate (per sub-task)

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- `cargo test -p brain-core -p brain-protocol -p brain-sdk-rust` host-runnable subset clean.
- Clippy `-D warnings` clean.
- No `unwrap()` outside tests; `expect("invariant: …")` where unreachable.

## After 17

Phase 18 — relations layer. Builds on the same entity+statement scaffolding with typed edges and traversals. Similar plan structure.
