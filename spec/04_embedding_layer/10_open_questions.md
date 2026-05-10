# 04.10 Open Questions

Embedding-layer questions unresolved as of this spec version.

---

## OQ-EL-1: INT8 quantization

**Issue.** The model runs at FP32 by default. INT8 quantization reduces weights by 4× and inference latency by ~2×, with marginal accuracy loss.

**Options.**

a) **Stay FP32.** Simpler; baseline accuracy preserved.

b) **INT8 quantized weights.** A separate quantized model file (`bge-small-en-v1.5-int8.safetensors`). Configurable per deployment.

c) **Mixed precision.** FP32 for some layers, INT8 for others, optimized for accuracy.

**Recommendation.** Defer until first benchmark cycle reveals whether the latency improvement justifies the accuracy cost. INT8 is well-understood and easy to add.

---

## OQ-EL-2: Multi-modal embeddings

**Issue.** v1 is text-only. Image and audio embeddings would require a different model and possibly different vector dim.

**Options.**

a) **Single multi-modal model.** CLIP-family or similar. Vector dim and storage layout change.

b) **Per-modality model + separate arenas.** Each modality has its own embedding model; vectors are tagged with modality at storage time.

c) **Defer.** Stay text-only in v1.

**Recommendation.** Defer. Multi-modality is mostly a "different layers everywhere" change, not a localized embedding-layer change. Treat it as a v2 milestone.

---

## OQ-EL-3: Cross-language deployments

**Issue.** `bge-small-en-v1.5` is English-only. Non-English deployments need a different model.

**Options.**

a) **Configurable model.** Use `bge-m3` (multilingual) for multilingual deployments. Already supported via the `model_path` configuration.

b) **Multiple active models.** Different agents in the same deployment use different models based on language. Complex; cross-agent queries become difficult.

c) **Translate-then-embed.** Pre-translate non-English content to English. Adds latency and dependency on a translation service.

**Recommendation.** Option (a) — single configured model per deployment. Multilingual deployments use a multilingual model. Cross-language operation within a single deployment is not a goal.

---

## OQ-EL-4: Dynamic batching window

**Issue.** The GPU batching window is configured statically (default 2 ms). Dynamic adjustment based on load could improve performance.

**Options.**

a) **Static window.** Status quo. Operators tune for their workload.

b) **Adaptive.** The window expands under low load (waiting longer for bigger batches) and contracts under high load (better latency). Heuristic-driven.

**Recommendation.** Defer. The static window is simple and good enough for most workloads. If profiling reveals scenarios where adaptation helps significantly, revisit.

---

## OQ-EL-5: Per-agent embedding model

**Issue.** All agents in a deployment share the embedding model. Some applications want different agents with different models (e.g., different specializations).

**Options.**

a) **Single model per deployment.** Status quo. Different specializations require different deployments.

b) **Per-agent model selection.** Each agent's encode and recall use the agent's configured model. Storage layout would need to handle mixed-fingerprint shards as a normal case rather than a migration window.

**Recommendation.** Out of scope. The architecture is designed around a single active model per shard. Cross-model querying within a shard adds complexity without clear benefit; deployments wanting multi-model run separate deployments.

---

## OQ-EL-6: Embedding quality observability

**Issue.** The substrate doesn't directly observe embedding quality. If the chosen model produces poor vectors for the deployment's content, queries silently degrade.

**Options.**

a) **No direct observation.** Operators evaluate quality externally (sample queries, compare to expected results).

b) **Built-in quality benchmark.** A small fixed dataset that the substrate periodically embeds and queries; reports metrics.

c) **Customer-supplied benchmark.** The deployment uploads its own benchmark; substrate runs it on demand.

**Recommendation.** Specify a built-in benchmark in [16. Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/) that runs at startup and periodically. This catches gross issues. For deployment-specific quality, operators are on their own — too varied to standardize.

---

## OQ-EL-7: Embedding for other operations

**Issue.** Currently, only encode and recall use the embedding layer. Could other operations benefit?

Examples:

- Filter expressions in RECALL could be embedded for semantic-aware filters.
- ADMIN tools might want to find memories matching descriptions semantically.

**Options.**

a) **Stay encode/recall.** Status quo.

b) **Generic embed RPC.** A cognitive operation that just embeds text; clients can use it for arbitrary purposes.

**Recommendation.** Defer. The current usage is sufficient. A generic embed RPC could be added if a clear use case appears.

---

## OQ-EL-8: Streaming embedding for long content

**Issue.** Truncation at 512 tokens loses content. For long content, the substrate could embed multiple chunks and combine them (e.g., averaging vectors).

**Options.**

a) **Truncate.** Status quo. The agent is responsible for chunking long content.

b) **Auto-chunk-and-aggregate.** Long inputs are split into 512-token chunks; each is embedded; vectors are averaged or otherwise aggregated.

c) **Allow longer inputs via different model.** A long-context model exists; could be configured.

**Recommendation.** Stay with truncate (a). Auto-aggregation has subtle semantic implications (averaging losing information) and is better addressed at the application layer where the chunking decisions can be informed by context.
