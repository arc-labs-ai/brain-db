# 13.02 LexicalRetriever

Normative spec for the lexical retriever. Sits beside
`00_purpose.md` (which gives the three-retriever overview) and
`01_rrf_fusion.md` (which gives the cross-retriever fusion rule).

The LexicalRetriever is one of the three retrievers in §13/00. It
operates over the two per-shard tantivy indexes laid out in
§10/06 (tantivy layout) and is fed by the workers in §15.

## 1. Surface

Trait shape:

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
    pub timeout_ms: u32,         // config default 50; the plan sets 1 s (§8)
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
overview in §13/00 §"LexicalRetriever").

Operators may override per call via `LexicalRetrieverConfig`.

Score scale is internal to lexical. Cross-retriever ordering uses
**rank**, not score (§13/01 RRF fusion). A consumer that compares
BM25 scores to cosine similarities is wrong.

## 3. Tokenizer pipeline

Binding for §10/06 (tantivy schema) and the tokenizer impl.

Steps, in order
(`crates/brain-index/src/tantivy_shard/tokenizer.rs:1-66`):

1. **Unicode normalization** — NFC. Non-NFC inputs normalize
   before any other step.
2. **Lowercase** — full Unicode-aware lowercase.
3. **Sublanguage preservation** — before generic splitting, one
   combined regex pass emits standalone tokens (not stemmed, not
   split further by the next step):
   - URL: `\bhttps?://\S+`.
   - Ticket-style code ID: `\b[A-Za-z][A-Za-z0-9]+-\d+\b` — matches
     `ACME-1247`. Case-insensitive at match time, lowercased on
     emission, so `acme-1247` and `ACME-1247` are one token.
   - Dot/underscore-joined identifier:
     `\b[a-zA-Z_][a-zA-Z0-9_]*[._][a-zA-Z0-9_.]+\b` — matches
     `brain_storage`, `foo.bar.baz`, `module.fn`. The **internal
     separator is required**: a plain English word has none, so it
     falls through to step 4 and is stemmed like any other word. A
     pattern that matched bare lowercase runs would protect the
     whole language from stemming.
4. **Generic tokenization** — the residue between protected
   matches is tokenized by a positive word pattern,
   `[\p{L}\p{N}]+` (letter and number runs), rather than by
   enumerating separators.
5. **Stop-word removal** — **NO**. BM25's `idf` term
   demotes high-frequency words naturally; aggressive stop-word
   removal would break exact-ID queries on tokens like `to` or
   `of` that appear inside larger preserved identifiers.
6. **Porter / English stemming** — applied only to the
   generic-tokenization output, NOT to the sublanguage tokens
   from step 3.

The analyzer registers on each per-shard `tantivy::Index` under the
name `"default"`, overriding tantivy's built-in, so `TEXT` fields pick
it up without a schema-version bump
(`crates/brain-index/src/tantivy_shard/tokenizer.rs:26-29`).

The pipeline is the same for both `LexicalScope::MemoryText` and
`LexicalScope::StatementText`; per-field overrides are explicitly
out of scope.

## 4. Scope dispatch

| Scope | Index file (§10/06) | `RankedItem.id` type |
|---|---|---|
| `MemoryText` | `memory_text.tantivy/` | `MemoryId` |
| `StatementText` | `statements.tantivy/` | `StatementId` |

A single `retrieve()` call queries exactly one index. The query
router (§13/05) issues two retriever calls when both scopes apply.

Cross-shard ranking: out of scope — retrieval is **per-shard**.
A multi-shard query fan-outs at the router (§13/05 dispatch),
each shard runs its own LexicalRetriever, and the router merges
by rank. This matches Brain's single-writer-per-shard discipline.

## 5. Filters

Binding for §10/06 field set. Filters are AND-ed against the
BM25 query.

`LexicalFilters` (memory scope):
- `space_id: Option<SpaceId>` — exact match.
- `kind: Option<MemoryKind>` — exact match.
- `created_at: Option<RangeInclusive<u64>>` — unix-ms range.

`LexicalFilters` (statement scope):
- `kind: Option<StatementKind>` — exact match.
- `predicate_id: Option<PredicateId>` — exact match on
  `predicate_name` field.
- `confidence_bucket: Option<RangeInclusive<u8>>` — buckets 0–9
  representing 0.1 increments per §10/06.
- `extracted_at: Option<RangeInclusive<u64>>` — unix-ms range.

A filter on a field absent from the scope's schema returns
`LexicalError::QueryParseFailed`.

## 6. Result shape and idempotency

- `Vec<RankedItem>` ordered by descending BM25 score.
- `rank` is 1-based and dense (1, 2, 3, …) within the returned
  slice.
- `top_k` is applied **after** scoring and filtering; if fewer
  matches exist, the slice is shorter than `top_k`.
- `score` is the raw BM25 score returned by tantivy. Treat as
  opaque for cross-retriever fusion.
- `snippet` is optional. Brain may always return `None`. If
  populated, format is plain text with the matched terms wrapped
  in `[[ ... ]]` markers (HTML escaping is the caller's problem).

**Idempotency:** two calls with the same `(query, scope, config)`
return identical `Vec<RankedItem>` between commits. The text
indexer workers (§15) commit on group cadence; between
commits the retriever's view is frozen.

## 7. Errors

`LexicalError` taxonomy (binding for §04/07 error code map):

| Variant | Trigger | Visible to clients |
|---|---|---|
| `IndexUnavailable` | Index is mid-rebuild (§10/06) or corrupt at open. | Yes — clients retry. |
| `QueryParseFailed` | Empty query, invalid filter, or filter field not in scope schema. | Yes — client bug. |
| `Timeout` | Query exceeded `config.timeout_ms`. | Yes — degraded response. |

The budget the planner sets is **soft**: exceeding it records the lane as
`Timeout` and warns, but does not cancel the in-flight tantivy query, and a
late result still reaches fusion
(`crates/brain-planner/src/retrieval/planner/logic.rs:55-65`).

An **empty result** (`Ok(vec![])`) is NOT an error. The
retriever does not interpret zero matches.

## 8. Performance

Pinned in §19 (perf targets):

| Operation | p50 | p99 |
|---|---|---|
| Memory @ 100K, single-term | 10 ms | 50 ms |
| Memory @ 100K, multi-term + filter | 15 ms | 70 ms |
| Statement @ 1M, single-term | 10 ms | 50 ms |
| Statement @ 1M, multi-term + filter | 15 ms | 70 ms |

These are the lexical-retriever acceptance gates. The `lexical_retrieve`
bench (`crates/brain-index/benches/lexical_retrieve.rs`) reports per-query
timings at a 10K corpus on manual dispatch and asserts nothing; the 100K/1M
targets above are the acceptance gates, validated on reference hardware (see
[`../19_benchmarks/05_acceptance_test_suite.md`](../19_benchmarks/05_acceptance_test_suite.md)
§"Performance tests").

The planned per-retriever budget is 1 s, not the 50 ms `LexicalRetrieverConfig`
default: the figures above are the lexical lane in isolation, while the budget
has to cover the slowest lane in a shared fan-out.

## 9. Boundaries

- LexicalRetriever does **not** embed text (that's `brain-embed`).
- LexicalRetriever does **not** decide which scope to use; that's
  the query router (§13/05).
- LexicalRetriever does **not** write to tantivy; that's the
  workers in §15.
- Snippet generation reuses tantivy's
  `SnippetGenerator::from_query`.
