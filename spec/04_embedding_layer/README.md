# 04. Embedding Layer

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the embedding layer; SDK authors needing to understand model semantics |
| Voice | Hybrid (rationale + normative) |
| Depends on | [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/) |
| Referenced by | [05. Storage: Arena & WAL](../05_storage_arena_wal/), [09. Cognitive Operations](../09_cognitive_operations/), [15. Failure Modes + Recovery](../15_failure_recovery/) |

## What this spec defines

Layer L2 of the architecture — the layer that converts text into vectors. It defines:

- The chosen model (`bge-small-en-v1.5`) and the alternatives considered.
- The tokenization pipeline.
- The inference path: CPU and (optional) GPU.
- The LRU cache that absorbs repeated cues.
- The model fingerprint and how it propagates through the data model.
- The model migration procedure for upgrading the embedding model.

The embedding layer is the substrate's most ML-native component. The choices here have outsized impact on the system's quality and operational characteristics.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_model_choice.md`](01_model_choice.md) | Why bge-small-en-v1.5; rejected alternatives |
| [`02_tokenization.md`](02_tokenization.md) | The WordPiece tokenizer and its constraints |
| [`03_inference.md`](03_inference.md) | The candle-based inference path |
| [`04_normalization.md`](04_normalization.md) | L2 normalization and its consequences |
| [`05_caching.md`](05_caching.md) | The LRU cue cache |
| [`06_batching_gpu.md`](06_batching_gpu.md) | GPU batching path |
| [`07_fingerprinting.md`](07_fingerprinting.md) | The model fingerprint and how it propagates |
| [`08_migration.md`](08_migration.md) | The migration procedure for model upgrades |
| [`09_failure_modes.md`](09_failure_modes.md) | What can go wrong; detection and response |
| [`10_open_questions.md`](10_open_questions.md) | Unresolved questions |
| [`11_references.md`](11_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
