# 00.05 External References

> **TL;DR.** Source papers, library documentation, and adjacent-system links cited throughout the spec. Internal `[link](path)` cross-references are inline in their text and not duplicated here.

This file collects external sources (academic papers, RFC documents, library docs, adjacent-system architecture pages) referenced anywhere in Brain's spec. It exists so a reader who wants to understand the academic / engineering precedent for any spec decision can find the source without grep.

## Algorithms and data structures

- **HNSW** — Malkov, Yashunin (2018). *Efficient and robust approximate nearest neighbor search using hierarchical navigable small world graphs*. IEEE TPAMI. https://arxiv.org/abs/1603.09320
- **BM25** — Robertson, Zaragoza (2009). *The probabilistic relevance framework: BM25 and beyond*. Foundations and Trends in Information Retrieval, 3(4).
- **RRF (Reciprocal Rank Fusion)** — Cormack, Clarke, Buettcher (2009). *Reciprocal Rank Fusion outperforms Condorcet and individual rank learning methods*. SIGIR.
- **IVF + Product Quantization** — Jégou, Douze, Schmid (2010). *Product quantization for nearest neighbor search*. IEEE TPAMI.
- **GLiNER (zero-shot NER)** — Zaratiana et al. (2024). *GLiNER: Generalist Model for Named Entity Recognition using Bidirectional Transformer*. https://arxiv.org/abs/2311.08526
- **BGE-small-en-v1.5** — BAAI (2023). Model card: https://huggingface.co/BAAI/bge-small-en-v1.5
- **bge-reranker-base** — BAAI (2023). 110M parameters, MIT-licensed. Model card: https://huggingface.co/BAAI/bge-reranker-base
- **CRC32C** — Castagnoli et al. (1993). Used in Brain's frame headers and WAL records.
- **BLAKE3** — O'Connor, Aumasson, Neves, Wilcox-O'Hearn (2020). Used for content-hash fingerprints. https://github.com/BLAKE3-team/BLAKE3
- **UUIDv7** — RFC 9562. Time-ordered UUIDs used for `AgentId`, `RequestId`, `EntityId`, `StatementId`, `RelationId`, `AuditId`.

## Memory-layer evaluations

- **LongMemEval** (ICLR 2025) — Temporal-reasoning benchmark for agent memory systems. https://github.com/xiaowu0162/LongMemEval
- **LoCoMo** — Long-Conversation Memory benchmark.
- **DMR (Dialogue Memory Recall)** — Zep's headline benchmark.
- **Mem0 production audit (32-day)** — 97.8% of stored memories were junk (hash duplicates, hallucinated profiles, feedback-loop contamination).
- **Harvard D3 research** — Findings on aggressive pre-store filtering being essential for memory-system quality.

## Storage and concurrency

- **redb** — Embedded Rust key-value store backing Brain's metadata. https://github.com/cberner/redb
- **tantivy** — Full-text search library; per-shard inverted indexes in Brain. https://github.com/quickwit-oss/tantivy
- **Glommio** — Thread-per-core async runtime over io_uring. https://github.com/DataDog/glommio
- **candle** — Rust ML framework; Brain runs BGE-small inference via candle. https://github.com/huggingface/candle
- **arc-swap** — Atomic pointer swap for lock-free config + index handoff.
- **crossbeam-epoch** — Epoch-based memory reclamation.
- **io_uring** — Linux async I/O. https://kernel.dk/io_uring.pdf

## Wire format

- **rkyv** — Zero-copy archived serialization (internal on-disk storage). https://github.com/rkyv/rkyv
- **bytemuck** — Plain-old-data type casts for raw vector payloads. https://github.com/Lokathor/bytemuck
- **RFC 2119** — Key words for use in requirements (MUST / SHOULD / MAY).

## Adjacent database architectures

- **Neo4j** — Labeled-property graph; pointer-based traversal; native graph storage Block format. https://neo4j.com/docs/operations-manual/current/ + https://neo4j.com/blog/developer/neo4j-graph-native-store-format/
- **Pinecone serverless** — Storage-compute separation; freshness layer; geometric partitioning. https://www.pinecone.io/blog/serverless-architecture/
- **Pinecone IVF+PQ** — Vector compression for billion-scale corpora. https://www.pinecone.io/learn/vector-database/
- **Weaviate** — Per-tenant shard multi-tenancy; bucketed storage; tenant offloading. https://weaviate.io/blog/weaviate-multi-tenancy-architecture-explained + https://docs.weaviate.io/weaviate/concepts/storage
- **CockroachDB** — Range-based sharding with Raft replication. https://github.com/cockroachdb/cockroach/blob/master/docs/design.md
- **FoundationDB** — Decoupled-role transactional KV store. https://apple.github.io/foundationdb/architecture.html
- **TigerBeetle** — Static allocation, deterministic execution, VOPR-style simulation testing. https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/ARCHITECTURE.md
- **arc-labs Recall** — The sibling memory-layer project; Postgres + pgvector backed. https://github.com/arc-labs/recall

## Memory-system field background

- **Mem0** — https://arxiv.org/html/2504.19413v1
- **Zep / Graphiti** — https://arxiv.org/html/2501.13956v1
- **MemGPT / Letta** — https://arxiv.org/pdf/2310.08560
- **MemoryBank** — https://arxiv.org/pdf/2305.10250
- **LangMem** — https://langchain-ai.github.io/langmem/
- **Cognee** — https://www.cognee.ai/blog/fundamentals/ai-memory-in-five-scenes
- **Anthropic memory tool** — https://www.anthropic.com/news/context-management
- **Anthropic prompt caching** — https://genta.dev/resources/prompt-caching-llm-guide
- **OpenAI temporal agents cookbook** — https://cookbook.openai.com/examples/partners/temporal_agents_with_knowledge_graphs/temporal_agents
- **Stanford "Lost in the Middle"** (2023) — LLM accuracy drop when relevant context buried mid-prompt.

## Standards and protocols

- **RFC 9562** — UUID Formats (v7 in particular).
- **RFC 6585** — Additional HTTP Status Codes (for retry-after semantics).
- **WebSocket RFC 6455** — Used when Brain's HTTP transport handles streaming subscriptions.
- **Server-Sent Events** — https://html.spec.whatwg.org/multipage/server-sent-events.html

---

Internal cross-references between spec sections are inline in the prose with `[link](path)` syntax and are not duplicated here. To find every spec file that cites an external reference, grep the spec tree.
