# 02 — The two-layer model

Brain has two layers. The substrate is always on; the knowledge
layer is opt-in. This chapter explains what that means in
practice, when you'd want each, and what's actually different
about a deployment that has the knowledge layer active versus
one that doesn't.

If you read [chapter 01](01-what-brain-is.md), you saw the
two-layer diagram. This chapter unpacks it.

---

## The substrate

The substrate is what's running the moment Brain boots. It is
the vector-memory store described in chapter 01: every piece of
text an agent encodes becomes a **memory**, with three pieces
of state:

```
Memory
├── text         — the original UTF-8 string
├── vector       — 384 floats (the embedding)
└── metadata     — agent_id, kind, salience, timestamps, edges …
```

The substrate's job is to:

- Take text, embed it, store all three pieces durably.
- Find memories whose embedding is similar to a query.
- Track salience (how "alive" each memory feels), let it decay
  over time, and let the agent boost it on access.
- Let the agent forget memories, gracefully, with a grace
  period for clean-up.
- Survive crashes without losing any acknowledged write.

This is the whole product for many deployments. A chatbot that
needs "remember our last five conversations and resurface
similar ones" only needs the substrate. So does an autonomous
agent that just wants a long-term scratchpad. The substrate is
not a stepping stone — it is the complete first product, and
many users will never need anything else.

---

## The knowledge layer

The knowledge layer is what activates when you tell Brain
*what kinds of things you care about*. That telling-Brain step
is called **declaring a schema**.

Here's a schema, in plain English:

> "In my deployment, I have **People** (with names and email
> addresses), **Projects** (with names and start dates), and
> three kinds of statement: `works_on(Person, Project)`,
> `manages(Person, Person)`, and `prefers(Person, …)`."

Once you declare that — by sending a `SCHEMA_UPLOAD` to Brain
— a switch flips inside the shard. From that moment on, every
new memory passes through **extractors** that mine it for
those declared types. The extractors produce:

- **Entities** — concrete Persons and Projects, deduplicated
  across memories.
- **Statements** — typed claims like `Priya works_on Atlas`,
  with the memories they came from as evidence.
- **Relations** — typed edges between entities.

These go into the same database file, in tables alongside the
substrate's memory table. Queries can now ask structured
questions ("everyone who works on Atlas") and they're answered
from the typed tables, not by similarity search.

> **What's a schema, then?**
>
> A schema in Brain is a small declaration in a custom DSL that
> says what entity types, predicate types, and relation types
> exist in your domain, plus what extractors mine for them.
> It's not a SQL schema (no columns, no foreign keys) — it's
> more like a *type system* for the cognitive content your
> agent cares about. Chapter 15 covers it in detail.

The knowledge layer is **always derived from** the substrate.
Every entity has memories that mentioned it; every statement
points back to the memories that support it; every relation
has the memories that witnessed it. If you wiped the knowledge
tables tomorrow, Brain could rebuild them from the substrate
(plus the schema) by re-running the extractors.

That's the load-bearing invariant: **the substrate is
authoritative; the knowledge layer is derived.** Chapter 24
explains why this matters for disaster recovery and trust.

---

## The schema gate

Inside each shard, the substrate / knowledge boundary is one
boolean: *has any schema been declared on this shard?*

This boolean is called the **schema gate**. Every cognitive
operation that could care — chiefly `recall` — checks the
gate when a request arrives:

```
recall(cue):
  if schema_gate == false:
    pure substrate path
    → find vectors similar to cue
    → return ranked memories
  else:
    hybrid path
    → also run lexical search
    → also run graph traversal if there's an entity anchor
    → fuse the results
    → return ranked items
```

The gate's only states are *off* (no schema declared) and *on*
(at least one schema declared). It flips from off to on the
moment a successful `SCHEMA_UPLOAD` commits.

In v1, **the gate is one-way**: once you've declared a schema
on a shard, the shard stays in knowledge-active mode
permanently. There is no `SCHEMA_DROP` opcode. The honest
reason is that dropping a schema means draining a bunch of
derived state (entities, statements, relations, the tantivy
text indexes, the LLM cache) and v1 doesn't ship that
admin path. If you need to start over, you start with a fresh
data directory.

---

## What activates when the gate flips

The instant the schema gate flips on:

