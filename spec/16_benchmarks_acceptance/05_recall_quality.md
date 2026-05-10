# 16.05 Recall Quality

The accuracy and quality criteria for RECALL — the core operation that makes Brain useful.

## 1. The metric

Recall@K: of the true top-K results (computed by exhaustive search), how many does Brain's HNSW return?

Formula:

```
recall@K = |HNSW_top_K ∩ exhaustive_top_K| / K
```

If HNSW returns the perfect set: recall = 1.0. If it misses 2 out of 10: recall = 0.8.

## 2. The targets

| Metric | Target |
|---|---|
| Recall@10 (default settings) | ≥ 0.95 |
| Recall@10 (tuned for higher) | ≥ 0.99 |
| Recall@100 | ≥ 0.90 |
| Recall@1 | ≥ 0.97 (top-1 should almost always be correct) |

These are MUST for v1.

## 3. The conditions

Targets apply at:
- 1M memories per shard.
- Default HNSW parameters (M=16, ef_construction=200, ef_search=64).
- Mix of fresh and aged data.

## 4. The data dependence

Recall varies with data:
- Well-clustered data: high recall easy.
- Uniformly random data: harder to achieve high recall.

Brain's targets assume realistic data: AI agent memories with some structure.

For pathological cases (e.g., near-orthogonal vectors), recall may be lower. Such cases are documented but not the primary target.

## 5. The "ef_search" tuning

ef_search trades recall for latency:

| ef_search | Recall@10 | Latency |
|---|---|---|
| 32 | ~0.92 | ~2 ms |
| 64 (default) | ~0.95 | ~3-5 ms |
| 128 | ~0.98 | ~6-10 ms |
| 256 | ~0.99 | ~12-20 ms |

Brain ships with ef_search = 64. Operators can tune for their needs.

## 6. The "M" trade-off

M (max edges per node) trades index size for quality:

| M | Recall@10 | Index Size |
|---|---|---|
| 8 | ~0.93 | small |
| 16 (default) | ~0.95 | medium |
| 32 | ~0.97 | large |
| 64 | ~0.98 | very large |

M=16 is a balance. Larger M for higher recall, smaller for memory savings.

## 7. The "post-rebuild" recall

Right after a fresh build:
- Recall: ~0.95-0.97 at default settings.
- This is the "best case" the substrate offers.

After many tombstones accumulate:
- Recall drops over time.
- The maintenance worker rebuilds when recall drops below ~0.85.

## 8. The recall-degradation check

Periodic measurement:
- Take a sample of cues.
- Compare HNSW results to exhaustive.
- Compute recall.

This is the `brain_hnsw_recall_estimate` metric.

It runs every hour by default.

## 9. The "above 1M" recall

At larger scales:
- 10M memories: recall ~0.92-0.94 at default settings.
- 100M memories: recall depends heavily on data structure; may need larger M.

Acceptance is at 1M. Larger scales are supported with possible tuning.

## 10. The "filtered search" recall

When filters are applied (e.g., agent ID):
- HNSW may filter post-search (fast, but reduces recall if many candidates filtered).
- Or pre-search (slower).

Brain's default: post-filter for typical filter selectivity (~10-50% pass).

For very low selectivity (e.g., 1% pass), recall may be artificially low. Operators tune ef_search higher in such cases.

## 11. The "exact-match" recall

For the original text used during ENCODE:
- RECALL with that exact text should return the memory.
- Similarity should be ≥ 0.99 (essentially 1.0, modulo numerical precision).

This is the "smoke test" — can the substrate find what it stored?

Tested: encode 1000 memories; query each with its original text; verify each returns the correct top-1.

## 12. The "near-duplicate" detection

Two memories with very similar text (paraphrases, slight edits):
- Should be highly ranked relative to dissimilar memories.

Tested: encode pairs of paraphrases; query with one of each pair; verify both rank in top-K.

## 13. The "semantic match" quality

Beyond exact and near-duplicate:
- Conceptually similar but textually different memories should be highly ranked.

This depends on the embedding model's quality, not just HNSW. Brain provides good embeddings (BGE) but the upper bound is the model.

## 14. The "long tail" cases

Some queries have:
- No good match in the data.
- Many equally-mediocre matches.

The substrate returns its best K, even if all are weak. Application logic decides if results are "good enough" via the similarity score.

## 15. The score interpretation

Cosine similarity score in [-1, 1]:
- > 0.9: very similar.
- 0.7 - 0.9: similar.
- 0.5 - 0.7: somewhat related.
- < 0.5: weak relationship.

Applications threshold based on these. The substrate doesn't filter by score; that's application logic.

## 16. The "consistent ranking" property

Within a single substrate state:
- The same query returns the same results in the same order.
- Subject to no concurrent state changes.

For repeated queries: deterministic. (HNSW's randomness is at build time, not search time.)

## 17. The "recall after FORGET" semantics

When memory M is FORGOTTEN:
- It shouldn't appear in subsequent queries.
- Tombstone logic enforces this.

If M does appear (post-FORGET): bug.

Tested: FORGET 100 memories; verify they don't appear in queries.

## 18. The "quality regression" guarding

In CI, quality is checked against baselines:
- Build the index with the test dataset.
- Compute recall on a query set.
- Compare to the previous release.
- If recall drops > 1% absolute: investigate.

Catches regressions in HNSW or embedder.

## 19. Quality vs Performance

The defaults (M=16, ef_search=64) balance:
- Recall ≥ 0.95 (acceptable).
- Latency p99 ≤ 25 ms (acceptable).

Tuning goes one way or the other:
- For higher recall: more memory and longer search.
- For lower latency: lower recall.

Operators choose based on their needs.

## 20. The "honesty" about recall limits

HNSW is approximate:
- 0.95 recall means 5% of answers might miss the true top-K.
- For most agent applications: acceptable.

For applications that need 100% recall:
- Use exhaustive search (slow).
- Or, use HNSW for candidates + exhaustive re-rank.

Brain provides HNSW; for niche requirements, applications layer on top.

---

*Continue to [`06_durability_criteria.md`](06_durability_criteria.md) for durability criteria.*
