# 01.02 Background and Prerequisites

This file establishes the conceptual background the architecture rests on. Read it if you want to be confident the document's vocabulary and assumptions match yours. Skip ahead to [`03_primitives.md`](03_primitives.md) if you've recently shipped an agent application and built ANN indexes from scratch.

The topics in order:

1. [LLMs, context windows, and the limits of "just give it more context"](#1-llms-context-windows-and-the-limits-of-just-give-it-more-context)
2. [Vectors, embeddings, and similarity](#2-vectors-embeddings-and-similarity)
3. [Approximate nearest neighbor search and HNSW](#3-approximate-nearest-neighbor-search-and-hnsw)
4. [The vector database landscape](#4-the-vector-database-landscape)
5. [Agent memory frameworks](#5-agent-memory-frameworks)
6. [Async runtimes: work-stealing vs thread-per-core](#6-async-runtimes-work-stealing-vs-thread-per-core)
7. [Linux I/O primitives](#7-linux-io-primitives)

---

## 1. LLMs, context windows, and the limits of "just give it more context"

An LLM's **context window** is the maximum amount of text the model can attend to in a single forward pass. As of early 2026, frontier models advertise context windows ranging from ~128K tokens (older deployments) to ~2M tokens (longest-context offerings). Marketing copy implies that long-context models obviate the need for retrieval and memory systems: dump everything in, let attention figure it out.

In practice this doesn't work, for four reasons.

### 1.1 Cost

Inference cost scales linearly with input tokens. A 1M-token context per call, at typical 2026 frontier-model API pricing, is multiple dollars per turn. An agent making dozens of turns per session multiplies that. Cost remains the single most binding constraint on agent deployment, and trading capacity for cost via long context is rarely the right move.

### 1.2 Latency

The first token's latency scales with prefill compute, which is `O(n²)` in sequence length on standard transformer attention. Even with efficient attention variants and KV caching, multi-megabyte contexts add seconds of TTFT (time to first token). For an agent that interacts with a user in real time, multi-second prefill latency makes the system unusable.

### 1.3 Attention degradation at length

Multiple studies have shown long-context LLMs perform worse at retrieving information from the middle of long contexts — the so-called ["lost in the middle"](https://arxiv.org/abs/2307.03172) effect (Liu et al., 2023). Cramming more content into context doesn't proportionally increase recall; the model is biased toward content near the start and end of the input. Newer models reduce but do not eliminate this effect.

### 1.4 No structure

Even if a long-context model could perfectly recall everything, an agent needs *structured* access to its memory:

- "What memories are most causally upstream of this observation?" — graph traversal.
- "Have I seen anything semantically equivalent to X?" — similarity search with confidence calibration.
- "What's the path from my current state to this goal?" — search over a state space.

A flat token sequence is the wrong shape for any of these. The substrate approach inverts the trade-off: keep the context window small (a few thousand tokens of *relevant* memories selected by the substrate), and let the substrate carry unlimited structured state outside the model.

---

## 2. Vectors, embeddings, and similarity

An **embedding model** is a neural network that maps a piece of text (or other content) to a dense vector of fixed dimensionality. The geometric relationship between vectors approximates the semantic relationship between texts: similar meanings produce similar vectors, by some distance measure (typically cosine similarity or dot product).

### 2.1 The model we use

Brain uses [`bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5) from BAAI's [FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding) project. This model maps English text up to 512 tokens to a 384-dimensional `f32` vector. It is MIT-licensed, runs efficiently on CPU (~5–10 ms per encode on commodity hardware), and ships under the BAAI General Embedding family.

The full justification for this model — the alternatives considered, the trade-offs accepted — is in [04. Embedding Layer](../04_embedding_layer/). For architecture purposes, what matters is:

- The model is local: text never leaves the substrate's process. We are the layer that does this.
- The vector dimensionality (384) and dtype (`f32`) determine arena slot size and ANN index sizing.
- The model produces L2-normalized vectors (or we normalize them); cosine similarity reduces to dot product.

### 2.2 What this means for the architecture

Every memory is, internally, a 1536-byte payload (384 × 4 bytes) plus metadata. Similarity between two memories is a dot product (~400 floating-point operations on each pair). "Similar to a cue" means "high dot product with the cue vector". Storage and indexing decisions are dominated by the size and distribution of these vectors.

The system is non-modal in v1: we handle English text only. Multi-modal storage (images, audio) is a possible v2 extension; the storage and index layers don't care about modality, but the embedding layer and cognitive operations would need rework.

---

## 3. Approximate nearest neighbor search and HNSW

The naïve way to find the top-k vectors most similar to a cue is brute-force: compute the dot product against every stored vector, sort, return the top-k. Cost: `O(N × d)` per query, where `N` is the number of stored vectors and `d` is the dimensionality. For 1M vectors at 384-dim this is ~400M ops per query — workable on modern CPUs (~1 ms with SIMD) but doesn't scale to 10⁷ or beyond.

### 3.1 ANN as a quality-vs-speed trade-off

**Approximate nearest neighbor (ANN)** algorithms trade recall (the fraction of true neighbors found) for speed. By accepting that some queries will miss a few of the absolute nearest neighbors, ANN algorithms achieve query times that scale much better than brute-force — often `O(log N)` per query.

For most cognitive applications, exact recall is not required. An agent asking "what did I learn that's similar to this question?" benefits more from sub-millisecond latency than from absolute correctness; missing the 11th-best match in favor of the 10th is rarely consequential.

### 3.2 HNSW

The dominant ANN algorithm in production is **HNSW** (Hierarchical Navigable Small World), introduced by [Malkov & Yashunin in arXiv:1603.09320](https://arxiv.org/abs/1603.09320) (refined through 2018 and published in IEEE TPAMI).

A one-paragraph summary: HNSW builds a multi-layer graph where each node is a vector. Higher layers are sparse (long-range links), lower layers are dense (short-range links). To search, start at the top layer and greedily walk toward the cue, descending to the next layer when no neighbor is closer than the current node. This finds approximate neighbors in `O(log N)` graph hops, each requiring a few dot products. For 1M vectors, top-10 search completes in ~100–500 µs on a modern CPU at >95% recall.

The tradeoff parameters are:

- **`M`** — max edges per node in the graph (typically 16–48).
- **`ef_construction`** — search width during build (controls index quality, also build time).
- **`ef_search`** — search width during query (controls recall-vs-latency on the hot path).

We use HNSW because:

- It's the best-in-class ANN algorithm by recall-vs-latency Pareto frontier on dense vectors, validated across benchmarks like [ann-benchmarks](https://github.com/erikbern/ann-benchmarks).
- It supports incremental insertion — important because an agent's memory grows continuously, not in batches.
- It has well-understood failure modes (especially around deletion and high-dim "hubness" effects).
- Mature implementations exist in Rust ([`hnsw_rs`](https://github.com/jean-pierreBoth/hnswlib-rs)).

The ANN substrate is detailed in [06. ANN Index](../06_ann_index/).

---

## 4. The vector database landscape

Several open-source projects implement vector storage and search. Brain is not a vector database, but we owe a clear comparison so the reader knows where Brain sits in the landscape.

### 4.1 Qdrant

[**Qdrant**](https://github.com/qdrant/qdrant), Apache 2.0, written in Rust. Self-described as "Vector Search Engine for the next generation of AI applications." Fast, well-engineered, focused on filtered ANN search with rich metadata filters. The closest existing system to Brain in implementation language and quality bar.

### 4.2 Milvus

[**Milvus**](https://github.com/milvus-io/milvus), written in Go and C++. Self-described as "high-performance vector database built for scale." Larger architecture, designed for very-large-scale deployments with multi-tier storage. Powers AI applications by efficiently organizing and searching vast amounts of unstructured data.

### 4.3 Weaviate

[**Weaviate**](https://github.com/weaviate/weaviate), written in Go. Self-described as "open-source, cloud-native vector database that stores both objects and vectors, enabling semantic search at scale." Combines vector similarity search with keyword filtering, retrieval-augmented generation, and reranking in a single query interface. Notable for built-in integrated embedding model support (OpenAI, Cohere, HuggingFace).

### 4.4 Chroma

[**Chroma**](https://github.com/chroma-core/chroma), Apache 2.0. Self-described as "the open-source data infrastructure for AI." Lighter-weight than Milvus or Weaviate, emphasizes developer ergonomics and embeddability.

### 4.5 LanceDB

[**LanceDB**](https://github.com/lancedb/lancedb), built on the Lance columnar format. Self-described as "the multimodal AI lakehouse" — designed for fast, scalable, and production-ready vector search over multimodal data. Notable for its columnar approach and multi-modal focus.

### 4.6 What they have in common

All five are vector databases. Their primary abstraction is *the collection of vectors*. Cognitive operations (recall, plan, reason) and write-side concerns (embedding ownership, salience tracking, decay, consolidation) are out of scope for them. An agent built on top of any of them needs the additional cognitive layer to be assembled by the application.

Brain takes the opposite frame: vectors are an internal implementation detail of a cognitive substrate. The substrate's primary abstraction is *the agent's memory*, and the API speaks in cognitive operations. See [`08_comparison.md`](08_comparison.md) for the side-by-side comparison.

---

## 5. Agent memory frameworks

The closest existing systems to Brain in *intent* are agent memory frameworks built at the application layer.

### 5.1 Letta

[**Letta**](https://github.com/letta-ai/letta) (formerly MemGPT). Self-described: "Build AI with advanced memory that can learn and self-improve over time." Provides a stateful agent server with hierarchical memory: "core memory" lives in the LLM context, "archival memory" lives in a vector store. Implemented as a Python-based stateful agent platform with full agent runtime, not just memory.

### 5.2 Mem0

[**Mem0**](https://github.com/mem0ai/mem0). Self-described as "The Memory Layer for Personalized AI." A library that adds memory operations (add, search, update) on top of existing vector and metadata stores. More library-shaped than Letta's full-platform approach.

### 5.3 LangChain

[**LangChain**](https://github.com/langchain-ai/langchain). Self-described as "the agent engineering platform." Memory is one feature among many; the framework's main contribution is composition of LLMs with tools and stores.

### 5.4 LlamaIndex

[**LlamaIndex**](https://github.com/run-llama/llama_index). Open-source framework for building agentic applications, with strong indexing and retrieval primitives, plus a separate enterprise document-processing platform.

### 5.5 How they differ from Brain

These frameworks differ from Brain in two consequential ways:

1. They are **application frameworks**, not infrastructure. They run in the agent's process, share the agent's resources, and lean on whatever underlying stores you point them at. They do not own the data path.
2. They target **Python-first developer experience**. Brain targets a wire-protocol-first integration model, where the substrate is a separate process — a database — and any language can talk to it.

A reasonable mental model: Letta and Mem0 are to Brain as SQLAlchemy is to PostgreSQL. The frameworks are useful at the application layer; Brain is what they would talk to if a substrate existed at this level of the stack.

---

## 6. Async runtimes: work-stealing vs thread-per-core

Brain is implemented in Rust on Linux. The choice of async runtime is consequential for the latency floor; this section explains the choice.

### 6.1 Work-stealing runtimes

[**Tokio**](https://github.com/tokio-rs/tokio) is the canonical example. From its README, Tokio is "A runtime for writing reliable, asynchronous, and slim applications with the Rust programming language. It is: Fast (zero-cost abstractions, bare-metal performance); Reliable (leverages Rust's ownership, type system, and concurrency model); Scalable (minimal footprint, handles backpressure and cancellation naturally)."

Tokio maintains a global pool of worker threads and a global queue of pending tasks. When a worker finishes a task, it picks up the next available task — possibly stealing from another worker's local queue if its own is empty. This produces excellent average throughput and adapts gracefully to workload imbalance.

The cost is tail latency. Every time a task migrates between threads, you pay cache-line invalidation, memory synchronization, and potentially TLB shootdowns. For workloads where median latency matters, this is invisible noise. For workloads where p99 and p99.9 matter — where you've promised sub-millisecond latency under load — work-stealing is a liability you can't fully tune away.

### 6.2 Thread-per-core runtimes

Thread-per-core runtimes invert the model. Each CPU core owns a fixed slice of the work. Tasks never migrate. State that's owned by core 5 stays on core 5; only core 5's L1/L2 cache ever holds it. There are no work-stealing costs because there's no work-stealing.

Two production-grade thread-per-core runtimes exist for Rust:

**[Glommio](https://github.com/DataDog/glommio)**, from DataDog. From its README: "Glommio is a Cooperative Thread-per-Core crate for Rust & Linux based on `io_uring`. Like other rust asynchronous crates, it allows one to write asynchronous code that takes advantage of rust `async`/`await`, but unlike its counterparts, it doesn't use helper threads anywhere." Mature, used in production at DataDog for time-series ingestion. Supported Linux kernel: 5.8+. Requires at least 512 KiB of locked memory for `io_uring`.

**[Monoio](https://github.com/bytedance/monoio)**, from ByteDance. Self-described as a "thread-per-core Rust runtime with io_uring/epoll/kqueue." Similar shape to Glommio, slightly different design choices (supports non-Linux platforms via epoll/kqueue). Smaller community, slightly less mature documentation.

### 6.3 The Seastar lineage

Both Rust thread-per-core runtimes draw on the [**Seastar**](https://github.com/scylladb/seastar) framework, the C++ substrate underlying [ScyllaDB](https://github.com/scylladb/scylladb). From the Seastar README: "SeaStar is an event-driven framework allowing you to write non-blocking, asynchronous code in a relatively straightforward manner (once understood). It is based on futures."

ScyllaDB built its low-tail-latency reputation on this architecture; we're applying the same lesson to a different problem domain.

### 6.4 The choice

Brain uses **Glommio** for the runtime and follows the thread-per-core discipline throughout the codebase. Reasons:

- It's the more mature of the two Rust options, with documented production use at DataDog.
- Documentation and ecosystem are stronger than Monoio's.
- The Linux-only constraint (since Glommio is io_uring-only) matches our deployment target.

The trade-off accepted: Linux-only deployment. For our deployment target (server-side infrastructure), this is not a constraint — production servers are Linux anyway.

---

## 7. Linux I/O primitives

Brain depends heavily on Linux-specific I/O facilities. Each is briefly introduced here; deeper coverage is in [05. Storage: Arena & WAL](../05_storage_arena_wal/).

### 7.1 mmap

[**`mmap(2)`**](http://man7.org/linux/man-pages/man2/mmap.2.html) maps a file into the process's virtual address space. Reads from the mapped region trigger demand-paging from the page cache. We use mmap for the vector arena, so that vectors are accessible as zero-copy `&[f32]` slices and the OS handles working-set vs cold-set transitions automatically.

### 7.2 madvise

[**`madvise(2)`**](http://man7.org/linux/man-pages/man2/madvise.2.html) provides hints to the kernel about access patterns over a memory range. We use `MADV_RANDOM` (HNSW search has unpredictable access patterns) and `MADV_DONTDUMP` (exclude large arenas from core dumps).

`MADV_HUGEPAGE` is **not** applicable to our arena. Per the [Linux Transparent Hugepage docs](https://github.com/torvalds/linux/blob/master/Documentation/admin-guide/mm/transhuge.rst):

> "Currently THP only works for anonymous memory mappings and tmpfs/shmem. But in the future it can expand to other filesystems."

Our arena lives on a regular filesystem (ext4, xfs, btrfs), so the `MADV_HUGEPAGE` hint has no effect. See [`05_hardware.md`](05_hardware.md) §3 for the operational implications.

### 7.3 fallocate

[**`fallocate(2)`**](http://man7.org/linux/man-pages/man2/fallocate.2.html) preallocates blocks for a file without writing them. Used to grow the arena and pre-size WAL segments without per-block metadata churn. The flags are defined in [`include/uapi/linux/falloc.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/falloc.h).

### 7.4 mremap

[**`mremap(2)`**](http://man7.org/linux/man-pages/man2/mremap.2.html) extends or moves an existing memory mapping. Used when the arena needs to grow beyond its current capacity.

### 7.5 fdatasync

[**`fdatasync(2)`**](http://man7.org/linux/man-pages/man2/fdatasync.2.html) flushes a file's data (but not metadata) to durable storage. Used as the durability barrier for the WAL. Faster than `fsync(2)` because it skips inode timestamp updates.

### 7.6 O_DIRECT

`O_DIRECT` is a flag to [`open(2)`](http://man7.org/linux/man-pages/man2/open.2.html) that bypasses the page cache, performing direct DMA between user-space buffers and the storage device. Imposes alignment requirements on offsets and buffers (typically 512 bytes). Used for the WAL, where page cache would be pure overhead.

The flag is defined in the kernel headers at [`include/uapi/asm-generic/fcntl.h`](https://github.com/torvalds/linux/blob/master/include/uapi/asm-generic/fcntl.h):

```c
#define O_DIRECT    00040000   /* direct disk access hint */
```

### 7.7 io_uring

[**`io_uring`**](https://github.com/axboe/liburing) is the modern Linux async I/O interface, introduced in kernel 5.1 and continually expanded since. Replaces the older `aio(7)` interface. `io_uring` supports submitting multiple I/O operations in one syscall and harvesting completions asynchronously, with optional polling modes for lowest-latency operation. We use `io_uring` throughout for both the wire-protocol I/O and for the storage layer's writes.

### 7.8 pwritev2 with RWF_DSYNC

[**`pwritev2(2)`**](http://man7.org/linux/man-pages/man2/pwritev2.2.html) is vectored writes with per-call sync semantics. The `RWF_DSYNC` flag is defined in [`include/uapi/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h):

```c
#define RWF_DSYNC   ((__force __kernel_rwf_t)0x00000002)
```

This lets a single write request both transfer data and durably commit it, avoiding a separate `fdatasync` call. Available via [`io_uring_prep_writev2`](https://github.com/axboe/liburing/blob/master/man/io_uring_prep_writev2.3) in liburing.

### 7.9 FICLONE (reflink)

[**`FICLONE`** and `FICLONERANGE`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h) are ioctl operations for reflink — block-level copy-on-write between files. They give us instant snapshots without copying data.

Reflink support varies by filesystem:

- **btrfs** — supports reflink intrinsically, since the [filesystem is itself copy-on-write](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/btrfs.rst).
- **xfs** — supports reflink with `mkfs.xfs -m reflink=1`. The reflink option is the default in modern xfsprogs.
- **ext4** — does **not** support reflink in any common configuration. ext4 falls back to full file copies for snapshots.

The filesystem choice is documented in [`05_hardware.md`](05_hardware.md) §4.

### 7.10 Further reading

If you are unfamiliar with these primitives, [The Linux Programming Interface](http://man7.org/tlpi/) by Michael Kerrisk is the standard reference; the man pages linked above are the authoritative source.

---

*Continue to [`03_primitives.md`](03_primitives.md) for the cognitive primitives.*
