# Phase 5 — Task 5.1: Model loader + fingerprint

**Classification:** moderate. First real code in `brain-embed`. Loads BGE-small from disk via candle-transformers, computes the spec §07 §3 fingerprint, runs a warm-up inference. Pulls in 4 substantial workspace deps (candle-core/-nn/-transformers, tokenizers) but doesn't yet expose the user-facing `embed()` API — that's 5.3.

**Spec:** `spec/04_embedding_layer/01_model_choice.md` (BGE-small properties), `spec/04_embedding_layer/03_inference.md` §1, §5, §9, §11 (candle, FP32, load sequence, safetensors-only), `spec/04_embedding_layer/07_fingerprinting.md` §3 (the fingerprint algorithm, literal).

## 1. Scope

In:

- `crates/brain-embed/Cargo.toml` — enable workspace deps: candle-core, candle-nn, candle-transformers, tokenizers, blake3, parking_lot, tracing, thiserror. Dev: tempfile.
- `crates/brain-embed/src/lib.rs` — module declarations + re-exports (replaces the 25-line stub).
- `crates/brain-embed/src/error.rs` (new) — `EmbedError` enum with model-load variants.
- `crates/brain-embed/src/config.rs` (new) — `EmbedderConfig { model_path, device, dtype, warmup_iters, … }`.
- `crates/brain-embed/src/fingerprint.rs` (new) — `compute_fingerprint(config_bytes, tokenizer_bytes, weights_blake3)` per spec §07 §3. Pure function; no I/O.
- `crates/brain-embed/src/model.rs` (new) — `ModelHandle` struct owning the loaded BERT + tokenizer + fingerprint. `ModelHandle::load(&EmbedderConfig)` is the entry point.

Out (deferred):

- **Public `Embedder` facade** with cache + batcher — 5.3 / 5.4 / 5.5 / `embedder.rs`. 5.1 ships `ModelHandle` as a building block.
- **Tokenization wrappers** — 5.2 wraps the raw `tokenizers::Tokenizer` instance loaded here.
- **Forward pass + pooling** — 5.3. 5.1's warm-up runs a forward pass but throws the result away.
- **GPU device handling** — `EmbedderConfig.device` accepts only `Device::Cpu` in v1; CUDA value rejected.
- **Hot-swap model** — spec §03 §10 says not in v1.

## 2. Spec quotes that bind the design

> **§03 §1:** "Brain uses HuggingFace candle for inference." → candle-core / candle-nn / candle-transformers are the inference layer.
>
> **§03 §5:** "The model runs in FP32 by default." → `DType::F32` for v1.
>
> **§03 §9 (load sequence):**
> 1. Read `config.json` for model architecture parameters.
> 2. Read `tokenizer.json` for the tokenizer.
> 3. Load weights from `model.safetensors` (preferred) or `pytorch_model.bin`.
> 4. Compute the model fingerprint.
> 5. Initialize the model on the configured device.
> 6. Run a warm-up inference.
>
> **§03 §11 (safetensors only):** "The substrate refuses to load pickle weights from any path other than the configured model directory."
>
> **§07 §3 (fingerprint algorithm):**
> ```rust
> let mut hasher = blake3::Hasher::new();
> hasher.update(b"config.json:");
> hasher.update(&config);
> hasher.update(b"tokenizer.json:");
> hasher.update(&tokenizer);
> let weights_hash = blake3_hash_file(model_dir.join("model.safetensors"));
> hasher.update(b"weights:");
> hasher.update(&weights_hash);
> hasher.update(b"vector_dim:");
> hasher.update(&384u32.to_le_bytes());
> hasher.update(b"normalize:");
> hasher.update(&[1u8]);
> let full = hasher.finalize();
> full.as_bytes()[..16].try_into().unwrap()
> ```
> Literal; we implement it byte-for-byte.

## 3. Design decisions

### 3.1 Public surface: `ModelHandle`, not `Embedder` yet

5.1 ships **building blocks**, not the user-facing API. The `Embedder` facade (5.x) composes `ModelHandle` with the tokenizer wrapper (5.2), forward pass (5.3), batcher (5.4), and cache (5.5).

```rust
pub struct ModelHandle {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
    config: bert::Config,                  // parsed config.json
    fingerprint: [u8; 16],
    device: candle_core::Device,
}

impl ModelHandle {
    pub fn load(config: &EmbedderConfig) -> Result<Self, EmbedError>;
    pub fn fingerprint(&self) -> [u8; 16];
    pub fn device(&self) -> &Device;
    // forward(input_ids, attention_mask) — used by 5.3
    pub(crate) fn forward(...) -> Result<Tensor, EmbedError>;
}
```

`forward` is `pub(crate)` for now; 5.3 promotes it (or wraps it) when the pooling layer lands.

### 3.2 `EmbedderConfig`

