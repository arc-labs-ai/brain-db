# 21 — Lexical search and ranked fusion

Chapters 20 explained how Brain finds vectors that are
similar to a query. This chapter is about the *other*
side of search: **classical text matching** (BM25), and
how Brain combines BM25's results with vector search
through **rank fusion** (RRF).

Vector search and text search are good at different
things; using both together is most of where hybrid
retrieval (chapter 17) gets its recall quality from.

---

## Why text search still matters

Vector search is paraphrase-friendly: it finds memories
about "sprint planning" even when the cue says "iteration
scheduling." This is great when the user's vocabulary
might differ from the stored memory's vocabulary.

But sometimes the user's vocabulary is *exact* and the
embedding fuzzes it:

- The user asks for `ACME-1247`. The vector embedding
  might place this near similar-shaped IDs, which is
  fine but not what you want.
- The user asks for "deploy" specifically. The embedding
  might rank "release" higher (semantically similar but
  literally different).
- The user asks for "Priya Ramesh." Vector search finds
  any text mentioning Priya; what you wanted was the
  exact-name match.

For these cases, you want a search algorithm that pays
attention to the literal *terms* the user used. That's
what BM25 does.

---

## BM25 in plain English

**BM25** is a ranking function used in classical text
search systems. It ranks documents by how well their
words match the query, with two intuitions:

1. **Term frequency matters.** A document that uses the
   query terms many times is more relevant than one that
   uses them once. But the relationship is sub-linear —
   the second occurrence of "deploy" matters more than the
   tenth.
2. **Common terms matter less.** A document containing
   "the" isn't more relevant than one containing
   "deployment" just because "the" appears more often.
   BM25 weights each query term by how rare it is across
   the whole corpus. Rare terms (`acme-1247`) dominate the
   score; common terms (`the`) barely count.

The formula:

```
BM25(doc, query) = Σ_term  IDF(term) × TF_norm(term, doc)
```

Where `IDF` (inverse document frequency) is "how rare is
this term in the corpus" and `TF_norm` is "how often does
this term appear in this document, normalised by
document length."

You almost never need to compute this by hand. The
implementation handles it. The mental model is:

> A document scores high if it contains *rare* query
> terms *many* times.

