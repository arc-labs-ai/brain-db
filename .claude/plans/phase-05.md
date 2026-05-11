# Phase 5 — Embedding Layer

Orientation plan. Surfaces the spec-grounded decisions before sub-task 5.1's plan goes in. Implementation lives in `crates/brain-embed/` (currently a 25-line stub).

## 0. Goal

`brain-embed` exposes an `Embedder` that takes UTF-8 text and returns a 384-dim L2-normalised `f32` vector deterministically, by running BGE-small-en-v1.5 via candle. Plus an LRU cache for repeat queries and (when GPU is configured) a batcher for high-throughput batched inference. After Phase 5 lands:

- 1K+ texts/sec sustained on the reference CPU (with the cache cold).
- Identical text → identical vector across runs.
- Model fingerprint computable via the spec §04/07 §3 algorithm.

Tag: `phase-5-complete`.

## 1. Spec grounding (13 files)

| Spec § | Topic | Sub-task anchor |
|---|---|---|
| 00 Purpose | substrate owns embedding; clients send text | read first |
| **01 Model choice** | **`bge-small-en-v1.5`, 384-dim FP32 ~130 MiB, MIT, English** | 5.1 |
| 02 Tokenization | BERT WordPiece, max 512 tokens, padding + attention mask | 5.2 |
| 03 Inference | candle framework, CPU default, 5–10 ms/item, FP32, deterministic on CPU | 5.3 |
| 04 Normalization | mean-pool over token embeddings + L2 normalise to unit vector | 5.3 |
| **05 Caching** | **LRU keyed by BLAKE3-16(text), 10K-entry default, fingerprint-aware invalidation** | 5.5 |
| 06 Batching + GPU | 2 ms window, max batch 64, **CPU has no internal batching** (spec §03 §7) | 5.4 (GPU path); CPU passthrough |
| **07 Fingerprinting** | **16-byte BLAKE3 over config + tokenizer + weights + (dim, normalize)** | 5.1 / 5.6 |
| 08 Migration | `ADMIN_MIGRATE_EMBEDDINGS` re-embeds; out of v1 5.x scope (Phase 9) | n/a |
| 09 Failure modes | OOM, NaN/Inf detection, GPU faults | inline in 5.3 |
| 10 Open questions | quantization, fine-tuning hooks, multi-modal | n/a |

## 2. Crate-level structure

```
crates/brain-embed/
├── Cargo.toml          (add: candle-core, candle-nn, candle-transformers,
│                              tokenizers, blake3, parking_lot, tracing;
│                              dev: criterion, tempfile)
└── src/
    ├── lib.rs          (re-exports + Embedder facade)
    ├── config.rs       (EmbedderConfig: model_path, max_batch, cache_size, …)
    ├── model.rs        (ModelHandle: loads safetensors, computes fingerprint)
    ├── tokenize.rs     (Tokenizer wrapper: truncate + pad + attention mask)
    ├── forward.rs      (forward pass + mean pool + L2 normalise)
    ├── batcher.rs      (GPU-only batching window; CPU bypass)
    ├── cache.rs        (LRU + fingerprint-validated invalidation)
    ├── fingerprint.rs  (BLAKE3 over canonical model identity per spec §07 §3)
    └── embedder.rs     (top-level Embedder API: text → vector with cache)
```

Plus `benches/throughput.rs` (5.7) and `tests/determinism.rs` (5.6).

## 3. Cross-crate boundaries

`brain-embed` is a **service crate**, not a closed leaf. It:

