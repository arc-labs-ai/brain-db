# GPU Acceleration & Horizontal Scaling — Architecture

> **Audience:** operators and platform architects planning a high-throughput Brain deployment. **Status:** forward-looking architecture guide. This document is written at the architecture level — planes, components, contracts, and data flows; no implementation detail. It covers both halves of scaling: **Part I** moves the model workloads from CPU to GPU and scales them on traffic; **Part II** covers the storage plane — storage, the memory hierarchy, and the indexes — and scales it on corpus size and write rate. Together they take the system to millions of memories and thousands of queries per second.

---

## 1. The two planes

Brain is best understood as **two planes with opposite scaling laws**, currently co-resident in one process:

- **The storage plane** — durable, *stateful*. It owns the vector arena, the write-ahead log, the metadata store, and the in-memory vector and text indexes. It is I/O-, memory-, and cache-bound. **It scales with the size of the corpus and the write rate.** It does not benefit from a GPU.

- **The inference plane** — ephemeral, *stateless*. It turns text into numbers: embedding, reranking, entity extraction (NER), and optional LLM extraction. It is matmul-bound. **It scales with query and ingest traffic, independent of corpus size.** It is the only part that belongs on a GPU.

The architecture's central move is to **separate these planes physically** so each scales on its own axis: GPU compute elastic with traffic, durable storage sticky with data. Everything below follows from that separation.

---

## 2. The inference workloads

Four model workloads make up the inference plane, ordered by how often they run:

| Workload | Model class (today) | Size class | Triggered by | On the latency path? | Batchable |
|---|---|---|---|---|---|
| **Embedding** | small bi-encoder (BGE-small, 384-dim) | ~33 M params | every write **and** every query cue | **yes** | yes — high value |
| **Rerank** | cross-encoder (bge-reranker-base) | ~278 M params | **every query** (always-on) | **yes** | yes — ~50 pairs/query |
| **NER extraction** | zero-shot tagger (GLiNER, DeBERTa backbone) | ~140 M params | every write (asynchronous) | no | yes |
| **LLM extraction** | large generative model (optional, self-hosted) | 7 B–70 B | a subset of writes (best-effort) | no | yes |

A fifth workload — **vector similarity search** — lives in the storage plane on CPU today and only becomes a GPU candidate at extreme scale (§7).

**The sizing driver is rerank.** Reranking is *always-on*: every query embeds its cue **and** runs a cross-encoder over the top fused candidates (~50 query–candidate pairs). On CPU this is the dominant tail. On a single mid-range inference GPU, embedding and rerank both collapse to single-digit milliseconds and amortize across concurrent requests through batching. **GPU adoption is driven by query throughput, not corpus size.**

---

## 3. The governing constraint: the storage runtime must never block on inference

This single architectural fact shapes every decision in this document.

The storage plane runs on a **single-thread-per-core, non-blocking event-loop runtime**. Each core owns one shard's state exclusively and processes work cooperatively; there is no shared worker pool, and a core must never make a blocking, synchronous call. A GPU invocation is exactly that — a blocking device call with host↔device transfer and kernel execution. Issuing it from inside a shard would **stall that core's entire event loop** for the duration.

Compounding this: a GPU only delivers its throughput advantage when fed **large batches**. The storage plane's natural unit of work is *one request at a time per core* — batch size one — which would waste the overwhelming majority of a modern GPU.

**Conclusion:** inference cannot be co-scheduled with storage. It must be **disaggregated into a separate batching tier** that (a) runs off the storage cores and (b) aggregates many requests into one GPU batch.

### The contract that makes this clean

Brain already treats "turn text into a vector" (and, by extension, "rerank these candidates," "extract entities") as an **abstract capability behind a stable contract**, not as an inline computation hard-wired into the storage path. That contract accepts a *batch* of inputs, returns results, and advertises a **model-identity stamp** (so stored vectors know which model produced them). Because the boundary is already an abstraction, the local CPU provider can be replaced by a **remote, GPU-backed provider** without changing anything in the storage plane.

This is the seam the disaggregation runs through: same contract, different provider — one talks to a local CPU, the other to a remote GPU inference service.

---

## 4. Target topology

