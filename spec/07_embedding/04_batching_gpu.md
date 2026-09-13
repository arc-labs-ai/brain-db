# 07.04 Batching and GPU

The optional GPU inference path. This file specifies how Brain batches multiple inference requests onto a GPU when one is available.

## 1. When GPU helps

GPU embedding makes sense when:

- The workload sustains a meaningful inference rate (≥ 500/s).
- The cost of a GPU is justified by the throughput gains.
- The deployment can manage GPU drivers and monitoring.

For lower-rate workloads, CPU-only is simpler and has acceptable latency.

## 2. The trade-off

Per-item:

| Path | Latency | Throughput |
|---|---|---|
| CPU, single | 5–10 ms | ~200/s/core |
| GPU, single (no batching) | ~5 ms | ~200/s |
| GPU, batched (32) | ~10 ms wall, 0.3 ms amortized | ~3,000/s |
| GPU, batched (128) | ~30 ms wall, 0.25 ms amortized | ~10,000/s |

GPU shines with batching. Single-item GPU inference is comparable to CPU but worse — kernel launch overhead matters for small workloads.

Brain batches automatically when GPU is enabled. The batching happens at the embedding layer; clients are unaware.

## 3. The batching window

Brain gathers in-flight inference requests over a small time window (default: **2 ms**). At the end of the window, all gathered requests are submitted as a single batch.

The trade-off:

- **Larger window** = bigger batches = better throughput, worse latency.
- **Smaller window** = smaller batches = worse throughput, better latency.

2 ms is the sweet spot for Brain's target workloads. It's:

- Short enough that p50 latency isn't dominated by the wait.
- Long enough to gather meaningful batches at high QPS.

A workload at 5,000 RPS averages 10 requests per 2 ms window — a useful batch. A workload at 100 RPS averages 0.2 requests per window — usually a single-item batch.

## 4. The batching strategy

```
loop {
    let mut batch = Vec::new();
    let deadline = Instant::now() + batching_window;

    // Always include at least one item
    batch.push(receive_request().await);

    // Gather more until window expires or batch is full
    while batch.len() < max_batch_size && Instant::now() < deadline {
        match receive_request_with_timeout(deadline - Instant::now()).await {
            Some(req) => batch.push(req),
            None => break,  // Timeout
        }
    }

    // Submit batch
    let vectors = gpu_inference(batch.iter().map(|r| &r.text)).await?;

    // Distribute results to requesters
    for (req, vec) in batch.into_iter().zip(vectors.into_iter()) {
        req.respond(vec).await;
    }
}
```

`max_batch_size` is configurable; default 64.

## 5. Per-shard batchers

Each shard has its own batcher. This:

- Avoids cross-shard coordination.
- Keeps the batch's items tied to one shard's executor.
- Trades batch size for parallelism — multiple smaller batches running simultaneously rather than one giant batch.

For workloads with many shards, this means GPU utilization is shared across shards. Brain may also support a "global batcher" that pools requests from all shards onto a shared GPU; this is configuration option for deployments that want maximum GPU efficiency.

## 6. GPU memory management

The model's weights live in GPU memory permanently after load. ~130 MiB at FP32, less at FP16/INT8.

Activations are allocated per batch. For a batch of 64 with 512-token sequences:

- Token embeddings: 64 × 512 × 384 × 4 = ~50 MiB.
- Per-layer activations: ~50 MiB × 6 layers = ~300 MiB.
- Total per batch: ~350 MiB.

A 16 GiB GPU comfortably hosts the model and several concurrent batches. A 4 GiB GPU is tight; reduce `max_batch_size` to fit.

## 7. GPU selection

For multi-GPU systems, Brain uses GPU 0 by default. Configuration can pin to a specific device:

```
[embedding.gpu]
device_id = 0          # or 1, 2, etc.
```

Multi-GPU inference (sharding the model across GPUs) is not supported. The model is small enough to fit on a single GPU; multi-GPU is unnecessary complexity.

## 8. Mixed CPU/GPU operation

A failure on the GPU (CUDA error, OOM, driver issue) can fall back to CPU:

- Configuration: `gpu_fallback_cpu = true` (default false).
- On GPU failure: log the error, route subsequent inferences to CPU.
- Periodically retry GPU; if it recovers, resume.

The fallback is operational insurance for transient GPU issues. Persistent GPU failures (driver crashes, hardware faults) are escalated to operators rather than silently absorbed.

## 9. Latency under batching

With batching, single-item latency increases:

- Pre-batch wait: 0–2 ms (the batching window).
- Inference: 5–30 ms (depending on batch size).
- Post-batch dispatch: < 1 ms.

For p50, expect ~10 ms with GPU batched (compared to 7 ms CPU). For p99 under load, GPU batched is much better — CPU saturates at ~200/core/s, GPU stays in the same ballpark up to 10K/s.

## 10. Fairness within a batch

All items in a batch finish at the same wall time. An item that arrived just before the deadline waits the same as one that arrived just after the previous batch closed.

This means latency within a batch isn't strictly fair — early arrivals wait longer than late ones — but the unfairness is bounded by the batching window (2 ms). Over many requests, this averages out.

## 11. Backpressure

If the GPU can't keep up with the inference rate (e.g., due to QPS spikes or GPU contention), the batcher's queue fills up. When the queue exceeds a threshold:

- New requests are queued briefly.
- If the queue exceeds a higher threshold, new requests are rejected with `EmbeddingOverloaded`.

Clients receiving `EmbeddingOverloaded` should back off and retry. Clients should do this with exponential backoff.

## 12. Batch heterogeneity

Texts in a batch can have different lengths (different token counts). The model handles batched inputs of varying length via attention masking — shorter sequences are padded; the mask tells the model to ignore padding.

The padding cost: for a batch where one item is 512 tokens and others are 50, the batch runs at the speed of the longest item. This is wasteful if the batch's items have very different lengths.

Brain doesn't currently sort items by length within a batch (a "sequence bucketing" optimization). It's a possible future-version improvement; for now, Brain accepts the inefficiency.

## 13. Determinism

GPU inference may produce slightly different results than CPU (different floating-point order, different libraries). For Brain's purposes, this is acceptable — cosine similarity is robust to small perturbations.

Cross-run determinism: the GPU's output is deterministic for a given input on the same hardware and driver. Different GPUs (same model, same drivers) produce bit-identical output for the same inputs.

The cache treats GPU and CPU outputs as equivalent — a vector cached from CPU inference is returned for a query that would have been GPU-inferred, and vice versa.

## 14. The CPU-only deployment

If GPU is not available or not configured, Brain runs CPU-only. There is no batching on the CPU path; each request goes through the model independently. CPU inference scales by adding cores.

A CPU-only deployment is simpler to operate (no drivers, no GPU monitoring). For typical agent workloads (≤ 500 inferences/s), CPU-only is sufficient.

---

*Continue to [`05_fingerprinting.md`](05_fingerprinting.md) for the model fingerprint.*
