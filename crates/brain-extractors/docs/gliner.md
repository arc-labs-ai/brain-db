# GLiNER v2.1 inference module

Standalone, zero-shot named-entity recognition. The module lives at
`crates/brain-extractors/src/gliner/` and is not yet wired into the
classifier extractor — the follow-up "swap" slice will land that.

## Architecture

GLiNER v2.1 (`urchade/gliner_small-v2.1`) is a DeBERTa-v3-small
backbone (hidden = 768, 6 encoder layers, 128K SentencePiece vocab)
combined with two head networks:

1. **Span-representation head (`markerV0`)**
   - `project_start` : `Linear(768, 3072) -> ReLU -> Linear(3072, 768)`
   - `project_end`   : `Linear(768, 3072) -> ReLU -> Linear(3072, 768)`
   - `out_project`   : `Linear(1536, 6144) -> ReLU -> Linear(6144, 512)`
2. **Label-projection MLP**
   - `Linear(768, 3072) -> ReLU -> Linear(3072, 512)`

The backbone consumes:

```text
[CLS] [ENT] label_1 [ENT] label_2 ... [ENT] label_K [SEP] word_tokens [SEP]
```

`[ENT]` is a tokenizer addition (vocab row appended at conversion
time) and acts as a marker the head can pool to obtain a per-label
embedding. Word embeddings use first-subtoken pooling: for every
input word, the hidden state at the first subtoken becomes the word
representation.

For every word `i` and width `k in [0, max_width)`, the head builds a
span representation by concatenating `project_start(h_i)` and
`project_end(h_{i+k})`, applying ReLU, then `out_project` to land in
the 512-d head space. Scoring is a single einsum
`BLKD,BCD->BLKC` (in code: matmul + reshape).

Decode is post-sigmoid threshold (default 0.5) followed by greedy
flat-NER: candidates are sorted by score descending, accepted iff
no character overlap with any already-accepted span.

## Public API

```rust
use brain_extractors::{GlinerConfig, GlinerModel, GlinerSpan};

let model = GlinerModel::load(path, GlinerConfig::default())?;
let spans: Vec<GlinerSpan> = model.predict(
    "Alice Wong works at Acme Corp.",
    &["Person", "Organization"],
)?;
```

Labels are passed verbatim (case-sensitive) — upstream does not
lowercase them, and the tokenization of label tokens depends on
casing.

## Input limits

| Knob | Default | Source |
|------|---------|--------|
| `max_width` (words per span) | 12 | gliner_config.json |
| `max_len` (subtokens) | 384 | gliner_config.json |
| `max_labels` | 25 | upstream tooling heuristic |
| `threshold` | 0.5 | upstream default |

Predictions reject inputs whose total subtoken count (including
markers + the two `[SEP]` tokens) exceeds `max_len`, returning
`GlinerError::InputTooLong`. Labels beyond `max_labels` are rejected
with `GlinerError::TooManyLabels`.

## Performance characteristics

- Backbone forward: one DeBERTa-v3-small pass per `predict()` call.
  Compute scales linearly with sequence length and labels (labels
  occupy markers + their subtokens in the same sequence).
- Memory: roughly `model_weights + 2 * (seq_len * 768) * 4 bytes`
  per call at F32, halved at F16.
- F16 is recommended on GPU; CPU prefers F32 (DeBERTa-v2 in candle
  has not been audited end-to-end at F16 on CPU).

## Bootstrapping

GLiNER v2.1 ships PyTorch pickle (`pytorch_model.bin`). Brain loads
it directly via candle's `PthTensors` — no conversion step, no torch
dependency. `.devcontainer/bootstrap-model.sh` just downloads:

- `pytorch_model.bin` + `gliner_config.json` from `urchade/gliner_small-v2.1`
- `tokenizer.json` + `config.json` + `spm.model` from `microsoft/deberta-v3-small`

### Auto-discovery at startup

By default, Brain probes
`$XDG_DATA_HOME/brain/models/gliner-small-v2.1/` (falling back to
`~/.local/share/brain/models/gliner-small-v2.1/` when `XDG_DATA_HOME`
is unset) for the four required files: `pytorch_model.bin`,
`tokenizer.json`, `config.json`, `gliner_config.json`. If all four
are present, the shard wires the classifier tier automatically — no
environment variable required. Run `./.devcontainer/bootstrap-model.sh` to
populate this location.

Set `[extractors.classifier] model_path = "<dir>"` (or the env override
`BRAIN__EXTRACTORS__CLASSIFIER__MODEL_PATH=<dir>`) to point at a different
directory. When set, the override wins unconditionally and the XDG cascade
is skipped; if the loader can't open that directory the server fail-stops
rather than silently degrading to pattern-only.

The `[ENT]` special token is added to the tokenizer at load time
(`tokenizers::AddedToken::from("[ENT]", true)`); the trained pickle's
embedding matrix is already sized for the extended vocab row, so the
runtime-assigned id lines up with the row the model was trained
against.

Pickle key layout this module expects:

- `backbone.*` — DeBERTa-v2 model (loads cleanly into the candle v2 module).
- `head.project_start.{0,2}.{weight,bias}`
- `head.project_end.{0,2}.{weight,bias}`
- `head.out_project.{0,2}.{weight,bias}`
- `label_proj.{0,2}.{weight,bias}`

## Testing

Unit tests in `gliner/tests.rs` (always run):

- `tokenizer_word_split_preserves_offsets`
- `prompt_construction_inserts_ent_tokens`
- `span_enumeration_respects_max_width`
- `decode_filters_by_threshold`
- `decode_resolves_overlaps_greedy`
- `head_forward_with_synthetic_weights_produces_expected_shape`

Integration test (gated on `BRAIN_NER_MODEL_PATH`):

- `real_inference_detects_person_and_organization_in_alice_works_at_acme`

To run the gated test against a downloaded model directory:

```bash
BRAIN_NER_MODEL_PATH=/path/to/gliner-small-v2.1 \
    cargo test -p brain-extractors --lib gliner:: -- --ignored
```
