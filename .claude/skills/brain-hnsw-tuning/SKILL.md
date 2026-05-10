---
name: brain-hnsw-tuning
description: Verify HNSW parameter choices (M, ef_construction, ef_search, layers) match spec §06/02; flag deviations. Fires on diffs in crates/brain-index/.
when-to-use: |
  Triggers:
    - Diff in crates/brain-index/**/*.rs that touches HNSW config or parameters
    - User says "tune HNSW" / "what's a good M / ef?"
    - Recall quality regression — investigating index params
    - Adding a new index variant
trigger-files:
  - crates/brain-index/**/*.rs
spec-refs:
  - spec/06_ann_index/02_parameters.md
  - spec/06_ann_index/04_search.md
  - spec/06_ann_index/07_maintenance.md
---

# HNSW Tuning

## When to use

Any change to the HNSW index parameters or the search/insertion/maintenance code paths in `brain-index`. The spec pins these parameters because changing them has correctness, recall-quality, and latency consequences.

## Spec-pinned parameters (§06/02)

| Parameter | Spec value | Why |
|---|---|---|
| `M` (max neighbors per layer 0) | 16 | Balanced graph; doubles to 32 in upper layers |
| `M_max0` (layer 0 max) | 32 | 2× of M, per Malkov & Yashunin |
| `ef_construction` | 200 | Build-time search width; recall vs build cost |
| `ef_search` (default) | 100 | Search-time width; recall vs latency |
| `ef_search` (cap) | 500 | Hard cap; spec §16/02 latency targets bound this |
| Distance | cosine | Spec §06/01; vectors are L2-normalized |
| Levels | dynamic via geometric distribution with `mL = 1 / ln(M)` | Standard HNSW |

Values come from spec §06/02 — verify before assuming.

## Hard rules

- **Don't change `M` or `M_max0`** without a spec change. Graph topology depends on these; existing indexes won't be compatible.
- **`ef_search` is per-query.** A request can override up to 500. Default is 100.
- **`ef_construction` is build-time.** Higher values → better recall, slower build. Spec'd at 200.
- **Cosine distance only.** L2-normalized vectors; check `wide` / `matrixmultiply` SIMD usage matches the spec's hot path.
- **Persistence per §06/06.** The HNSW graph is mmap'd; lazy-load is OK; full deserialization on cold start is the failure mode.

## Workflow

1. **Locate the parameter constants.** They should live in one place (e.g., `brain-index/src/params.rs`) so audits are easy.
2. **Cross-check against spec §06/02.** Each constant matches the spec value.
3. **Search path.** Confirm `ef_search` is parameterized at the query API; default to 100; cap at 500.
4. **Insertion path.** Confirm `ef_construction = 200` is used during builds.
5. **Concurrency.** Per §06/08, insertions and searches share the graph; verify the read-side uses `ArcSwap<HnswGraph>` or the project's chosen lock-free swap pattern.
6. **Filtered search.** Per §06/09, post-filtering vs. pre-filtering trade-off — confirm the chosen strategy and its impact on `ef_search` (filtering reduces effective recall, so `ef` may need to scale up).

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `M = 32` (not 16) | Doubles graph memory; not spec'd | Restore to 16 unless spec changes |
| `ef_search = 50` default | Below spec; recall drops | Default 100 |
| Per-query `ef_search` unbounded | Latency unbounded | Cap at 500 |
| Euclidean distance | Spec is cosine | Use cosine; vectors are L2-normalized |
| `RwLock<HnswGraph>` on hot read | Lock contention | `ArcSwap` + crossbeam-epoch |

## Quality checks

- **Recall@10** for a synthetic dataset (per §16/05). Default params should yield ≥ 95% on the spec'd benchmark.
- **Build time** for 1M vectors at `ef_construction = 200`. Use as a regression baseline.
- **Search latency** at `ef_search = 100`: p99 ≤ 5ms per spec §16/02.

## Cross-references

- `rust-perf` — generic hot-path discipline.
- `brain-invariants` — invariants applicable to the read path.
- spec §06.

## Source / Adaptations

Project-local. Operationalizes spec §06.
