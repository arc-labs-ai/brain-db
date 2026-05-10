# 04.03 Inference

The model forward pass — the step that converts token IDs into a vector. This file specifies the inference path: the framework, the device, the runtime characteristics.

## 1. The framework: candle

Brain uses [HuggingFace candle](https://github.com/huggingface/candle) for inference.

From the candle README: "candle is a minimalist ML framework for Rust with a focus on performance (including GPU support) and ease of use."

Why candle:

- **Pure Rust.** No Python bindings, no FFI to Torch or TensorFlow. Fits cleanly with Brain's deployment as a single Rust binary.
- **CPU and GPU support.** Same code path, different devices.
- **HuggingFace ecosystem.** First-class support for HuggingFace model formats (safetensors, GGUF) and tokenizers.
- **Active development.** Actively maintained by HuggingFace.

We considered:

- **PyTorch via [tch-rs](https://github.com/LaurentMazare/tch-rs).** Mature but pulls in libtorch (large C++ dependency).
- **ONNX Runtime via [ort](https://github.com/pykeio/ort).** Good performance, but ONNX as an exchange format adds an extra step.
- **Building inference from scratch.** Not a feasible amount of work.

Candle is the cleanest fit for Brain's "Rust binary, no other runtime dependencies" deployment story.

## 2. Device selection

The embedding layer supports two devices:

### 2.1 CPU

The default. Inference uses candle's optimized CPU kernels. On modern x86_64 with AVX2:

- Per-text inference: 5–10 ms.
- Concurrent inference across cores: scales linearly until memory bandwidth saturates (typically ~16 cores).

Candle uses BLAS where available (via `mkl-sys` or `accelerate` on macOS). For predictable performance in production, we recommend running with [Intel MKL](https://www.intel.com/content/www/us/en/developer/tools/oneapi/onemkl.html) on x86 or BLIS on AMD.

### 2.2 GPU

Optional. Requires:

- A CUDA-capable NVIDIA GPU.
- CUDA drivers installed in the environment.
- Brain compiled with `--features cuda`.

When enabled, inference is dispatched to the GPU. Throughput goes up dramatically (10K+ items/s on an A100) at the cost of:

- Additional memory (GPU VRAM for weights and activations).
- Latency floor for single-item inference (slightly higher than CPU due to kernel launch overhead).
- Operational complexity (drivers, monitoring, GPU sharing if multi-tenant).

GPU is most useful for high-throughput workloads with batching. See [`06_batching_gpu.md`](06_batching_gpu.md).

## 3. The forward pass

For `bge-small-en-v1.5`, the forward pass:

1. **Embeddings layer** — token IDs → 384-dim embeddings, plus position embeddings.
2. **Transformer encoder** — 6 layers of multi-head self-attention + feed-forward.
3. **Pooling** — take the `[CLS]` token's output.
4. **Projection** — final linear layer maps to the 384-dim output space.
5. **Normalization** — applied by the substrate (see [`04_normalization.md`](04_normalization.md)).

The model's weights:

- ~33 million parameters total.
- ~130 MiB at FP32 precision.
- ~33 MiB at INT8 quantization (smaller, slightly less accurate).

We ship FP32 by default. INT8 quantization is a v2 optimization ([01.10 OQ-6](../01_system_architecture/10_open_questions.md)).

## 4. Memory layout

The model's weights are loaded into memory at startup and stay resident. Loading takes 100–500 ms depending on disk speed.

The model's activations (intermediate tensors during the forward pass) are allocated per-call. Candle handles this; it pools allocations for efficiency.

For batched inference (GPU path), the activations scale with batch size. Limits on batch size are governed by available GPU memory.

## 5. Precision

The model runs in FP32 by default. FP16 (half precision) is supported on GPUs that have it; INT8 is supported via separate quantized model files.

The trade-offs:

- **FP32:** baseline. ~130 MiB weights, full precision.
- **FP16:** 65 MiB weights, ~half the memory, slight accuracy loss (typically negligible for embedding tasks).
- **INT8:** 33 MiB weights, more accuracy loss but still acceptable for embedding.

Production deployments may use FP16 on GPU to fit larger batches; INT8 is a future option for very-resource-constrained CPU deployments.

## 6. Latency profile

For a single CPU inference:

- Tokenization: 0.05 ms.
- Model forward pass: 5–10 ms (sequence-length-dependent).
- Normalization: 0.005 ms.
- Total: 5–10 ms.

Variability depends on:

- **Sequence length.** Shorter sequences are faster (fewer attention computations).
- **CPU load.** Concurrent inferences contend for SIMD execution units.
- **Cache state.** First inference after model load is slower due to cold caches; warm steady state is faster.

For p99 latency, the dominant factor is concurrent load. A heavily-loaded server with 16 cores all doing inference will see p99 of 15–25 ms; a lightly-loaded server stays at p50 ≈ 7 ms.

## 7. Concurrency

Multiple Glommio executors can call inference concurrently. Each call runs on the current core. The model's weights are shared across all callers via `Arc<Model>`; the activations are per-call.

The substrate doesn't internally batch CPU inference. Each request goes through the model independently. CPU batching has marginal benefits at small batch sizes and adds complexity; we don't bother.

GPU inference is batched ([`06_batching_gpu.md`](06_batching_gpu.md)). The GPU benefits from larger batches; the substrate gathers in-flight requests within a small time window and submits them together.

## 8. Inference errors

Inference can fail due to:

- **Resource exhaustion** — out of memory (CPU or GPU), too many concurrent calls.
- **Numerical issues** — NaN or Inf in activations (very rare with well-trained models, possible if weights are corrupted).
- **Driver errors** (GPU only) — CUDA errors, GPU resets.

Error handling:

- CPU exhaustion: queue depth check at the embedding-layer entry; reject with `ServiceUnavailable` if queue is full.
- GPU errors: log, fall back to CPU if configured, otherwise return `EmbeddingFailed`.
- Numerical issues: detected after inference (norm check); the embedding is rejected and logged for investigation.

## 9. Loading the model

At startup:

1. Read `config.json` for model architecture parameters.
2. Read `tokenizer.json` for the tokenizer.
3. Load weights from `model.safetensors` (preferred) or `pytorch_model.bin`.
4. Compute the model fingerprint (BLAKE3 over a canonical form of model + tokenizer + version metadata).
5. Initialize the model on the configured device (CPU or GPU).
6. Run a warm-up inference (a few times) to eagerly initialize JITs and caches.

The full startup is typically 1–3 seconds.

## 10. Reload and hot-swap

Model reload (changing the active model) is not supported on a running server in v1. To change the model:

1. Run `ADMIN_MIGRATE_EMBEDDINGS` to re-embed all stored memories with the new model. (Detailed in [`08_migration.md`](08_migration.md).)
2. Update the configuration to point to the new model.
3. Restart the server.

A future v2 feature would support hot-swap: the new model is loaded alongside the old, queries route based on the memory's fingerprint, migration runs in the background. This is non-trivial and deferred.

## 11. Disk format

The substrate prefers `model.safetensors` over `pytorch_model.bin`:

- **safetensors** is HuggingFace's modern weight format. Memory-mapped friendly, no arbitrary code execution risk on load.
- **pytorch_model.bin** is the older PyTorch pickle format. Carries arbitrary code execution risk; we accept it for compatibility but warn at load.

The substrate refuses to load pickle weights from any path other than the configured model directory, and always validates the file's hash against the configured fingerprint at load.

## 12. Deterministic inference

For a given input, the model is deterministic — repeated inference produces the same vector. We rely on this for the cue cache to be useful.

Caveats:

- **CPU vs GPU** may produce slightly different outputs due to different rounding modes. For most retrieval purposes, the difference is negligible (cosine similarity changes in the 6th–8th decimal place).
- **Different CPU instruction sets** (AVX-512 vs AVX2) may produce slightly different outputs. Same negligibility.

We don't try to make CPU and GPU bit-identical. We do treat them as equivalent for the cache (the same fingerprint maps to either device's output; inconsistency between cached and freshly-computed vectors is accepted as noise).

---

*Continue to [`04_normalization.md`](04_normalization.md) for L2 normalization.*