```rust
pub struct EmbedderConfig {
    /// Directory containing `config.json`, `tokenizer.json`,
    /// `model.safetensors`. Validated at load.
    pub model_path: PathBuf,
    pub device: Device,           // v1: Cpu only
    pub dtype: DType,             // v1: F32 only
    pub warmup_iters: usize,      // default 3
}
```

`device` and `dtype` are present-but-restricted for forward compatibility. A `Device::Cuda(_)` value returns `EmbedError::UnsupportedDevice` at load — the field exists so the GPU sub-task (Phase 5.x / 11+) doesn't have to change the struct shape.

### 3.3 Refuse `.bin` pickle, full stop

Spec §03 §11 says "we accept it for compatibility but warn at load." That's lax. For v1 we **refuse** — only safetensors. The pickle-attack risk in a Rust binary loading arbitrary code from a model file isn't worth the compatibility win.

Spec deviation: the spec wording allows pickle with a warning; we refuse it. **SD-5.1-1** logged: spec § 03 §11 allows pickle-with-warning; we refuse outright as a security-conservative v1 choice. Reconciliation: spec PR to require safetensors-only in v1, with pickle-with-warning relegated to a backward-compat option.

### 3.4 Fingerprint: literal spec §07 §3 algorithm

`compute_fingerprint` takes the three byte slices and the dim/normalize constants. Pure function for testability:

```rust
pub fn compute_fingerprint(
    config_bytes: &[u8],
    tokenizer_bytes: &[u8],
    weights_blake3: &[u8; 32],   // BLAKE3 of weights file (32 raw)
    vector_dim: u32,
    normalize: bool,
) -> [u8; 16];
```

Caller (`ModelHandle::load`) reads the three files, hashes the weights separately, and passes the bytes in. Tests pass canned bytes and validate against pre-computed hashes — no model file required.

Why the literal algorithm: spec §04/07 §4 promises "for a given model directory, the fingerprint is deterministic"; deviation here would invalidate every fingerprint stored in `model_fingerprints` from Phase 3.8.

### 3.5 BLAKE3 of weights via streaming `Hasher::update`, not load-whole-file

The weights file is ~130 MiB. We don't need to load all of it into memory just to hash it. `blake3::Hasher::update_reader(&mut File)` (or read in 64 KiB chunks) hashes streamingly.

### 3.6 Warm-up inference

Spec §03 §9 step 6: "Run a warm-up inference (a few times) to eagerly initialize JITs and caches." Spec §03 §6 quantifies: cold inference is slower than warm. Default 3 warm-up runs on a fixed token sequence (`[101, 119, 102]` — `[CLS] . [SEP]`, the shortest valid input). Results discarded.

### 3.7 Error taxonomy

Single `EmbedError` enum, grown across sub-tasks. 5.1 adds:

```rust
#[derive(thiserror::Error, Debug)]
pub enum EmbedError {
    #[error("model path does not exist or is not a directory: {0}")]
    ModelPathInvalid(PathBuf),

    #[error("config.json missing or unreadable in {dir}: {source}")]
    ConfigRead { dir: PathBuf, #[source] source: std::io::Error },

    #[error("config.json failed to parse: {0}")]
    ConfigParse(String),

    #[error("tokenizer.json missing or unreadable in {dir}: {source}")]
    TokenizerRead { dir: PathBuf, #[source] source: std::io::Error },

    #[error("tokenizer.json failed to load: {0}")]
    TokenizerParse(String),

    #[error("model.safetensors missing in {0}; pickle (.bin) weights are refused")]
    WeightsMissing(PathBuf),

    #[error("weights load failed: {0}")]
    WeightsLoad(String),

    #[error("unsupported device (v1 is CPU-only): {0:?}")]
    UnsupportedDevice(Device),

    #[error("warm-up inference failed: {0}")]
    WarmupFailed(String),
}
```

candle's error types are wrapped via `.map_err(|e| EmbedError::X(e.to_string()))` — candle uses `anyhow::Error` extensively and isn't `#[non_exhaustive]`, so we stringify rather than re-export.

### 3.8 Where the loaded `Tokenizer` lives

`ModelHandle` owns the `tokenizers::Tokenizer`. Sub-task 5.2 will add a thin wrapper module (`tokenize.rs`) that does padding + truncation + attention-mask construction — but the underlying `Tokenizer` instance stays inside `ModelHandle` to avoid two owners of the same object.

5.2 will likely add an accessor: `ModelHandle::tokenizer(&self) -> &Tokenizer` or pass-through encode methods.

### 3.9 `parking_lot` not needed in 5.1

