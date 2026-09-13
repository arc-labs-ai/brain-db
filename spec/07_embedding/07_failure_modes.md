# 07.07 Embedding Layer Failure Modes

What can go wrong in the embedding layer and how Brain responds.

## 1. Model file missing or corrupted

**Failure mode.** At startup, Brain can't find the model files at the configured path, or the files fail integrity checks.

**Detection.** Startup checks:

- File existence (`config.json`, `tokenizer.json`, `model.safetensors`).
- File parsing (config and tokenizer JSON validates).
- Weight file integrity (BLAKE3 hash matches a stored expected hash, if available).

**Response.** Brain refuses to start. The error message identifies the failing file and the expected vs actual state.

**Operator action.** Restore from backup or re-download the model. Verify file permissions.

## 2. Inference NaN / Inf

**Failure mode.** The model produces a vector containing NaN or Inf elements. This is rare with well-trained models and reasonable inputs but can occur with:

- Pathological input text (extremely long after tokenization, or specific adversarial inputs).
- Numerical instability in the model (very rare for production models).
- Hardware faults (cosmic ray bit flips, memory errors).

**Detection.** Post-inference check: `vector.iter().all(|x| x.is_finite())`. NaN or Inf elements fail the check.

**Response.** The encode operation fails with `EmbeddingNumericFailure`. The text is logged (truncated) for diagnostic. Brain doesn't store the bad vector.

**Client action.** Treat as a transient error; retry once. If reproducible, escalate — likely indicates a bug or hardware issue.

## 3. Inference timeout

**Failure mode.** The model's forward pass takes much longer than expected.

**Detection.** A per-inference timeout (default 5 seconds — far longer than any normal inference). Exceeded timeouts cancel the inference.

**Response.** The encode operation fails with `EmbeddingTimeout`. Brain logs the event with the input text length.

**Operator action.** Investigate whether Brain is overloaded (CPU saturated) or whether the model has a pathological case. Adjust the timeout if a deployment legitimately has long-running inference (very long sequences on slow hardware).

## 4. Tokenization produces zero tokens

**Failure mode.** The input text, after tokenization, produces an empty token sequence (excluding special tokens). This can happen if:

- The text contains only whitespace.
- The text contains only characters outside the tokenizer's vocabulary (all `[UNK]`).

**Detection.** Tokenizer output check: `tokens.len() > 0` (excluding special tokens).

**Response.** The encode operation fails with `EmptyTextAfterTokenization`. Brain suggests the text be cleaned or replaced.

**Client action.** Validate input text before encoding. Clients should do basic validation (non-empty after Unicode whitespace trim).

## 5. Tokenization exceeds maximum

**Failure mode.** The input text tokenizes to more than 512 tokens. Not actually a failure — Brain truncates and proceeds.

**Detection.** Tokenizer output check: if `tokens.len() > 512`, truncate.

**Response.** The encode succeeds but with a truncation warning. The metadata indicates that truncation occurred.

**Client action.** For long content, chunk before encoding, or accept the truncation if the lead is what matters.

## 6. GPU OOM

**Failure mode.** GPU memory is insufficient for the requested batch.

**Detection.** CUDA OOM error during inference.

**Response.**

- If `gpu_fallback_cpu = true`: the batch is re-routed to CPU. Latency increases for this batch but operations succeed.
- If `gpu_fallback_cpu = false`: the batch fails with `EmbeddingOOM`.

**Operator action.** Reduce `max_batch_size`, allocate more GPU memory, or switch to a smaller model (FP16 or INT8 quantized).

## 7. GPU driver / CUDA error

**Failure mode.** A CUDA call fails for a reason other than OOM (driver crash, hardware error, kernel mismatch).

**Detection.** CUDA error returned from candle's GPU inference path.

**Response.**

- The current batch fails with `EmbeddingGPUError`.
- A circuit breaker trips: subsequent operations fall back to CPU until the GPU recovers.
- Periodically, Brain retries GPU operations to see if the issue has resolved.

**Operator action.** Investigate the GPU's state. CUDA driver issues often require a host reboot or driver reinstall.

## 8. Cache poisoning

**Failure mode.** A vector cached from inference contains incorrect data — either due to a software bug or hardware fault.

