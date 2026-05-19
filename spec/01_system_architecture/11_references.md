# 01.11 References and Further Reading

This file collects the canonical references for the architecture, organized by topic. The links are stable URLs to authoritative sources; many are GitHub repositories where the source code or documentation lives.

Where references appeared inline in earlier sections of this spec, they are repeated here for ease of finding.

---

## 1. Cognitive science

### Memory and forgetting

- **Hermann Ebbinghaus, "Über das Gedächtnis" (1885)** — the original work on the forgetting curve. The exponential-decay functional form Brain uses for salience decay derives from Ebbinghaus's measurements. [Wikipedia summary](https://en.wikipedia.org/wiki/Forgetting_curve).

- **Endel Tulving, "Episodic and Semantic Memory" (1972)** — the original distinction between episodic memory (specific events) and semantic memory (general knowledge). Brain's `MemoryKind` enum carries forward this distinction.

- **Alan Baddeley, "Working memory" (1992)** — the multi-component working memory model. Influences Brain's design of the working set as separate from long-term storage.

### Vector representation of cognition

- **Pentti Kanerva, "Hyperdimensional Computing" (2009)** — the foundational paper for representing cognitive structures in high-dimensional vectors. Brain's VSA algebra (bind, bundle, unbind) traces to this line of work.

- **Tony Plate, "Holographic Reduced Representations" (1995)** — circular convolution as the bind operation. Used in `REASON`.

---

## 2. ANN and HNSW

- **Yu. A. Malkov & D. A. Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"** — the HNSW paper. [arXiv:1603.09320](https://arxiv.org/abs/1603.09320). Original 2016 preprint, refined through 2018, published in IEEE TPAMI.