```
                       ┌──────────────────────────────────────────────┐
   clients ─TCP/HTTP──▶│  EDGE / API TIER  (stateless front)           │
                       │  • request admission + routing                │
                       │  • micro-batch window (collect across conns)  │
                       │  • embedding-cache lookup (skip GPU on hit)    │
                       └──────┬────────────────────────────┬───────────┘
                              │ batched inference requests  │ per-shard data ops
                              ▼                             ▼
        ┌───────────────────────────────┐   ┌───────────────────────────────────┐
        │  INFERENCE PLANE (stateless)   │   │  STORAGE PLANE (stateful)          │
        │  autoscaled GPU pool           │   │  one shard per core; sticky data   │
        │  ┌──────────────────────────┐  │   │  ┌────────┐ ┌────────┐ ┌────────┐  │
        │  │ embedding model          │  │   │  │ shard0 │ │ shard1 │ │ shardN │  │
        │  │ reranker                 │  │   │  │ arena  │ │ arena  │ │ arena  │  │
        │  │ NER tagger               │  │   │  │ WAL    │ │ WAL    │ │ WAL    │  │
        │  │ (LLM extractor — sep.)   │  │   │  │ meta   │ │ meta   │ │ meta   │  │
        │  └──────────────────────────┘  │   │  │ index  │ │ index  │ │ index  │  │
        │  scales on QPS                 │   │  └────────┘ └────────┘ └────────┘  │
        └───────────────────────────────┘   │  scales on corpus size + writes    │
                                             └───────────────────────────────────┘
```

- **Edge / API tier** — stateless. Terminates client connections, applies the micro-batch window, consults the embedding cache, and fans data operations to the right shards. The batch window is where many small requests become one GPU batch.
- **Inference plane** — stateless GPU pool. Models loaded once per replica, shared across all batched requests. Autoscale on GPU utilization / queue depth. A replica failure degrades latency, never data.
- **Storage plane** — stateful shard nodes. CPU + RAM + fast local disk; **no GPU**. Scale on corpus size and write rate. A shard owns its data exclusively.
- **Embedding cache** — sits in front of the GPU pool, keyed by model identity + content. A high hit rate (repeated cues, re-encodes) removes load from the GPU entirely and is the cheapest scaling lever (§8).

The planes communicate over the network. This is what "fully scalable" means here: **the elastic plane and the durable plane grow independently.**

---

## 5. Where batching happens

Two architectural placements; choose per deployment:

1. **Batch at the edge tier (recommended).** Before a write or query is routed to a shard, the edge tier collects inference requests across all in-flight connections into a short time window and issues one batched call to the GPU pool. The cue embedding (for both writes and queries) and the rerank pass happen here, *around* the shard fan-out. The storage plane sees only finished vectors and finished orderings.
2. **Call out from the plane boundary.** Shards stay purely storage; any operation needing inference hands off across the plane boundary, awaits the GPU result, then proceeds. Simpler to retrofit, but adds a network hop to each request's latency.

Either way the invariant holds: **a storage core never touches a GPU.**

> **Tokenization is a CPU step.** Converting text to token IDs runs on the edge tier's CPU; only the transformer forward pass goes to the GPU. Pipeline the two so the GPU is never idle waiting on tokenization, and provision enough edge-tier CPU to keep the GPU fed (§8).

---

## 6. Libraries & frameworks (per workload)

### 6.1 Encoder models — embedding, rerank, NER

Small (≤ ~300 M params), latency-sensitive, batchable. Served by an inference server with **dynamic batching** over a graph-optimizing runtime.

| Layer | Recommended | Role |
|---|---|---|
| Inference runtime | **TensorRT** (peak) or **ONNX Runtime** (CUDA / TensorRT execution provider) | Fuse + quantize (FP16/INT8). 3–10× over eager execution. |
| Serving + batching | **NVIDIA Triton Inference Server** | Dynamic batching, multi-model hosting, concurrent instances, metrics. The standard for an encoder fleet. |
| Turnkey embed/rerank | **Hugging Face Text Embeddings Inference (TEI)**, **NVIDIA NIM** | Purpose-built for bi-encoder embedding + cross-encoder reranking with batching — fastest path to a production endpoint. |
| Attention kernels | **FlashAttention-2** (built into the above) | Matters for the 512-token rerank / NER sequences. |

### 6.2 LLM extraction (only if self-hosted)

| Layer | Recommended |
|---|---|
| Serving | **vLLM** (continuous batching), **TensorRT-LLM** (peak), or **SGLang** |
| Quantization | **FP8** (Hopper), **AWQ / GPTQ** (Ampere) — fit bigger models, 2–4× throughput |
| Turnkey | **NVIDIA NIM for LLMs**, **HF TGI** |

