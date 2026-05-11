# Sub-task 5.2 — Tokenization

Wraps the HuggingFace `tokenizers` crate (already loaded by 5.1's `ModelHandle`) into a small, single-purpose facade that produces the three tensors BERT needs: `input_ids`, `token_type_ids`, `attention_mask`. Lives in `crates/brain-embed/src/tokenize.rs`.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/02 §1 | BERT WordPiece, uncased, 30 522 vocab; HuggingFace `tokenizers` crate |
| §04/02 §2 | Pipeline: normalize → pre-tokenize → WordPiece encode → add `[CLS]`/`[SEP]` → truncate → pad → output `(input_ids, attention_mask, token_type_ids=0)` |
| §04/02 §3 | **Hard cap 512 tokens**, right-side truncation (drop tail); single-sequence truncation `max_length - 1` then append `[SEP]` |
| §04/02 §3 | Truncation MAY be surfaced as a warning / metadata flag |
| §04/02 §5 | `[UNK]` is acceptable; no special handling required |
| §04/02 §7 | Tokenizer is loaded once at startup, immutable, shared by all calls |
| §04/02 §8 | Tokenizer is thread-safe for encoding — no per-call mutex needed |
| §04/02 §11 | Wire surface does *not* expose tokenization details; `ADMIN_TOKENIZE` opcode is the only inspection surface (deferred to Phase 9) |

The `tokenizers` crate (`Tokenizer::from_file` already wired by 5.1) does steps 1–4 internally — we just need to configure truncation + padding correctly, run `encode_batch` (or `encode`), and surface the three tensors.

## 1. Scope

**In scope for 5.2:**
- `Tokenized` struct: owns the three `Tensor`s plus metadata (`actual_lengths`, `truncated_flags`).
- `encode_single(text: &str, max_length: usize, device: &Device) -> Result<Tokenized, EmbedError>` — single-input path.
- `encode_batch(texts: &[&str], max_length: usize, device: &Device) -> Result<Tokenized, EmbedError>` — batched path; pads to the longest tokenised input in the batch (no shorter, no longer; cap at `max_length`).
- Truncation detection: was the unbounded tokenisation longer than `max_length`? If so, the corresponding `truncated_flags` entry is `true`. Tracing-warn-with-rate-limit per spec §04/02 §3.
- New `EmbedError` variants: `TokenizationFailed(String)`, `TensorBuild(String)`.
- Unit tests in `tokenize.rs`:
  - Tokenizer round-trip on a built-in tiny WordPiece fixture (avoids needing BGE-small for unit tests).
  - Truncation detection.
  - Attention mask correctness (`1` for real tokens, `0` for `[PAD]`).
  - Batch padding (different-length inputs pad to max).
  - `token_type_ids` is all-zero per spec §04/02 §2 step 7.

**NOT in scope (later sub-tasks):**
- Forward pass / pooling / L2-normalise — 5.3.
- Public `Embedder::embed()` API — composed in a later 5.x step.
- `ADMIN_TOKENIZE` opcode — Phase 9.
- Batcher infrastructure — 5.4.
- Cache — 5.5.

## 2. Module surface

```rust
// crates/brain-embed/src/tokenize.rs

use candle_core::{Device, Tensor};
use tokenizers::Tokenizer;

use crate::error::EmbedError;

/// Hard cap from spec §04/02 §3. Operators cannot raise this; raising
/// it would push past the model's training-time max sequence length.
pub const MAX_TOKEN_LENGTH: usize = 512;

/// Tokenised text ready to feed into [`crate::model::ModelHandle::forward`].
/// Tensors live on `device`; all three have shape `(batch, seq_len)`.
#[derive(Debug)]
pub struct Tokenized {
    pub input_ids: Tensor,         // u32, [batch, seq_len]
    pub token_type_ids: Tensor,    // u32, [batch, seq_len] — all zeros (spec §02 §2)
    pub attention_mask: Tensor,    // u32, [batch, seq_len] — 1 for real, 0 for [PAD]
    /// Number of non-pad tokens per row, in batch order.
    pub actual_lengths: Vec<usize>,
    /// `true` iff the corresponding input was tokenised to more than
    /// `MAX_TOKEN_LENGTH` *before* truncation (spec §04/02 §3).
    pub truncated_flags: Vec<bool>,
}

pub fn encode_single(
    tokenizer: &Tokenizer,
    text: &str,
    device: &Device,
) -> Result<Tokenized, EmbedError>;

pub fn encode_batch(
    tokenizer: &Tokenizer,
    texts: &[&str],
    device: &Device,
) -> Result<Tokenized, EmbedError>;
```

Re-export `Tokenized` and `MAX_TOKEN_LENGTH` from `lib.rs`. Keep `encode_*` free functions (no struct around the tokeniser; `ModelHandle::tokenizer()` already owns it).

## 3. Implementation decisions

### 3.1 Detecting truncation

`tokenizers` truncates silently when configured. To detect "would have been longer than 512", we tokenise **without** truncation first, observe the length, then truncate ourselves. Two options:

- **(A) Use `tokenizer.encode(text, true)` with truncation already set on the `Tokenizer`** — fast path but loses the "would-have-been-longer" signal.
- **(B) Encode without truncation, check length, truncate manually if needed.** Costs one extra pass through the WordPiece for the long tail, but the long tail is < 1% of typical inputs (per spec §04/02 §3 "longer texts are truncated").
- **(C) Encode with truncation enabled, then run a cheaper length check on the input** — for *suggesting* truncation, count whitespace-separated words and compare to `512 * 0.75 ≈ 380`. Imprecise; rejected.

**Choice: (B).** The truncation-warning signal matters for the spec §09 failure-mode story (operators need to know when content is being lost). The extra cost is negligible — tokenisation is < 0.1 ms per text (spec §02 §6) and only inputs *over* the cap pay it. We compute `[CLS] + body + [SEP]` lengths ourselves so we don't depend on the tokeniser's internal truncation mode being settable globally.

### 3.2 Tokenizer state mutation

The `tokenizers::Tokenizer` API has `with_truncation` / `with_padding` as `&mut self` methods. We **don't** mutate the tokeniser at encode time — spec §04/02 §7 says the tokeniser is immutable after load; §08 says it's thread-safe at encode time, which only holds if we don't mutate it. Instead, we call `encode` with no truncation/padding configured, then assemble the three tensors ourselves.

The trade-off: we re-implement padding (trivial: pad token id is `0` for BERT-uncased; attention-mask 0; token-type-ids 0) and the special-token sandwich. WordPiece encoding itself is still done by the crate.

Confirmed against spec §04/02 §2: special-token insertion is one of the steps in the pipeline. `tokenizers::Tokenizer::encode(text, add_special_tokens=true)` does this for us — we keep that flag on and just disable truncation/padding.

### 3.3 The `[PAD]` token id

For BERT-uncased the `[PAD]` token id is `0`. We don't hardcode it — fetch it via `tokenizer.token_to_id("[PAD]")`. If absent (corrupt tokeniser?), `EmbedError::TokenizationFailed`.

### 3.4 Tensor dtype

BERT expects `u32` for `input_ids` / `token_type_ids` / `attention_mask`. Spec §04/03 doesn't pin a dtype; candle's `BertModel::forward` signature in 0.8 takes `Tensor` (dtype inferred). We use `DType::U32` consistently with 5.1's `warmup_once`.

### 3.5 Empty-batch / empty-text handling

- `encode_batch(&[])` → `EmbedError::TokenizationFailed("empty batch")`. The forward pass needs at least one row; calling it with zero rows is a programmer error, not a user error.
- `encode_single("")` → tokenises to `[CLS] [SEP]` (length 2). Valid; produces an embedding of the empty string. Tested.

### 3.6 Rate-limiting truncation warnings

Spec §04/02 §3 says "a warning may be logged". On a busy server, every long input would spam logs. Decision: emit `tracing::warn!` per truncated input but at the `target = "brain_embed::truncation"` target so operators can filter; no global rate-limiter at this layer. Phase 11 observability work can add counters/sampling if needed.

### 3.7 Why no `Tokenizer` wrapper struct

The phase doc anticipated a wrapper. The reality is the `tokenizers::Tokenizer` instance already lives on `ModelHandle`; wrapping it in a new struct just to add `encode_single`/`encode_batch` adds an indirection without value. Free functions taking `&Tokenizer` + `&Device` keep the surface tight and easy to test (we can construct a tiny BERT tokeniser in tests without a `ModelHandle`).

### 3.8 Test fixture

The `tokenizers` crate ships with a `Tokenizer::from_pretrained` that downloads — we won't use that (no network in tests). Two test paths:

- **Hand-built BPE tokeniser**: `tokenizers::Tokenizer::new(BertWordPieceTokenizer::new(...))` — viable but verbose.
- **Tiny `tokenizer.json` checked into `tests/fixtures/tokenizer-tiny.json`**: 30-token toy vocab covering `[CLS]`/`[SEP]`/`[PAD]`/`[UNK]` + a handful of WordPiece pieces. Fast, deterministic, no network.

**Choice: tiny tokenizer.json fixture** — gives realistic round-trip semantics for unit tests without requiring BGE-small or network. ~2 KB checked into `tests/fixtures/`.

If building this fixture turns out to be fiddly during implementation, fall back to: skip unit tests that need a real tokeniser, rely on integration test gated on `BRAIN_EMBED_MODEL_DIR`. (This decision moves to the implementation step; surface in the commit.)

## 4. New deps / changes

- **No new workspace deps.** `tokenizers`, `candle-core`, `thiserror` already pulled in by 5.1.
- `EmbedError`: add `TokenizationFailed(String)` and `TensorBuild(String)` variants. Both already implied by 5.1's pattern.

## 5. Files written / changed

```
crates/brain-embed/src/tokenize.rs           [new]
crates/brain-embed/src/error.rs              [edit: add 2 variants]
crates/brain-embed/src/lib.rs                [edit: module decl + re-exports]
crates/brain-embed/tests/fixtures/
    tokenizer-tiny.json                      [new, ~2 KB]   (only if path A in §3.8)
```

No `Cargo.toml` change.

## 6. Verify checklist

- `cargo build -p brain-embed` clean.
- `cargo test -p brain-embed` — 11 existing + ~6 new unit tests + 1 ignored integration.
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-embed` no diff.

## 7. Commit message (draft)

```
feat(brain-embed): WordPiece tokenization with truncation detection (sub-task 5.2)

Wraps the HuggingFace `tokenizers` crate into `encode_single` /
`encode_batch` free functions producing the three tensors BERT needs
(input_ids, token_type_ids=0, attention_mask). Detects pre-truncation
length so operators see a warning when content is being dropped.

- `Tokenized` carries the three Tensors + `actual_lengths` +
  `truncated_flags` per row.
- Truncation cap is `MAX_TOKEN_LENGTH = 512` (spec §04/02 §3, hard).
- Tokeniser state is read-only at encode time per spec §02 §7-§8 — we
  configure truncation/padding *inside* `encode_*`, never mutating the
  shared tokeniser.
- Adds `EmbedError::TokenizationFailed`, `EmbedError::TensorBuild`.
- Tests use a 30-token toy WordPiece tokenizer fixture (~2 KB) so unit
  tests need neither BGE-small nor network.

Verify: cargo build/test/clippy -p brain-embed.
```

## 8. Risks

- **Fixture authorship cost**: if hand-rolling a tiny WordPiece tokenizer.json proves harder than expected (the format is documented but loose), fall back to gating tokenization unit tests behind `BRAIN_EMBED_MODEL_DIR` like the load integration test. Decision happens at implementation time.
- **API drift in `tokenizers` 0.21**: any signature change in `Tokenizer::encode(text, add_special_tokens)` breaks 5.2. Workspace pins `tokenizers = "0.21"`; safe.
- **Tensor dtype mismatch**: 5.1's `warmup_once` uses `DType::U32`; 5.3 will need to match. Pinning here.

## 9. Out-of-scope flags

- No `[CLS]`-vs-mean-pool decision here. That's 5.3 (and the spec is consistent: **`[CLS]` pooling** per §04/02 §10 + §04/03 §6 #3 — the phase-05 orientation plan's mean-pool note was wrong; will correct in 5.3's plan).
- No batching policy here. 5.4 decides how to assemble a batch; 5.2 just consumes a slice it's given.

---

PLAN READY.