- **`hnsw_rs`** — Rust implementation of HNSW. [GitHub: jean-pierreBoth/hnswlib-rs](https://github.com/jean-pierreBoth/hnswlib-rs). Brain's ANN layer is built on this crate.

- **ann-benchmarks** — comparative benchmark suite for ANN algorithms. [GitHub: erikbern/ann-benchmarks](https://github.com/erikbern/ann-benchmarks). Used as a reference for HNSW parameter tuning.

- **Liu et al., "Lost in the Middle: How Language Models Use Long Contexts" (2023)** — empirical study of long-context degradation. [arXiv:2307.03172](https://arxiv.org/abs/2307.03172). Cited in [`02_background.md`](02_background.md) §1.3.

---

## 3. Embedding models

- **BAAI FlagEmbedding project** — the source of `bge-small-en-v1.5`. [GitHub: FlagOpen/FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding).

- **`bge-small-en-v1.5` model card** — [HuggingFace: BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5). MIT-licensed, 384-dim output.

- **HuggingFace `tokenizers`** — fast WordPiece tokenizer used by Brain. [GitHub: huggingface/tokenizers](https://github.com/huggingface/tokenizers).

- **HuggingFace `candle`** — Rust ML framework used by Brain for inference. [GitHub: huggingface/candle](https://github.com/huggingface/candle).

---

## 4. Vector database landscape

- **Qdrant** — Rust vector search engine. [GitHub: qdrant/qdrant](https://github.com/qdrant/qdrant). Apache 2.0.

- **Milvus** — Go/C++ vector database for scale. [GitHub: milvus-io/milvus](https://github.com/milvus-io/milvus).

- **Weaviate** — Go vector database with integrated embedding model support. [GitHub: weaviate/weaviate](https://github.com/weaviate/weaviate).

- **Chroma** — embeddable AI data infrastructure. [GitHub: chroma-core/chroma](https://github.com/chroma-core/chroma). Apache 2.0.

- **LanceDB** — multimodal AI lakehouse on the Lance columnar format. [GitHub: lancedb/lancedb](https://github.com/lancedb/lancedb).

---

## 5. Agent memory frameworks

- **Letta (formerly MemGPT)** — stateful agent server with hierarchical memory. [GitHub: letta-ai/letta](https://github.com/letta-ai/letta).

- **Mem0** — memory layer for personalized AI. [GitHub: mem0ai/mem0](https://github.com/mem0ai/mem0).

- **LangChain** — agent engineering platform. [GitHub: langchain-ai/langchain](https://github.com/langchain-ai/langchain).

- **LlamaIndex** — open-source framework for agentic applications. [GitHub: run-llama/llama_index](https://github.com/run-llama/llama_index).

---

## 6. Graph databases

- **Neo4j** — the canonical graph database. [neo4j.com](https://neo4j.com/). Cypher query language.

- **Memgraph** — in-memory graph database. [memgraph.com](https://memgraph.com/).

---

## 7. Async runtimes and thread-per-core systems

- **Tokio** — work-stealing async runtime for Rust. [GitHub: tokio-rs/tokio](https://github.com/tokio-rs/tokio). The dominant runtime in the Rust ecosystem; Brain does not use it.

- **Glommio** — DataDog's thread-per-core async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio). Brain's runtime. Linux-only, requires kernel ≥ 5.8.

- **Monoio** — ByteDance's thread-per-core async runtime. [GitHub: bytedance/monoio](https://github.com/bytedance/monoio). Considered as an alternative to Glommio; Glommio chosen for maturity.

- **Seastar** — C++ thread-per-core framework underlying ScyllaDB. [GitHub: scylladb/seastar](https://github.com/scylladb/seastar). The original lineage that Glommio and Monoio draw from.

- **ScyllaDB** — the production demonstration that thread-per-core gives sub-millisecond p99 at scale. [GitHub: scylladb/scylladb](https://github.com/scylladb/scylladb).

---

## 8. Linux I/O primitives

The authoritative source for Linux system calls is the kernel itself. The man pages at [man7.org](http://man7.org/) are the canonical documentation; the kernel source on GitHub provides the headers.

### io_uring

- **`liburing`** — the userspace library for io_uring. [GitHub: axboe/liburing](https://github.com/axboe/liburing). Maintained by Jens Axboe (the io_uring author).

- **`io_uring_prep_writev2`** — the man page for the vectored-write submission helper. [Source on GitHub](https://github.com/axboe/liburing/blob/master/man/io_uring_prep_writev2.3).

### Kernel UAPI headers

Used to get exact constants:

- **`include/uapi/linux/fs.h`** — definitions for `RWF_DSYNC`, `FICLONE`, `FICLONERANGE`. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

- **`include/uapi/asm-generic/fcntl.h`** — definitions for `O_DIRECT`. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/asm-generic/fcntl.h).

- **`include/uapi/linux/falloc.h`** — definitions for `fallocate` flags. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/linux/falloc.h).

### Linux kernel documentation

- **`Documentation/admin-guide/mm/transhuge.rst`** — Transparent Hugepage documentation. The source for the claim that THP doesn't apply to file-backed mmaps on regular filesystems. [Source on GitHub](https://github.com/torvalds/linux/blob/master/Documentation/admin-guide/mm/transhuge.rst).

- **`Documentation/filesystems/btrfs.rst`** — btrfs documentation, including reflink behavior. [Source on GitHub](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/btrfs.rst).

### Reference book

- **Michael Kerrisk, "The Linux Programming Interface" (No Starch Press, 2010).** [tlpi book site](http://man7.org/tlpi/). The standard reference for Linux system programming. Older than current kernels but the foundational concepts haven't changed.

---

## 9. Rust crates Brain depends on

### Core runtime and storage

- **`glommio`** — async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio).
- **`redb`** — embedded ACID key-value store. [GitHub: cberner/redb](https://github.com/cberner/redb).
- **`rkyv`** — zero-copy structured serialization. [GitHub: rkyv/rkyv](https://github.com/rkyv/rkyv).
- **`bytemuck`** — safe bit-cast operations. [GitHub: Lokathor/bytemuck](https://github.com/Lokathor/bytemuck).
- **`hnsw_rs`** — HNSW implementation. [GitHub: jean-pierreBoth/hnswlib-rs](https://github.com/jean-pierreBoth/hnswlib-rs).

### Concurrency

- **`crossbeam-epoch`** — epoch-based memory reclamation. [GitHub: crossbeam-rs/crossbeam](https://github.com/crossbeam-rs/crossbeam).
- **`arc-swap`** — atomic swap of `Arc` values. [GitHub: vorner/arc-swap](https://github.com/vorner/arc-swap).

### Math and SIMD

- **`matrixmultiply`** — fast matrix multiplication. [GitHub: bluss/matrixmultiply](https://github.com/bluss/matrixmultiply).
- **`wide`** — portable SIMD wrappers. [GitHub: Lokathor/wide](https://github.com/Lokathor/wide).

### ML

- **`candle`** — HuggingFace's Rust ML framework. [GitHub: huggingface/candle](https://github.com/huggingface/candle).
- **`tokenizers`** — fast tokenization. [GitHub: huggingface/tokenizers](https://github.com/huggingface/tokenizers).

### Networking and protocol

- **`rustls`** — pure-Rust TLS. [GitHub: rustls/rustls](https://github.com/rustls/rustls).

### Hashing

- **BLAKE3** — content fingerprinting hash. [GitHub: BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

### Configuration

- **`figment`** — layered configuration. [GitHub: SergioBenitez/Figment](https://github.com/SergioBenitez/Figment).

---

## 10. Standards and specifications

### Identifiers

- **RFC 9562** — UUID Formats including UUIDv7. [datatracker.ietf.org/doc/rfc9562](https://datatracker.ietf.org/doc/rfc9562/). Brain uses UUIDv7 for `agent_id`, `request_id`, and persistent shard identifiers.

### Wire conventions

- **RFC 2119** — Key words for use in RFCs to Indicate Requirement Levels. [datatracker.ietf.org/doc/html/rfc2119](https://datatracker.ietf.org/doc/html/rfc2119). MUST, SHOULD, MAY semantics used throughout the spec.

### Hashing

- **CRC32C (Castagnoli polynomial)** — used for header and payload checksums in the wire protocol. The polynomial is 0x1EDC6F41; SSE 4.2 and ARMv8.0+ have hardware acceleration.

---

## 11. Background on database design

These aren't directly cited in the architecture but inform the design philosophy.

- **Edgar F. Codd, "A Relational Model of Data for Large Shared Data Banks" (1970).** The paper that introduced relational databases. Brain follows Codd's separation of declarative query from imperative execution, applied to a different abstraction (cognition rather than relations).

- **Pat Helland, "Life beyond Distributed Transactions" (2007).** On building scalable systems without distributed transactions. Influences Brain's per-shard linearizability with eventual cross-shard consistency.

- **Martin Kleppmann, "Designing Data-Intensive Applications" (O'Reilly, 2017).** A modern survey of database design. Useful background for the trade-offs we navigate.

---

## 12. Source for these specs

The Brain specifications themselves are versioned alongside the code. Each document includes a status block at the top (see [`README.md`](README.md)) identifying its version.

The full spec series is structured as documents 00 through 16, organized into per-spec directories. See [`../00_master_overview/`](../00_master_overview/) for the index.

---

## 13. How to read further

Different readers will want different next steps:

- **If you're going to implement Brain or a part of it:** read [02. Data Model](../02_data_model/) next, then the relevant detail spec for your component.
- **If you're going to write a client SDK:** read [03. Wire Protocol](../03_wire_protocol/) and [13. SDK Design](../13_sdk_design/).
- **If you're going to operate Brain in production:** read [14. Observability + Operations](../14_observability_ops/) and [15. Failure Modes + Recovery](../15_failure_recovery/).
- **If you're evaluating Brain against alternatives:** [`08_comparison.md`](08_comparison.md) is the place to start.
- **If you're building an agent application that will use Brain:** read [03. Wire Protocol](../03_wire_protocol/) §10 (handshake), [09. Cognitive Operations](../09_cognitive_operations/) (semantics), and [13. SDK Design](../13_sdk_design/) (the language-level interface).

---

*This concludes Spec 01. The next document is [00. Master Overview & Glossary](../00_master_overview/), which is technically numbered first but reads better after this architecture spec.*
