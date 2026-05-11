# Phase 5 — Embedding Layer

## Goal

Substrate-owned embedding: clients send text, server embeds. Load BGE-small via candle, batch concurrent requests, cache results. After this phase, you can take a string and get a deterministic 384-dim vector at ≥ 1K texts/sec on the reference machine.

## Prerequisites

- [x] Phase 4 complete.

## Reading list

1. [`spec/04_embedding_layer/00_purpose.md`](../../spec/04_embedding_layer/00_purpose.md)
2. [`spec/04_embedding_layer/01_model_choice.md`](../../spec/04_embedding_layer/01_model_choice.md) — **why BGE-small.**
3. [`spec/04_embedding_layer/02_tokenization.md`](../../spec/04_embedding_layer/02_tokenization.md)
4. [`spec/04_embedding_layer/03_inference.md`](../../spec/04_embedding_layer/03_inference.md)
5. [`spec/04_embedding_layer/04_normalization.md`](../../spec/04_embedding_layer/04_normalization.md) — mean pooling + L2 normalize.
6. [`spec/04_embedding_layer/06_batching_gpu.md`](../../spec/04_embedding_layer/06_batching_gpu.md)
7. [`spec/04_embedding_layer/05_caching.md`](../../spec/04_embedding_layer/05_caching.md)
8. [`spec/04_embedding_layer/07_fingerprinting.md`](../../spec/04_embedding_layer/07_fingerprinting.md)
9. [`spec/04_embedding_layer/08_migration.md`](../../spec/04_embedding_layer/08_migration.md)

## Outputs

- `crates/brain-embed` exports `Embedder`, `EmbedderConfig`.
- BGE-small loads from a local model file (downloaded out-of-band, NOT at runtime).
- 1K texts/sec sustained.
- Cache hit on identical strings.
- Tag: `phase-5-complete`.

## Sub-tasks

### Task 5.1 — Model loader (BGE-small)
**Reads:** `spec/04_embedding_layer/08_migration.md`
**Writes:** `crates/brain-embed/src/model.rs`
**What to build:**
- Load tokenizer (HuggingFace tokenizers crate).
- Load model weights (candle-transformers BERT).
- The model file path is configured (no auto-download — that's an operational concern).
**Done when:** A test loads a checked-in tiny test model fixture and embeds "hello world" to a non-trivial vector.

### Task 5.2 — Tokenization
**Reads:** `spec/04_embedding_layer/02_tokenization.md`
**Writes:** `crates/brain-embed/src/tokenize.rs`
**Done when:** Truncation at max_length (512); padding for batches; attention masks correct.

### Task 5.3 — Forward pass + pooling
**Reads:** `spec/04_embedding_layer/03_inference.md`, `spec/04_embedding_layer/04_normalization.md`
**Writes:** `crates/brain-embed/src/forward.rs`
**Done when:** Mean-pooled output matches a reference implementation (e.g. sentence-transformers in Python) to within numerical noise (ε = 1e-4).

### Task 5.4 — Batching window
**Reads:** `spec/04_embedding_layer/06_batching_gpu.md`
**Writes:** `crates/brain-embed/src/batcher.rs`
**What to build:**
- Channel-fed batcher: collect requests for up to `batch_window_ms` (e.g. 5ms) or until `batch_size` (e.g. 32), then dispatch as one forward pass.
- Each request gets a oneshot channel to receive its result.
**Done when:** Concurrent embed calls amortize forward-pass cost; per-call latency ≤ 1.5x the batched per-text latency.

### Task 5.5 — LRU cache (text → vector)
**Reads:** `spec/04_embedding_layer/05_caching.md`
**Writes:** `crates/brain-embed/src/cache.rs`
**Done when:** Identical text returns cached vector; cache size configurable; eviction is LRU.

### Task 5.6 — Determinism test
**Reads:** `spec/04_embedding_layer/07_fingerprinting.md`
**Writes:** `crates/brain-embed/tests/determinism.rs`
**Done when:** The same input produces bit-identical output across 100 runs. (Numerical determinism may require pinning candle's matmul; document if not.)

### Task 5.7 — Throughput benchmark
**Reads:** `spec/16_benchmarks_acceptance/03_throughput_targets.md`
**Writes:** `crates/brain-embed/benches/throughput.rs`
**Done when:** ≥ 1K texts/sec sustained on reference hardware (best-effort if not available; record baseline).

## Phase exit checklist

- [x] Sub-tasks 5.1–5.7 complete.
- [x] `cargo test -p brain-embed` green (53 passed; integration tests gated on `BRAIN_EMBED_MODEL_DIR` skip cleanly without it).
- [x] Determinism test wired (`tests/determinism.rs`, 5 properties; gated on env var).
- [x] Throughput bench wired with hand-timed 1 000/s floor assert (`benches/throughput.rs`; gated on env var).
- [x] Tag `phase-5-complete`.

Sub-task 5.4 ships the dispatch *surface* (`Dispatcher` trait + `CpuDispatcher` passthrough) rather than the GPU window-and-batch machinery the original sketch implied — spec §04/03 §7 + §14 are explicit that CPU has no internal batching. The window+batch design is reserved for a future GPU sub-task behind the same trait.

Spec deviations logged in `docs/spec-deviations.md`:
- SD-5.1-1: refuse `pytorch_model.bin` outright (arbitrary-code-execution risk).
- SD-5.1-2: safetensors loaded via the safe full-file loader to preserve `#![forbid(unsafe_code)]` in `brain-embed`.

## Notes

The model file is large (~150 MB for BGE-small). Don't check it into git. Document the download/install process in `docs/notes/embedding-model-install.md`.
