# 01 — What Brain is

**Read this first.** This chapter answers "what is this thing?"
without assuming you know what a vector database is, what an
embedding is, or what `mmap` means. The vocabulary builds up
across the next 26 chapters; this one is the on-ramp.

---

## The one-paragraph answer

Brain is a **cognitive substrate for AI agents** — a database
whose primary operations are the things an agent does with its
memory: *encode* what just happened, *recall* something
relevant, *plan* a route to a goal, *reason* across what it
knows, and *forget* what's no longer useful. It stores text +
the meaning of that text (as numbers) + optionally a typed
graph of who-said-what-about-whom, and it answers questions
about all three.

If you've used a database before, the mental shortcut is:
"Postgres, but the verbs are `encode/recall` instead of
`INSERT/SELECT`, and it understands what your text *means*, not
just what tokens are in it."

---

## The problem Brain solves

Modern AI agents — chatbots, autonomous workflows,
copilot-style assistants — have a memory problem.

They have a *context window*: the chunk of recent text the
language model sees on each call. A few thousand tokens, maybe
a hundred thousand on a long-context model. Once you exceed
that, **the agent forgets**. It forgets the user's name. It
forgets the goal of the conversation. It re-asks questions it
already asked.

> **What is a context window?**
>
> A language model's context window is the maximum amount of
> text it can read in one call. Everything outside that window
> is invisible to the model. Models commonly have windows of
> 8K, 32K, 200K, or 1M tokens.
>
> See [Wikipedia: Large language model](https://en.wikipedia.org/wiki/Large_language_model).

The usual fix is **retrieval-augmented generation (RAG)**: when
the agent needs information, an *external store* finds the most
relevant past content and pastes it into the context window
before calling the model. The store typically uses *embeddings*
— numerical representations of meaning — to find "what's
relevant."

> **What is an embedding?**
>
> An embedding is a list of numbers (for Brain: 384 floats)
> that represents the meaning of a piece of text. Two texts
> that mean similar things end up with similar numbers. Two
> texts that mean different things end up far apart.
> Embeddings are produced by models like BERT or BGE-small.
>
> See [Wikipedia: Word embedding](https://en.wikipedia.org/wiki/Word_embedding)
> and chapter 08 of this tier.

That basic setup — *vector store + retrieval glue* — works for
demos. In production it stops being enough almost immediately:

- The agent has *episodic* memories ("we met yesterday") and
  *semantic* memories ("the user lives in Seattle") and
  *events* ("the deploy failed at 02:31"). A flat list of
  embeddings doesn't distinguish them.
- The agent forgets — but forgetting *gracefully*, with grades
  of "less salient" rather than "still there but invisible,"
  isn't something a key-value store does.
- The agent wants to ask structured questions: *who reports to
  Priya?* — which a similarity search can't answer.
- The agent should resurface contradictions: yesterday Priya
  preferred async meetings; today she said she prefers
  in-person. A "give me the most similar memory" search just
  picks one.
- The agent should be able to forget *durably* and provably —
  for privacy, for retraction, for plain hygiene — which means
  storage that knows what it owes who.

Brain is the answer to "what if the substrate did all of that,
instead of you wiring four tools together?"

---

## The shape of Brain

```
                          your AI agent
                                │
                                │  encode(text)
                                │  recall(cue)
                                │  forget(memory_id)
                                │
                ┌───────────────▼───────────────┐
                │           Brain               │
                │                               │
                │  ┌─────────────────────────┐  │
                │  │   knowledge layer       │  │
                │  │   (optional, opt-in)    │  │
                │  │                         │  │
                │  │   entities,             │  │
                │  │   statements,           │  │
                │  │   relations             │  │
                │  └────────────┬────────────┘  │
                │               │ derived from  │
                │               ▼               │
                │  ┌─────────────────────────┐  │
                │  │   substrate             │  │
                │  │   (always present)      │  │
                │  │                         │  │
                │  │   memories =            │  │
                │  │     text + vector       │  │
                │  │     + metadata          │  │
                │  └─────────────────────────┘  │
                └───────────────────────────────┘
```

Two layers. The **substrate** is the vector-memory store: every
piece of text the agent encodes becomes a *memory* with an
embedding and metadata. The **knowledge layer** is optional and
sits on top: if you declare a *schema* describing what kinds of
entities and statements you care about, Brain will mine your
memories for those structured things and let you query them
directly.

Both layers live in the same server. The substrate is always
on. The knowledge layer activates the moment you declare a
schema, and not before — a deployment that never declares a
schema runs as a pure vector substrate and pays nothing for
the unused machinery.

Chapter 02 covers the two layers in detail. Chapter 03 walks
through a concrete encode-recall-forget session.

---

## Why "cognitive substrate" and not "vector database"

The label matters because it changes what you expect.

A **vector database** (Pinecone, Qdrant, Milvus, …) gives you
the storage layer: put vectors in, get nearest-neighbours out.
Memory management, decay, the typed graph on top, the
provenance from a derived claim back to the source memory —
those are *your problem*. You write the orchestration; the
vector DB is one component in a pipeline.

A **cognitive substrate** owns the whole vertical:

- It generates the embeddings itself (you send text, not
  vectors).
- It runs background workers that decay, consolidate, and
  garbage-collect memories on its own clock.
- It maintains the typed graph on top of memories without
  asking the agent to keep it in sync.
- It records *every state mutation* in a write-ahead log so
  recovery from a crash is automatic and lossless.
- It gives you idempotent, retryable operations on the wire
  (you can safely retry an `encode` and not get duplicates).

The vector DB is a hard drive. The cognitive substrate is a
hard drive plus the file system plus the parts of an OS that
know what files are *for*.

This is also why Brain's API verbs are different. A vector DB
has `upsert(id, vector)`. Brain has `encode(text)` — because
the substrate owns embedding, and because the operation is
*encoding a memory*, not *inserting a row*.

Chapter 16 of this tier explains the five verbs in detail.

---

## What Brain is *not*

Equally important — Brain is **not**:

- **A SQL database.** It doesn't run joins, aggregations,
  window functions, or transactions across arbitrary tables.
  If you need those, run Postgres alongside Brain.
- **A general-purpose graph database.** It doesn't do Cypher
  or SPARQL. Its graph is typed and shaped by the schema you
  declare; it's optimised for the kinds of queries an agent
  asks ("everyone Priya reports to"), not for arbitrary graph
  algorithms.
- **An LLM.** It doesn't generate text. It *uses* small
  models (one for embedding, optionally another for extraction)
  but it's not a chatbot you talk to.
- **A drop-in for LangChain or LlamaIndex.** Those are agent
  frameworks. Brain is the storage layer underneath them. You
  can use Brain *from* LangChain through the Rust SDK or wire
  protocol, but Brain doesn't try to be the orchestrator.
- **A queue, a message broker, a cache, a job runner.** It's a
  database with cognitive verbs, not a general distributed
  system primitive.
- **Distributed across machines in v1.** It scales horizontally
  by adding *shards* on the same node (more cores = more
  shards). Cross-node clustering and replication are deferred
  to a future version — the v1 design admits this honestly and
  documents it as a known limitation rather than papering over
  it.

Chapter 04 compares Brain to specific systems in detail.

---

## When you'd want Brain

Three signs you should look at Brain:

1. **Your agent has a memory problem.** It re-asks the same
   questions, contradicts itself across sessions, or loses
   context when the conversation gets long. You've already
   tried "stuff more into the context window" and "do RAG over
   embeddings."

2. **You want structured facts about your domain.** Not just
   "what text did the user say," but "what does the system
   *know* about Priya right now, given everything she's
   told us?" You'd build this yourself; Brain has it built-in
   via the knowledge layer.

3. **You care about provenance.** When the agent claims
   something, you want to know *why*: which memory or memories
   support that claim, and what to do when those memories get
   deleted. Brain's substrate-is-authoritative invariant is
   the foundation for this.

If two of the three apply, read on. If none of them do, a
plain vector DB is probably enough.

---

## When you wouldn't want Brain

Equally fair to call out:

- **You're prototyping a single-session chatbot.** A flat
  vector store and a session memory is simpler.
- **You need replication and cross-region failover today.** v1
  doesn't ship that; come back in a later version, or use
  Brain plus an external backup-and-restore loop.
- **You're storing things that aren't text-shaped.** Brain
  embeds text. Images, audio, binary blobs aren't its
  business.
- **You need to query SQL-shaped tabular data with
  arbitrary joins.** Use a SQL database.
- **You don't have a Linux deployment target.** Brain depends
  on `io_uring`, a Linux-specific kernel feature, for its
  storage and concurrency primitives. macOS and Windows aren't
  supported runtimes (you can develop on either with the
  storage layer stubbed out, but production is Linux).

---

## What's coming in the next chapters

The rest of this tier builds the vocabulary the rest of the
docs use:

- **Part 1 (chapters 02–04)** orientation: the two-layer model
  in detail, a concrete tour, and how Brain compares to the
  systems it sits next to.
- **Part 2 (chapters 05–09)** the *memory* side of Brain: what
  a memory is, the three memory kinds, salience and decay,
  embeddings, vector similarity.
- **Part 3 (chapters 10–15)** the *knowledge layer*: entities,
  statements, the three statement kinds, relations, extractors,
  schemas.
- **Part 4 (chapters 16–17)** the verbs: the five cognitive
  operations and what hybrid retrieval actually does.
- **Part 5 (chapters 18–23)** the systems layer: storage,
  durability, mmap'd arenas, exact-vs-approximate search,
  ranked fusion, concurrency and async runtimes, sharding.
  Each chapter explains the OS-level concept it depends on.
- **Part 6 (chapters 24–25)** design principles: the seven
  invariants Brain promises, determinism, idempotency, replay.
- **Chapters 26–27** are reference: a glossary and an FAQ.

You don't have to read these in order. But chapters 02 and 03
are useful before anything else — they nail down the two
layers and what an end-to-end session looks like.

---

## Where to go next

- **Linear path:** [chapter 02](02-two-layer-model.md) — the
  two-layer model in detail.
- **Hands-on first:** [chapter 03](03-guided-tour.md) — a
  concrete `encode → recall → forget` walkthrough.
- **Comparing systems:** [chapter 04](04-vs-other-systems.md) —
  Brain vs Postgres, vs vector DBs, vs graph DBs.
- **Want to install and run something?**
  [`../tutorials/01-quickstart-docker.md`](../tutorials/01-quickstart-docker.md)
  has Brain running in five minutes.
