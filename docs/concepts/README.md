# Concepts

**Audience:** anyone trying to understand Brain — what it is,
what it does, why it works the way it does. No prior experience
with databases, machine learning, or operating-system primitives
is assumed.

**Goal:** *understanding*. Not "how do I install X"
(that's [`../guides/`](../guides/)). Not "what's the exact
signature of Y" (that's [`../reference/`](../reference/)). Not
"what does this code look like inside" (that's
[`../architecture/`](../architecture/)). These pages explain
*what Brain is*, *why it's built this way*, and *the
vocabulary the rest of the docs use*.

Read these first if Brain is new to you. Come back when
something in the rest of the docs doesn't click.

---

## How to read these pages

Each chapter is short — ten minutes or so — and starts with what
question it answers. Chapters that mention a piece of jargon
either define it on the spot or open a small sidebar like this:

> **What is mmap?**
>
> mmap is a Unix system call that maps a file directly into a
> program's memory, so reading the file looks like reading an
> array. The operating system handles loading the actual disk
> blocks behind the scenes.
>
> See [Wikipedia: Memory-mapped file](https://en.wikipedia.org/wiki/Memory-mapped_file).

Sidebars give you just enough to follow the chapter. The
outbound link is for when you want more.

Code examples are pseudocode — language-neutral, readable like
English. The real SDK is in
[`../reference/sdk-rust.md`](../reference/sdk-rust.md).

---

## Pages

### Part 1 — Orientation

| # | Page | Read when |
|---|---|---|
| 01 | [`01-what-brain-is.md`](01-what-brain-is.md) | First. One page on what Brain is and isn't. |
| 02 | [`02-two-layer-model.md`](02-two-layer-model.md) | Wondering "what's the difference between substrate and knowledge layer." |
| 03 | [`03-guided-tour.md`](03-guided-tour.md) | You want to see an `encode → recall → forget` session before reading the abstract bits. |
| 04 | [`04-vs-other-systems.md`](04-vs-other-systems.md) | "How is this different from Postgres / a vector DB / a graph DB / LangChain?" |

### Part 2 — Memories and meaning

| # | Page | Read when |
|---|---|---|
| 05 | [`05-memories.md`](05-memories.md) | "What exactly is a memory in Brain?" |
| 06 | [`06-memory-kinds.md`](06-memory-kinds.md) | The Episodic / Semantic / Consolidated distinction. |
| 07 | [`07-salience-decay-consolidation.md`](07-salience-decay-consolidation.md) | "Why does Brain talk about forgetting curves?" |
| 08 | [`08-embeddings.md`](08-embeddings.md) | "What's an embedding? Why 384 numbers?" |
| 09 | [`09-vector-similarity.md`](09-vector-similarity.md) | "How does Brain decide two memories are *similar*?" |

### Part 3 — The knowledge layer

| # | Page | Read when |
|---|---|---|
| 10 | [`10-entities.md`](10-entities.md) | "What's an entity? When does Brain make one?" |
| 11 | [`11-statements.md`](11-statements.md) | "What's a statement? How is it different from a memory?" |
| 12 | [`12-fact-preference-event.md`](12-fact-preference-event.md) | The three statement kinds and when each applies. |
| 13 | [`13-relations.md`](13-relations.md) | "How does Brain represent that A reports to B?" |
| 14 | [`14-extractors.md`](14-extractors.md) | "Who turns text into entities and statements?" |
| 15 | [`15-schemas.md`](15-schemas.md) | "What does declaring a schema do?" |

### Part 4 — The verbs

| # | Page | Read when |
|---|---|---|
| 16 | [`16-cognitive-operations.md`](16-cognitive-operations.md) | "Why `encode/recall/plan/reason/forget` instead of `put/get/query`?" |
| 17 | [`17-hybrid-retrieval.md`](17-hybrid-retrieval.md) | "What does `recall` actually do under the hood when a schema is declared?" |

### Part 5 — Systems vocabulary

*The chapters in this part have inline sidebars explaining
operating-system primitives (mmap, fsync, the page cache, …)
and async-runtime concepts (Tokio, Glommio, io_uring, …) with
outbound links for the curious.*

| # | Page | Read when |
|---|---|---|
| 18 | [`18-storage-and-durability.md`](18-storage-and-durability.md) | "What does Brain do to survive a crash?" |
| 19 | [`19-mmap-and-arenas.md`](19-mmap-and-arenas.md) | "What's an arena? How is reading a memory just a memory access?" |
| 20 | [`20-indexes-exact-vs-approximate.md`](20-indexes-exact-vs-approximate.md) | "Why is search 'approximate'? Doesn't that miss things?" |
| 21 | [`21-lexical-and-fusion.md`](21-lexical-and-fusion.md) | "Why does Brain run multiple search algorithms and combine their results?" |
| 22 | [`22-concurrency-and-async.md`](22-concurrency-and-async.md) | "What's a thread-per-core architecture? Why two async runtimes?" |
| 23 | [`23-sharding-and-isolation.md`](23-sharding-and-isolation.md) | "How does Brain scale? Why doesn't v1 have replication?" |

### Part 6 — Design and reference

| # | Page | Read when |
|---|---|---|
| 24 | [`24-invariants-and-trust.md`](24-invariants-and-trust.md) | "What does Brain *promise*, and how do I know it keeps the promise?" |
| 25 | [`25-determinism-idempotency-replay.md`](25-determinism-idempotency-replay.md) | "If I retry, do I get the same answer? What if an LLM is involved?" |
| 26 | [`26-glossary.md`](26-glossary.md) | Look up an unfamiliar word. |
| 27 | [`27-faq.md`](27-faq.md) | Common questions before they become support tickets. |

---

## Where to go next

- **Want to use Brain?** Jump to
  [`../tutorials/01-quickstart-docker.md`](../tutorials/01-quickstart-docker.md).
- **Want to put Brain in production?**
  [`../guides/`](../guides/) has deployment, configuration, and
  security guides.
- **Want exact field names, opcodes, error codes?**
  [`../reference/`](../reference/).
- **Want to read the source?** [`../architecture/`](../architecture/)
  is the deep-dive engineering tier — same topics as here but
  with code citations and implementation detail.

---

## How this tier relates to the rest

These pages are about *what things are* and *why*. Other tiers
answer different questions:

| If you're asking | Look in |
|---|---|
| "What is X and why does it exist?" | This tier (`concepts/`) |
| "How do I do X?" | [`../guides/`](../guides/) and [`../tutorials/`](../tutorials/) |
| "What's the exact name / signature / value?" | [`../reference/`](../reference/) |
| "How does the code do X internally?" | [`../architecture/`](../architecture/) |
| "Something's wrong, how do I fix it?" | [`../runbooks/`](../runbooks/) |

A concepts page that starts duplicating a reference table or
explaining "step 3: edit `Cargo.toml`" is drifting from its
job. The chapter should link to the right tier and stay at the
level of *what* and *why*.