- **Depends on**: `brain-core` for `MemoryId`-adjacent types (the fingerprint is a `[u8; 16]` byte array; doesn't need a new type yet). Candle and tokenizers are pure-Rust workspace deps; no FFI cascade.
- **Does NOT depend on**: brain-storage, brain-metadata, brain-index. The embedder is a pure function (text → vector) plus a side-effecting cache. The Phase 7 ops crate composes it with the storage stack.
- **Consumed by**: Phase 7 cognitive operations (ENCODE/RECALL embed before storing/searching). Phase 9 server is the network surface.

The model fingerprint produced here is what brain-metadata's `model_fingerprints` table stores (already implemented in Phase 3.8 with `ModelInfo`). The integration happens in Phase 7.

## 4. Design decisions to surface before 5.1

### 4.1 Model file shipping

BGE-small weights are ~130 MiB FP32. Spec §03 §11 says safetensors-only (the `.bin` pickle format is refused as a security risk). The model can't be in git.

**Options:**
- (A) Operator downloads out-of-band; configure a path. Tests use a tiny fixture or skip when env var unset.
- (B) Auto-download from HuggingFace on first startup. Operationally simpler but adds a HTTP dep + license/audit complications.
- (C) Bundle the model as a separate `brain-embed-model` crate published via crates.io. Cleanest for production; tooling-heavy.

**Recommendation: (A).** Spec §01 §7 explicitly describes `model_path` as a config knob; matches operator-control philosophy. Tests use a `BRAIN_EMBED_MODEL_PATH` env var; if unset, integration tests `#[ignore]`. Phase doc already calls this out ("model file is large; don't check in").

### 4.2 GPU support in v1?

Spec §06 covers GPU batching; CLAUDE.md §6 approved `candle-core` (which supports CUDA). Phase doc says "configurable via `--features cuda`".

**Options:**
- (A) CPU-only in v1; GPU deferred. Simpler; ships faster.
- (B) Feature-gated GPU from 5.1 onward. Adds CI matrix complexity.
- (C) GPU-only in v1. Spec rejects this — CPU is the default per §02 §1.

**Recommendation: (A) with feature plumbing.** Cargo feature `cuda` exists in 5.1's manifest but doesn't enable anything until a future Phase 5.x or Phase 11+. The batcher (5.4) ships disabled on CPU — the spec is explicit (§03 §7: "the substrate doesn't internally batch CPU inference"). Phase 5 scope = make 1K texts/s on CPU work cleanly.

### 4.3 Phase doc vs spec contradiction on CPU batching

**Spec §03 §7:** "The substrate doesn't internally batch CPU inference. Each request goes through the model independently."

**Phase doc §5.4:** "Channel-fed batcher: collect requests for up to `batch_window_ms` (e.g. 5ms) or until `batch_size` (e.g. 32)" — implies CPU batching.

Spec wins. **Resolution:** 5.4 implements the batcher infrastructure (channel-fed, oneshot per request) but it's a passthrough on the CPU path — each request goes through the model immediately. The same code activates batching when the GPU path lands (5.x or Phase 11+).

Document the divergence in the 5.4 plan. Phase doc gets a one-line update.

### 4.4 Sync vs async API

Spec §03 §7 says "multiple Glommio executors can call inference concurrently." That implies async API for the Phase 9 server. brain-embed itself can ship sync (each call blocks on inference) and let Phase 9 wrap it in `spawn_blocking` or equivalent.

**Recommendation: sync API in v1.** Specifically:
```rust
impl Embedder {
    pub fn embed(&self, text: &str) -> Result<[f32; 384], EmbedError>;
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; 384]>, EmbedError>;
}
```

The cache, fingerprint computation, and inference are all CPU-bound; no async benefit at this layer. The Phase 9 server wraps `embed` in a blocking-pool dispatch if it wants. If GPU + batching land later, the batcher provides its own async surface (oneshot channels per request) without changing this API.

### 4.5 Test fixture for the model

Tests can't load a 130 MiB model on every run. Three approaches:

- (A) `BRAIN_EMBED_MODEL_PATH` env var — `#[ignore]` integration tests if unset.
- (B) Tiny BERT fixture (~1 MiB) checked into git as test data. Architecturally identical so the code paths are exercised; vectors won't match BGE-small but determinism/round-trip works.
- (C) Mock `Embedder` trait that returns hashes-as-vectors for unit tests; real model only at integration scope.

**Recommendation: (A) + (C).** Unit tests use a `Embedder` trait + mock implementation (deterministic hash → vector for testing the cache, fingerprint, batcher state machine). Integration tests (`tests/`) take the env var; CI sets it. Deferred to operator: the actual BGE-small inference correctness is validated by the determinism test (5.6) — that test gates on the model being available.

### 4.6 Determinism caveats

Spec §03 §12 says CPU output is deterministic per (input, instruction set) but **CPU ≠ GPU bit-identical**, and **AVX-512 ≠ AVX2 bit-identical**. The cache treats them as equivalent.

**Decision for 5.6:** the determinism test asserts that the same input on the same machine produces bit-identical output across 100 runs. Cross-instruction-set determinism is out of scope. Document this caveat in the test.

### 4.7 LRU cache key collision

Spec §05 §2 truncates BLAKE3 to 16 bytes. Collision probability at 10⁶ entries: ~10⁻¹⁹. Acceptable per spec. We trust the truncation.

### 4.8 Fingerprint computation

Spec §07 §3 spells out the algorithm exactly:
```
BLAKE3(b"config.json:" + config_bytes
     + b"tokenizer.json:" + tokenizer_bytes
     + b"weights:" + BLAKE3(weights_file_bytes)
     + b"vector_dim:" + 384u32.to_le_bytes()
     + b"normalize:" + [1u8])  // truncated to 16 bytes
```

This is mechanical; 5.1 implements it directly. The brain-metadata `model_fingerprints` table from Phase 3.8 stores it.

## 5. The 7 sub-tasks (re-confirmed against spec)

| # | Title | Spec anchor | Notes |
|---|---|---|---|
| 5.1 | Model loader + fingerprint | §01, §03, §07 §3 | Load safetensors via candle-transformers BERT; compute fingerprint per §07 §3; warm-up inference; refuse `.bin` pickle |
| 5.2 | Tokenization | §02 | tokenizers crate; truncate to 512; build attention mask; pad to batch max |
| 5.3 | Forward pass + pooling + normalize | §03, §04 | candle forward pass; **mean-pool**, not [CLS]; L2 normalise (spec §04). Phase doc said "[CLS] token's output" — spec §04 says mean-pool. Spec wins; phase doc gets corrected |
| 5.4 | Batcher infrastructure | §06 (mostly future) | Channel + oneshot per request. CPU path is passthrough (spec §03 §7); GPU path enables batching window when `--features cuda` is set (deferred to v1.x or Phase 11+) |
| 5.5 | LRU cache | §05 | `lru` crate (new workspace dep) keyed by `[u8; 16]` BLAKE3-truncated hash; values `(vector, fingerprint, inserted_at)`; fingerprint mismatch → discard + re-compute |
| 5.6 | Determinism test | §03 §12 | Same input → bit-identical output across 100 runs on the same machine; integration test, gated on `BRAIN_EMBED_MODEL_PATH` |
| 5.7 | Throughput benchmark | §16/03 | criterion bench measuring texts/sec at default config (cache cold); ≥ 1K target, best-effort on the dev hardware |

**Spec deviations expected:**
- **Phase doc said [CLS] pooling; spec says mean-pool.** Phase doc is wrong; 5.3 follows spec. No SD needed — implementation is spec-faithful.
- **Phase doc said 5ms / 32 batch on CPU; spec says no CPU batching.** Phase doc is wrong; 5.4 follows spec. No SD needed.
- **No new SDs expected** unless candle's API surprises us mid-implementation.

## 6. New dependencies

Already in workspace `[workspace.dependencies]`:
- `candle-core = "0.8"`, `candle-nn = "0.8"`, `candle-transformers = "0.8"`
- `tokenizers = "0.21"`
- `blake3 = "1"` (used by fingerprint + cache key)
- `parking_lot = "0.12"` (already used by brain-index)
- `tracing = "0.1"`
- (dev) `criterion = "0.5"`, `tempfile = "3"`

**New at workspace level:**
- `lru = "0.12"` (or `0.13`) — for the LRU cache (5.5). Small, well-maintained, pure-Rust. Spec §05's `LruCache<TextHash, CachedEmbedding>` shape.

That's the only net-new dep. Will surface in the 5.5 plan.

## 7. Phase exit criteria

- [ ] Sub-tasks 5.1–5.7 ✅.
- [ ] `just verify` green (with brain-embed now active in the workspace).
- [ ] Determinism test passes (`BRAIN_EMBED_MODEL_PATH` available in CI).
- [ ] Throughput baseline recorded — ≥ 1K texts/sec on the reference CPU; best-effort on dev hardware with the number noted.
- [ ] Fingerprint computation matches a Python reference implementation byte-for-byte (sanity check during 5.1).
- [ ] Tag `phase-5-complete`.

## 8. Open items for the user before 5.1

Three calls worth confirming up front:

1. **Model file approach:** env-var path + `#[ignore]` integration tests (recommended) vs auto-download vs bundled crate?
2. **GPU scope:** CPU-only in v1 (recommended) vs feature-gated CUDA from 5.1?
3. **Embedder API shape:** sync `embed(&str) -> Result<[f32; 384], _>` (recommended) vs async from day 1?

After confirmation, sub-task 5.1's plan goes in next.

PLAN READY.
