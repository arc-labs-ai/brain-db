# 07.02 Inference Pipeline

Tokenization, model inference, and L2 normalization — the three stages that turn raw text into a stored, query-ready 384-dim unit vector.

## Tokenization

Before text can be embedded, it must be tokenized.

### 1. The tokenizer

`bge-small-en-v1.5` uses the BERT WordPiece tokenizer with the English vocabulary.

- **Vocabulary size:** 30,522 tokens (the standard BERT-base-uncased vocab).
- **Special tokens:** `[CLS]`, `[SEP]`, `[UNK]`, `[PAD]`, `[MASK]`.
- **Casing:** uncased (the model was trained on lowercased text).
- **Implementation:** [HuggingFace `tokenizers`](https://github.com/huggingface/tokenizers), Rust crate.

### 2. The pipeline

Text → tokens:

1. **Normalize** — Unicode NFC normalization, strip accents, lowercase.
2. **Pre-tokenize** — split on whitespace and punctuation.
3. **WordPiece encode** — break each pre-token into vocabulary subwords. `"unaffordable"` → `["un", "##afford", "##able"]`.
4. **Add special tokens** — prepend `[CLS]`, append `[SEP]`.
5. **Truncate** — if longer than max length, truncate to `max_length - 1` tokens then append `[SEP]`.
6. **Pad** — pad to a uniform length within a batch using `[PAD]`.
7. **Output** — token_ids, attention_mask, token_type_ids (always zeros for single-segment inputs).

Steps 1–4 produce a logical sequence; steps 5–6 prepare it for the model.

### 3. The maximum length

The model's training maximum is **512 tokens**. Inputs longer than this are truncated.

For a typical English text:

- 1 token ≈ 0.75 words (rough average).
- 512 tokens ≈ 380 words ≈ 2,000–3,000 characters.

Longer inputs are truncated to 512 tokens. The truncation is right-side (later tokens dropped), which preserves the beginning of the text. For agent memories, this is usually correct — the most-relevant content tends to be near the start (a topic statement, a name, a key fact).

The truncation behavior:

- The cap is a hard limit; longer inputs lose tail content.
- The original text is stored in full; only the embedded vector is computed from the truncated portion.
- A warning may be logged or returned in metadata when truncation happens, so the agent can adjust if it notices.

### 4. Long-text strategies

For texts longer than 512 tokens, the agent has options:

#### 4.1 Truncation (default)

The server truncates and embeds. The vector represents the first ~2,000 characters; the stored text is intact.

#### 4.2 Application-level chunking

The agent splits the long text into chunks before encoding. Each chunk becomes a separate memory. Edges (`PART_OF`) link them to a parent memory representing the whole.

This is recommended for content the agent expects to query later — chunking gives finer-grained recall.

#### 4.3 Application-level summarization

The agent generates a summary (perhaps via the LLM) and encodes the summary as a memory; the full text is stored elsewhere or as an attached resource.

This is recommended for content that's primarily about its gist rather than its details.

Brain doesn't auto-chunk or auto-summarize; that's the application's responsibility. Auto-chunking would impose semantic decisions (where to split) that Brain is not in a position to make correctly.

### 5. Handling unknown characters

WordPiece's `[UNK]` token represents anything the vocabulary can't break down. This typically means:

- Characters outside the model's training distribution (uncommon scripts, emojis).
- Compound rare words.

The model has been trained to handle some `[UNK]` tokens; performance degrades gracefully. For typical agent text (English with occasional unusual content), unknown tokens are rare and not consequential.

If a deployment regularly encodes content with many `[UNK]`s, that's a signal to consider a different model (multilingual BGE, or a model with a richer vocabulary).

### 6. Performance

Tokenization is fast — < 0.1 ms per text in normal cases. The HuggingFace `tokenizers` library is heavily optimized:

- Compiled in Rust (no Python overhead).
- Uses fast string algorithms.
- Releases the GIL when called from Python.

In the embedding layer's latency budget (5–10 ms total CPU), tokenization is < 2% of the cost. Worth getting right but not worth optimizing further.

### 7. Tokenizer configuration

The tokenizer is loaded from a tokenizer.json file in the model directory:

```
/var/brain/models/bge-small-en-v1.5/
├── tokenizer.json       # tokenizer config + vocab
├── config.json          # model config
├── pytorch_model.bin    # or model.safetensors — the weights
```

`tokenizer.json` is loaded at startup and held for the process lifetime. Tokenizer state is read-only and shared across all embedding calls.

### 8. Threading

The HuggingFace tokenizer is thread-safe for encoding. Multiple Glommio executors on different cores can call it concurrently without coordination. There's no per-tokenizer mutex; the tokenizer's internal state is purely read-only at encode time.

### 9. Pre-tokenization considerations

Some preprocessing decisions matter for retrieval quality:

#### 9.1 Lowercasing

The model is uncased; the tokenizer lowercases automatically. Case is not preserved in the vector.

The original text retains case (Brain stores text verbatim). If the agent later wants to display the memory, it sees the original case. Search, however, is case-insensitive at the model level — `"Brain"` and `"brain"` produce identical vectors.

#### 9.2 Whitespace

Multiple consecutive whitespace characters are collapsed to single spaces by the pre-tokenizer. Leading and trailing whitespace is stripped.

#### 9.3 Punctuation

Standard ASCII punctuation is split as separate tokens. Unicode punctuation may be `[UNK]` depending on the character.

#### 9.4 Numbers

Numbers are tokenized as digit sequences. Long numbers may be split into pieces (`"1234567"` → `["1234", "##567"]`).

### 10. The role of [CLS]

BERT-family models output a representation per input token. For embeddings, the convention is to use the `[CLS]` token's output as the sentence representation, possibly with mean-pooling over all tokens.

For `bge-small-en-v1.5`, the official approach is to use the `[CLS]` representation (after a final linear projection). The implementation follows this convention.

### 11. Token-level introspection

Tokenization details are not exposed to clients. The wire protocol carries text, not tokens. Clients can't ask "how was this tokenized?" through the API.

For debugging, operators can use the `ADMIN_TOKENIZE` opcode (admin-only) to test how a specific text tokenizes. This is operationally useful for understanding why a query did or didn't match expected content.

### 12. Vocabulary versioning

The tokenizer's vocabulary is part of the model. Different model versions may have different vocabularies. When the operator changes the model, the tokenizer changes too — and the model fingerprint covers this (the fingerprint hashes the tokenizer config along with the model weights).

A mid-flight tokenizer change (without a model change) is not supported. The tokenizer is loaded at startup and immutable.

### 12a. Asymmetric retrieval — query vs passage

`bge-small-en-v1.5` was trained as an **asymmetric retrieval** model: stored passages are embedded raw, but query text is prepended with a fixed retrieval prefix before tokenization. Skipping the prefix is the single biggest recall-quality regression a deployment can take on short queries ("who is X", "where is Y") — the un-prefixed query vector points at a generic factual centroid rather than a "what looks like a useful passage for this query" centroid.

The prefix is:

```
Represent this sentence for searching relevant passages:
```

(with a trailing space — the prefix is concatenated directly, no separator token).

Brain applies this asymmetry through a dispatcher-level split:

| Code path | Method | Why |
|---|---|---|
| ENCODE memory text | `Dispatcher::embed` | passage — stored vector |
| Entity-create canonical name | `Dispatcher::embed` | passage — stored vector (resolver tier-3 looks up by passage similarity) |
| RECALL cue, PLAN cue, REASON cue | `Dispatcher::embed_query` | query — looked up against passage vectors |
| Retrieval SemanticRetriever query | `Dispatcher::embed_query` | query |
| Statement embed worker | `Dispatcher::embed` | passage — stored vector |

`embed_query` is a default-implemented trait method that concatenates the prefix and forwards to `embed`. Implementations may override the default (for example, a multilingual deployment swapping to a model whose prefix differs, or to no prefix). The model fingerprint covers the choice of model but **not** the prefix; a deployment that overrides the prefix and then changes back without rebuilding the index will see degraded recall until the cue cache cycles. (Operator-visible knob; not a correctness invariant.)

Cue-cache impact: §05's cache is keyed on the input text. The prefix is applied **before** the cache lookup, so query and passage entries for the same surface text are independent cache rows. No new cache machinery is needed.

## Inference

The model forward pass — the step that converts token IDs into a vector.

### 13. The framework: candle

[HuggingFace candle](https://github.com/huggingface/candle) is used for inference.

From the candle README: "candle is a minimalist ML framework for Rust with a focus on performance (including GPU support) and ease of use."

Why candle:

- **Pure Rust.** No Python bindings, no FFI to Torch or TensorFlow. Fits cleanly with deployment as a single Rust binary.
- **CPU and GPU support.** Same code path, different devices.
- **HuggingFace ecosystem.** First-class support for HuggingFace model formats (safetensors, GGUF) and tokenizers.
- **Active development.** Actively maintained by HuggingFace.

Alternatives considered:

- **PyTorch via [tch-rs](https://github.com/LaurentMazare/tch-rs).** Mature but pulls in libtorch (large C++ dependency).
- **ONNX Runtime via [ort](https://github.com/pykeio/ort).** Good performance, but ONNX as an exchange format adds an extra step.
- **Building inference from scratch.** Not a feasible amount of work.

Candle is the cleanest fit for the "Rust binary, no other runtime dependencies" deployment story.

### 14. Device selection

The embedding layer supports two devices:

#### 14.1 CPU

The default. Inference uses candle's optimized CPU kernels. On modern x86_64 with AVX2:

- Per-text inference: 5–10 ms.
- Concurrent inference across cores: scales linearly until memory bandwidth saturates (typically ~16 cores).

Candle uses BLAS where available (via `mkl-sys` or `accelerate` on macOS). For predictable performance in production, running with [Intel MKL](https://www.intel.com/content/www/us/en/developer/tools/oneapi/onemkl.html) on x86 or BLIS on AMD is recommended.

#### 14.2 GPU

Optional. Requires:

- A CUDA-capable NVIDIA GPU.
- CUDA drivers installed in the environment.
- The binary compiled with `--features cuda`.

When enabled, inference is dispatched to the GPU. Throughput goes up dramatically (10K+ items/s on an A100) at the cost of:

- Additional memory (GPU VRAM for weights and activations).
- Latency floor for single-item inference (slightly higher than CPU due to kernel launch overhead).
- Operational complexity (drivers, monitoring, GPU sharing if multi-tenant).

GPU is most useful for high-throughput workloads with batching. See [`04_batching_gpu.md`](04_batching_gpu.md).

### 15. The forward pass

For `bge-small-en-v1.5`, the forward pass:

1. **Embeddings layer** — token IDs → 384-dim embeddings, plus position embeddings.
2. **Transformer encoder** — 6 layers of multi-head self-attention + feed-forward.
3. **Pooling** — take the `[CLS]` token's output.
4. **Projection** — final linear layer maps to the 384-dim output space.
5. **Normalization** — applied next (see the L2 Normalization section below).

The model's weights:

- ~33 million parameters total.
- ~130 MiB at FP32 precision.
- ~33 MiB at INT8 quantization (smaller, slightly less accurate).

FP32 ships by default. INT8 quantization is a future-major-version optimization ([OQ-6 in the open-questions archive](../00_overview/04_open_questions_archive.md#oq-6-vector-quantization)).

### 16. Memory layout

The model's weights are loaded into memory at startup and stay resident. Loading takes 100–500 ms depending on disk speed.

The model's activations (intermediate tensors during the forward pass) are allocated per-call. Candle handles this; it pools allocations for efficiency.

For batched inference (GPU path), the activations scale with batch size. Limits on batch size are governed by available GPU memory.

### 17. Precision

The model runs in FP32 by default. FP16 (half precision) is supported on GPUs that have it; INT8 is supported via separate quantized model files.

The trade-offs:

- **FP32:** baseline. ~130 MiB weights, full precision.
- **FP16:** 65 MiB weights, ~half the memory, slight accuracy loss (typically negligible for embedding tasks).
- **INT8:** 33 MiB weights, more accuracy loss but still acceptable for embedding.

Production deployments may use FP16 on GPU to fit larger batches; INT8 is a future option for very-resource-constrained CPU deployments.

### 18. Latency profile

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

### 19. Concurrency

Multiple Glommio executors can call inference concurrently. Each call runs on the current core. The model's weights are shared across all callers via `Arc<Model>`; the activations are per-call.

CPU inference is not internally batched. Each request goes through the model independently. CPU batching has marginal benefits at small batch sizes and adds complexity; it is not used.

GPU inference is batched ([`04_batching_gpu.md`](04_batching_gpu.md)). The GPU benefits from larger batches; in-flight requests are gathered within a small time window and submitted together.

### 20. Inference errors

Inference can fail due to:

- **Resource exhaustion** — out of memory (CPU or GPU), too many concurrent calls.
- **Numerical issues** — NaN or Inf in activations (very rare with well-trained models, possible if weights are corrupted).
- **Driver errors** (GPU only) — CUDA errors, GPU resets.

Error handling:

- CPU exhaustion: queue depth check at the embedding-layer entry; reject with `ServiceUnavailable` if queue is full.
- GPU errors: log, fall back to CPU if configured, otherwise return `EmbeddingFailed`.
- Numerical issues: detected after inference (norm check); the embedding is rejected and logged for investigation.

### 21. Loading the model

At startup:

1. Read `config.json` for model architecture parameters.
2. Read `tokenizer.json` for the tokenizer.
3. Load weights from `model.safetensors` (preferred) or `pytorch_model.bin`.
4. Compute the model fingerprint (BLAKE3 over a canonical form of model + tokenizer + version metadata).
5. Initialize the model on the configured device (CPU or GPU).
6. Run a warm-up inference (a few times) to eagerly initialize JITs and caches.

The full startup is typically 1–3 seconds.

### 22. Reload and hot-swap

Model reload (changing the active model) is not supported on a running server. To change the model:

1. Run `ADMIN_MIGRATE_EMBEDDINGS` to re-embed all stored memories with the new model. (Detailed in [`06_migration.md`](06_migration.md).)
2. Update the configuration to point to the new model.
3. Restart the server.

A future major version may support hot-swap: the new model is loaded alongside the old, queries route based on the memory's fingerprint, migration runs in the background. This is non-trivial and deferred.

### 23. Disk format

`model.safetensors` is preferred over `pytorch_model.bin`:

- **safetensors** is HuggingFace's modern weight format. Memory-mapped friendly, no arbitrary code execution risk on load.
- **pytorch_model.bin** is the older PyTorch pickle format. Carries arbitrary code execution risk; pickle weights are accepted for users with PyTorch-trained weights but warned at load.

Brain refuses to load pickle weights from any path other than the configured model directory, and always validates the file's hash against the configured fingerprint at load.

### 24. Deterministic inference

For a given input, the model is deterministic — repeated inference produces the same vector. The cue cache depends on this.

Caveats:

- **CPU vs GPU** may produce slightly different outputs due to different rounding modes. For most retrieval purposes, the difference is negligible (cosine similarity changes in the 6th–8th decimal place).
- **Different CPU instruction sets** (AVX-512 vs AVX2) may produce slightly different outputs. Same negligibility.

CPU and GPU are not made bit-identical. They are treated as equivalent for the cache (the same fingerprint maps to either device's output; inconsistency between cached and freshly-computed vectors is accepted as noise).

## L2 Normalization

After the model produces a 384-dim vector, it is normalized to unit L2 norm.

### 25. The operation

For a vector `v = [v_0, v_1, ..., v_383]`:

```
norm = sqrt(sum(v_i² for i in 0..384))
v_normalized = v / norm
```

After normalization:

```
||v_normalized||₂ = 1.0  (within floating-point precision)
```

### 26. Why normalize

#### 26.1 Cosine similarity = dot product

For two unit vectors `a` and `b`:

```
cos_sim(a, b) = (a · b) / (||a|| · ||b||)
              = (a · b) / (1 · 1)
              = a · b
```

The dot product of unit vectors is the cosine similarity. Storing only normalized vectors means similarity is a single SIMD-friendly fused-multiply-add chain — no division at query time, no per-vector norm precomputation.

#### 26.2 Numerical stability

Normalized vectors stay bounded. Intermediate computations (in HNSW search, in attractor dynamics) are easier to reason about when all vectors live on the unit sphere.

#### 26.3 Geometric uniformity

All vectors have the same magnitude. Distance and similarity computations are uniform across the space. Without normalization, vectors with larger magnitudes would dominate similarity scores.

### 27. The model's output

`bge-small-en-v1.5` produces vectors that are *approximately* unit-norm. The model is trained with a normalization-aware loss, so its outputs naturally cluster near the unit sphere.

But "approximately" isn't good enough. The norm typically falls in [0.95, 1.05]. Re-normalization to exactly unit-norm is applied; the dot-product simplification depends on it.

### 28. Implementation

```rust
fn l2_normalize(v: &mut [f32; 384]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();

    if norm < 1e-8 {
        // Pathological: zero vector. Should never happen post-inference,
        // but defensive: leave as-is and let downstream handle.
        return;
    }

    let inv_norm = 1.0 / norm;
    for x in v.iter_mut() {
        *x *= inv_norm;
    }
}
```

SIMD-accelerated versions exist for AVX2 and NEON; they are used on supported architectures. The non-SIMD version is the reference.

Cost: ~50 ns on modern CPU. Negligible compared to the 5–10 ms of inference.

### 29. Pre vs post normalization

A subtle point: should normalization happen before or after the model's projection layer?

The bge-small-en-v1.5 model outputs are already roughly normalized — meaning the projection layer is trained to produce normalizable vectors. Normalization happens **after** the model's output, which:

- Doesn't affect retrieval quality.
- Gives guaranteed unit norm.
- Costs ~50 ns.

Some literature suggests normalizing before downstream operations rather than after the model. For this use case, post-output normalization is the right place — it's the single point where vectors enter Brain's storage and indexing.

### 30. Norm validation on input

For `ENCODE_VECTOR_DIRECT` (clients submitting their own vectors), the norm is validated:

```rust
const NORM_TOLERANCE: f32 = 1e-3;

fn validate_unit_norm(v: &[f32; 384]) -> Result<(), InvalidVector> {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();

    if (norm - 1.0).abs() > NORM_TOLERANCE {
        return Err(InvalidVector::NotNormalized { norm });
    }
    Ok(())
}
```

The tolerance (1e-3) accommodates floating-point precision while catching gross errors (a vector with norm 0.5 or 1.5 is clearly not normalized).

If a client submits a non-unit vector, the request is rejected with `InvalidVector::NotNormalized`. The error includes the actual norm so the client can debug.

Client vectors are not auto-normalized. The reasoning: if the client supplies a non-unit vector, that's a bug in their pipeline, not something to silently fix. The strict policy catches mistakes early.

### 31. Norm validation on read

When a vector is read from the arena (during HNSW search, attractor dynamics, etc.), the norm is optionally re-validated. By default, this is **off** on the hot path — too expensive for every read.

The integrity-check worker periodically scans vectors and validates norms. Vectors with bad norms are flagged for re-embedding. See [15. Background Workers](../15_background_workers/00_purpose.md) §Integrity.

### 32. NaN and Inf handling

Vectors with NaN or Inf elements are invalid. These are detected:

- During normalization: dividing by NaN propagates NaN; the result is rejected.
- On client input (`ENCODE_VECTOR_DIRECT`): explicit check rejects vectors containing NaN/Inf.

Errors return `InvalidVector::ContainsNaN` or `InvalidVector::ContainsInf`.

### 33. Zero vectors

A zero vector (all elements 0.0) has zero norm and is undefined under normalization. Zero vectors are refused:

- **From the model:** if the model emits an exact-zero output, something is wrong. Log and reject.
- **From clients:** `ENCODE_VECTOR_DIRECT` with a zero vector is rejected with `InvalidVector::ZeroVector`.

In practice, well-trained models never emit exact zero vectors for reasonable inputs.

### 34. Norm drift over operations

Some downstream operations (attractor dynamics, VSA bind/bundle) may produce intermediate vectors that aren't unit-norm. This is fine — the operations re-normalize their outputs before using them as queries or storing them.

The invariant: *stored* vectors and *query* vectors are unit-norm. *Intermediate* computations may temporarily produce non-unit vectors.

### 35. Cosine vs Euclidean

Cosine similarity is used throughout. For unit vectors:

```
euclidean_dist(a, b)² = ||a - b||²
                     = ||a||² - 2(a·b) + ||b||²
                     = 1 - 2(a·b) + 1
                     = 2 - 2(a·b)
```

So Euclidean distance² = 2 - 2 × dot product. Sorting by Euclidean distance ascending is equivalent to sorting by dot product descending. The choice between cosine and Euclidean is purely cosmetic for unit vectors.

Cosine similarity (dot product) is used directly. HNSW's hnsw_rs supports it natively.

---

*Continue to [`03_caching.md`](03_caching.md) for the cue cache.*
