# Architecture

**Audience:** engineers who need to reason about Brain's internals —
performance, durability, failure modes, extension surfaces.

**Goal:** *deep understanding*. Twelve chapters that explain how
Brain is built and why each major decision was made. If you only
want to *use* Brain, start in [`../tutorials/`](../tutorials/) or
[`../guides/`](../guides/); if you want to *look something up*,
[`../reference/`](../reference/) is the place. This tier is for
the people who need to know what happens between an `encode` call
landing on the server and the WAL `fsync` that acknowledges it.

The shape is modelled on TiKV's *Deep Dive* and PostgreSQL's
*Internals* — long-form chapters, numbered for stable citation.
Link to `architecture/03-arena-and-wal.md#group-commit` and the
link still works after a refactor.

## Chapters

| # | Chapter | Covers |
|---|---|---|
| 01 | [`01-system-architecture.md`](01-system-architecture.md) | Connection-layer / shard split, request lifecycle, layering |
| 02 | [`02-wire-protocol.md`](02-wire-protocol.md) | Frame format, rkyv codec, zero-copy reads |
| 03 | [`03-arena-and-wal.md`](03-arena-and-wal.md) | Slot layout, WAL group commit, recovery |
| 04 | [`04-hnsw-index.md`](04-hnsw-index.md) | Index choice, parameter rationale, maintenance |
| 05 | [`05-redb-metadata.md`](05-redb-metadata.md) | Table layout, idempotency, knowledge-layer tables |
| 06 | [`06-embedding-pipeline.md`](06-embedding-pipeline.md) | BGE-small via candle, batching, cache |
| 07 | [`07-background-workers.md`](07-background-workers.md) | The twelve workers, intervals, ownership |
| 08 | [`08-tokio-glommio-boundary.md`](08-tokio-glommio-boundary.md) | Send/!Send split, channel discipline |
| 09 | [`09-knowledge-layer.md`](09-knowledge-layer.md) | When it activates, storage layout |
| 10 | [`10-extractors.md`](10-extractors.md) | Pattern → classifier → LLM tier composition |
| 11 | [`11-hybrid-retrieval-rrf.md`](11-hybrid-retrieval-rrf.md) | Three retrievers, RRF fusion (k=60) |
| 12 | [`12-query-router.md`](12-query-router.md) | Routing decisions, filter pushdown |

## Reading order

Linear is fine, but the chapters are independent enough to dip
into. Some pragmatic paths:

- **Operator wanting durability intuition** → 01, 03, 07.
- **Developer extending the wire protocol** → 01, 03, 02 (read
  arena/WAL before wire — frames carry handles into the arena,
  so the wire format is easier to reason about once you know
  what it points at).
- **Developer adding a worker** → 07, 08.
- **Developer building a knowledge-layer feature** → 09, 10, 11, 12.

## How chapters cite the implementation

Every non-trivial claim is anchored to a source file in
[`../../crates/`](../../crates/):

```
… group commit batches up to 64 records per fsync
(`crates/brain-storage/src/wal/group_commit.rs:142`).
```

Line numbers drift over time. If a citation looks wrong, the
truth is in the file — grep the symbol, not the line. Each
chapter ends with a **Where it lives in the code** section that
maps the chapter's topics to crates and modules so you can keep
reading after the prose ends.

If you read code that contradicts a chapter, the chapter is
stale. Open an issue or send a PR.

## Conventions inside chapters

- Diagrams are ASCII so they render anywhere this Markdown does.
- Numbers (slot sizes, intervals, defaults) reflect the current
  shipped configuration unless explicitly flagged as historical.
- **Substrate** = the vector-memory core (encode / recall / plan /
  reason / forget over embeddings). **Knowledge layer** = the
  typed entities / statements / relations layer that activates
  when a schema is declared. Chapter 09 covers the boundary.
- Acronyms get expanded the first time they appear in each
  chapter; chapters do not assume you have read the previous one.
