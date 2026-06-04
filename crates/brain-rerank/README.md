# brain-rerank

> Cross-encoder reranker (bge-reranker-base) for the retrieval query pipeline.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Loads `BAAI/bge-reranker-base` (a `BertForSequenceClassification` with
`num_labels=1`) via `candle` and scores `(query, candidate)` pairs. The
substrate calls `CrossEncoder::score_pairs` after RRF fusion picks the top-N
candidates; each pair's score is the raw logit from the classification head
(higher is more relevant), and the caller re-sorts the fused list by these
scores. The reranker is **optional**: when no on-disk weights are found (or the
device is unsupported), `try_load` returns `Ok(None)` and the retrieval pipeline
falls back to RRF-only ranking, logging the skip at `info`.

## Key modules

| Item | Role |
|---|---|
| `CrossEncoder` (`model`) | Model load + `score_pairs`; `DEFAULT_MAX_TOKEN_LEN`. |
| `RerankService` (`service`) | Service wrapper around the cross-encoder. |
| `auto_discover_model_dir` | Resolve model dir via `BRAIN_RERANK_MODEL_DIR` → `$XDG_DATA_HOME` → `$HOME/.local/share/brain/models/`. |
| `try_load` | Best-effort load: `Ok(None)` when the directory is absent, hard errors on corrupt weights. |
| `RerankError` | Error taxonomy. |

## Where it fits

Built on `candle-core`/`candle-nn`/`candle-transformers` + `tokenizers`, with
`flume` for the service channel. It plugs into `brain-planner`'s retrieval
pipeline as the post-fusion rerank stage; the model is gated by the deploy-time
`config.rerank.enabled` load flag.

## Spec

- [`../../spec/13_retrievers/06_post_processing.md`](../../spec/13_retrievers/06_post_processing.md)
- [`../../spec/13_retrievers/01_rrf_fusion.md`](../../spec/13_retrievers/01_rrf_fusion.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
