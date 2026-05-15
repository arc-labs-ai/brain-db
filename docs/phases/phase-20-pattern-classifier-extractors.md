# Phase 20: Pattern + Classifier Extractors

## Goal

Implement the extractor framework, run pattern extractors synchronously on ENCODE, run classifier extractors near-foreground, write extraction audit logs, ship one built-in pattern extractor (`brain.entity_mentions`) and one built-in classifier (NER).

## Prerequisites

- Phase 19 complete.

## Reading list

- `22_extractors/00_purpose.md`
- `27_knowledge_workers/00_purpose.md` (pattern + classifier sections)
- `25_provenance_versioning/00_purpose.md` (audit log)

## Outputs

- Extractor trait and registry.
- Pattern extractor implementation (regex-based).
- Classifier extractor implementation (call out to a model).
- Built-in `brain.entity_mentions` extractor.
- Built-in `brain.basic_ner` classifier (small bundled model).
- Audit log writes for every extraction.
- ENCODE handler invokes extractors synchronously for pattern + classifier kinds.

## Sub-tasks

### 20.1 Extractor trait and registry

**Reads:** `22_extractors/00_purpose.md`.
**Writes:** `crates/brain-core/src/extractor/mod.rs`.
**Done when:** `Extractor` trait, `ExtractorRegistry` (maps ExtractorId to instance), `ExtractedItem` output type.
**Pitfalls:** Trait must be object-safe (dyn Extractor). Outputs are an enum: Entity, Statement, Relation.

### 20.2 Pattern extractor

**Reads:** `22_extractors/00_purpose.md` (pattern section).
**Writes:** `crates/brain-extractors/src/pattern.rs`.
**Done when:** loads patterns from declared extractor, applies regex to memory text, resolves entities or produces statements per the target.
**Pitfalls:** Use `regex` crate; precompile patterns at schema load. Cap pattern compile time and runtime per memory.

### 20.3 Classifier extractor harness

**Reads:** `22_extractors/00_purpose.md` (classifier section).
**Writes:** `crates/brain-extractors/src/classifier.rs`.
**Done when:** loads a pinned model, runs inference per memory, produces extracted items. Initially CPU-only (no GPU dependency).
**Pitfalls:** Pin model framework version. Use ONNX runtime or pure-Rust inference (e.g., `candle`).

### 20.4 Built-in entity mentions pattern

**Reads:** `22_extractors/00_purpose.md` (built-ins).
**Writes:** `crates/brain-extractors/src/builtin/entity_mentions.rs`.
**Done when:** ships with patterns for common person names (English, capital initials), email addresses, ticket IDs (`[A-Z]+-\d+`).
**Pitfalls:** Patterns are heuristic; precision/recall trade-offs documented.

### 20.5 Built-in basic NER classifier

**Reads:** `22_extractors/00_purpose.md` (built-ins).
**Writes:** `crates/brain-extractors/src/builtin/basic_ner.rs`.
**Done when:** small bundled CONLL-trained NER model classifies person, org, place mentions.
**Pitfalls:** Model size matters; aim for ~50 MB or less. Document accuracy.

### 20.6 Extraction audit log

**Reads:** `25_provenance_versioning/00_purpose.md` (audit).
**Writes:** `crates/brain-metadata/src/audit_ops.rs`.
**Done when:** every extraction writes an audit entry; admin can query audits by memory, extractor, time.

### 20.7 ENCODE handler integration

**Reads:** `22_extractors/00_purpose.md`; `spec/09_cognitive_operations/`.
**Writes:** `crates/brain-server/src/handlers/substrate/encode.rs` (extended).
**Done when:** on ENCODE, after memory write succeeds: for each active pattern + classifier extractor whose trigger matches, run synchronously, write outputs.
**Pitfalls:** Don't extend ENCODE's contract with extraction failures — if an extractor fails, log it and continue. Memory write succeeds independently.

### 20.8 Tests

**Writes:** `tests/knowledge_extractors_pattern_classifier.rs`.
**Done when:** end-to-end: schema with pattern+classifier extractor → ENCODE memory → entity + statement created → query returns them. Audit log shows the extraction.

## Done-when (phase)

- Pattern extractors run on ENCODE without breaking ENCODE latency budget significantly (P99 ≤ 20 ms).
- Classifier extractor runs and produces entities.
- Built-ins ship and work on a sample dataset.
- Audit logs queryable.

## Pitfalls

- Don't enable LLM extractors here (phase 21).
- Don't trigger backfill (phase 24).
- Classifier model bundling: consider distribution size; users may prefer to download separately.
