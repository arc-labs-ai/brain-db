# brain-embed

> Built-in embedding service (BGE-small) for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The substrate owns the embedding model — clients send text, the server embeds.
This crate runs BGE-small-en-v1.5 (the default) via `candle`, tokenizes input,
runs the forward pass, and returns a 384-dim L2-normalised `f32` vector. It also
computes a fingerprint over the model weights + tokenizer + config bytes so
storage can detect cross-model byte drift, and offers a caching dispatcher to
avoid re-embedding identical text.

## Key modules

| Module | Role |
|---|---|
| `model` | Inference pipeline: model load, tokenize, forward/pooling, L2-normalise. Exposes `ModelHandle`, `embed_text`/`embed_batch`, `VECTOR_DIM`, `MAX_TOKEN_LENGTH`. |
| `dispatcher` | Caller-facing surface: `Dispatcher` trait, `CpuDispatcher`, `CachingDispatcher` (LRU), `BGE_QUERY_PREFIX`. |
| `config` | `EmbedderConfig` — model path, device, dtype, warm-up. |
| `fingerprint` | `compute_fingerprint` + blake3 helpers over weights/tokenizer/config. |
| `error` | `EmbedError` taxonomy. |

## Where it fits

Built on `candle-core`/`candle-nn`/`candle-transformers` + `tokenizers`, with
`lru` for the embedding cache and `blake3` for fingerprints. It is a leaf
dependency of the read/write path: `brain-planner`, `brain-ops`, and
`brain-workers` all hold a `Dispatcher` to turn cue/memory text into vectors.

## Spec

- [`../../spec/07_embedding/00_purpose.md`](../../spec/07_embedding/00_purpose.md)
- [`../../spec/07_embedding/02_inference_pipeline.md`](../../spec/07_embedding/02_inference_pipeline.md)
- [`../../spec/07_embedding/05_fingerprinting.md`](../../spec/07_embedding/05_fingerprinting.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