Keep the LLM on a **separate** GPU pool from the encoder fleet — its batch dynamics (long generation) are unlike the encoders' (single forward pass), so co-tenancy hurts both.

### 6.3 GPU vector search (extreme scale only)

| Layer | Recommended |
|---|---|
| GPU ANN | **NVIDIA cuVS / CAGRA** (GPU-native graph ANN) or **FAISS-GPU** (IVF-PQ) |
| Foundation | **RAFT** primitives |

Relevant only past the point where CPU index economics break (§7). For the mid-scale target, CPU graph-ANN + product quantization is the correct tool.

### 6.4 Orchestration & observability

- Model pool behind **KServe** or **Ray Serve** on Kubernetes for autoscaling.
- **DCGM exporter → Prometheus** for GPU metrics, folded into the existing observability surface.

---

## 7. Models — current and GPU-enabled upgrades

GPUs do more than speed up today's models; they unlock larger, higher-recall ones that are infeasible on CPU.

| Workload | CPU default | GPU upgrade options | Architectural consequence |
|---|---|---|---|
| Embedding | small bi-encoder, 384-dim | base (768) / large (1024) / multilingual multi-vector encoders | **Storage migration — see box.** |
| Rerank | base cross-encoder | large / v2 multilingual cross-encoders | Higher precision per cut. Swap behind the same contract. |
| NER | small zero-shot tagger | medium / large / multilingual taggers | Better zero-shot recall on the active-schema entity types. |
| LLM extract | external service | self-hosted 7–14 B (quantized) up to 70 B | Removes the external dependency and per-call cost. |

> ### ⚠️ Embedding dimensionality is a storage decision, not a model swap
> Each stored vector occupies a **fixed-width record** in the arena, sized for the current 384-dimension model. A higher-dimensional embedding produces wider vectors that **do not fit the existing record layout** — adopting one is an *arena format migration* (resize the record, re-lay the arena), not a configuration change.
>
> Independently, **the embedding model has an identity stamp** recorded against every stored vector. Changing the model rotates that identity, which invalidates every existing vector and mandates a **full re-embed of the corpus**. The system already models this as a versioned migration — register the new model identity, re-embed (a perfect bulk job for the GPU pool), then retire the old identity. Treat any embedding upgrade as a planned migration: **resize the record layout → register the new model identity → GPU bulk re-embed → retire the old identity.**

---

## 8. GPU hardware requirements

### 8.1 Encoder fleet (embedding + rerank + NER)

All three encoder models together are **under ~4 GB in FP16**. This is a *throughput* problem (many concurrent batches), not a *capacity* problem (huge VRAM).

| GPU | VRAM | Fit |
|---|---|---|
| **NVIDIA L4** | 24 GB | **Sweet spot.** Low power, cost-efficient, modern FP8. Hosts all three encoders with batch headroom. |
| **A10 / A10G** | 24 GB | Common cloud inference GPU; hosts all three encoders. |
| **L40S** | 48 GB | High throughput; room for the encoders *plus* a small quantized LLM. |
| (avoid for prod) T4 | 16 GB | Older generation, reduced batch; dev/low-QPS only. |

A single **L4** comfortably serves the encoder fleet for a mid-size deployment. Use hardware partitioning (MIG on larger cards) or multiple model instances to isolate the three models on one GPU.

### 8.2 LLM extraction (if self-hosted)

| Model size | Quantization | GPU |
|---|---|---|
| 7–8 B | FP16 | A10 (24 GB) / L40S (48 GB) |
| 7–14 B | FP8 / AWQ | L4 (24 GB) / L40S |
| 70 B | FP8 | 2× H100 (80 GB) or 1× H200 (141 GB), tensor-parallel |

### 8.3 GPU vector search (extreme scale only)

The index resides in VRAM. **A100 / H100 (80 GB)** for tens of millions of 384-dim vectors per GPU. Only here do you trade cheap host RAM for expensive VRAM residency.

### 8.4 Interconnect

- **PCIe Gen4/5** suffices for encoder inference (small tensors).
- **NVLink** matters only for multi-GPU tensor-parallel LLM serving or sharded GPU ANN.
- For encoders the practical bottleneck is **host↔device transfer of tokenized batches**, not compute — pin memory and overlap transfers with compute (the serving layer handles this).

---

## 9. Server / node requirements (the non-GPU side still matters)