1. **Extractors start running.** Every subsequent `encode` runs
   pattern extractors synchronously, classifier extractors
   close to it, and queues LLM extractors for background
   processing. Chapter 14 covers the three tiers.
2. **Extraction outputs land in knowledge tables.** Entity,
   statement, and relation rows accumulate. Chapters 10–13
   cover what's in each.
3. **The lexical and graph retrievers become live.** A
   `recall` now potentially fans out to three retrievers
   (semantic, lexical, graph) instead of one. Chapters 17 and
   20–21 explain.
4. **The audit log starts recording extractions.** Every
   extractor run, success or failure, writes an audit row.
   Operators can see exactly what Brain did with each memory.

What doesn't change:

- **Existing memories are unchanged.** Memories encoded before
  schema declaration aren't automatically re-extracted. If you
  want them processed, an admin RPC kicks off a *backfill*
  worker that runs extractors over the historical memories.
- **The wire protocol's substrate verbs work the same.**
  `encode/recall/forget` look identical from the client side.
  What's different is what Brain does *under* `recall`.

---

## What changes on disk

Both layers share the same shard directory. A substrate-only
shard's files:

```
data/shard-0/
├── shard.uuid
├── arena.bin           ← memory vectors live here
├── metadata.redb       ← memory metadata, edges, idempotency, …
└── wal/                ← the write-ahead log
    └── seg-000001.wal
```

A knowledge-active shard adds more, in the same directory:

```
data/shard-0/
├── shard.uuid
├── arena.bin
├── metadata.redb         ← now also has entity/statement/relation tables
├── wal/
├── entity.hnsw           ← embeddings for entities
├── statement.hnsw        ← embeddings for statements
├── memory_text.tantivy/  ← BM25 text index over memories
├── statements.tantivy/   ← BM25 text index over statements
└── llm_cache.redb        ← cache of LLM-extractor responses
```

