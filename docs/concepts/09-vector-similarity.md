# 09 — Vector similarity

Chapter 08 explained what an embedding is — 384 numbers
representing a piece of text's meaning. This chapter explains
how Brain decides that two embeddings (and therefore two
pieces of text) are *similar*.

The answer is simple in hindsight: **meaning is geometry**.
Texts with similar meanings produce embeddings that are close
together in the 384-dimensional space the model maps into.
Computing similarity is computing geometric distance.

---

## The mental model: points in space

Imagine a 384-dimensional space. Every memory in Brain is a
point in that space — its embedding tells you where the point
is.

You can't actually picture 384 dimensions, so picture 2 or 3:

```
   y
   ▲
   │
   │       ● "Priya prefers async meetings"
   │      ╱
   │     ╱
   │    ● "Sprint planning should be async"
   │   ╱
   │  ╱
   │ ╱
   │╱
   ●─────────────────────────────────────► x
   "The deploy of auth-service failed"
```

The two "async meetings" sentences land near each other. The
"deploy failed" sentence lands somewhere else entirely. The
embedding model is what put them in those positions; it was
trained on millions of paired sentences specifically so that
similar-meaning sentences end up near each other.

> **Why does this work?**
>
> Because the training did exactly that. During training,
> the model is shown pairs of sentences labelled "these
> mean the same thing" and "these mean different things,"
> and its weights are adjusted to make the embeddings of
> similar pairs close together and dissimilar pairs far
> apart. After training on millions of such pairs, the
> behaviour generalises to unseen text.
>
> See [Wikipedia: Sentence embedding](https://en.wikipedia.org/wiki/Sentence_embedding)
> for an overview.

In 384 dimensions, "close together" needs a precise
definition. That's what cosine similarity is.

---

## Cosine similarity

Cosine similarity between two vectors `A` and `B` is:

```
cos(θ) = (A · B) / (|A| × |B|)
```

Where:

- `A · B` is the *dot product* — `sum(a_i * b_i)` across all
  384 components.
- `|A|` and `|B|` are the magnitudes — `sqrt(sum(a_i²))`.
- `θ` is the angle between the two vectors.

The intuition with the angle picture:

```
        B
       ╱
      ╱
     ╱ θ
    ╱
   ╱___________ A
```

- If `A` and `B` point in *exactly the same direction*, the
  angle `θ` is 0, `cos(0) = 1`. Maximum similarity.
- If `A` and `B` are *perpendicular*, the angle is 90°,
  `cos(90°) = 0`. No similarity.
- If `A` and `B` point in *exactly opposite directions*, the
  angle is 180°, `cos(180°) = -1`. Maximum dissimilarity.

So cosine similarity gives you a number in `[-1, 1]`:

```
   -1.0            0.0              1.0
    │               │                │
  opposite      orthogonal        identical
```

Brain reports similarity as a `f32` in this range. In
practice for L2-normalised embeddings from a model like BGE,
you almost never see negative similarities — the embeddings
all land in roughly the same hemisphere of the unit sphere
(meaning is rarely *opposite*; it's more often *unrelated*,
which is closer to 0).

---

## Why L2 normalisation matters

Chapter 08 mentioned that Brain L2-normalises every embedding
before storing it. That makes `|A| = |B| = 1` for every
vector, which simplifies the cosine formula to:

```
cos(θ) = A · B          # because |A| × |B| = 1 × 1 = 1
```

A pure dot product. This is **much faster** to compute on
modern hardware than the full formula — modern CPUs and GPUs
have specialised instructions for dot products of floating-
point vectors. Brain's vector search is bottlenecked on this
dot-product computation, so eliminating the magnitudes is a
real win.

L2 normalisation also has a geometric interpretation: every
vector lives on the surface of a unit hypersphere (a sphere
in 384 dimensions). The relevant distance between two points
on a sphere is the angle between them — which is exactly what
cosine measures.

> **Where does "L2" come from?**
>
> The "L2 norm" is one of several ways to measure a vector's
> length. L1 norm is `sum(|a_i|)` (Manhattan distance). L2 is
> `sqrt(sum(a_i²))` (Euclidean distance). L∞ is `max(|a_i|)`.
> L2 is by far the most common; "normalising" without
> qualification usually means L2.
>
> See [Wikipedia: Norm (mathematics)](https://en.wikipedia.org/wiki/Norm_(mathematics)).

---

## A two-dimensional worked example

To make this concrete, let's drop from 384 dimensions to 2.
Three pieces of text, three points:

```
A = (0.6, 0.8)      ← "Priya prefers async meetings"
B = (0.5, 0.87)     ← "Sprint planning should be async"
C = (0.95, 0.31)    ← "The deploy of auth-service failed"
```

All three are L2-normalised: `|A| = |B| = |C| = 1`.

Compute cosine similarity:

```
A · B = (0.6)(0.5) + (0.8)(0.87)  = 0.30 + 0.696 = 0.996
A · C = (0.6)(0.95) + (0.8)(0.31) = 0.57 + 0.248 = 0.818
B · C = (0.5)(0.95) + (0.87)(0.31) = 0.475 + 0.270 = 0.745
```

So:

```
sim(A, B) = 0.996       ← very high — A and B mean almost the same thing
sim(A, C) = 0.818       ← moderate — both contain "the team," but topics differ
sim(B, C) = 0.745       ← lower — even less topical overlap
```

In real BGE embeddings the numbers won't be quite this clean
(384 dimensions of noise), but the relative pattern holds:
sentences about the same topic cluster, sentences about
different topics don't.

---

## How Brain uses similarity in recall

`recall(cue)` is the verb that exposes similarity to clients:

1. Brain embeds `cue` into a 384-dim vector — call it `q`.
2. Brain searches the index for vectors `v` where `q · v` is
   high.
3. Brain returns the top-K matches, each with the similarity
   score.

The search step isn't an exhaustive scan of every memory's
vector — that would be slow for large indexes. Brain uses
**HNSW** (chapter 20), an approximate-nearest-neighbour
algorithm that finds *most* of the truly-closest matches in
`O(log N)` instead of `O(N)`. The trade-off is that occasional
"would-be top hits" can be missed; the parameter `ef_search`
controls how aggressive the search is.

---

## What does a similarity score mean in practice?

A practical reading guide for `cosine similarity` between
BGE-small embeddings on English sentences:

| Score | Interpretation |
|---|---|
| 0.95–1.00 | Near-paraphrase or duplicate. "Priya prefers async meetings" vs "Priya wants async meetings." |
| 0.80–0.95 | Same topic, possibly different angle. "Priya prefers async" vs "Sprint planning should be async." |
| 0.65–0.80 | Related topic. "Priya prefers async" vs "We should meet less often." |
| 0.50–0.65 | Loosely related. The texts share some concepts but aren't really about the same thing. |
| 0.30–0.50 | Largely unrelated. The texts share occasional words at most. |
| 0.00–0.30 | Unrelated. Essentially noise. |

These are rough buckets, not thresholds. A 0.80 score on a
specialised vocabulary (legal jargon, medical text) might
mean less than a 0.80 on common English; recall quality
depends on the embedder's training data.

Most practical recall workloads:

- **Set `top_k` to what you need** (10-100) and let the
  ranker pick.
- **Don't filter by raw similarity threshold** unless you have
  a calibrated reason. Filtering at 0.7 might drop legitimate
  matches.
- **If you need calibrated thresholds**, evaluate on your own
  data first.

---

## Other similarity measures (and why Brain uses cosine)

Cosine isn't the only way to measure similarity between
vectors. Briefly, the alternatives:

- **Euclidean distance**: `sqrt(sum((a_i - b_i)²))`. The
  straight-line distance in 384-D space. Common in older
  ML systems.
- **Dot product (without normalisation)**: just `A · B`. Used
  in some recommender systems where magnitude carries
  signal.
- **Manhattan / L1**: `sum(|a_i - b_i|)`. Rare for embeddings.

Brain uses cosine because:

1. **It's invariant to magnitude.** Two vectors pointing the
   same direction get the same similarity score regardless of
   their lengths. This matches the intuition that "meaning"
   doesn't depend on magnitude.
2. **It's the standard for sentence embeddings.** BGE,
   sentence-transformers, OpenAI's embeddings — all are
   trained with cosine as the similarity metric. Using a
   different metric at query time would mismatch the training.
3. **It collapses to dot product after L2 normalisation.**
   Fast to compute on modern hardware.

If a future model used Euclidean distance instead, Brain
could be reconfigured — but the BGE model and the cosine
metric are paired choices, and changing one likely changes
the other.

---

## What similarity *isn't*

Three clarifications:

- **Similarity is not truth.** A high similarity between
  embeddings says the two texts mean similar things; it
  doesn't say either is *true*. The agent has to reason about
  truthiness separately (chapter 11 covers confidence on
  knowledge-layer statements).
- **Similarity is not certainty.** Recall returns ranked
  candidates, not "the answer." Multiple candidates may all
  be high-similarity and the agent decides which to use.
- **Similarity is not a probability.** A score of 0.8 doesn't
  mean "80% likely the right answer." It's a geometric
  measurement of an embedding-space distance.

Operationally, the right way to use similarity scores is as
*rankings* among candidates. The absolute numbers matter less
than the relative ordering.

---

## When scores are "tightly clustered"

You'll sometimes see this in `brain recall`:

```
3 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

The shell prints this when every top-K result is within `Δ<0.001`
of the highest score. It's a signal that **the ranking is not
trustworthy** — the embedder isn't actually discriminating among
these results. Causes, in order of likelihood:

1. **The embedder isn't loaded.** Brain falls back to a noop
   dispatcher in some test modes; every embedding is the same
   uniform vector, so every cosine similarity is the same. Check
   the server logs for "loaded BGE model" — its absence means
   you're on the noop path.
2. **The query is genuinely too generic for the corpus.** A
   query like "thing" against a corpus of specialised technical
   memories will find weak matches everywhere — and they'll cluster.
3. **All matches are near-duplicates of the query AND each other.**
   Less common, but possible — and arguably "tightly clustered" is
   the right thing to surface in that case anyway.

When you see the warning, treat all top-K results as roughly
equal-scored. Don't pick the top result as "the answer" — either
sample the whole set, or refine the query.

For the recall output format details, see
[`../reference/shell/output-formats.md#recall`](../reference/shell/output-formats.md#recall).

---

## Recap

- Two pieces of text are "similar" if their embeddings are
  close in 384-dimensional space.
- **Cosine similarity** is the standard measure: dot product
  of two unit vectors, in `[-1, 1]`.
- Brain L2-normalises every embedding, so cosine = dot
  product — much faster to compute.
- A similarity of 0.95+ is near-duplicate; 0.7–0.9 is "same
  topic"; below 0.5 starts being noise on standard English.
- Recall returns ranked candidates, not "the answer." Use
  similarity as a ranking, not a threshold.
- The shell's **"tightly clustered" warning** surfaces when
  ranking isn't trustworthy — usually because the embedder
  isn't loaded.

---

## Where to go next

- **What an embedding *is*:** [chapter 08](08-embeddings.md).
- **What Brain does with embeddings under the hood:**
  [chapter 20](20-indexes-exact-vs-approximate.md) — HNSW
  and approximate-nearest-neighbour search.
- **The verbs that use similarity:** [chapter 16](16-cognitive-operations.md)
  — recall, plan, reason.
- **The other half of search:** [chapter 21](21-lexical-and-fusion.md)
  — BM25 text search and how Brain combines it with vector
  search.
