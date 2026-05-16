# Setting up the bundled NER model

The classifier extractor's built-in `brain.basic_ner` is wired to
a BERT-style token-classification model loaded at runtime from an
operator-provided directory. This matches the substrate's
[embedder pattern](../../brain-embed/src/config.rs) — same
operator surface, same security posture (safetensors only, no
pickle), same fingerprint discipline.

The substrate does **not** bundle or auto-download the model.

## Recommended model

[`dslim/bert-base-NER`](https://huggingface.co/dslim/bert-base-NER)
— Apache-2.0, ~433 MB f32 (~110 MB f16). Trained on CONLL-2003
with `PER` / `ORG` / `LOC` / `MISC` classes. Token classification
F1 ≈ 91% on the CONLL-2003 test set.

Smaller alternatives:
- [`dslim/distilbert-NER`](https://huggingface.co/dslim/distilbert-NER) — ~250 MB, F1 ≈ 89%.
- [`Davlan/bert-base-multilingual-cased-ner-hrl`](https://huggingface.co/Davlan/bert-base-multilingual-cased-ner-hrl) — multilingual support, ~700 MB.

## Layout the substrate expects

```
${BRAIN_NER_MODEL_PATH}/
├── config.json           # BertConfig
├── tokenizer.json        # tokenizers crate format
├── model.safetensors     # weights (no pickle)
└── labels.txt            # one label per line; classifier head's output indices
```

`labels.txt` MUST match the order of the model's output head. For
`dslim/bert-base-NER` the order is:

```
O
B-MISC
I-MISC
B-PER
I-PER
B-ORG
I-ORG
B-LOC
I-LOC
```

(Read from `id2label` in the model's `config.json`.)

## Download + convert script

```bash
# 1. Install Hugging Face CLI + safetensors converter once.
pip install huggingface_hub safetensors

# 2. Pull the repo into a working dir.
mkdir -p ~/brain-models/ner
cd ~/brain-models/ner

huggingface-cli download dslim/bert-base-NER \
    --local-dir . \
    --local-dir-use-symlinks False

# 3. Convert PyTorch weights to safetensors if not already present.
#    (dslim/bert-base-NER already ships safetensors; this is the
#    fallback for repos that don't.)
if [ ! -f model.safetensors ]; then
    python -c "
import safetensors.torch as st
import torch
state = torch.load('pytorch_model.bin', map_location='cpu')
st.save_file(state, 'model.safetensors')
"
fi

# 4. Extract labels.txt from config.json's id2label.
python -c "
import json
with open('config.json') as f:
    cfg = json.load(f)
labels = [cfg['id2label'][str(i)] for i in range(len(cfg['id2label']))]
with open('labels.txt', 'w') as f:
    f.write('\n'.join(labels) + '\n')
"

# 5. Point the substrate at the directory.
export BRAIN_NER_MODEL_PATH=~/brain-models/ner

# 6. Verify the substrate accepts the model on startup. The
#    info-level log line `loaded classifier model directory ...`
#    confirms the load path succeeded.
RUST_LOG=brain_extractors=info cargo run --bin brain-server
```

## Security posture

The substrate **refuses** to load `pytorch_model.bin` (pickle).
Operators MUST convert to safetensors. This matches phase 5's
[embedder pickle-refusal](../../../spec/04_embedding_layer/03_inference.md)
policy.

The model directory MUST be read-only at runtime — the substrate
never writes back into it.

## Fingerprinting

The substrate fingerprints `config.json + tokenizer.json +
model.safetensors` with BLAKE3 and truncates to 16 bytes. The
fingerprint hex is reported in the `extractor_audit` row's
`model_metadata` blob and on the `EXTRACTOR_LIST` wire response,
so operators can verify the production model matches what they
intended to deploy.

A model swap that changes any of those three files produces a new
fingerprint — downstream statements get a fresh
`extractor_version` stamping and the stale-extraction detector
(§25/00) flags older outputs.

## Smoke test

Run the `#[ignore]`'d smoke test in `classifier::tests` to confirm
end-to-end inference:

```bash
BRAIN_NER_MODEL_PATH=~/brain-models/ner \
    cargo test -p brain-extractors --lib \
        classifier::tests::real_inference -- --ignored --nocapture
```

Expected output: at least one `PER` span over `"Alice met Bob in
Paris."` and at least one `LOC` span over `"Paris"`.

## Status

- Phase 20.3 (this commit) — load path validates the model
  directory; candle runtime is wired in phase 20.6 (ENCODE
  integration). Until then, configured models load without errors
  but every dispatch returns `Failure(reason: "runtime not
  wired")`.
- Phase 20.6 — candle forward pass + linear classifier head
  inference live.
- Phase 20.7 — `brain.basic_ner` registered via the system schema
  so it picks up the configured `BRAIN_NER_MODEL_PATH`
  automatically.
