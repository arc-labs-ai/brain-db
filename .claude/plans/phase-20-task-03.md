# 20.3 — Classifier framework + bundled NER

`ClassifierExtractor: Extractor` powered by an operator-provided
BERT-style token-classification model loaded via candle, matching
`brain-embed`'s `EmbedderConfig` operator surface.

## Decision: operator-provided, not binary-bundled

Per the conversation with the user, route 1 was selected for
optimal production behavior. After surveying the prior precedent
(`brain-embed::ModelHandle::load` taking a `model_path: PathBuf`
from `EmbedderConfig`), the **production-correct** flavour of
route 1 is operator-provided weights — no `include_bytes!`, no
Cargo feature gate, no licensing in the substrate.

Rationale:
- Matches the substrate's existing model-control surface 1:1.
  Operators already know how to wire `model_path` for the
  embedder; the classifier reuses the same pattern.
- Zero binary inflation. The 25–250 MB NER weights stay outside
  the binary; deployments without classifier extractors pay
  nothing.
- Operators control licensing + model provenance.
- Phase 22+ can layer Cargo-feature-gated bundling on top
  without changing this code.

When `model_path` is unset or points at a missing / corrupt
directory, the classifier registers in a **degraded state** —
every dispatch writes a `Failure { reason: "classifier model not
loaded" }` audit row and returns zero items. ENCODE never fails.

A companion doc `crates/brain-extractors/docs/bundled-ner.md`
shows operators how to download + convert
`dslim/bert-base-NER` (Apache-2.0) to candle safetensors.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-extractors/src/classifier.rs` | `ClassifierConfig`, `ClassifierModel` trait, `BertTokenClassifier` impl, `ClassifierExtractor`. |
| `crates/brain-extractors/src/lib.rs` | Add `pub mod classifier;` + re-exports. |
| `crates/brain-extractors/Cargo.toml` | Add `candle-core` / `candle-nn` / `candle-transformers` / `tokenizers` deps. |
| `crates/brain-extractors/docs/bundled-ner.md` | Operator setup guide for downloading + converting dslim/bert-base-NER. |
| `crates/brain-extractors/src/labels.rs` | BIO label decoder (B-PER / I-PER → PER span). |

## Public surface

```rust
#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Directory containing config.json / tokenizer.json /
    /// model.safetensors / labels.txt. Unset on substrate-only
    /// deployments that don't use classifier extractors.
    pub model_path: Option<PathBuf>,
    pub device: Device,                // v1: Cpu only.
    pub dtype: DType,                  // v1: F32 only.
    pub max_seq_len: usize,            // default 256.
    pub warmup_iters: usize,           // default 1.
}

pub trait ClassifierModel: Send + Sync {
    fn predict(&self, text: &str) -> Result<Vec<TokenClassification>, ExtractorError>;
    /// Pinned model identifier (file BLAKE3 fingerprint hex,
    /// truncated to 16 bytes). Bumps when weights change.
    fn version(&self) -> &str;
}

pub struct TokenClassification {
    pub label: String,                 // "PER", "ORG", "LOC", or ""
    pub text: String,                  // the merged span
    pub start: usize,                  // byte offset in original text
    pub end: usize,
    pub confidence: f32,
}

pub struct BertTokenClassifier { ... }

impl BertTokenClassifier {
    pub fn load(config: &ClassifierConfig) -> Result<Self, ExtractorError>;
}

pub struct ClassifierExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    model: Arc<dyn ClassifierModel>,
    confidence_threshold: f32,
    /// `None` when the model failed to load. Dispatches return
    /// `Failure(reason: "classifier model not loaded")`.
    loaded: bool,
}

impl ClassifierExtractor {
    pub fn new(...) -> Self;
    pub fn degraded(id, name, target, version, threshold, reason) -> Self;
}

impl Extractor for ClassifierExtractor { ... }
```

## Inference path

1. Tokenize via the `tokenizers` crate; cap at `max_seq_len`.
2. Forward through `BertModel` (candle-transformers); take hidden states.
3. Apply a linear classifier head loaded from
   `classifier.weight` / `classifier.bias` in `model.safetensors`.
4. Argmax per token → label index → label string via `labels.txt`.
5. BIO decoder collapses `B-PER I-PER I-PER` sequences into one
   `TokenClassification { label: "PER", start, end, text }`.
6. Confidence is the softmax probability of the dominant label.
7. Compare to `confidence_threshold`; below threshold → skip.

Projection per `target`:
- `Entity { entity_type }` — emit one `EntityMention` per detected
  span. Entity type qname is the extractor's declared target; the
  resolver decides whether `PER` / `ORG` / `LOC` label matches.
  v1 phase 20 only projects spans whose label appears in the
  extractor's declared scope (e.g., `target: entity Person` matches
  `PER` only).
- `Statement` / `Relation` / `EntityOrStatement` — phase 22+. v1
  emits no items (writes Skipped audit).

## Determinism

Per spec §22/02 §2:
- Single-threaded inference: `candle_core::Device::Cpu` only;
  rayon is implicitly off in the BERT forward we call.
- F32 dtype only.
- Pinned tokenizer (file hash in `version()`).
- Pinned weights (same).
- No seed needed — argmax is deterministic.

## Operator setup doc

`docs/bundled-ner.md` covers:
- Download `dslim/bert-base-NER` from Hugging Face.
- Convert PyTorch weights to safetensors via
  `python -m safetensors.convert ...`.
- Drop `config.json`, `tokenizer.json`, `model.safetensors`,
  `labels.txt` (one label per line) into `${BRAIN_MODELS}/ner/`.
- Set `BRAIN_NER_MODEL_PATH=${BRAIN_MODELS}/ner` in deployment env.
- Verify with `cargo run -p brain-cli -- ner-probe "Alice met Bob"`
  (CLI command added in phase 22+ admin).

## Tests

`classifier.rs` unit tests cover the **framework path**, not real
inference (no model fixture in-repo):

1. `config_default_disables_classifier` — `ClassifierConfig::default()`
   has `model_path: None`.
2. `load_returns_error_when_path_is_none` — explicit signal that
   no model is configured.
3. `load_returns_error_when_directory_missing` — bad path → clear error.
4. `load_returns_error_when_required_files_missing` — missing
   tokenizer / config / weights.
5. `degraded_extractor_dispatch_writes_failure` — `ClassifierExtractor::
   degraded(...)` returns `Failure` status with reason on every
   `run()`.
6. `degraded_extractor_returns_zero_items`.
7. `bio_decoder_collapses_per_span` — labels.rs unit test for
   `B-PER I-PER I-PER → "PER"`.
8. `bio_decoder_handles_o_label` — `O` between two `B-PER` spans
   produces two separate detections.
9. `bio_decoder_handles_bare_b_tag` — `B-PER` followed by `O`
   produces a one-token span.
10. `labels_load_from_text_file` — `labels.txt` reader.

Real model loading is exercised by an **operator-side smoke test**
(phase 20.9) when `BRAIN_NER_MODEL_PATH` is set in CI; absent that
env var the test is `#[ignore]`d.

## Out of scope

- The actual model conversion script — operator-side, documented.
- GPU / FP16 / INT8 — same as brain-embed v1.
- Multi-shard model load coordination — operator ensures `model_path`
  is on a shared FS or symlinked per shard.
- ENCODE wiring — phase 20.6.
- System schema integration — phase 20.7.

## Single commit

`feat(extractors): 20.3 — classifier framework + operator-provided NER`

## Verification

```
cargo test -p brain-extractors
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
