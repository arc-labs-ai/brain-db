# 23.02 LexicalRetriever

Normative spec for the lexical retriever (phase 22). Sits beside
`00_purpose.md` (which gives the three-retriever overview) and
`01_rrf_fusion.md` (which gives the cross-retriever fusion rule).

The LexicalRetriever is one of the three retrievers in §23/00. It
operates over the two per-shard tantivy indexes laid out in
§26/01 and is fed by the workers in §27/02.

## 1. Surface

Trait shape (binding for the phase 22.5 implementation):

```rust
pub trait LexicalRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError>;
}

pub struct LexicalQuery {
    pub terms: Vec<String>,
    pub phrase_clauses: Vec<Vec<String>>,
    pub filters: LexicalFilters,
}

pub enum LexicalScope {
    MemoryText,
    StatementText,
}

pub struct LexicalRetrieverConfig {
    pub top_k: usize,            // default 64
    pub bm25_k1: f32,            // default 1.2
    pub bm25_b: f32,             // default 0.75
    pub min_score: Option<f32>,  // None = no cutoff
    pub timeout_ms: u32,         // default 50
}

pub struct RankedItem {
    pub id: ItemId,              // MemoryId | StatementId per scope
    pub rank: u32,                // 1-based rank within this retriever
    pub score: f32,               // BM25 score; not cross-retriever comparable
    pub snippet: Option<String>,  // optional highlighted excerpt
}
```

`retrieve()` is **read-only**. No side effects.

## 2. BM25 parameters

Defaults: `k1 = 1.2`, `b = 0.75` (matching tantivy defaults and the
overview in §23/00 §"LexicalRetriever").

Operators may override per call via `LexicalRetrieverConfig`.

Score scale is internal to lexical. Cross-retriever ordering uses
**rank**, not score (§23/01 RRF fusion). A consumer that compares
BM25 scores to cosine similarities is wrong.

## 3. Tokenizer pipeline

Binding for §26/01 §2 (schema) and phase 22.2 (tokenizer impl).

Steps, in order:

1. **Unicode normalization** — NFC. Non-NFC inputs normalize
   before any other step.
2. **Lowercase** — full Unicode-aware lowercase.
3. **Sublanguage preservation** — before generic splitting, two
   regex passes emit standalone tokens (not stemmed, not split
   further by the next step):
   - URL pattern: `\bhttps?://\S+`.
   - Code/ID pattern: `[A-Z][A-Z0-9]+-\d+` (ticket IDs like
     `ACME-1247`), plus dot/underscore-joined identifiers
     `[a-z_][a-zA-Z0-9_.]+`. These survive verbatim so exact-ID
     queries work.
4. **Generic tokenization** — split on whitespace and Unicode
   punctuation.
5. **Stop-word removal** — **NO** in v1. BM25's `idf` term
   demotes high-frequency words naturally; aggressive stop-word
   removal would break exact-ID queries on tokens like `to` or
   `of` that appear inside larger preserved identifiers.
6. **Porter / English stemming** — applied only to the
   generic-tokenization output, NOT to the sublanguage tokens
   from step 3.

The pipeline is the same for both `LexicalScope::MemoryText` and
`LexicalScope::StatementText`; per-field overrides are explicitly
out of scope for v1.

## 4. Scope dispatch

| Scope | Index file (§26/01) | `RankedItem.id` type |
|---|---|---|
| `MemoryText` | `memory_text.tantivy/` | `MemoryId` |
| `StatementText` | `statements.tantivy/` | `StatementId` |

A single `retrieve()` call queries exactly one index. The query
router (§24 hybrid query, phase 23) issues two retriever calls
when both scopes apply.

Cross-shard ranking: out of scope for v1 — retrieval is
**per-shard**. A multi-shard hybrid query fan-outs at the router
(§24/00 dispatch), each shard runs its own LexicalRetriever, and
the router merges by rank. This matches the substrate's
single-writer-per-shard discipline.

## 5. Filters

Binding for §26/01 §2 field set. Filters are AND-ed against the
BM25 query.

`LexicalFilters` (memory scope):
- `agent_id: Option<AgentId>` — exact match.
- `kind: Option<MemoryKind>` — exact match.
- `created_at: Option<RangeInclusive<u64>>` — unix-ms range.

`LexicalFilters` (statement scope):
- `kind: Option<StatementKind>` — exact match.
- `predicate_id: Option<PredicateId>` — exact match on
  `predicate_name` field.
- `confidence_bucket: Option<RangeInclusive<u8>>` — buckets 0–9
  representing 0.1 increments per §26/01 §2.
- `extracted_at: Option<RangeInclusive<u64>>` — unix-ms range.

A filter on a field absent from the scope's schema is a
`LexicalError::QueryParseFailed`.

## 6. Result shape and idempotency

- `Vec<RankedItem>` ordered by descending BM25 score.
- `rank` is 1-based and dense (1, 2, 3, …) within the returned
  slice.
- `top_k` is applied **after** scoring and filtering; if fewer
  matches exist, the slice is shorter than `top_k`.
- `score` is the raw BM25 score returned by tantivy. Treat as
  opaque for cross-retriever fusion.
- `snippet` is optional. v1 may always return `None`; phase 22.5
  decides whether to populate. If populated, format is plain
  text with the matched terms wrapped in `[[ ... ]]` markers
  (HTML escaping is the caller's problem).

**Idempotency:** two calls with the same `(query, scope, config)`
return identical `Vec<RankedItem>` between commits. The text
indexer workers (§27/02) commit on group cadence; between
commits the retriever's view is frozen.

## 7. Errors

`LexicalError` taxonomy (binding for §03/10 error code map):

| Variant | Trigger | Visible to clients |
|---|---|---|
| `IndexUnavailable` | Index is mid-rebuild (§26/01 §5) or corrupt at open. | Yes — clients retry. |
| `QueryParseFailed` | Empty query, invalid filter, or filter field not in scope schema. | Yes — client bug. |
| `Timeout` | Query exceeded `config.timeout_ms`. | Yes — degraded response. |

An **empty result** (`Ok(vec![])`) is NOT an error. The
retriever does not interpret zero matches.

## 8. Performance

Pinned in §16/02 §2.9 (phase 22 perf targets):

| Operation | p50 | p99 |
|---|---|---|
| Memory @ 100K, single-term | 10 ms | 50 ms |
| Memory @ 100K, multi-term + filter | 15 ms | 70 ms |
| Statement @ 1M, single-term | 10 ms | 50 ms |
| Statement @ 1M, multi-term + filter | 15 ms | 70 ms |

These are the phase-22 acceptance gates. Sub-task 22.8 validates.

## 9. Boundaries

- LexicalRetriever does **not** embed text (that's `brain-embed`,
  phase 5).
- LexicalRetriever does **not** decide which scope to use; that's
  the query router (§24, phase 23).
- LexicalRetriever does **not** write to tantivy; that's the
  workers in §27/02.
- Snippet generation reuses tantivy's
  `SnippetGenerator::from_query`; phase 22.5 owns the call.