> **What's BM25?**
>
> BM25 is a ranking function used in classic text search
> (Lucene, Elasticsearch, tantivy). Given a query and a set of
> documents, it scores each document by how often the query's
> terms appear, weighted by how rare each term is in the
> corpus. It's the lexical counterpart to vector search.
> Chapter 21 covers it.
>
> See [Wikipedia: Okapi BM25](https://en.wikipedia.org/wiki/Okapi_BM25).
>
> **What's tantivy?**
>
> Tantivy is a Rust full-text search library, conceptually
> similar to Apache Lucene. Brain uses it to maintain BM25
> indexes over memory and statement text.
>
> See [tantivy on GitHub](https://github.com/quickwit-oss/tantivy).

The substrate-only files are still there — `arena.bin`, the
WAL, the metadata `.redb` — they just keep working as before.
The new files start empty and fill up as extractors run.

Importantly, a substrate-only shard creates *zero* of the
knowledge-layer files. They literally do not exist on disk.
Pure-substrate deployments don't pay for the unused machinery.

---

## A side-by-side comparison

Same question, both modes, what Brain does:

### `encode("Priya prefers async meetings")`

**Substrate-only:**
1. Embed the text → 384-float vector.
2. Allocate a memory slot, write the vector + metadata.
3. Append a record to the write-ahead log; fsync; respond OK.
4. Insert the memory into the vector index.

**Knowledge-active (with a relevant schema declared):**
1. Steps 1–4 above (identical).
5. Pattern extractor: match for "Priya" → emit Person entity
   candidate.
6. Classifier extractor: predict that this is a Preference
   statement.
7. Background queue: LLM extractor will refine the prediction.
8. Audit log: record what each extractor produced.

The substrate part is unchanged. Knowledge-layer work is on
top — pattern and classifier run synchronously (microseconds
to milliseconds); the LLM tier runs in the background.

### `recall("what does Priya think of meetings?")`

**Substrate-only:**
1. Embed the query.
2. Find vectors similar to the embedded query (vector search).
3. Return the top N memories, ranked by similarity.

**Knowledge-active:**
1. Embed the query.
2. *Route the query*: detect that "Priya" is an entity anchor
   and "think of" is a preference-shaped question.
3. Run **three retrievers in parallel**:
   - Semantic: vectors similar to the query (as before).
   - Lexical: text search via BM25 for "Priya" / "meetings".
   - Graph: entities related to Priya, statements about her
     of kind Preference.
4. **Fuse** the three ranked lists into one (chapter 21).
5. Apply filters (time range, confidence, statement kind).
6. Return the top N items, which may be memories *or*
   statements *or* entities depending on what was relevant.

The substrate's `recall` answers "what memories are similar?";
the knowledge layer's `recall` can also answer "what does
Brain *know* about this?"

---

## When to declare a schema

Some heuristics. Not rules — just signs to look at.

**Lean substrate-only when:**

- You don't have well-defined types in your domain. The agent
  just remembers free text and recalls similar text. A note-
  taking assistant for a single user is a good example.
- You don't need to answer "who/what/when" structured
  questions. The agent's queries are paraphrase-driven —
  "anything about meetings," "things I said last week."
- You can't afford LLM extraction costs and the patterns +
  classifier wouldn't get you enough recall on their own.
  Chapter 14 covers extractor cost.
- You're prototyping. The schema is a design commitment;
  changing it later is supported but not free.

**Lean knowledge-active when:**

- Your domain has real types ("user," "project," "ticket,"
  "incident") and queries naturally name them.
- You want the agent to surface contradictions across
  memories. ("Priya said X on Monday but Y on Tuesday — which
  is the current preference?")
- You want provenance: when the agent claims something, you
  want to know which specific memories support the claim.
- You want to filter by structured attributes (date ranges,
  confidence thresholds, statement kind) that aren't
  vector-shaped.

Many deployments end up here in the long run. The opt-in is
how Brain makes the cost honest — you choose when you're ready
for it.

---

## Substrate-only is a first-class deployment posture

This is worth flagging directly because it's an unusual design
choice for a product with a "knowledge layer."

A lot of database-shaped products treat "the simple mode" as a
demo or a stepping stone. Brain doesn't. The substrate is the
*complete first product*. A deployment that:

- Boots a fresh Brain server,
- Never sends `SCHEMA_UPLOAD`,
- Encodes and recalls memories for years,

is **doing it right**. There's no nagging banner that says
"upgrade to the full feature set." The empty knowledge tables
on disk are essentially free (B-trees with zero rows cost
basically nothing). The extractor workers run their loops but
do no work because the registry is empty.

The reasoning: the knowledge layer is a *real* commitment.
It adds CPU cost (classifier inference on every encode), maybe
dollar cost (LLM extraction), disk cost (entity HNSW, two
tantivy indexes, the LLM cache), and operational complexity
(extractor audit, schema migrations, backfill).

If you don't need that, you shouldn't pay for it. So Brain
makes substrate-only a first-class deployment shape, with
identical wire protocol, identical SDK, identical metrics —
just with an extra capability you opt into when you want it.

---

## The trust model in one sentence

You'll see this expanded in chapter 24, but it's worth
previewing here because it's a consequence of the two-layer
model:

> The substrate is authoritative. The knowledge layer is
> derived. Everything in the knowledge layer can be
> reconstructed from the substrate plus the schema; nothing in
> the substrate can be reconstructed from the knowledge
> layer.

That's why the substrate's durability story (chapter 18) is
so paranoid — it's the bottom of the trust stack. The
knowledge layer can be wiped and rebuilt; the substrate
cannot.

---

## Recap

- Brain has two layers: a vector substrate (always on) and a
  knowledge layer (opt-in via schema declaration).
- The substrate stores text + embedding + metadata. The
  knowledge layer derives entities, statements, and relations
  from the substrate.
- A **schema gate** per shard tracks whether the knowledge
  layer is active. It flips from off to on once, when the
  first schema is uploaded.
- Substrate-only is a first-class deployment posture, not a
  stepping stone. Many production deployments live there
  forever.
- The substrate is authoritative; the knowledge layer is
  derived. The next 26 chapters keep returning to that.

---

## Where to go next

- **See it in action:** [chapter 03](03-guided-tour.md) walks
  through a concrete `encode → recall → forget` session in
  pseudocode, in both modes.
- **Compare:** [chapter 04](04-vs-other-systems.md) — Brain
  vs Postgres, vs vector DBs, vs graph DBs.
- **Dig into a layer:**
  - Substrate: [chapter 05](05-memories.md) onward.
  - Knowledge layer: [chapter 10](10-entities.md) onward.
- **Read the architecture:** if you want the same picture with
  code citations, [`../architecture/01-system-architecture.md`](../architecture/01-system-architecture.md)
  is the engineering tier's equivalent of this chapter.
