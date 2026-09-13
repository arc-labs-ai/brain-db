# 07. Embedding Layer

> **TL;DR.** Brain owns the embedding model. Clients send text; the server runs BGE-small-en-v1.5 via candle to produce 384-dim L2-normalized vectors. Tokenization, inference, normalization, and an LRU cue cache live here, plus the model fingerprint that gates cross-model comparison and the migration procedure for switching models. Server-owned embedding gives semantic dedup, deterministic recall, cue caching, and per-deployment model lock-in that BYO-vector designs cannot match. The CPU model forward pass dominates request latency.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the embedding layer; client implementers needing to understand model semantics |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md), [02. Data Model](../02_data_model/00_purpose.md) |
| Referenced by | [08. Storage](../08_storage/00_purpose.md), [05. Operations](../05_operations/00_purpose.md), [18. Failure Recovery](../18_failure_recovery/00_purpose.md) |

## What this spec defines

Layer L2 of the architecture (see [01.04](../01_architecture/04_layers.md)) — the component responsible for converting text into vectors, and the only layer that does machine learning work. It defines:

- The chosen model (`bge-small-en-v1.5`) and the alternatives considered.
- The tokenization pipeline.
- The inference path: CPU and (optional) GPU.
- The LRU cache that absorbs repeated cues.
- The model fingerprint and how it propagates through the data model.
- The model migration procedure for upgrading the embedding model.

The embedding layer is Brain's most ML-native component. The choices here have outsized impact on the system's quality and operational characteristics.

## What this document covers

- Why Brain owns embedding, rather than accepting pre-computed vectors. ([§ 4 below](#4-why-brain-owns-embedding))
- The chosen model and the alternatives rejected. ([`01_model_choice.md`](01_model_choice.md))
- Tokenization and its bounds. ([`02_inference_pipeline.md`](02_inference_pipeline.md))
- The inference path. ([`02_inference_pipeline.md`](02_inference_pipeline.md))
- L2 normalization and what it gives Brain. ([`02_inference_pipeline.md`](02_inference_pipeline.md))
- The LRU cache that absorbs repeated cues. ([`03_caching.md`](03_caching.md))
- The optional GPU batching path. ([`04_batching_gpu.md`](04_batching_gpu.md))
- The model fingerprint as a versioning mechanism. ([`05_fingerprinting.md`](05_fingerprinting.md))
- The model migration procedure. ([`06_migration.md`](06_migration.md))

## What this document does not cover

- **The vector storage layout.** Defined in [08. Storage](../08_storage/00_purpose.md).
- **How vectors are searched.** Defined in [09. Indexing](../09_indexing/00_purpose.md).
- **How vectors are used in operations.** Defined in [05. Operations](../05_operations/00_purpose.md).
- **The wire-protocol shape of ENCODE.** Defined in [04. Wire Protocol](../04_wire_protocol/00_purpose.md) §07.

## 1. The role of the embedding layer

Brain accepts text from agents and stores memories that can be queried by similarity. To enable similarity search, text must be mapped to a vector space where distance approximates semantic relatedness. This is the embedding layer's job.

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
    /// Used internally for GPU batching; clients don't have a multi-embed.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError>;
}
```

The trait abstracts model identity from callers: nothing outside this layer knows which model is in use, except through the fingerprint.

## 4. Why Brain owns embedding

[01.04 §L2](../01_architecture/04_layers.md#l2-embedding-layer) summarizes this; here's the long version.

Server-owned embedding gives Brain five capabilities that are difficult or impossible if the client supplied vectors:

### 4.1 Semantic deduplication

When Brain embeds, it can detect that two different texts produced near-identical vectors and either merge them or warn the agent. With client-supplied vectors, Brain has no view into "is this the same content I've seen before".

### 4.2 Automatic re-embedding on model upgrade

When the operator changes the embedding model, Brain can re-embed all stored content. With client-supplied vectors, Brain would have to ask each client to re-encode every memory — a coordination problem that doesn't have a clean solution.

### 4.3 Cue caching

Brain caches embedded cues. Frequent queries (and especially repeated queries from the same agent within a session) skip inference entirely. Client-supplied vectors don't get this benefit.

### 4.4 Per-deployment model lock-in

The operator chooses the model. Agents using Brain get the operator's choice; they don't have to coordinate model versions or worry about cross-model incompatibility within a deployment.

### 4.5 Embedding correctness

Brain verifies that vectors are well-formed (correct dimensionality, finite, normalized). With client-supplied vectors, ill-formed inputs would be Brain's problem to detect or tolerate.

### 4.6 The cost

Brain has to host an ML inference workload. This is non-trivial:

- The model has weights (typically 30–500 MiB) that need to be loaded and resident.
- Inference uses CPU or GPU, both of which need to be available.
- The model's quality directly affects retrieval quality; choosing the model is a decision with cognitive consequences.

Brain accepts this cost. The capabilities §4.1–4.5 outweigh the operational complexity.

## 5. The escape hatch

For deployments that genuinely need to bring their own vectors — domain-specific or multi-modal models that the operator can't or doesn't want to host — Brain provides `ENCODE_VECTOR_DIRECT`. The protocol carries a vector along with a model fingerprint; Brain stores it as-is.

This is detailed in [04. Wire Protocol](../04_wire_protocol/00_purpose.md) §07.4 and [OQ-5 in the open-questions archive](../00_overview/04_open_questions_archive.md#oq-5-external-vector-ingestion). It exists to support domain-specific or multi-modal vectors the operator cannot host inside Brain; it is not the default path.

## 6. Position in the architecture

The embedding layer sits between the connection layer (L1) and the planner (L3):

- Receives text from L1 as part of `ENCODE`, `RECALL`, `PLAN`, `REASON` requests.
- Returns the vector plus the model fingerprint.
- The planner / executors below use the vector for ANN search, attractor dynamics, etc.

The layer has its own configuration, its own error space, and its own observability. It's a clean module, replaceable in principle (e.g., for a different model family) without changing the layers above or below it.

---

*Continue to [`01_model_choice.md`](01_model_choice.md) for the model selection rationale.*