**Detection.** Implicit: Brain validates norm on cached vectors infrequently. Most cache hits are not validated.

**Response.** None directly — Brain trusts cached vectors. If a vector is bad, queries using it produce poor results until the cached entry is evicted.

**Mitigation.** Restart Brain to clear the cache. Restart on schedule or on-demand if poor query quality is observed.

The cache uses bytecount and CRC for integrity in the persistent storage backups; in-memory caches don't have integrity checks (would slow lookups significantly).

## 9. Model load takes too long

**Failure mode.** Loading the model exceeds the startup timeout (default: 60 seconds).

**Detection.** Startup-phase timer.

**Response.** Brain fails to start with `ModelLoadTimeout`. The error message indicates the configured path and the time spent.

**Operator action.** Common causes:

- Slow disk (the model is being read from a slow device).
- Network filesystem (model on NFS or similar).
- Concurrent contention (model load racing with another process).

Move the model to local fast storage; ensure no contention.

## 10. Mismatched config / weights

**Failure mode.** The `config.json` and the weights file are inconsistent — e.g., config says 6 layers but weights have 12.

**Detection.** During candle's model construction, a shape mismatch causes a load error.

**Response.** Startup fails with `ModelConfigMismatch`.

**Operator action.** Verify the model directory contains a consistent set of files. Re-download if necessary.

## 11. Cross-fingerprint query without migration

**Failure mode.** Operator changes the model without migrating, then queries return few or no results.

**Detection.** This is observable, not flagged automatically: the `ADMIN_STATS` shows the fingerprint distribution and a high "excluded by fingerprint mismatch" rate.

**Response.** Continue serving (queries match the subset that has the new fingerprint, which may be empty if no encodes have happened).

**Operator action.** Run `ADMIN_MIGRATE_EMBEDDINGS` to convert old-fingerprint memories to the new fingerprint. See [`06_migration.md`](06_migration.md).

## 12. Unsupported model file format

**Failure mode.** The model files are in a format Brain doesn't support (e.g., an old PyTorch pickle that candle can't load, or a custom format).

**Detection.** Candle's load function returns an error.

**Response.** Startup fails with `UnsupportedModelFormat`.

**Operator action.** Convert the model to a supported format (safetensors). HuggingFace provides conversion tools.

## 13. Pickle weights with security warning

**Failure mode.** The operator configures a `pytorch_model.bin` (pickle format), which has arbitrary code execution risk.

**Detection.** File extension or magic-byte check on load.

**Response.** Brain logs a warning ("loading pickle weights; this format can execute arbitrary code"). It still proceeds with the load. If `embedding.refuse_pickle = true` (a configuration option, default `false`), Brain refuses to load pickle and fails startup with `PickleRefused`.

**Operator action.** Convert to safetensors format. The conversion is offline and produces an equivalent file with no execution risk.

## 14. Concurrent embedding overload

**Failure mode.** Too many concurrent embedding requests; the system can't keep up.

**Detection.** Per-shard queue depth. If a queue exceeds `embed_queue_max` (default 1000), Brain is overloaded.

**Response.** New embedding requests are rejected with `EmbeddingOverloaded`. The operations that already have requests in flight continue to completion.

**Client action.** Back off and retry. Clients should do this with exponential backoff plus jitter.

## 15. Determinism violations

**Failure mode.** The same input produces different vectors on different invocations. This shouldn't happen but could if:

- The model has non-deterministic operations (rare; some attention mechanisms can have it).
- A random seed is changing across calls.

**Detection.** Brain doesn't actively detect this. It would manifest as poor cache hit rates and subtle quality issues.

**Response.** None automatic.

**Operator action.** Verify the model is deterministic (compare vectors across multiple inferences of the same text). If non-deterministic, switch to a different model or a different inference framework.

## 16. Out-of-distribution inputs

**Failure mode.** The agent encodes content very different from the model's training distribution. Vectors may be poorly-distributed in vector space, leading to bad similarity scores.

**Detection.** Brain doesn't detect this directly. Symptoms include poor recall quality and unusual confidence calibration.

**Response.** None automatic.

**Mitigation.** Choose a model that matches the deployment's content distribution. For specialized domains, a domain-specific model.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