GPUs accelerate inference; the storage plane's appetite is unchanged. Two node classes:

### 9.1 Storage nodes (stateful, no GPU)

| Resource | Requirement | Rationale |
|---|---|---|
| **CPU** | one fast core per shard + headroom | Thread-per-core execution; tokenization, index search, and vectorized distance math are CPU work. Wide SIMD (AVX-512 / NEON) helps. |
| **RAM** | in-memory indexes + arena page-cache residency; budget ~2–3× live arena size | The vector graph index lives in RAM (~150 B/vector); the arena is memory-mapped and wants to stay in page cache. |
| **Disk** | fast, high-IOPS NVMe with direct-write + durability-barrier support; sized for arena + log retention + snapshots + text index | Durable writes are the write-path floor; the arena is memory-mapped. |
| **Network** | 25–100 GbE | Shard fan-out + feeding batched text to the inference plane. |

### 9.2 GPU inference nodes (stateless)

| Resource | Requirement |
|---|---|
| GPU | per §8 — start with one L4 / A10 for the encoder fleet |
| CPU | enough cores to keep tokenization ahead of the GPU (8–16) |
| RAM | 2–4× total model footprint for staging + batch buffers (32–64 GB) |
| Network | 25–100 GbE — must not starve the GPU of input batches |
| Disk | model-weight cache only; otherwise stateless |

> **Co-location vs separation.** A single-box dev/staging setup can run the storage process and an in-process GPU provider on one GPU machine. **Production should separate the GPU pool from the storage nodes** so each scales independently and an inference-node failure never touches durable data.

---

## 10. Capacity planning

Conservative planning anchors (FP16, dynamic batching, L4-class GPU). **Measure on your hardware — these are anchors, not guarantees.**

- **Embedding:** ~2–5 k texts/s per GPU at batch 32–64; sub-10 ms inside the batch window.
- **Rerank (~50 pairs/query):** ~200–600 queries/s per GPU, depending on candidate-text length. **Usually the GPU sizing driver, because rerank is always-on.**
- **NER:** asynchronous; size for sustained ingest rate, not latency. ~0.5–2 k docs/s per GPU.

### Sizing procedure
1. Measure peak **query QPS** and peak **write QPS**.
2. Rerank load = `query_QPS × 50 pairs` → the dominant GPU term → rerank GPU count.
3. Embedding load = `(write_QPS + query_QPS) × (1 − cache_hit_rate)` → embedding GPU count (usually shares the encoder GPU).
4. Add NER for ingest; size the LLM pool separately if self-hosting.
5. **The embedding cache hit rate is the biggest lever** — every hit is a GPU call avoided. Maximize cache residency before adding GPUs.

### Latency budget (production hardware, GPU tier)
- Embedding, cache hit: negligible (metadata lookup). Cache miss, batched: a few milliseconds including the window.
- Rerank, 50 pairs batched: single-digit milliseconds.
- Query p99 with GPU rerank: well below the CPU-bound tail; dominated by the batch-window length you choose.

---

## 11. Deployment tiers

| Tier | Inference plane | Storage plane | Use |
|---|---|---|---|
| **Dev** | CPU only | 1–2 shards, single box | local development; inference slow but correct |
| **Staging** | 1× L4 / A10, single replica | 2–8 shards, same box | realistic latency, single node |
| **Production-mid** | 1–2× L4 / L40S encoder pool, separate nodes | storage nodes sized on corpus | thousands of QPS, disaggregated planes |
| **Production-large** | Autoscaled encoder pool + separate LLM pool (H100) + optional GPU ANN | multi-node sharded cluster | tens of millions of memories, high QPS, self-hosted LLM |

---

## 12. Migration path (incremental; each step ships independently)

1. **Cache first (no GPU).** Tune the embedding cache and the batch window. For read-heavy, repetitive traffic this often defers GPU adoption entirely.
2. **Remote embedding.** Stand up a GPU-backed embedding endpoint and point the inference contract's provider at it. Embedding leaves the storage CPU; wire the edge-tier batch window.
3. **Remote rerank.** Same pattern. **Highest impact**, because rerank is always-on and currently the heaviest CPU term.
4. **Remote NER.** Move the tagger to the GPU pool — easy, since it is asynchronous and off the latency path.
5. **Self-hosted LLM (optional).** Separate large-VRAM pool; redirect the extraction backend.
6. **GPU vector search (only at extreme scale).** Migrate the index to GPU-native ANN per §7.

