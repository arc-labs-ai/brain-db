# 04.01 Model Choice

Brain ships with **`bge-small-en-v1.5`** as its default embedding model. This file documents the choice and the alternatives rejected.

## 1. The chosen model

[`bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5) from BAAI's [FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding) project.

Properties:

| Property | Value |
|---|---|
| Output dimensionality | 384 |
| Output dtype | `f32` |
| Output normalization | L2 (we apply it; the model emits roughly-normalized vectors that we re-normalize for safety) |
| Maximum input length | 512 tokens |
| Tokenizer | BERT WordPiece, English vocab |
| License | MIT |
| Parameters | ~33 million |
| Model size on disk | ~130 MiB (FP32 weights) or ~33 MiB (quantized) |
| Inference latency on CPU | 5–10 ms per item on modern x86_64 |
| Inference latency on GPU | ~0.5–1 ms per item, batched |

## 2. The selection criteria

Choosing an embedding model is choosing a long-term commitment. The model's vectors live in storage for as long as the data does; switching is expensive ([§ 8](#8-the-cost-of-switching)). The criteria we used:

### 2.1 Quality

The model has to produce embeddings useful for retrieval. We measured this against:

- **MTEB (Massive Text Embedding Benchmark)** retrieval scores. `bge-small-en-v1.5` ranks well above its size class and competitive with models 5× larger.
- **Spot-checks on agent-style queries** ("recall what I said about budgets", "find similar issues") — qualitative agreement with human judgment.

### 2.2 Latency

Embeddings are on the hot path. Larger models are slower; we want sub-15 ms p99 on commodity CPU. The 33M-parameter model fits.

### 2.3 Resource footprint

A model that fits in CPU cache is dramatically faster than one that doesn't. 130 MiB FP32 weights fit comfortably in L3 cache on modern server CPUs. Larger models lose this advantage.

### 2.4 License

MIT. We can ship it with Brain. Many models have non-commercial licenses or research-only restrictions; those are non-starters.

### 2.5 English coverage

The first deployment target is English. `bge-small-en-v1.5` is English-only — no waste capacity on languages we don't use.

### 2.6 Maintenance

The BAAI team maintains the model and ships periodic improvements within the version family. We benefit from their work without operating it ourselves.

## 3. The alternatives considered

### 3.1 Larger BGE variants

`bge-base-en-v1.5` (768 dim, ~110M params) and `bge-large-en-v1.5` (1024 dim, ~335M params) are the larger siblings. They produce slightly better retrieval quality.

**Rejected because:**

- Larger vectors mean more storage. 768 dim × 4 bytes = 3 KiB per slot, doubling arena size. 1024 dim × 4 bytes = 4 KiB per slot.
- Higher inference latency.
- Marginal quality improvement; retrieval quality is bottlenecked at the data side (what the agent encoded), not the model side.

If a deployment really needs higher quality, swapping to `bge-base-en-v1.5` is supported via configuration. The default is small.

### 3.2 OpenAI's text-embedding-3-small

OpenAI's hosted embedding model. 1536 dim, high quality.

**Rejected because:**

- Hosted only. Brain's design is local-first; calling out to a third party for every embed is a non-starter on latency, on cost, and on operator-control.
- Not portable. Air-gapped deployments can't use it.
- Vendor lock-in.

If an operator wants to use it, the `ENCODE_VECTOR_DIRECT` escape hatch lets the application embed externally and submit vectors. That's a valid pattern; just not the default.

### 3.3 Sentence Transformers / MiniLM

`all-MiniLM-L6-v2` is a popular small embedding model. Comparable size to `bge-small`, slightly weaker on retrieval benchmarks.

**Rejected because:**

- BGE outperforms it on MTEB by a meaningful margin.
- Apache 2.0 license is fine, but BGE's MIT is equivalent.
- BGE is more actively maintained.

### 3.4 Domain-specific models

For specialized deployments (legal, medical, code), domain-tuned models exist. We're not in any of those domains by default.

**Rejected because:**

- Brain is general-purpose.
- Operators with domain needs can configure a different model.
- Multi-modal models (CLIP family) handle image/text but don't fit the agent-text use case as well.

### 3.5 Custom / fine-tuned models

We don't ship a model trained or fine-tuned by us.

**Rejected because:**

- Building a custom model would be a multi-month project.
- The off-the-shelf BGE is already very good.
- Operators who want a custom model can plug it in via the embedding-layer configuration.

## 4. Why 384 dimensions

The dimensionality is set by the model. 384 dim × 4 bytes = 1.5 KiB per vector.

For the substrate's purposes:

- **Storage:** 1.5 KiB per memory's vector. 1M memories = 1.5 GiB arena.
- **HNSW computational cost:** dot product over 384 dims = 384 fused multiply-add ops. With AVX2 (8 floats per instruction), ~48 instructions. SIMD-friendly.
- **HNSW index size:** at typical M=16, ~16 edges/node × 8 bytes/edge ≈ 130 bytes overhead per node, plus per-vector metadata. Total overhead ~150 bytes/memory.

384 hits a sweet spot: rich enough to capture semantic distinctions, small enough to keep the working set hot in cache.

Alternatives within the same model family:

- 768 dim (BGE-base): 2× storage, 2× compute, marginally better retrieval.
- 1024 dim (BGE-large): 2.7× storage, 2.7× compute, slightly better retrieval.

We chose the smallest variant.

## 5. The model fingerprint

Every memory carries the fingerprint of the model that produced its vector. The fingerprint is detailed in [`07_fingerprinting.md`](07_fingerprinting.md). Summary: it's a 16-byte BLAKE3-derived identifier that uniquely identifies a (model, version, configuration) tuple.

Mismatch between the cue's fingerprint and a memory's fingerprint excludes the memory from query results. This is the substrate's protection against cross-model embedding noise.

## 6. The output is normalized

The model produces vectors that are roughly unit-norm. The substrate re-normalizes them to exactly unit L2 norm before storage and indexing. After normalization:

- Cosine similarity = dot product.
- All vectors live on the unit sphere in 384-dim space.
- Distance computations are simpler and faster.

[§ 04_normalization.md](04_normalization.md) details this.

## 7. The model is configuration-replaceable

While `bge-small-en-v1.5` is the default, the embedding layer's interface lets operators swap in a different model:

- A larger BGE variant for higher quality.
- A multilingual model for non-English deployments.
- A custom model for specialized domains.

The configuration knob:

```
[embedding]
model_path = "/var/brain/models/bge-small-en-v1.5"   # default
# model_path = "/var/brain/models/bge-large-en-v1.5"  # alternative
```

The model is loaded at startup. Different configurations produce different fingerprints; existing data must be re-embedded if the model changes. See [`08_migration.md`](08_migration.md).

## 8. The cost of switching

Once a deployment commits to a model, switching has costs:

- **Re-embedding** every stored memory takes proportional time. For 10M memories at 200/s/core CPU: ~14 hours per core, parallelizable.
- **Storage** stays the same (vectors are the same dim if the new model is the same family, different if not).
- **Quality** may improve or degrade; experiment carefully before committing.
- **Operational** disruption: during migration, queries see partial results.

For these reasons, the model choice is a long-term commitment. The default is conservative — `bge-small-en-v1.5` works well across many deployments and migrations are deferred until a real reason to change appears.

## 9. The non-default path: external models via ENCODE_VECTOR_DIRECT

For deployments that want a model the substrate doesn't host (e.g., a multi-modal model, a fine-tuned domain model), the protocol exposes `ENCODE_VECTOR_DIRECT` ([03. Wire Protocol](../03_wire_protocol/) §7.4). The client computes the vector externally and submits it with a fingerprint identifying the model.

This bypasses the embedding layer entirely. The substrate stores the vector, indexes it, and respects the fingerprint for cross-model exclusion. The substrate doesn't run inference itself.

Use cases:

- Multi-modal: image + text embeddings via CLIP, run client-side.
- Domain-specific: a fine-tuned legal-text model.
- Multilingual: a translation-aware model the substrate doesn't ship.

The substrate doesn't restrict the model fingerprint, but two consequences:

- The client is responsible for embedding consistency. If they submit vectors from different models with the same fingerprint, the substrate can't tell.
- The client misses substrate features like cue caching (substrate doesn't see the source text) and re-embedding on model upgrade (substrate doesn't have the model).

For most agent applications, the default path (substrate-owned embedding with `bge-small-en-v1.5`) is the right choice.

---

*Continue to [`02_tokenization.md`](02_tokenization.md) for the tokenizer.*
