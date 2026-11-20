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

## 3. Inference takes much longer than expected

**Failure mode.** The model's forward pass runs far past its 5–10 ms envelope — a saturated CPU, or a pathological input.

**Response.** It runs to completion. There is **no per-inference timeout** and no `EmbeddingTimeout`: a forward pass is pure compute with no I/O to hang on, so a slow one is slow rather than stuck, and cancelling it would discard work that was about to finish while leaving the condition that caused the slowness untouched. The pass is on the shard's blocking thread, so the shard's reactor stays schedulable throughout ([`02_inference_pipeline.md`](02_inference_pipeline.md) §19) and the delay is confined to the operation waiting on that embed.

**Detection.** Latency, not an error — the operation's own p99 and `brain_extractor_cycle_duration_seconds` for worker-side embeds. A truly pathological input is bounded another way: sequences are truncated at the model's maximum (§5), so there is no unbounded-length case to time out.

**Operator action.** Treat it as saturation, not as a fault: check whether the shard's CPU is contended, then scale out on the shard axis.

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

> **§6 and §7 describe the GPU path, which is not available.** The model loader refuses any device other than `Device::Cpu` with `EmbedError::UnsupportedDevice` before it does any I/O, so neither failure mode can occur and neither named error exists. They are kept as the shape of that path's failure handling. See [`02_inference_pipeline.md`](02_inference_pipeline.md) §14.2.

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

**Detection.** This is observable, not flagged automatically: `GET /metrics` carries the fingerprint distribution and the "excluded by fingerprint mismatch" rate.

**Response.** Continue serving (queries match the subset that has the new fingerprint, which may be empty if no encodes have happened).

**Operator action.** Converting old-fingerprint memories needs a re-embedding migration, which [`06_migration.md`](06_migration.md) specifies and nothing implements. Until it exists, the way out is to keep the model that wrote the corpus, or to re-ingest.

## 12. Weights absent or in an unsupported format

**Failure mode.** The model directory has no `model.safetensors` — it holds a `pytorch_model.bin`, a custom format, or nothing.

**Detection.** The loader looks for `model.safetensors` by name.

**Response.** Shard spawn fails with `EmbedError::WeightsMissing`, whose message names the directory and states that pickle (`.bin`) weights are refused.

**Operator action.** Convert the model to safetensors. HuggingFace provides conversion tools; the conversion is offline and produces an equivalent file with no execution risk.

## 13. Pickle weights

**Failure mode.** The operator points the model directory at `pytorch_model.bin`, a format whose load executes arbitrary code from the file.

**Response.** It does not load. There is no warning-and-proceed path and no configuration option that permits it — a refusal an operator can switch off is not a mitigation. The failure is §12's `WeightsMissing`: from the loader's point of view, a directory with only pickle weights is a directory with no weights.

**Operator action.** Convert to safetensors before the model reaches Brain.

## 14. Concurrent embedding load

**Failure mode.** More embedding work arrives than the shard can keep up with.

**Response.** Nothing is rejected. There is no embedding queue, no depth limit and no `EmbeddingOverloaded` — concurrency is bounded structurally rather than by admission control. Each shard has one in-flight embed at a time, on its own blocking thread, with the forward pass's fan-out confined to the shard's core ([`02_inference_pipeline.md`](02_inference_pipeline.md) §19). Work past that queues behind the shard's own channel, which is where backpressure already lives, and a saturated shard slows down instead of shedding embeds.

**What this costs.** Latency, visibly: a shard under embedding load serves its own ENCODEs more slowly. What it does not cost is a shard's neighbours — measured p95 scheduling-latency inflation from a concurrent embed is 1.00–1.02 (§19) — or correctness, since no embed is ever dropped for load.

**Operator action.** Scale out. Throughput scales on the shard axis (2.2× from 4 shards to 9 on a 10-CPU box), not by widening one forward pass.

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