Steps 2–4 are the 80/20: the entire encoder fleet on one GPU removes essentially all inference tail latency.

---

## 13. Risks & caveats (read before committing budget)

- **The storage-runtime constraint is absolute.** Inference is never co-scheduled with a storage core (§3). The disaggregated inference tier is not optional for production.
- **An embedding-dimension change is a storage migration**, not a config change (§7) — fixed-width record resize + full re-embed.
- **An embedding-model change forces a full re-embed**, because stored vectors are bound to the model identity. Plan it as a versioned bulk migration on the GPU pool.
- **Batching trades latency for throughput.** The batch window is the floor on per-request latency — too small under-utilizes the GPU, too large widens p99. Tune to the SLA.
- **Tokenization can bottleneck.** It stays on CPU at the edge tier; provision cores to keep the GPU fed.
- **Cold start.** Engine build + model load is slow; pre-warm replicas before they take traffic.
- **Cost discipline.** A single L4 covers the encoder fleet for most deployments — do not over-provision. The LLM pool is the expensive tier; quantize and right-size.

---

## 14. What to measure (before and after GPU adoption)

- Embedding throughput vs batch size (CPU baseline → GPU) and sensitivity to cache hit rate.
- Rerank latency at ~50 pairs across candidate-text lengths — the always-on cost.
- End-to-end query p50/p99 with rerank on: CPU vs GPU plane.
- Ingest throughput (write → entities persisted) for the NER stage.
- GPU utilization / queue depth under sustained peak QPS.

---

# Part II — The storage plane: storage, memory, indexes

The inference plane (Part I) scales on traffic. The storage plane scales on **corpus size and write rate**, and it is where durability, the memory hierarchy, and the index structures live. This part defines how it is laid out, what must be in RAM, how the indexes consume resources, and how it scales out.

## 15. The durable substrate (what each shard owns on disk)

A shard is **share-nothing**: it owns a complete, independent copy of every storage structure for its slice of the corpus. Nothing is shared between shards on disk. The structures, by role:

| Structure | Role | Growth driver | Residency |
|---|---|---|---|
| **Vector arena** | the source-of-truth vectors, in fixed-width records | corpus size (linear) | memory-mapped; hot working set in page cache |
| **Write-ahead log (WAL)** | durability — every mutation is logged before acknowledgment | write rate × retention window | sequential on disk; not cached |
| **Metadata store** (B-tree) | per-memory metadata + the typed graph (entities, statements, relations, predicates) + audit, idempotency, content-hash, schema state | corpus size + graph density | on disk; hot pages in page cache |
| **Vector graph index** (ANN) | navigable graph for similarity search | corpus size | **fully resident in RAM** |
| **Lexical text indexes** | inverted indexes for keyword/BM25 retrieval (memory text + statement text) | corpus size + text length | memory-mapped; hot segments in page cache |
| **Extraction cache** | memoized results of the expensive LLM-extraction tier | extraction volume | small; mostly on disk |
| **Snapshots** | point-in-time consistent copies for backup/restore and fast recovery | snapshot frequency × retention | cold on disk / object storage |

The design rule: **the source of truth is the arena + the WAL.** Every index (vector graph, lexical, graph traversal) is a *derived* structure that can be rebuilt from the arena and metadata. This is what makes recovery and re-sharding tractable — indexes are caches, not ground truth.

## 16. The memory hierarchy — what must be in RAM, what can page

Storage-node RAM sizing is the single most important capacity decision. Three tiers:

1. **Must be resident (RAM, non-negotiable).** The **vector graph index** lives entirely in RAM — its random-access traversal pattern makes paging it to disk pathological. Budget on the order of **~150 bytes per vector** for the graph structure (edges + back-pointers), *on top of* the vectors themselves. This is the dominant fixed RAM cost and the first thing to exceed a node's memory at scale.

2. **Wants to be resident (page cache, hot working set).** The **arena** and the **metadata B-tree** and the **lexical indexes** are memory-mapped; they are served from the OS page cache. You don't size RAM to hold *all* of them — you size it to hold the **working set**: the slices touched by live traffic (recent + frequently recalled memories). A cold page is a disk read; a warm deployment keeps the working set resident.

3. **Lives on disk (NVMe).** The full arena, full metadata, full lexical indexes, the WAL, and snapshots. Cold data pages in on demand.

