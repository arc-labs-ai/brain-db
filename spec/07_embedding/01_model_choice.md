# 07.01 Model Choice

Brain ships with **`bge-small-en-v1.5`** as its default embedding model. This file documents the choice and the alternatives rejected.

## 1. The chosen model

[`bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5) from BAAI's [FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding) project.

Properties:

| Property | Value |
|---|---|
| Output dimensionality | 384 |
| Output dtype | `f32` |
| Output normalization | L2 (Brain applies it; the model emits roughly-normalized vectors that Brain re-normalizes for safety) |
| Maximum input length | 512 tokens |
| Tokenizer | BERT WordPiece, English vocab |
| License | MIT |
| Parameters | ~33 million |
| Model size on disk | ~130 MiB (FP32 weights) or ~33 MiB (quantized) |
| Inference latency on CPU | 5–10 ms per item on modern x86_64 |
| Inference latency on GPU | ~0.5–1 ms per item, batched |

## 2. The selection criteria

Choosing an embedding model is choosing a long-term commitment. The model's vectors live in storage for as long as the data does; switching is expensive ([§ 8](#8-the-cost-of-switching)). The criteria:

### 2.1 Quality

The model has to produce embeddings useful for retrieval. The measurement basis:

- **MTEB (Massive Text Embedding Benchmark)** retrieval scores. `bge-small-en-v1.5` ranks well above its size class and competitive with models 5× larger.
- **Spot-checks on agent-style queries** ("recall what I said about budgets", "find similar issues") — qualitative agreement with human judgment.

### 2.2 Latency

Embeddings are on the hot path. Larger models are slower; Brain targets sub-15 ms p99 on commodity CPU. The 33M-parameter model fits.

### 2.3 Resource footprint

A model that fits in CPU cache is dramatically faster than one that doesn't. 130 MiB FP32 weights fit comfortably in L3 cache on modern server CPUs. Larger models lose this advantage.

### 2.4 License

MIT. Brain can ship it. Many models have non-commercial licenses or research-only restrictions; those are non-starters.

### 2.5 English coverage

The first deployment target is English. `bge-small-en-v1.5` is English-only — no waste capacity on languages Brain does not use.

### 2.6 Maintenance

The BAAI team maintains the model and ships periodic improvements within the version family. Brain benefits from their work without operating it directly.

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

For specialized deployments (legal, medical, code), domain-tuned models exist. Brain is not in any of those domains by default.

**Rejected because:**

- Brain is general-purpose.
- Operators with domain needs can configure a different model.
- Multi-modal models (CLIP family) handle image/text but don't fit the agent-text use case as well.

### 3.5 Custom / fine-tuned models

Brain does not ship a model trained or fine-tuned in-project.

**Rejected because:**

- Building a custom model would be a multi-month project.
- The off-the-shelf BGE is already very good.
- Operators who want a custom model can plug it in via the embedding-layer configuration.

## 4. Why 384 dimensions

The dimensionality is set by the model. 384 dim × 4 bytes = 1.5 KiB per vector.

For Brain's purposes:

- **Storage:** 1.5 KiB per memory's vector. 1M memories = 1.5 GiB arena.
- **HNSW computational cost:** dot product over 384 dims = 384 fused multiply-add ops. With AVX2 (8 floats per instruction), ~48 instructions. SIMD-friendly.
- **HNSW index size:** at typical M=16, ~16 edges/node × 8 bytes/edge ≈ 130 bytes overhead per node, plus per-vector metadata. Total overhead ~150 bytes/memory.

384 hits a sweet spot: rich enough to capture semantic distinctions, small enough to keep the working set hot in cache.

Alternatives within the same model family:

- 768 dim (BGE-base): 2× storage, 2× compute, marginally better retrieval.
- 1024 dim (BGE-large): 2.7× storage, 2.7× compute, slightly better retrieval.

Brain uses the smallest variant.

## 5. The model fingerprint

Every memory carries the fingerprint of the model that produced its vector. The fingerprint is detailed in [`05_fingerprinting.md`](05_fingerprinting.md). Summary: it's a 16-byte BLAKE3-derived identifier that uniquely identifies a (model, version, configuration) tuple.

Mismatch between the cue's fingerprint and a memory's fingerprint excludes the memory from query results. This is Brain's protection against cross-model embedding noise.

## 6. The output is normalized

The model produces vectors that are roughly unit-norm. Brain re-normalizes them to exactly unit L2 norm before storage and indexing. After normalization:

- Cosine similarity = dot product.
- All vectors live on the unit sphere in 384-dim space.
- Distance computations are simpler and faster.

[§ 02_inference_pipeline.md](02_inference_pipeline.md) details this.

## 7. The model is configuration-replaceable

While `bge-small-en-v1.5` is the default, the embedding layer's interface lets operators swap in a different model:

- A larger BGE variant for higher quality.
- A multilingual model for non-English deployments.
- A custom model for specialized domains.

The configuration knob is `[embedder] model`, and it takes a **model name**, not a path:

```toml
[embedder]
model = "bge-small-en-v1.5"
```

The on-disk directory — the one holding `config.json`, `tokenizer.json` and `model.safetensors` — is resolved through a four-step cascade, first match wins:

1. `BRAIN_EMBED_MODEL_DIR`, if set. Must be absolute; a relative value is a configuration error rather than a path resolved against an unpredictable cwd.
2. `[embedder] model` itself, if it starts with `/` — an absolute path literal.
3. `$XDG_DATA_HOME/brain/models/<model>`.
4. `~/.local/share/brain/models/<model>`.

The env override exists so a developer can point at a local checkout without editing TOML; the XDG default is what makes a first run work with no configuration at all. The published container image sets the env var to the bundled copy at `/opt/brain/models/bge-small-en-v1.5` — the model ships **in the image**, so there is no download, no volume and no egress at startup. The GLiNER classifier model ships the same way.

Resolution is path-only: whether the directory exists is the loader's business, which is why a missing model is a clear load failure rather than a config parse error.

The model is loaded at startup. Different configurations produce different fingerprints; existing data must be re-embedded if the model changes. See [`06_migration.md`](06_migration.md).

## 8. The cost of switching

Once a deployment commits to a model, switching has costs:

- **Re-embedding** every stored memory takes proportional time. For 10M memories at 200/s/core CPU: ~14 hours per core, parallelizable.
- **Storage** stays the same (vectors are the same dim if the new model is the same family, different if not).
- **Quality** may improve or degrade; experiment carefully before committing.
- **Operational** disruption: during migration, queries see partial results.

For these reasons, the model choice is a long-term commitment. The default is conservative — `bge-small-en-v1.5` works well across many deployments and migrations are deferred until a real reason to change appears.

## 9. The non-default path: external models via ENCODE_VECTOR_DIRECT

For deployments that want a model Brain doesn't host (e.g., a multi-modal model, a fine-tuned domain model), the protocol exposes `ENCODE_VECTOR_DIRECT` ([04. Wire Protocol](../04_wire_protocol/00_purpose.md) §7.4). The client computes the vector externally and submits it with a fingerprint identifying the model.

This bypasses the embedding layer entirely. Brain stores the vector, indexes it, and respects the fingerprint for cross-model exclusion. Brain doesn't run inference itself.

Use cases:

- Multi-modal: image + text embeddings via CLIP, run client-side.
- Domain-specific: a fine-tuned legal-text model.
- Multilingual: a translation-aware model Brain doesn't ship.

Brain doesn't restrict the model fingerprint, but two consequences:

- The client is responsible for embedding consistency. If they submit vectors from different models with the same fingerprint, Brain can't tell.
- The client misses server-side features like cue caching (the server doesn't see the source text) and re-embedding on model upgrade (the server doesn't have the model).

For most agent applications, the default path (server-owned embedding with `bge-small-en-v1.5`) is the right choice.

---

*Continue to [`02_inference_pipeline.md`](02_inference_pipeline.md) for the tokenizer.*
