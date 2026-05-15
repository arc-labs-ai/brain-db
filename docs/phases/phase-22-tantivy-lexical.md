# Phase 22: Tantivy / Lexical Retrieval

## Goal

Integrate tantivy for BM25 over memory text and statement text. Implement the LexicalRetriever. Indexes are maintained by workers on writes.

## Prerequisites

- Phase 15 (storage) and 17 (statements) complete.

## Reading list

- `23_retrievers/00_purpose.md` (LexicalRetriever section)
- `26_knowledge_storage/00_purpose.md` (tantivy section)
- `27_knowledge_workers/00_purpose.md` (indexer workers)

## Outputs

- tantivy integration in `brain-index`.
- Memory text indexer worker.
- Statement text indexer worker.
- LexicalRetriever implementation.
- Wire opcodes integrated into RECALL/QUERY paths.

## Sub-tasks

### 22.1 tantivy dependencies and shard initialization

**Reads:** `26_knowledge_storage/00_purpose.md` (tantivy directory layout).
**Writes:** `crates/brain-index/Cargo.toml`, `crates/brain-index/src/tantivy_init.rs`.
**Done when:** tantivy 0.21+ added; per-shard tantivy directories created with correct schema.

### 22.2 Tokenizer setup

**Reads:** `26_knowledge_storage/00_purpose.md` (tokenizer).
**Writes:** `crates/brain-index/src/tokenizer.rs`.
**Done when:** custom tokenizer (lowercase, English stemming, URL/code-identifier preservation) installed and tested.

### 22.3 Memory text indexer worker

**Reads:** `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-workers/src/memory_text_indexer.rs`.
**Done when:** on ENCODE, worker adds the memory to memory_text.tantivy; on FORGET, removes; periodic commit + segment merge.

### 22.4 Statement text indexer worker

**Reads:** `27_knowledge_workers/00_purpose.md`.
**Writes:** `crates/brain-workers/src/statement_text_indexer.rs`.
**Done when:** on statement create/tombstone/supersede, index updated. Statement text repr: `subject_canonical_name + predicate + object_text`.

### 22.5 LexicalRetriever implementation

**Reads:** `23_retrievers/00_purpose.md` (LexicalRetriever).
**Writes:** `crates/brain-core/src/retriever/lexical.rs`.
**Done when:** `LexicalRetriever::retrieve(query, scope, config) -> Vec<RankedItem>`; returns BM25-ranked candidates from memory or statement index per scope.

### 22.6 Index rebuild capability

**Reads:** `26_knowledge_storage/00_purpose.md` (rebuild section).
**Writes:** `crates/brain-workers/src/index_rebuilder.rs`.
**Done when:** admin can trigger full rebuild of tantivy from authoritative redb tables; rebuild runs without downtime (writes new index next to old, atomic swap on completion).

### 22.7 Recovery on startup

**Reads:** `26_knowledge_storage/00_purpose.md` (WAL replay).
**Writes:** in shard startup logic.
**Done when:** on startup, tantivy index opened; if index is corrupt or missing, fall back to rebuild.

### 22.8 Tests

**Writes:** `tests/knowledge_lexical.rs`.
**Done when:** ENCODE memories → query by exact term → returns matching. Same for statements. Performance test: 100K memories indexed, P50 query ≤ 10 ms.

## Done-when (phase)

- tantivy indexes built on ENCODE and statement create.
- LexicalRetriever returns BM25-ranked items.
- Restart recovery works.
- Index rebuild works.
- Performance budgets met.

## Pitfalls

- tantivy's commit semantics: commit too frequently is slow; commit too rarely loses recent docs on crash. Match the substrate's WAL+checkpoint discipline (section 05): commit every N writes or T seconds, recover from WAL if needed.
- Index size grows; segment merge is expensive. Schedule merges during low-traffic windows.
- Test with non-English characters; the tokenizer must not crash.