**RAM budget per shard ≈ (resident vector graph index) + (working-set page cache for arena + metadata + lexical) + headroom.** A useful planning rule is **2–3× the live arena size** in RAM, but the right number depends on the hot/cold ratio of your workload — a recency-skewed workload needs far less than the full corpus resident.

> **The density lever is product quantization — see §18.** It is what decouples "how many vectors a shard can hold" from "how much RAM the node has."

## 17. The index layer — three structures, three profiles

Retrieval fuses three independently-built indexes (their ranked outputs are combined by reciprocal-rank fusion, then optionally reranked — Part I). Each scales differently:

| Index | Answers | Resource profile | Maintenance |
|---|---|---|---|
| **Vector ANN** (navigable graph + product quantization) | "semantically nearest" | RAM-resident graph; compressed codes for scan; arena for exact re-rank | background graph-maintenance + periodic codebook (re)training |
| **Lexical** (inverted / BM25) | "keyword match" | memory-mapped segments, page-cache served; modest RAM | background segment indexing + merges |
| **Graph** (typed entities / statements / relations) | "connected to" (traversal) | lives in the metadata B-tree; page-cache served | maintained transactionally with writes |

Architectural consequences for scaling:

- **The vector index dominates RAM; the lexical and graph indexes dominate neither.** When a node runs out of memory, it is almost always the vector graph index plus the vector working set — focus capacity work there (RAM, or PQ density, or more shards).
- **Indexes are derived and rebuildable.** Rebuilds (graph maintenance, segment merges, codebook retraining) run as **background work on the storage cores**, off the acknowledgment path. They consume CPU and I/O bandwidth but never block durability. Provision headroom on the storage nodes (CPU + I/O) for this maintenance, especially during bulk ingest.
- **A stale or degraded index never returns wrong data** — it degrades recall, not correctness, and the maintenance workers converge it. This decoupling is what lets the system stay available while indexes rebuild.

## 18. Product quantization — the memory-density lever

The vectors are the bulk of the data, and full-precision vectors are expensive to keep scannable in RAM. **Product quantization (PQ)** compresses each vector into a short code (a handful of bytes) by splitting it into sub-vectors and replacing each with the nearest entry in a learned codebook. The architecture uses PQ as the scan tier and the full-precision arena as the exact re-rank tier:

- **Search** runs over the compact PQ codes (asymmetric distance) to produce candidates cheaply, then **re-ranks the top candidates against the exact vectors in the arena.** Approximate-then-exact: fast and accurate.
- **The density payoff:** PQ codes are a fraction of the size of full vectors, so a shard can keep far more vectors *scannable per gigabyte of RAM* than full-precision search would allow. PQ is the primary lever that raises per-shard capacity without adding nodes.
- **The cost:** a codebook must be **trained** on representative vectors, and **retrained** as the distribution drifts (a background job; an untrained/bootstrap codebook gives degraded recall until it converges). Quantization also introduces a small recall loss, recovered by the exact re-rank step and, in Part I, by the cross-encoder rerank.

**Scaling implication:** before adding storage nodes for capacity, confirm PQ is trained and doing its job — it can multiply per-shard vector capacity and defer horizontal scale-out.

## 19. Sharding — the horizontal scale-out axis

Sharding is **both the parallelism model and the scale-out model**, and it is share-nothing:

- **One shard per core.** Each shard is a single-threaded, non-blocking owner of its complete storage substrate (§15). There is no cross-shard lock and no shared on-disk state. Adding cores/nodes adds shards adds capacity and throughput — linearly, until the network or the fan-out merge becomes the limit.
- **Placement.** A memory belongs to exactly one shard, chosen at write time (e.g. by a stable partition key). Reads for a single known item route directly; corpus-wide reads fan out.
- **Cross-shard queries fan out and merge.** A similarity or hybrid query is sent to every shard, each runs its own local retrieval over its own indexes, and the partial ranked results are **merged at the edge tier** (the same rank-fusion machinery, applied across shards). Latency is the *slowest* shard plus the merge — so balanced shards matter.
- **Scale on two independent axes:** add shards (cores/nodes) for **more corpus** or **more write parallelism**; this is orthogonal to the GPU pool, which scales for **more query/ingest traffic**. The two planes never contend for the same resource.

**When to add a shard/node** (vs. add RAM or lean on PQ): when a single shard's *resident* footprint (vector graph index + working set) approaches the node's RAM, when its NVMe approaches the arena + retention + snapshot budget, or when a single core saturates on write throughput. Until one of those binds, denser shards (more RAM, PQ) are cheaper than more shards.

