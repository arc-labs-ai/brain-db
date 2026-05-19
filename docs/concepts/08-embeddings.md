# 08 — Embeddings

Brain stores every memory as text *and* as 384 numbers. This
chapter explains what those numbers are, where they come
from, why 384 of them, and what makes "meaning is geometry"
the rough idea behind vector search.

No prior ML experience is assumed. You should leave this
chapter able to read sentences like "BGE-small produces
384-dim L2-normalised embeddings" without your eyes glazing
over.

---

## The one-sentence version

An **embedding** is a list of numbers (in Brain's case, 384
floating-point numbers) that represents the *meaning* of a
piece of text — produced by a small AI model — such that
texts with similar meanings end up with similar numbers.

That's the whole concept. The rest of this chapter is just
unpacking what "similar numbers" means and what makes
embedding work.

---

## An analogy: thermometers

A thermometer maps a real-world phenomenon (how hot something
is) into a single number on a scale. Two things at similar
temperatures get similar readings. You can compare temperatures
by subtracting.

An embedding does the same thing for *meaning*, except:

- The scale isn't one number — it's 384.
- Two texts with similar meanings get similar 384-number
  readings.
- You compare meanings by computing a geometric distance
  between the two 384-number lists (chapter 09).

Where the thermometer reduces "heat" to one dimension, an
embedding reduces "meaning of an English sentence" to 384
dimensions. The number 384 is large enough that the model
can capture the variation in meaning that natural language
expresses, and small enough that storing and comparing
embeddings is cheap.

---

## Where embeddings come from

Embeddings come out of **neural network models**. Brain uses
[BGE-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5),
an open-source 33-million-parameter model from
[BAAI/FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding)
specialised for sentence-level embeddings.

The model is a BERT-style transformer:

> **What's BERT?**
>
> BERT (Bidirectional Encoder Representations from
> Transformers) is a 2018 model architecture from Google
> that became the foundation for many sentence-embedding
> models. It reads text bidirectionally (left and right
> context together), unlike older models that read it
> left-to-right.
>
> See [Wikipedia: BERT (language model)](https://en.wikipedia.org/wiki/BERT_(language_model)).

The mechanism, in plain English:

1. **Tokenize.** Split the input text into subword pieces
   ("tokens"). "Atlas" might be one token; "subordinate"
   might be two. The tokenizer is a fixed table that ships
   with the model.
2. **Embed each token.** Each token has a learned embedding
   (also a vector). The model maintains a big table of these.
3. **Run twelve transformer layers.** Each layer mixes the
   token embeddings based on their attention to each other —
   "Priya" attends more to "manager" if both appear in the
   same sentence than if they don't.
4. **Take the special `[CLS]` token's output.** The model
   prepends a fake `[CLS]` ("classify") token to every
   input; after twelve layers, its 384-dim hidden state is
   trained to summarise the sentence.
5. **L2-normalise.** Scale the 384 numbers so they form a
   unit vector (length 1). This is what makes cosine
   similarity and dot product equivalent (chapter 09).

That `[CLS]` output, normalised, is what Brain stores.

Two things to notice:

- The model was *trained* such that similar-meaning
  sentences produce similar `[CLS]` outputs. This isn't
  magic — it took a large training run on millions of
  paired sentences. The result is the released weights file.
- Brain doesn't *train* the model. Brain *runs* the
  trained model. The weights are a ~130 MB file shipped
  with the deployment.

---

## Why 384 dimensions

The number of dimensions is a property of the embedding
model, not Brain. BGE-small is fixed at 384 dims.

Other open models offer 768 dims
([all-MiniLM-L12-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L12-v2)),
1024 dims ([BGE-large](https://huggingface.co/BAAI/bge-large-en-v1.5)),
1536 dims (OpenAI's `text-embedding-3-small`), and various
others.

The trade-off:

- **Higher dimension = more expressive embedding.** A 1024-dim
  model can distinguish finer shades of meaning than a
  384-dim model on the same input.
- **Higher dimension = more storage and slower comparison.**
  Storing a 1024-dim vector is 4 KiB; storing 384-dim is
  1.5 KiB. Comparing two vectors is proportional to the
  dimension count.
- **Higher dimension = larger model and slower inference.**
  Bigger models with bigger outputs are slower to run.

384 dims sits at a pragmatic sweet spot: good enough quality
for sentence-level matching on common English text, small
enough that you can store millions of embeddings without
blowing the disk budget, fast enough that CPU inference is
viable.

If a deployment needs more quality, the embedding model is
swappable in principle (you change the model path in config).
Anything else that produces 384-dim L2-normalised float32
vectors via the same `BertModel` API will work.

> **Why "L2-normalised"?**
>
> A vector is "L2-normalised" if its length (Euclidean magnitude)
> equals 1. Mathematically: `sqrt(sum(x_i²)) == 1`. Vectors
> on the surface of a unit sphere are L2-normalised by
> definition. Normalising makes cosine similarity equivalent
> to a dot product, which is much faster to compute in SIMD
> code. Chapter 09 covers similarity in detail.
>
> See [Wikipedia: Unit vector](https://en.wikipedia.org/wiki/Unit_vector).

---

## The substrate owns the embedder

This is one of Brain's deliberate inversions versus typical
vector databases: **the substrate runs the embedding model,
not the client**.

When you call `encode(text)`, the text travels from your
client to Brain, and *Brain* runs BGE-small on it. The vector
never crosses the wire — it's generated on the server and
stored locally.

Why this matters:

1. **Dedup by semantic content.** If two clients encode
   the same text, Brain knows it's the same vector. It can
   detect duplicates by content, not just by client-
   generated hashes.
2. **Model upgrades are tractable.** If Brain's operator
   upgrades to a better embedding model, the substrate
   re-embeds every existing memory with the new model.
   Clients see nothing — same memory ids, same lookups,
   better recall quality. A vector database where the
   client computed embeddings can't do this.
3. **The cache is shared.** Brain caches recent text-to-
   vector mappings. A repeated cue ("what does Priya
   prefer?") hits the cache and skips inference entirely.
   A client-side cache works for one client; Brain's
   cache works across all clients on a shard.
4. **No model lock-in for clients.** The client doesn't
   need to ship the same model the substrate uses. SDKs
   in any language can call `encode(text)` and they all
   produce the *same* vectors because the same model runs
   on the server.

The cost is that Brain has to run ML inference. That's why
chapter 22 explains how the substrate hides inference work
behind cache hits and async work.

---

## How the cache works

Embedding is the most expensive step in `encode`:

| Step | Typical CPU time |
|---|---|
| Tokenization | ~50 µs |
| **Forward pass (12 transformer layers)** | **5–10 ms** |
| L2 normalisation | <1 µs |
| Cache write | <1 µs |

5–10 ms per encode is fine for one-off operations but adds up
under load. Brain mitigates with an **LRU cache** keyed by a
hash of the input text:

```
embed(text):
    key = BLAKE3(text)[..16]
    if key in cache and cache[key].fingerprint == current_model_fp:
        return cache[key].vector            # hit, <1 µs
    vector = run_bert_forward(text)         # miss, 5-10 ms
    cache[key] = { vector, fingerprint, inserted_at }
    return vector
```

Default cache size is 10,000 entries per shard. Hit rate
depends on workload:

- **Repeated cues** (chat bot asking the same question many
  times) — high hit rate, often 50%+.
- **Diverse one-off encodes** (logging events) — low hit
  rate, single-digit percent.
- **A common prompt template with variable input** —
  depends on whether the template is in the encoded text.

You can monitor `cache.hits / (cache.hits + cache.misses)` —
the hit-rate metric — to tune the cache size for your
workload.

---

## The model fingerprint

Every memory stores a 16-byte **fingerprint** of the model
that produced its vector. This is a hash that uniquely
identifies the model's behaviour:

```
fingerprint = BLAKE3(
    config_bytes ++
    tokenizer_bytes ++
    BLAKE3(weights_file) ++
    vector_dim ++
    normalize_flag
)[..16]
```

If anything changes — different config, different tokenizer,
different weights, different output dimension — the
fingerprint changes.

The substrate uses this to:

1. **Detect stale vectors.** A memory whose stored
   fingerprint differs from the current model's is "stale" —
   the vector was produced by an older model and may not be
   directly comparable to new ones.
2. **Trigger re-embedding.** A background worker can find
   stale memories and re-embed them with the current model.
3. **Invalidate the cache implicitly.** A cache entry with a
   non-matching fingerprint is treated as a miss. After a
   model upgrade, the cache repopulates from scratch over
   time as queries arrive.

You almost never need to think about the fingerprint
directly. It's there so that "we switched embedding models
last month" is a tractable operational event, not a data
migration project.

> **What's BLAKE3?**
>
> A modern cryptographic hash function — fast, parallelised,
> drop-in for older hashes like SHA-256 and MD5. Brain uses
> it for content-addressed identifiers, model fingerprints,
> and the embedding cache key.
>
> See [BLAKE3 official site](https://blake3.io/).

---

## The 512-token limit

BGE-small can read at most **512 tokens** at a time. Anything
longer gets truncated to that limit before the forward pass.

Tokens, not words: a long English sentence is roughly the
same number of tokens as words, but unusual words can be
multiple tokens ("undeniable" → "un" + "den" + "iable"),
and URLs/code identifiers often blow up into many tokens.

Practical implications:

- **A memory's text can be longer than 512 tokens.** Brain
  stores the full text. It's just the *embedding* that's
  computed from the first 512 tokens' worth.
- **Truncation is detected and surfaced.** The substrate
  marks a memory's metadata with a `truncated` flag if
  the embedder had to truncate.
- **For long documents, pre-segment client-side.** Split
  long content into paragraph-sized chunks and encode each
  separately. The agent now has many memories instead of
  one truncated one, and recall picks up whichever chunk
  matches.

The 512-token limit is a property of the BGE model
architecture, not Brain. A different embedding model (e.g.,
[Jina v2](https://huggingface.co/jinaai/jina-embeddings-v2-small-en),
with 8K context) has different limits.

---

## What an embedding is *not*

Three quick clarifications, because the abstraction sneaks
up on people:

- **An embedding is not a hash.** Two semantically-similar
  texts produce *similar* embeddings (close in vector
  space). A hash produces *unrelated* outputs for
  near-identical inputs.
- **An embedding is not lossless.** You can't reconstruct
  the original text from its embedding. The 384 numbers
  capture meaning, not letters.
- **An embedding is not a confidence score.** The numbers
  aren't probabilities; they're coordinates in a
  high-dimensional space. The thing you compare is two
  embeddings against each other, not one embedding
  against zero.

---

## Recap

- An embedding is 384 floating-point numbers that represent
  the meaning of a piece of text.
- Brain uses BGE-small-en-v1.5 to produce embeddings — a
  small BERT-style model, ~130 MB weights, runs on CPU.
- The substrate (not the client) computes embeddings. This
  enables semantic dedup, model upgrades, shared cache, and
  no client lock-in.
- Every embedding is L2-normalised so cosine similarity
  equals dot product. Comparing two embeddings is fast.
- The model fingerprint is the marker that says "this
  embedding was produced by this exact model" — used for
  detecting staleness and managing upgrades.
- The 512-token input limit comes from BGE's architecture.
  Brain stores the full text but embeds only the first
  512 tokens' worth.

---

## Where to go next

- **How embeddings get compared:** [chapter 09](09-vector-similarity.md)
  — cosine similarity, L2 normalisation, "meaning is
  geometry."
- **What an index does with them:** [chapter 20](20-indexes-exact-vs-approximate.md)
  — HNSW and approximate nearest-neighbour search.
- **What's in a memory:** [chapter 05](05-memories.md).
- **The architecture-tier version:**
  [`../architecture/06-embedding-pipeline.md`](../architecture/06-embedding-pipeline.md).
