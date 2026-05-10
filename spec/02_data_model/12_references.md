# 02.12 References

References specifically for the data model. The full series-wide reference list is in [01.11 References](../01_system_architecture/11_references.md); this file picks out what's most directly relevant to entities, identifiers, salience, and edges.

## 1. Cognitive science: memory taxonomy

- **Endel Tulving, "Episodic and Semantic Memory" (1972).** The original distinction. Brain's `MemoryKind` enum carries this forward, adding `Consolidated` for substrate-derived memories.

- **Hermann Ebbinghaus, "Über das Gedächtnis" (1885).** Source of the forgetting curve — exponential decay of memory retention. Used in [`05_salience.md`](05_salience.md).

- **Larry Squire, "Memory and the hippocampus: A synthesis from findings with rats, monkeys, and humans" (1992).** Memory consolidation theory. Influences [`07_memory_kinds.md`](07_memory_kinds.md) §4 (Consolidated kind).

- **Wikipedia: [Forgetting curve](https://en.wikipedia.org/wiki/Forgetting_curve)** — standard reference for the Ebbinghaus model.

- **Wikipedia: [Memory consolidation](https://en.wikipedia.org/wiki/Memory_consolidation)** — standard reference for the consolidation process.

## 2. Identifiers

- **RFC 9562 — UUID Formats including UUIDv7.** [datatracker.ietf.org/doc/rfc9562](https://datatracker.ietf.org/doc/rfc9562/). Brain uses UUIDv7 for `AgentId`, `RequestId`, and persistent `ShardId`.

- **`uuid` crate** — Rust UUID implementation. [GitHub: uuid-rs/uuid](https://github.com/uuid-rs/uuid). Supports v7.

## 3. Hashing

- **BLAKE3 specification.** [GitHub: BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3). Used for embedding model fingerprints and content fingerprints.

## 4. Embeddings and similarity

- **`bge-small-en-v1.5`** — the embedding model. [HuggingFace: BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5). 384-dim output; MIT-licensed.

- **BAAI FlagEmbedding project.** [GitHub: FlagOpen/FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding).

- **Cosine similarity and dot product.** Background reference: any introductory linear-algebra text. For unit-norm vectors, dot product equals cosine similarity.

## 5. Graph theory (edges)

- **Wikipedia: [Directed graph](https://en.wikipedia.org/wiki/Directed_graph)** — basic graph theory.

- **Wikipedia: [Property graph model](https://en.wikipedia.org/wiki/Property_graph)** — for context with graph databases.

## 6. Vector Symbolic Architectures (used by REASON)

- **Pentti Kanerva, "Hyperdimensional Computing" (2009).** The foundational paper. Background for the `REASON` operation's algebra.

- **Tony Plate, "Holographic Reduced Representations" (1995).** Specific to circular convolution as the bind operation.

## 7. Distributed-systems terminology

- **Pat Helland, "Life beyond Distributed Transactions" (2007).** The argument for entity-scoped consistency, which Brain inherits — agents are entities; cross-agent operations are eventually consistent.

- **Wikipedia: [Tombstone (data store)](https://en.wikipedia.org/wiki/Tombstone_(data_store))** — the term used in [`08_lifecycle.md`](08_lifecycle.md).

## 8. Adjacent specs in this series

These specs use the data model definitions:

- [03. Wire Protocol](../03_wire_protocol/) — wire encoding of memory records, identifiers, and edges.
- [05. Storage: Arena & WAL](../05_storage_arena_wal/) — physical storage of vectors and lifecycle state.
- [07. Metadata + Graph Store](../07_metadata_graph/) — physical storage of metadata, edges, contexts.
- [09. Cognitive Operations](../09_cognitive_operations/) — operations that manipulate these entities.

If something in this spec references "the storage layer" or "the wire protocol", the linked spec has the byte-level details.

---

*This concludes Spec 02. The next document in numerical order is [03. Wire Protocol](../03_wire_protocol/).*