## 20. Storage capacity planning

Per **1 million memories** at the current 384-dimension vector (planning anchors — measure on your data, since metadata/graph/text sizes are workload-dependent):

| Structure | ~Size / 1 M memories | Tier |
|---|---|---|
| Vector arena (fixed-width records) | ~1.6 GB | NVMe + page cache |
| Vector graph index | ~150 MB | **RAM (resident)** |
| PQ codes | tens of MB | RAM (scan tier) |
| Metadata + typed graph | workload-dependent (often hundreds of MB) | NVMe + page-cache hot pages |
| Lexical indexes | scales with text volume | NVMe + page-cache hot segments |
| WAL | write-rate × retention window | NVMe (sequential) |
| Snapshots | snapshot size × retention | NVMe / object storage |

### Sizing procedure (storage plane)
1. **Corpus size → arena NVMe** (records × fixed width) and **→ resident RAM** (graph index ~150 B/vector + PQ codes).
2. **Hot/cold ratio → working-set RAM** for page cache (arena + metadata + lexical). Recency-skewed workloads need far less than the full corpus.
3. **Write rate → WAL bandwidth and retention** (sustained writes/s × record size × retention seconds) and the I/O headroom for background index maintenance.
4. **Per-shard budget** = the above for one shard's slice; choose shard count so no shard exceeds node RAM (resident + working set) or NVMe.
5. **Verify PQ is trained** before buying RAM/nodes for capacity (§18).

### Storage-node hardware (recap of §9.1, in context)
- **CPU:** one fast core per shard + headroom for background index maintenance and vectorized distance math.
- **RAM:** resident vector index + working-set page cache; plan ~2–3× live arena, adjusted for the hot/cold ratio.
- **NVMe:** high-IOPS, durable-write-capable; sized for arena + WAL retention + snapshots + lexical indexes. The disk's durability latency is the floor on write acknowledgment.
- **Network:** 25–100 GbE for shard fan-out, cross-shard merge, and feeding batched text to the inference plane.

## 21. Durability, recovery & data lifecycle

The storage plane's correctness guarantees are independent of the GPU plane and unaffected by it.

- **Write-before-acknowledge.** No write is acknowledged to the client until its record is durably on disk in the WAL. Durability is the acknowledgment floor — it is why fast, low-latency NVMe with real durability barriers matters more than raw capacity.
- **Single-writer-per-shard.** Each shard has exactly one writer; reads are lock-free against a consistent snapshot. No write contention, no cross-shard coordination.
- **Integrity everywhere.** Records carry checksums; reads verify them and **fail-stop on mismatch** rather than returning corrupt data. Silent corruption is never tolerated.
- **Recovery = replay.** On restart a shard replays its WAL forward from the last checkpoint, verifying integrity and applying idempotently, then rebuilds its derived indexes from the arena + metadata. **Snapshots bound recovery time** by moving the replay start point forward; snapshot frequency trades recovery speed against snapshot I/O.
- **Lifecycle reduces the live set over time.** Background processes — salience **decay**, **consolidation** of related memories, **tombstone** reclamation after a grace window, and slot **compaction** — shrink the working set and reclaim arena space. At extreme scale this lifecycle is what keeps a shard's *live* footprint bounded even as total ingest grows, and it is the natural place to introduce **cold tiering** (archive aged-out shards/segments to object storage) before paying for more hot nodes.

**The end-state scaling picture:** durable, share-nothing storage shards (CPU + RAM + NVMe, sized on corpus and write rate) on one axis; a stateless, autoscaled GPU inference pool (sized on query/ingest traffic) on the other; an edge tier that batches inference, caches embeddings, and merges cross-shard results between them. Each axis scales without the other.

---

### Related architecture
- **The two-plane model** (§1) is the foundation; the storage plane's execution model (§3) is the binding constraint on the inference plane, and the share-nothing shard (§15, §19) is the binding structure of the storage plane.
- **Embedding pipeline** and **hybrid retrieval / rerank placement** — where the inference plane plugs into the read and write paths.
- **Arena, write-ahead log, and recovery** (§15, §21) — the durability substrate; **the vector / lexical / graph index layer** (§17) and **product quantization** (§18) — the memory-density and retrieval structures.
- **Performance targets** — the latency and throughput numbers both planes must satisfy.
