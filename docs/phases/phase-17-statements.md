# Phase 17: Statement Layer

## Goal

Implement the Statement table, the three statement kinds (Fact, Preference, Event), supersession chains, contradiction handling, and the statement HNSW for semantic retrieval of statements.

## Prerequisites

- Phase 16 complete.

## Reading list

- `19_statements/00_purpose.md`
- `17_knowledge_model/02_three_statement_kinds.md`
- `25_provenance_versioning/00_purpose.md`

## Outputs

- `Statement`, `StatementKind`, `StatementObject`, `Predicate` types in `brain-core`.
- redb tables: `statements`, `statements_by_subject`, `statements_by_predicate`, `statements_by_object_entity`, `statements_by_event_time`, `statements_by_evidence`, `statement_chain`, `evidence_overflow`, `predicates`.
- Statement HNSW per shard.
- Wire opcodes 0x40-0x46.
- SDK helpers for Fact, Preference, Event.

## Sub-tasks

### 17.1 Statement record types and rkyv

**Reads:** `19_statements/00_purpose.md` (schema).
**Writes:** `crates/brain-core/src/statement.rs`.
**Done when:** `Statement`, `StatementObject`, `EvidenceRef` types compile and round-trip.
**Pitfalls:** The tagged union for `StatementObject` is tricky for rkyv. Test all variants.

### 17.2 Predicate interning

**Reads:** `19_statements/00_purpose.md` (predicate vocabulary section).
**Writes:** `crates/brain-metadata/src/predicates.rs`.
**Done when:** `Predicate` defines / lookups by `(namespace, name)`. Builtin predicates auto-registered.
**Pitfalls:** Namespace separator (`:`); validate predicate name characters.

### 17.3 Statement create with index updates

**Reads:** `19_statements/00_purpose.md` (indexes section).
**Writes:** `crates/brain-metadata/src/statement_ops.rs`.
**Done when:** `statement_create` writes to `statements` table and all relevant indexes in one transaction.
**Pitfalls:** Many indexes. Use a helper that takes the statement and dispatches index writes. Test that supersession-chain updates work.

### 17.4 Preference supersession on create

**Reads:** `17_knowledge_model/02_three_statement_kinds.md` (supersession rules).
**Writes:** `crates/brain-core/src/supersession.rs`.
**Done when:** creating a Preference with same (subject, predicate) supersedes the existing current one. Chain root computed correctly.
**Pitfalls:** Compute chain_root: if old has no supersedes, root is old.id; else inherit from old.

### 17.5 Contradicting Facts surface

**Reads:** `17_knowledge_model/02_three_statement_kinds.md` (contradiction handling).
**Writes:** `crates/brain-core/src/contradiction.rs`.
**Done when:** when two current Facts have same (subject, predicate) but different object, both are stored; `list_contradictions` returns them.
**Pitfalls:** Don't auto-resolve. Surface to the caller.

### 17.6 Statement HNSW per shard

**Reads:** `26_knowledge_storage/00_purpose.md` (statement HNSW).
**Writes:** `crates/brain-index/src/statement_hnsw.rs`.
**Done when:** statements get an embedding (predicate + object_text + subject_canonical_name) and are indexed. Search returns ranked StatementIds.
**Pitfalls:** Embedding worker runs async; the statement is queryable by direct ID before the embedding is ready.

### 17.7 Wire opcodes 0x40-0x46

**Reads:** `28_knowledge_wire_protocol/00_purpose.md`.
**Writes:** `crates/brain-protocol/src/knowledge/statement.rs`, `crates/brain-server/src/handlers/knowledge/statement.rs`.
**Done when:** all 7 opcodes work end-to-end via test client.

### 17.8 SDK Fact/Preference/Event builders

**Reads:** `29_knowledge_sdk/00_purpose.md` (statement API).
**Writes:** `crates/brain-sdk-rust/src/knowledge/statement.rs`.
**Done when:** `client.fact()`, `client.preference()`, `client.event()` fluent APIs work.

### 17.9 Confidence aggregation across evidence

**Reads:** `25_provenance_versioning/00_purpose.md` (confidence aggregation).
**Writes:** `crates/brain-core/src/confidence.rs`.
**Done when:** function computing `1 - Π(1 - c_i * decay(age_i))` for a statement; called on create and when evidence changes.
**Pitfalls:** Decay function configurable per kind. Test with empty evidence (should return 0).

### 17.10 Tests

**Writes:** `tests/knowledge_statements.rs`.
**Done when:** create-supersede chain test, contradiction surface test, evidence aggregation test, HNSW round-trip.

## Done-when (phase)

- All three kinds work end-to-end.
- Supersession chains traverse correctly.
- Contradictions surface (and don't auto-resolve).
- Evidence aggregation produces correct confidence.
- Statement HNSW returns plausible neighbors for semantic queries.

## Pitfalls

- Don't activate the supersession sweeper worker yet — Phase 23 wires that in.
- No extraction yet. Statements are created via SDK or wire only.
