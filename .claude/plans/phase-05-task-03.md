# Sub-task 5.3 — Forward pass + `[CLS]` pooling + L2 normalise

Wires together what 5.1 (model) and 5.2 (tokenisation) produced: takes the loaded `ModelHandle` and `Tokenized` tensors, runs the BERT forward pass, extracts the `[CLS]` representation, L2-normalises, and returns a `Vec<[f32; 384]>` (or a single `[f32; 384]` for the single-input path).

After 5.3 the embedder produces vectors. Phase 5's "Embedder facade" still doesn't exist (that's later); 5.3 only ships the pure forward-+-pool-+-normalise pipeline.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/03 §3 | Forward pass: embeddings → 6-layer encoder → **`[CLS]` pooling** → final 384-dim projection |
| §04/03 §3 step 5 | Normalisation is the **substrate's** responsibility, not the model's |
| §04/02 §10 | "For `bge-small-en-v1.5`, the official approach is to use the `[CLS]` representation". Phase doc's mean-pool note was wrong — we follow spec |
| §04/04 §1 | `norm = sqrt(Σ v_i²); v_normalized = v / norm` |
| §04/04 §3 | Model outputs are *approximately* unit-norm (norm ∈ [0.95, 1.05]); we re-normalise to exact unit norm |
| §04/04 §4 | Reference implementation + 1e-8 guard against pathological zero norms |
| §04/04 §8 | NaN/Inf → reject (don't propagate) |
| §04/04 §9 | Zero vector from the model → reject (well-trained models never emit this) |
| §04/03 §8 | Inference can fail; numerical issues detected post-inference |

## 1. Scope

**In scope for 5.3:**
- `forward_pooled(handle: &ModelHandle, tokens: &Tokenized) -> Result<Vec<[f32; 384]>, EmbedError>` — batched forward + CLS pool + L2 normalise + NaN/Inf/zero check.
- `embed_text(handle: &ModelHandle, text: &str) -> Result<[f32; 384], EmbedError>` — convenience: tokenise + forward + pool + normalise, single text.
- `embed_batch(handle: &ModelHandle, texts: &[&str]) -> Result<Vec<[f32; 384]>, EmbedError>` — batched variant.
- `l2_normalize_in_place(v: &mut [f32; 384])` — pure scalar reference implementation per spec §04/04 §4.
- Two new `EmbedError` variants: `NumericFailure(String)` (NaN/Inf/zero), `OutputDimMismatch { expected, got }`.
- Unit tests for the pure helpers (`l2_normalize_in_place`, edge cases).
- An integration test gated on `BRAIN_EMBED_MODEL_DIR` that runs the full pipeline on a real BGE-small instance and checks: shape, unit norm, a pinned similarity property (identical text on consecutive calls → cosine ≈ 1.0).

**NOT in scope (later sub-tasks):**
- `Embedder` facade with cache — 5.5.
- Batcher with windowing — 5.4.
- Determinism test across 100 runs — 5.6.
- Throughput benchmark — 5.7.
- SIMD-accelerated L2 normalise — spec §04/04 §4 mentions SIMD variants exist; reference scalar is what ships. Optimise later if benches demand it.

## 2. Module surface

```rust
// crates/brain-embed/src/forward.rs

use crate::{
    error::EmbedError,
    model::ModelHandle,
    tokenize::Tokenized,
};

/// Output dimensionality. Pinned at 384 for v1 (BGE-small).
pub const VECTOR_DIM: usize = 384;

/// L2-normalise in place. Reference scalar implementation per spec
/// §04/04 §4. Returns the original norm so callers can decide whether
/// to reject (e.g. zero-vector check).
pub fn l2_normalize_in_place(v: &mut [f32; VECTOR_DIM]) -> f32;

/// Run the forward pass on already-tokenised input, extract `[CLS]`,
/// L2-normalise, and return one vector per batch row.
///
/// Rejects rows whose norm is below 1e-8 (zero-vector defence) or
/// whose values contain NaN / Inf.
pub fn forward_pooled(
    handle: &ModelHandle,
    tokens: &Tokenized,
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;

/// Convenience: tokenise + forward + pool + normalise, single text.
pub fn embed_text(
    handle: &ModelHandle,
    text: &str,
) -> Result<[f32; VECTOR_DIM], EmbedError>;

/// Convenience: tokenise + forward + pool + normalise, batch.
pub fn embed_batch(
    handle: &ModelHandle,
    texts: &[&str],
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;
```

Re-exports from `lib.rs`: `forward_pooled`, `embed_text`, `embed_batch`, `l2_normalize_in_place`, `VECTOR_DIM`.

Promote `ModelHandle::forward` from `pub(crate)` to truly `pub(crate)` (it already is) — internal call only.

## 3. Implementation decisions

### 3.1 `[CLS]` extraction from BERT last-hidden-state

`candle_transformers::models::bert::BertModel::forward` returns a tensor of shape `(batch, seq_len, hidden_dim)` (= `(B, L, 384)` for BGE-small). The `[CLS]` token is at sequence position 0 (we put it there in 5.2). Two extraction options:

- **(A)** `t.narrow(1, 0, 1)?.squeeze(1)?` — narrow seq-axis to position 0, then squeeze. Standard candle idiom.
- **(B)** `t.i((.., 0, ..))?` — candle's `IndexOp` macro. Equivalent; less verbose.

**Choice: (A) `narrow + squeeze`.** Explicit and visible; matches what a reader of the spec would write by hand. Both compile to the same kernel.

After extraction we have `(B, 384)`. Convert to `Vec<Vec<f32>>` via `tensor.to_vec2::<f32>()`, then validate + normalise per row.

### 3.2 BGE doesn't apply a pooler MLP

Some BERT models have a `pooler.dense` MLP applied to the CLS token. `bge-small-en-v1.5` does **not** — its sentence-card on HuggingFace says "CLS pooling, no pooler dense, normalize". `candle_transformers`' `BertModel` excludes the pooler by default (the `BertPooler` is a separate type). We do not apply any post-CLS MLP. Last-hidden-state at position 0 is the embedding.

This is a subtle but important detail: a Python reference using `transformers.AutoModel` for BGE produces the same numbers iff the user takes `last_hidden_state[:, 0]` directly, not `pooler_output`. We pin this expectation in a doc comment on `forward_pooled` and validate via the integration test.

### 3.3 Floating-point determinism caveat

Spec §04/03 §12 admits CPU forward is deterministic *for fixed instruction set + fixed input* but not across instruction sets. 5.6 lands the actual determinism test; 5.3 just needs to **not introduce** non-determinism. Things to avoid:

- No `f32::partial_cmp` over tensor data (we don't sort).
- No parallel reduction with non-associative summation choice (candle handles this; we don't touch it).
- L2 normalise uses a fixed left-to-right summation (`v.iter().map(|x| x*x).sum::<f32>()` — Rust's `Iterator::sum` for `f32` is left-fold).

### 3.4 The zero-vector / NaN / Inf guards

Per spec §04/04 §4 + §8 + §9, post-forward we must reject pathological outputs. Order of checks (cheapest first):

1. After CLS extraction, iterate each row's 384 floats once:
   - Track running sum of squares.
   - Branchless `is_finite` check (`!x.is_finite()` → NaN or Inf).
2. If any element is non-finite → `EmbedError::NumericFailure("non-finite output element")`.
3. If `norm_sq.sqrt() < 1e-8` → `EmbedError::NumericFailure("zero-norm output")`.
4. Otherwise multiply by `1.0 / norm` in place.

One pass for the finite check; a second pass for the multiply. Could fuse but the speed of a 384-element loop is irrelevant compared to the ~5-10 ms forward pass (spec §04/03 §6).

### 3.5 Output dim sanity check

`BertModel::forward` returns whatever shape its config says. BGE-small's `config.json` says `hidden_size: 384`. If we ever load a different model (say someone points `model_path` at BGE-base = 768), we want to fail loudly, not silently truncate. Decision: at the start of `forward_pooled`, check `tensor.dim(D::Minus1)? == VECTOR_DIM`; if not, `EmbedError::OutputDimMismatch { expected: 384, got: <n> }`.

This is the place that catches "operator pointed at the wrong model" most cheaply. The model fingerprint check is a *strong* version (covers more), but it requires consulting `model_fingerprints` from brain-metadata — that comes in Phase 7 when ops compose. Dim check at this layer is the minimum.

### 3.6 Attention mask handling

`BertModel::forward(input_ids, token_type_ids, attention_mask: Option<&Tensor>)` accepts an optional attention mask. We always pass it (`Some(&tokens.attention_mask)`) — without it the model attends to `[PAD]` positions, contaminating the `[CLS]` output for short rows in a mixed-length batch.

This is the one place 5.2's `attention_mask` actually matters; the warm-up in 5.1 doesn't need it because seq_len = 2 with no padding.

### 3.7 Why `Vec<[f32; 384]>` and not `ndarray::Array2<f32>` or a flat `Vec<f32>`

Returning `Vec<[f32; 384]>` matches the rest of Brain — `MemoryId`-adjacent storage, the HNSW index in brain-index, and the arena slot layout all use fixed-size 384-element arrays. Plus `[f32; 384]` is `Copy`-cheap-ish (it's not Copy, but moving is a 1.5 KB memcpy) and the type carries the dimension at compile time.

For batches the allocation cost is `batch * 1.5 KB`; at batch=64 that's 96 KB — fine.

### 3.8 NaN-detection cost

`f32::is_finite` is one branchless instruction per element (`fcmp` + extracted exception bit). 384 elements × batch is in the microseconds. Cheaper than running the model again to debug a bad vector.

### 3.9 Error variant naming

Choosing `NumericFailure` (rather than `BadOutput` or `NaN` or `ZeroVector`) so the one variant covers all three pathological cases (NaN, Inf, zero norm). The string payload says which. Keeps the public surface small.

### 3.10 Where the new code lives

`crates/brain-embed/src/forward.rs` — new file. Public functions; private helpers (`extract_cls`, `validate_and_normalise_row`).

5.1's `model.rs` already has `ModelHandle::forward(input_ids, token_type_ids, attention_mask: Option<&Tensor>) -> Result<Tensor, EmbedError>`. `forward_pooled` calls it.

### 3.11 Test strategy

**Pure-Rust unit tests (always run):**
- `l2_normalize_in_place` zeros out a unit vector → unchanged within ε.
- `l2_normalize_in_place` on a vector with norm 2 → norm becomes 1.
- `l2_normalize_in_place` on near-zero vector → returns small norm, doesn't divide by zero.
- (No `forward_pooled` unit test because BertModel needs real weights.)

**Integration test gated on `BRAIN_EMBED_MODEL_DIR`** (`tests/forward.rs`):
- Load real model.
- `embed_text("hello world")` → 384-dim, unit norm (within 1e-5), no NaN.
- `embed_text("hello world")` twice → cosine similarity = 1.0 (within 1e-6).
- `embed_batch(["hello", "world"])` → two vectors, each unit norm.
- (Optional, depending on disk space) pin one vector's first 8 floats against a known reference. Deferred to 5.6's determinism test — it's the right place.

### 3.12 Risk: candle 0.8's `IndexOp` / `narrow` surface

If `narrow(1, 0, 1)` followed by `squeeze(1)` doesn't compile or returns wrong shape on the candle version we have, fall back to `t.i((.., 0, ..))`. Verified by build during implementation; surfaces in the commit if option (A) fails.

## 4. Files written / changed

```
crates/brain-embed/src/forward.rs            [new]
crates/brain-embed/src/error.rs              [edit: + NumericFailure, + OutputDimMismatch]
crates/brain-embed/src/lib.rs                [edit: mod + re-exports]
crates/brain-embed/tests/forward.rs          [new — gated on BRAIN_EMBED_MODEL_DIR]
```

No `Cargo.toml` change. No new workspace deps.

## 5. Verify checklist

- `cargo build -p brain-embed` clean.
- `cargo test -p brain-embed` — existing 19 + ~4 new unit tests + 1 ignored integration.
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-embed` no diff.
- If `BRAIN_EMBED_MODEL_DIR` is set, `cargo test -p brain-embed --test forward` exercises the full pipeline (validates the BGE-small inference path end-to-end).

## 6. Commit message (draft)

```
feat(brain-embed): forward pass + CLS pooling + L2 normalise (sub-task 5.3)

Composes 5.1's ModelHandle with 5.2's Tokenized to produce
substrate-owned 384-dim L2-normalised f32 vectors.

- forward_pooled(handle, tokens) -> Vec<[f32; 384]> — runs BertModel
  forward with the attention mask, extracts [CLS] at seq position 0,
  validates each row (NaN/Inf/zero-norm rejected), then L2-normalises
  per spec §04/04 §4.
- embed_text / embed_batch — tokenise + forward + pool + normalise
  convenience entry points (no cache, no batcher; those land in 5.4
  and 5.5).
- VECTOR_DIM = 384 pinned at the crate boundary; OutputDimMismatch
  fails loudly if the loaded model has a different hidden_size (catches
  "operator pointed at the wrong model" cheaply at this layer).
- BGE-small uses CLS pooling with NO pooler MLP; spec §04/03 §3 +
  §04/02 §10. (Phase doc's mean-pool note was wrong; spec wins.)
- Adds EmbedError::NumericFailure (NaN/Inf/zero-norm) and
  EmbedError::OutputDimMismatch.
- Integration test in tests/forward.rs gated on BRAIN_EMBED_MODEL_DIR
  validates: shape, unit norm, repeatability (same text → cosine = 1).

Verify: cargo build/test/clippy -p brain-embed.
```

## 7. Risks

- **candle BERT pooler behaviour**: §3.2 — we rely on `BertModel` NOT applying the pooler MLP. If candle's default does apply it, we'd silently produce non-BGE-spec vectors. Mitigation: integration test against a Python reference value (deferred to 5.6); structural test that the output norm matches expectations (BGE pre-norm is in [0.95, 1.05] per spec §04/04 §3) — we can assert this in the integration test and fail noisily if it isn't.
- **Tensor shape surprises**: if `BertModel::forward` returns `(seq, batch, hidden)` instead of `(batch, seq, hidden)` (axis order varies by framework), the CLS extraction grabs the wrong row. Mitigation: integration test asserts `tensor.dims() == (batch, seq, 384)` before extraction.
- **No CPU SIMD norm**: spec §04/04 §4 says "SIMD-accelerated versions exist for AVX2 and NEON". We ship the scalar reference for now. If 5.7's throughput bench shows the norm is a bottleneck (extremely unlikely at ~50 ns vs ~5-10 ms of forward), revisit.

## 8. Out-of-scope

- No determinism guarantee yet — that's 5.6.
- No throughput optimisation — that's 5.7.
- No cache — that's 5.5.
- No batching window — that's 5.4 (and on CPU it's a passthrough per spec §04/03 §7 anyway).

---

PLAN READY.