`ModelHandle` is `Send + Sync` once constructed (candle's `BertModel` is). 5.1's `load` is single-threaded by nature. Concurrency arrives in 5.4 (batcher) and 5.5 (cache); `parking_lot` becomes a dep then. Removing the workspace-dep enable here keeps 5.1's manifest minimal.

Actually — `Tokenizer` from the `tokenizers` crate isn't trivially `Sync` last I checked. Will validate during implementation; if it isn't, wrap in `Arc<Mutex<Tokenizer>>` at the `Embedder` facade layer (5.x), not in `ModelHandle`.

## 4. Files touched

- `crates/brain-embed/Cargo.toml` — enable 7 workspace deps + 1 dev-dep (tempfile).
- `crates/brain-embed/src/lib.rs` — replace 25-line stub.
- `crates/brain-embed/src/error.rs` (new) — `EmbedError` enum, ~50 LOC.
- `crates/brain-embed/src/config.rs` (new) — `EmbedderConfig`, ~30 LOC.
- `crates/brain-embed/src/fingerprint.rs` (new) — pure fingerprint function + tests, ~100 LOC.
- `crates/brain-embed/src/model.rs` (new) — `ModelHandle::load` + warm-up + tests, ~150 LOC.
- `docs/spec-deviations.md` — append SD-5.1-1 (refuse pickle weights).

No changes to brain-core / brain-storage / brain-metadata / brain-index.

## 5. Tests (gated `#[cfg(test)]`)

### Unit tests — no model required (8 tests)

In `fingerprint.rs`:

1. **`fingerprint_round_trip_deterministic`** — same inputs → same `[u8; 16]`. Pre-computed expected value pinned (compute once via the test, hard-code in the assert).
2. **`fingerprint_differs_on_config_change`** — flip a byte in `config_bytes`; output differs.
3. **`fingerprint_differs_on_weights_change`** — flip a byte in `weights_blake3`; output differs.
4. **`fingerprint_differs_on_dim_change`** — change `vector_dim` 384 → 768; output differs.
5. **`fingerprint_known_vector`** — feed canned bytes `(b"alpha", b"beta", [0x42; 32], 384, true)`; assert the resulting `[u8; 16]` matches a hardcoded value. Validates byte ordering matches spec §07 §3 literal.

In `model.rs`:

6. **`load_rejects_missing_path`** — `EmbedderConfig.model_path = /nonexistent`; returns `ModelPathInvalid`.
7. **`load_rejects_missing_safetensors`** — point at a tempdir with config.json + tokenizer.json but no safetensors; returns `WeightsMissing`.
8. **`load_rejects_unsupported_device`** — `Device::Cuda(0)` returns `UnsupportedDevice`. Only runs when `Device::Cuda` is constructible (`cfg(feature = "cuda")` not in v1 — actually candle's Device enum is always available; the variant just panics at runtime without cuda. We can construct the variant for the test).

### Integration tests — require `BRAIN_EMBED_MODEL_PATH`

In `tests/load.rs` (new):

9. **`load_real_model_produces_nonzero_fingerprint`** — loads BGE-small from `$BRAIN_EMBED_MODEL_PATH`; fingerprint is not all zeros. Gated by `if std::env::var("BRAIN_EMBED_MODEL_PATH").is_err() { return; }` and a `tracing::info!` "model not available, skipping" log.
10. **`load_real_model_completes_warmup`** — same setup; load completes without `WarmupFailed`. Implicit: warm-up runs as part of `load`, so test #9 covers it. Skip as a separate test; document in test #9.

Total: **8 unit + 1 integration (gated)** = 9 tests. brain-embed test count: 1 → 9.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-embed"
```

Without `BRAIN_EMBED_MODEL_PATH`: 8 unit tests pass; integration test logs the skip + returns. With the env var set: 9 tests pass.

CI doesn't currently have the model; integration test will be a no-op on most runs. The fingerprint algorithm is fully exercised by unit tests (the canned-bytes `fingerprint_known_vector` test catches algorithm regressions).

## 7. Commit

Branch: new `feature/brain-embed` from `main`. AUTONOMY §5:

```
feat(brain-embed): model loader + fingerprint (sub-task 5.1)
```

Body summarises: candle-transformers BERT load, spec §07 §3 fingerprint algorithm (literal byte-for-byte), safetensors-only (SD-5.1-1: refuse pickle outright), warm-up inference, EmbedderConfig with forward-compatible device/dtype fields, 9 tests with model-required ones gated by env var.

## 8. Done when

- [ ] `ModelHandle::load(&EmbedderConfig)` loads a BGE-small directory end-to-end and runs the warm-up.
- [ ] `compute_fingerprint` matches spec §07 §3 byte-for-byte; pinned by `fingerprint_known_vector`.
- [ ] All four load-failure paths return typed errors (missing dir, missing config, missing safetensors, unsupported device).
- [ ] SD-5.1-1 logged in `docs/spec-deviations.md`.
- [ ] 8 unit tests green; integration test passes when `BRAIN_EMBED_MODEL_PATH` is set; clippy `-D warnings` clean workspace-wide.

PLAN READY.