> **More on BM25**
>
> The "BM" stands for "Best Match"; "25" is the version
> number (Stephen Robertson and Karen Spärck Jones
> developed earlier variants in the 1970s; BM25 emerged
> from work at City University in the 1990s).
>
> See [Wikipedia: Okapi BM25](https://en.wikipedia.org/wiki/Okapi_BM25).

---

## The lexical index: tantivy

Brain's lexical search uses [tantivy](https://github.com/quickwit-oss/tantivy),
a Rust full-text search library conceptually similar to
Apache Lucene. It maintains inverted indexes over text
content — fast structures for "give me all documents
containing term X."

> **What's an "inverted index"?**
>
> An index keyed by *term*, mapping each term to the list
> of documents that contain it. The opposite of a
> document-keyed index, hence "inverted." It's the data
> structure that makes "find documents containing
> 'deploy'" cheap.
>
> See [Wikipedia: Inverted index](https://en.wikipedia.org/wiki/Inverted_index).

A shard has two tantivy indexes:

- `memory_text.tantivy/` — indexes the text of every
  memory. Used to find memories whose text matches.
- `statements.tantivy/` — indexes statement text
  representations (subject name + predicate + object).
  Used to find statements whose text matches.

The index files live in their own directories under the
shard's data directory. Index maintenance is done by
background workers — memory text indexer, statement text
indexer (chapter 07).

---

## Tokenisation

Before BM25 can rank text, the text gets *tokenised*:
split into individual terms. Brain's tokenisation pipeline:

1. **Lowercase.** `Priya` → `priya`.
2. **Word splitter.** Split on whitespace and punctuation.
   `auth-service` → `auth`, `service`.
3. **Stemmer.** Strip word endings to a common stem.
   `deployment` → `deploy`. `running` → `run`. Brain uses
   the Porter stemmer.
4. **Custom token-preserving filter.** Keep URLs and
   code identifiers intact rather than breaking them into
   bits. `auth_service` stays one token; `https://acme.com`
   stays one token.

This matters because BM25's quality depends on the tokens
matching. Asking for "deploying" should match documents
containing "deployment" — same stem, after stemming.

> **What's stemming?**
>
> Mapping morphological variants of a word to a single
> form. The Porter stemmer is a deterministic rule-based
> algorithm from 1980 that strips common English suffixes
> (`-ing`, `-ed`, `-ly`, `-ization`, etc.).
>
> See [Wikipedia: Stemming](https://en.wikipedia.org/wiki/Stemming).

The tokenisation is the same for the index (built at
write time) and the query (built at recall time), so
matches work bidirectionally.

---

## Filters in BM25

The lexical retriever accepts the same filters as the
semantic retriever — agent ID, memory kind, time range,
predicate. These get pushed into tantivy's query layer as
*term filters* alongside the BM25 query:

```
Query: "priya prefer*"
Filter: agent_id = alice
Filter: created_at in [last 7 days]
```

Tantivy handles each filter at the index level: it
restricts the candidate set before BM25 scoring. So a
query with `agent_id = alice` doesn't have to score
every memory in the shard, just alice's.

---

## Two ranked lists, one result

After running semantic and lexical retrieval (and possibly
graph too, when an entity is anchored), Brain has multiple
ranked lists:

```
semantic ranking (memory_text):
    1. mem_018f2b…  similarity 0.91
    2. mem_019a3b…  similarity 0.85
    3. mem_01a4cc…  similarity 0.79
    ...

lexical ranking (BM25):
    1. mem_019a3b…  BM25 4.21
    2. mem_018f2b…  BM25 3.85
    3. mem_01b22d…  BM25 2.17
    ...
```

Both lists have rankings. The scores are *not*
comparable: similarity is `[-1, 1]`, BM25 is unbounded
positive. You can't add them directly.

The problem isn't just scale — it's *distribution*.
Cosine similarities cluster in a narrow band; BM25
scores spread widely. Normalising one to match the other
(min-max scaling, say) requires knowing the distribution
in advance.

This is what **rank fusion** addresses.

---

## RRF — Reciprocal Rank Fusion

The trick of RRF is to **ignore the scores and use only the
ranks**.

For each ranking, item *i* gets a contribution:

```
contribution_i = weight / (k + rank_i)
```

Where `weight` is the retriever's weight (default 1.0)
and `k` is a smoothing constant (default 60).

Sum the contributions across retrievers; sort by total.

Example with `k = 60`, equal weights:

```
mem_018f2b:  1/(60+1)   [semantic rank 1]
           + 1/(60+2)   [lexical  rank 2]
           = 0.0164 + 0.0161 = 0.0325

mem_019a3b:  1/(60+2)   [semantic rank 2]
           + 1/(60+1)   [lexical  rank 1]
           = 0.0161 + 0.0164 = 0.0325

mem_01a4cc:  1/(60+3)   [semantic rank 3]
           + 0             [not in lexical top]
           = 0.0159

mem_01b22d:  0             [not in semantic top]
           + 1/(60+3)   [lexical  rank 3]
           = 0.0159
```

Both top-ranked-by-one-retriever items (`mem_018f2b` and
`mem_019a3b`) tie at 0.0325 — they were each #1 in one
ranking, #2 in the other. The "single-retriever" items
(`mem_01a4cc`, `mem_01b22d`) trail.

That's the entire algorithm.

---

## Why RRF is good

Three reasons RRF beats other fusion methods:

1. **Score-scale invariant.** Doesn't matter that cosine
   is bounded and BM25 is unbounded. The ranks are
   comparable; the scores aren't used.
2. **Stable under small perturbations.** If two items
   have nearly-tied raw scores in semantic but are ranked
   5 and 6, RRF treats them as ranks 5 and 6 — no
   threshold debate.
3. **Tail smoothing.** With `k = 60`, rank 1 contributes
   `1/61 ≈ 0.0164`; rank 10 contributes `1/70 ≈ 0.0143`.
   Ratio ~1.15. One retriever ranking your right answer
   1st doesn't drown out another retriever ranking it
   10th.

The combination means RRF is *robust*. It works without
per-deployment calibration, without training data,
without manual tuning of distribution-matching
parameters. Plug retrievers in, RRF combines them.

### Why `k = 60`

The original RRF paper (Cormack et al., 2009) benchmarked
across many retrieval tasks and converged on `k = 60` as a
strong default. Larger `k` (more smoothing) helps when
individual retrievers are noisy. Smaller `k` (less
smoothing) helps when retrievers are well-calibrated.

Brain ships `k = 60`. Per-query overrides are accepted
for deployments that have evaluation data suggesting
better.

---

## Alternatives that were rejected

A few other fusion approaches Brain considered:

- **Weighted sum after normalisation.** Min-max
  normalise each retriever's scores, sum with weights.
  Rejected because cosine and BM25 distributions aren't
  Gaussian; min-max is unstable in practice.
- **Per-retriever calibration.** Train a calibrator
  (e.g., logistic regression) to map raw scores to a
  shared probability scale. Rejected because it
  requires labelled data per deployment, and most
  deployments don't have that.
- **Learned fusion.** Train a small neural net to take
  per-retriever scores + features → fused score.
  Promising but requires data; deferred.

RRF benchmarks competitively against all these in
published hybrid-retrieval evaluations. It's the
production default for a reason.

---

## Why three retrievers, not two

Chapter 17 covered this from the planner's angle. The
short version: semantic and lexical alone are good but
fall over for **entity-anchored queries** — "everyone who
reports to Priya." There's nothing semantic about that
question (it's a graph traversal), and there's no lexical
content beyond the entity name. The graph retriever
covers this gap.

Three retrievers cover, between them, almost any query
shape an agent might ask. The router (chapter 17) picks
which ones to invoke; RRF merges the outputs.

For pure-substrate deployments (no schema), there's only
the semantic retriever; recall is just vector search,
no fusion. The complexity scales with how much knowledge
layer you've activated.

---

## Per-retriever top-K cap

Each retriever returns at most `top_n` candidates (default
~100). Items beyond `top_n` don't enter fusion.

Consequence: a document ranked 200th in semantic but 1st
in lexical gets only the lexical contribution. The 200th-
place semantic contribution would have been negligible
anyway, but the cap formalises that.

`top_n = 100` is the right default for most workloads.
Higher values let "tail" items enter fusion (slower);
lower values speed fusion at some recall cost.

---

## Lexical vs vector: when each wins

A rough decision table for thinking about retrieval
behaviour:

| Query | Vector | Lexical | RRF result |
|---|---|---|---|
| "How does Priya feel about meetings?" | High (paraphrase) | Low (literal terms differ) | Vector dominates; result ranks highly. |
| "ACME-1247" | Low | High | Lexical dominates; result ranks highly. |
| "auth service rollout" | Both | Both | Both contribute; high fused score for true matches. |
| "Anyone who reports to Priya?" | Low | Low | Graph dominates (chapter 17). |
| Misspelled query "Pria" | Low | Misses Priya entirely (literal mismatch) | Recall suffers; either fix client-side or rely on the trigram fuzzy matcher in the entity resolver. |

The fused result is *usually* better than either alone
because most queries have mixed character: some semantic
shape, some literal-term importance. RRF lets each
retriever contribute its strength.

---

## Recap

- **Lexical search** uses BM25 — a classical algorithm
  that ranks documents by term frequency weighted by
  rarity.
- Brain's lexical indexes are managed by **tantivy**, a
  Rust full-text search library. Two per shard: one for
  memory text, one for statement text.
- **Tokenisation + stemming + token-preserving filters**
  turn raw text into matchable terms.
- **RRF** combines multiple rankings by reciprocal-rank
  sums — score-scale invariant, robust, no calibration
  needed.
- `k = 60`, per-retriever weights default 1.0, `top_n`
  default ~100. All overridable per query.
- The fused result is usually better than any single
  retriever because queries have mixed character.

---

## Where to go next

- **The hybrid retrieval flow:** [chapter 17](17-hybrid-retrieval.md).
- **HNSW (the vector side):** [chapter 20](20-indexes-exact-vs-approximate.md).
- **The architecture-tier deep dive:**
  [`../architecture/11-hybrid-retrieval-rrf.md`](../architecture/11-hybrid-retrieval-rrf.md).
- **Reference for tantivy:**
  [tantivy on GitHub](https://github.com/quickwit-oss/tantivy).
