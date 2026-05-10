# 04.00 Purpose

This document defines Brain's embedding layer — the component responsible for converting text into vectors. It's the second layer in the architecture (L2 in [01.04](../01_system_architecture/04_layers.md)) and the only layer that does machine learning work.

## What this document covers

- Why the substrate owns embedding, rather than accepting pre-computed vectors. ([§ 4 below](#4-why-the-substrate-owns-embedding))
- The chosen model and the alternatives rejected. ([`01_model_choice.md`](01_model_choice.md))
- Tokenization and its bounds. ([`02_tokenization.md`](02_tokenization.md))
- The inference path. ([`03_inference.md`](03_inference.md))
- L2 normalization and what it gives us. ([`04_normalization.md`](04_normalization.md))
- The LRU cache that absorbs repeated cues. ([`05_caching.md`](05_caching.md))
- The optional GPU batching path. ([`06_batching_gpu.md`](06_batching_gpu.md))
- The model fingerprint as a versioning mechanism. ([`07_fingerprinting.md`](07_fingerprinting.md))
- The model migration procedure. ([`08_migration.md`](08_migration.md))

## What this document does not cover

- **The vector storage layout.** Defined in [05. Storage: Arena & WAL](../05_storage_arena_wal/).
- **How vectors are searched.** Defined in [06. ANN Index](../06_ann_index/).
- **How vectors are used in cognitive operations.** Defined in [09. Cognitive Operations](../09_cognitive_operations/).
- **The wire-protocol shape of ENCODE.** Defined in [03. Wire Protocol](../03_wire_protocol/) §07.

## 1. The role of the embedding layer

The substrate accepts text from agents and stores memories that can be queried by similarity. To enable similarity search, text must be mapped to a vector space where distance approximates semantic relatedness. This is the embedding layer's job.

The pipeline:

```
text → tokenizer → token_ids → model → raw_vector → L2 normalize → vector
```

Every memory's vector goes through this pipeline. Every cue for `RECALL`/`PLAN`/`REASON` goes through it too. The layer is on the hot path for nearly every operation.

## 2. Latency budget

CPU inference dominates request latency:

- Tokenization: < 0.1 ms.
- Model forward pass: 5–10 ms.
- L2 normalization: < 0.01 ms.

Cache hits skip the model: a HashMap lookup keyed by text-hash is < 0.001 ms.

For a system targeting p99 < 25 ms on `ENCODE`, this layer's 5–10 ms is the dominant component of latency. Optimizations here move the user-perceived needle; optimizations elsewhere are noise by comparison.

## 3. The interface

The embedding layer's public interface:

```rust
trait EmbeddingProvider {
    /// The active model's fingerprint.
    fn fingerprint(&self) -> ModelFingerprint;

    /// The vector dimensionality (384 for bge-small-en-v1.5).
    fn dim(&self) -> usize;

    /// Embed a single text into a normalized vector.
    /// Returns the vector and a hit/miss indicator (for metrics).
    async fn embed(&self, text: &str) -> Result<(Vector, CacheState), EmbedError>;

    /// Embed multiple texts in a batch.
    /// Used internally for GPU batching; SDKs don't have a multi-embed.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError>;
}
```

The trait abstracts model identity from callers: nothing outside this layer knows which model is in use, except through the fingerprint.

## 4. Why the substrate owns embedding

[01.04 §L2](../01_system_architecture/04_layers.md#l2-embedding-layer) summarizes this; here's the long version.

Substrate-owned embedding gives us five capabilities that are difficult or impossible if the client supplied vectors:

### 4.1 Semantic deduplication

When the substrate embeds, it can detect that two different texts produced near-identical vectors and either merge them or warn the agent. With client-supplied vectors, the substrate has no view into "is this the same content I've seen before".

### 4.2 Automatic re-embedding on model upgrade

When the operator changes the embedding model, the substrate can re-embed all stored content. With client-supplied vectors, the substrate would have to ask each client to re-encode every memory — a coordination problem that doesn't have a clean solution.

### 4.3 Cue caching

The substrate caches embedded cues. Frequent queries (and especially repeated queries from the same agent within a session) skip inference entirely. Client-supplied vectors don't get this benefit.

### 4.4 Per-deployment model lock-in

The operator chooses the model. Agents using the substrate get the operator's choice; they don't have to coordinate model versions or worry about cross-model incompatibility within a deployment.

### 4.5 Embedding correctness

The substrate verifies that vectors are well-formed (correct dimensionality, finite, normalized). With client-supplied vectors, ill-formed inputs would be the substrate's problem to detect or tolerate.

### 4.6 The cost

The substrate has to host an ML inference workload. This is non-trivial:

- The model has weights (typically 30–500 MiB) that need to be loaded and resident.
- Inference uses CPU or GPU, both of which need to be available.
- The model's quality directly affects retrieval quality; choosing the model is a decision with cognitive consequences.

We accept this cost. The capabilities §4.1–4.5 outweigh the operational complexity.

## 5. The escape hatch

For deployments that genuinely need to bring their own vectors — domain-specific or multi-modal models that the operator can't or doesn't want to host — Brain provides `ENCODE_VECTOR_DIRECT`. The protocol carries a vector along with a model fingerprint; the substrate stores it as-is.

This is detailed in [03. Wire Protocol](../03_wire_protocol/) §07.4 and [01.10 OQ-5](../01_system_architecture/10_open_questions.md). It exists for compatibility but is not the default path.

## 6. Position in the architecture

The embedding layer sits between the connection layer (L1) and the planner (L3):

- Receives text from L1 as part of `ENCODE`, `RECALL`, `PLAN`, `REASON` requests.
- Returns the vector plus the model fingerprint.
- The planner / executors below use the vector for ANN search, attractor dynamics, etc.

The layer has its own configuration, its own error space, and its own observability. It's a clean module, replaceable in principle (e.g., for a different model family) without changing the layers above or below it.

---

*Continue to [`01_model_choice.md`](01_model_choice.md) for the model selection rationale.*
