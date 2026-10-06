# Setting up the bundled NER model

The classifier extractor's built-in `brain.gliner` is wired to
[GLiNER v2.1](https://huggingface.co/urchade/gliner_small-v2.1), a
zero-shot named-entity recognizer. The model is loaded at runtime
from an operator-provided directory and emits spans tagged with the
**active schema's entity-type qnames** — labels are passed per
inference call, not baked into the weights.

The published container image **does** bundle the model (see the `model`
stage in `Dockerfile`); `config/docker.toml` points the classifier at
`/opt/brain/models/gliner-small-v2.1`. Outside the image the substrate
neither bundles nor auto-downloads it: use `.devcontainer/bootstrap-model.sh`.

## Why GLiNER

- **Zero-shot.** The label set is the snapshot of the schema's
  entity types at shard startup (`brain:Person`, `brain:Organization`,
  ...). No per-schema retraining; no OntoNotes relabel layer.
- **Schema-driven.** When a deployment adds a new entity type via
  `SCHEMA_UPLOAD`, the classifier picks it up on the next shard
  startup. The label list is plumbed through `MaterializeDeps`.
- **Compact.** ~340 MB fp16 vs ~430 MB f32 for the OntoNotes BERT
  it replaces. DeBERTa-v3-small backbone (hidden = 768, 6 encoder
  layers).

## Layout the substrate expects

```
<classifier model_path>/      # [extractors.classifier] model_path, else XDG default
├── pytorch_model.bin     # GLiNER pickle weights (~611 MiB, fp32)
├── tokenizer.json        # DeBERTa-v3 SPM tokenizer (vanilla; [ENT] added at load)
├── spm.model             # SentencePiece source
├── config.json           # DeBERTa-v3-small backbone config
└── gliner_config.json    # max_width / max_len / hidden_size
```

The `<<ENT>>` marker token is required — it's the per-label pool
position the head reads from when computing label embeddings, and
`<<SEP>>` separates the label block from the text.

Both must be present **in `tokenizer.json` on disk**. The loader resolves
them with `token_to_id` and fails with `MissingToken` if either is absent
(`classifier/gliner/mod.rs:631`); it does not add them. `<<ENT>>` must be
id 128001 and `<<SEP>>` id 128002. `bootstrap-model.sh`'s
`patch_gliner_tokenizer` writes them, and the image's `model` stage applies
the identical patch.

## Download script

The reference path is `.devcontainer/bootstrap-model.sh`. It's a pure
download — no torch, no conversion, no Python at runtime:

```bash
# GLiNER's own files.
curl -L -o pytorch_model.bin \
    https://huggingface.co/urchade/gliner_small-v2.1/resolve/main/pytorch_model.bin
curl -L -o gliner_config.json \
    https://huggingface.co/urchade/gliner_small-v2.1/resolve/main/gliner_config.json

# DeBERTa-v3-small companion files (tokenizer / backbone config / SPM).
# From onnx-community, NOT microsoft: the upstream repo ships only
# spm.model + tokenizer_config.json, and the `tokenizers` crate needs a
# JSON tokenizer. microsoft/deberta-v3-small/tokenizer.json is a 404.
curl -L -o tokenizer.json \
    https://huggingface.co/onnx-community/deberta-v3-small/resolve/main/tokenizer.json
curl -L -o config.json \
    https://huggingface.co/onnx-community/deberta-v3-small/resolve/main/config.json
curl -L -o spm.model \
    https://huggingface.co/onnx-community/deberta-v3-small/resolve/main/spm.model

# Then patch <<ENT>>/<<SEP>> into tokenizer.json — see above; the model
# cannot load without them.
```

## Security posture

The pickle is loaded by candle's `PthTensors` (zip-archived
`torch.save` format). Brain treats the model directory as
read-only at runtime — the substrate never writes back into it.

## Fingerprinting

The substrate fingerprints `pytorch_model.bin` with BLAKE3 and
truncates to 16 bytes. The fingerprint hex is the
`ClassifierModel::version` value reported in the
`extractor_audit` row's `model_metadata` blob and on the
`EXTRACTOR_LIST` wire response, so operators can verify the
production weights match what they intended to deploy.

A model swap that changes the pickle blob produces a new
fingerprint — downstream statements get a fresh
`extractor_version` stamping and the stale-extraction detector
flags older outputs.

## Smoke test

Run the `#[ignore]`'d smoke test in `classifier::tests` to confirm
end-to-end inference:

```bash
BRAIN_NER_MODEL_PATH=~/.local/share/brain/models/gliner-small-v2.1 \
    cargo test -p brain-extractors --lib \
        classifier::tests::real_inference -- --ignored --nocapture
```

Expected output: at least one `brain:Person` span over `"Alice
met Bob in Paris."` and at least one `brain:Place` span over
`"Paris"`. GLiNER emits the qnames passed in verbatim — there is
no OntoNotes label-mapping layer.
