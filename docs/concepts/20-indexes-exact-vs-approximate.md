# 20 — Indexes: exact vs approximate

When you call `recall`, Brain searches a possibly-large
collection of vectors for the ones nearest your query.
This chapter explains why that search uses an
*approximate* algorithm — and why approximate is the right
choice for vector search, not a corner-cutting compromise.

The algorithm is called **HNSW**. We'll cover the
intuition without the math, then talk about what makes it
work well and what its trade-offs are.

---

## The problem

You have a shard with, say, 1 million memories. Each
memory has a 384-dimensional vector. A query arrives and
gets embedded into a query vector. You need to find the
*k* memories whose vectors are most similar to the query
(chapter 09: cosine similarity, dot product, "meaning is
geometry").

The naive approach: compute the similarity to *every*
stored vector, sort, take the top *k*. That's:

- 1,000,000 vector comparisons.
- Each comparison is a dot product of two 384-element
  vectors (~384 floating-point multiply-adds).
- ~400 million ops per query.

On a modern CPU with SIMD, that's roughly 100 ms per
query. For a shard with 10 million memories, multiply by
10. Linear in the size of the collection.

This is **exact** nearest-neighbour search. It's the
ground truth — it always finds the *truly* closest vectors.
And for 1 million memories at 100 ms per query, it's
already too slow for most production workloads.

> **Why is high-dimensional NN hard?**
>
> In low dimensions (2D, 3D) you can build a kd-tree or
> similar structure that does nearest-neighbour in
> `O(log N)`. In high dimensions (>20 or so) those
> structures degrade to `O(N)` because of the **curse of
> dimensionality**: as the number of dimensions grows,
> volumes become exponentially sparse and "tree pruning"
> stops working. Above ~30 dimensions, kd-trees and
> friends are no better than brute force.
>
> See [Wikipedia: Curse of dimensionality](https://en.wikipedia.org/wiki/Curse_of_dimensionality)
> and [Wikipedia: k-d tree](https://en.wikipedia.org/wiki/K-d_tree).

For 384 dimensions, exact search using a tree-based index
is no faster than brute force. You'd have to look at
*every* vector to be sure you didn't miss the truly
closest one.

The escape hatch is **approximate nearest neighbour**
(ANN): give up on the guarantee of finding *the* closest,
in exchange for `O(log N)` query time. That's the deal
HNSW makes.

---

## Approximate is fine

Why is approximate OK?

1. **The "truly closest" memory often isn't the answer
   you want.** Vector similarity is an approximation of
   "meaning closeness" to start with. The 95th-percentile
   match and the 100th-percentile match are usually both
   relevant; the algorithm's "miss" on the 100th gives
   you the 95th, which is fine.
2. **Recall vs precision.** ANN's failure mode is missing
   one of the true top-K results, not returning a wrong
   one. If you ask for the top 10 and the algorithm
   returns 10 items, *9 of which are in the true top 10*
   (recall@10 of 90%), the result quality is barely
   distinguishable from exact.
3. **The miss probability is tunable.** HNSW has a
   parameter (`ef_search`) that trades latency for recall.
   You can run faster and miss occasional results, or run
   slower and find them all.

Quoting some empirical numbers (chapter 04 of the
architecture tier has more):

| `ef_search` | recall@10 (1M vectors) | typical latency |
|---|---|---|
| 16 | ~85% | ~0.5 ms |
| 32 | ~92% | ~1 ms |
| 64 | ~96% | ~2 ms |
| 128 | ~98% | ~4 ms |

For most workloads, `ef_search = 64` (96% recall, 2 ms) is
the right balance. You're trading 4% of recall for ~50×
speed-up over brute force.

> **Recall vs precision in retrieval terms**
>
> *Recall* (in retrieval) = of all the relevant items that
> exist, how many did the algorithm find? *Precision* =
> of the items the algorithm returned, how many were
> relevant?
>
> ANN can miss relevant items (lower recall) but rarely
> returns *wrong* items (high precision — the items it
> *does* return are usually genuinely close to the query).
> So users perceive an ANN result as "slightly less
> complete" rather than "wrong."
>
> See [Wikipedia: Precision and recall](https://en.wikipedia.org/wiki/Precision_and_recall).

---

## HNSW: navigate by graph

**Hierarchical Navigable Small World.** Long name, simple
idea.

Picture a graph where each memory's vector is a node, and
edges connect "near" memories — points whose vectors are
close in similarity. Now layer multiple such graphs on
top of each other, with sparser connections at higher
layers:

```
        layer 3   ●─────●─────────────●            (sparse top)
                  │     │             │
        layer 2   ●──●──●──●──●──●────●            (medium)
                  │  │  │  │  │  │    │
        layer 1   ●●●●●●●●●●●●●●●●●●●●●            (dense)
                                                ─── etc.
        layer 0   ●●●●●●●●●●●●●●●●●●●●●●●●●●●     (all memories)
```

Every memory lives in layer 0; some are also promoted to
layer 1, fewer to layer 2, and so on. The number of
layers a memory lives on is set by a random sample at
insert time.

To find the *k* nearest neighbours of a query vector:

1. **Start at a chosen entry point** in the top layer.
2. **Greedy walk**: from the current node, move to its
   neighbour closest to the query. Repeat until no
   neighbour is closer.
3. **Drop a level.** Use the layer's stopping point as the
   entry point for the next layer down.
4. **Repeat** all the way to layer 0.
5. **Beam search at layer 0**: instead of one greedy
   path, explore a "beam" of `ef_search` candidates,
   keeping the best.
6. **Return the top *k*** from the beam.

The intuition: the higher layers are the "express
highway" — they get you close to the right neighbourhood
quickly. Lower layers are the local streets — they refine
the answer.

> **What's a "Small World" network?**
>
> A graph where most nodes aren't directly connected, but
> any node can reach any other in a small number of hops
> (typically ~`log N`). Social networks have this
> property; the web does too. HNSW deliberately constructs
> a graph with these properties so navigation is fast.
>
> See [Wikipedia: Small-world network](https://en.wikipedia.org/wiki/Small-world_network).

HNSW's worst-case query time is `O(log N)`. For a 10M-
memory shard, log2(10M) ≈ 23 — the algorithm touches a
few hundred nodes per query instead of millions.

> **Where to read more on HNSW**
>
> The original paper:
> [Malkov & Yashunin, 2018, "Efficient and robust
> approximate nearest neighbor search using
> Hierarchical Navigable Small World graphs"](https://arxiv.org/abs/1603.09320).
>
> A friendly visual explanation:
> [Pinecone's "Hierarchical Navigable Small Worlds" article](https://www.pinecone.io/learn/series/faiss/hnsw/).

---

## HNSW parameters

Three parameters control HNSW's behaviour. Brain's
defaults:

| Parameter | Default | What it controls |
|---|---|---|
| `M` | 16 | Graph density (edges per node per layer). |
| `ef_construction` | 200 | Search beam width during insertion. Affects build quality. |
| `ef_search` | 64 | Search beam width during query. Affects recall vs latency. |

You almost never need to tune these — the defaults are
the standard recommendations from the literature. Two
things worth knowing:

- **`M = 16`** is the recommended density for 384-dim
  vectors. Larger M → more edges per node → better
  recall, slower insertions, more memory. M=16 is the
  sweet spot for typical workloads.
- **`ef_search`** is overridable per query. A single
  high-precision recall can request `ef_search = 128`;
  bulk recall workloads can use 32. This is the
  performance knob clients have at runtime.

The architecture tier (chapter 04) has the full
breakdown. The concepts tier just notes: HNSW has three
knobs; defaults are good; one of them is overridable
per-query if you need to.

---

## Where the index lives in Brain

The memory HNSW lives in *RAM* — there's no on-disk format
for the graph itself. On a fresh boot, the substrate
rebuilds the index by scanning the metadata store and
re-inserting every active memory's vector.

This sounds expensive, but:

- For a 1M-memory shard, rebuild takes ~30 seconds.
- Recovery is normally fast because the WAL is small;
  the index rebuild can run in parallel with WAL replay.
- *Snapshots* persist the index periodically so
  recovery doesn't always need a full rebuild.

The snapshot worker (chapter 07, also architecture
chapter 07) writes the index alongside a snapshot of the
arena and metadata. A shard with a recent snapshot opens
fast; one without rebuilds from scratch.

When a schema is declared, two additional HNSW indexes
live alongside the memory one:

- **Entity HNSW** — embeddings of entity canonical names,
  used by the entity resolver (chapter 10).
- **Statement HNSW** — embeddings of statement text
  representations, used by the semantic retriever in
  hybrid recall (chapter 17).

Same algorithm, same parameter discipline. Different
indexes because different things are being indexed.

---

## Tombstones in the index

When `forget` happens (chapter 16), the memory's vector
isn't *removed* from the HNSW graph immediately. Removing
a node from a graph is expensive (you have to repair the
edges around the gap). Instead, the node is marked as
**tombstoned**:

```
slot.flags |= TOMBSTONED
hnsw.mark_tombstoned(memory_id)
```

The search algorithm walks the graph normally but skips
tombstoned nodes when collecting results. They're invisible
to recall.

What this means in practice:

- **Tombstones accumulate cheaply.** A forget is a flag
  flip, no graph repair.
- **Quality degrades slowly as tombstones accumulate.**
  The graph still has the dead nodes connected (for
  navigation), but they don't contribute to results.
- **At some point, you rebuild.** When the tombstone
  ratio exceeds a threshold (default 30%), the HNSW
  maintenance worker rebuilds the index from scratch,
  excluding tombstones. Chapter 07 covers the worker.

The rebuild produces a fresh, lean index with no
tombstones. Recall quality is back to optimum.

---

## What HNSW doesn't do

A few clarifications:

- **HNSW isn't sorting.** It's a navigable graph. There's
  no global ordering; you can't ask "give me memory at
  rank 100" without doing a search.
- **HNSW isn't perfect recall.** Approximate. You can
  tune the parameters higher for better recall at higher
  latency, but exact NN in 384 dimensions is intrinsically
  expensive.
- **HNSW doesn't handle filters natively.** Filters
  (like "only memories from agent X") are applied
  alongside or after the HNSW search, not by HNSW
  itself. Chapter 17 covers how this composition works
  for hybrid retrieval.

---

## What about other ANN algorithms

HNSW is the current best-of-breed for the cosine-similarity-
on-fixed-dimension workload Brain has. Other algorithms
exist:

- **IVF (Inverted File Index)** — partition the vector
  space into clusters; at query time, search only the
  most-relevant clusters. Used by FAISS and many older
  systems. Fast but lower recall than HNSW at equivalent
  parameters.
- **LSH (Locality-Sensitive Hashing)** — hash vectors into
  buckets where nearby vectors collide. Simple, but
  worse recall than HNSW.
- **PQ (Product Quantization)** — compress each vector to
  a few bytes via clustering. Often combined with IVF.
  Big memory wins; some recall loss. Used at very large
  scale.
- **Brute force with SIMD** — the baseline. Fine for
  smaller datasets (<100K vectors).

HNSW wins for Brain's scale because:

1. The vectors are pre-normalised (chapter 09), so the
   dot product is fast on modern hardware.
2. The dataset fits in RAM easily at typical shard sizes
   (1M memories = ~1.5 GB of vectors).
3. Recall quality matters more than absolute memory
   minimisation.

If your shard has >10M memories and RAM is tight,
PQ-based methods become attractive. The substrate's
index is replaceable in principle, but v1 ships HNSW
only.

---

## Recap

- **Exact nearest-neighbour** in 384-dim space is `O(N)`
  brute force; tree-based methods don't help.
- **Approximate** algorithms trade ~5% recall for
  `O(log N)` query time — usually a great deal.
- **HNSW** is the algorithm: a hierarchical small-world
  graph navigated greedily from top layer down.
- Three parameters (`M`, `ef_construction`, `ef_search`)
  control HNSW; defaults are well-chosen.
- The index lives in **RAM**, rebuilt from disk on cold
  start (sped up by snapshots).
- **Tombstones** make forget cheap; periodic rebuild
  keeps recall quality high.

---

## Where to go next

- **The lexical side of retrieval:** [chapter 21](21-lexical-and-fusion.md)
  — BM25 and ranked fusion.
- **How HNSW participates in hybrid retrieval:**
  [chapter 17](17-hybrid-retrieval.md).
- **The architecture-tier deep dive:**
  [`../architecture/04-hnsw-index.md`](../architecture/04-hnsw-index.md).
- **Why vector search needs an index at all:**
  [chapter 09](09-vector-similarity.md) — vector
  similarity.
