# 05 — Memories: the basic unit

A **memory** is the unit Brain stores and retrieves. This
chapter explains what a memory actually contains, what its
lifecycle looks like, and how Brain refers to memories with
a special kind of identifier that's safe across deletions.

Everything in Brain's substrate ([chapter 02](02-two-layer-model.md))
is made of memories. Everything in the knowledge layer is
*derived from* memories. So understanding what's in one is
foundational.

---

## The three pieces of a memory

When you call `encode(text)`, Brain creates a record with
three parts:

```
Memory
├── text         "Met Priya at the offsite, talked about Atlas."
├── vector       [0.012, -0.083, 0.044, …]   ← 384 floats
└── metadata     created_at:    2024-09-12 14:31:22 UTC
                 agent_id:      alice@acme.com
                 context_id:    default
                 kind:          Episodic
                 salience:      0.7
                 access_count:  0
                 …
```

The **text** is exactly what you sent. Brain stores it
verbatim — same bytes, same UTF-8. (If your text is longer
than 512 tokens after tokenisation, the embedder will
truncate the *embedding's input*, but Brain stores the full
text either way.)

The **vector** is 384 floating-point numbers. Brain computed
it from the text using the
[BGE-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5)
embedding model on the server. Two memories with similar text
have similar vectors. The vector is what makes vector search
work — chapter 08 covers how it's generated and chapter 09
covers how similarity is measured.

The **metadata** is everything else Brain needs to know about
this memory: who created it, when, what kind of memory it is,
how salient it is, what state it's in. Each field has a
purpose; the next section walks through them.

> **Why 384 numbers?**
>
> BGE-small produces 384-dimensional vectors. The number is a
> property of the model: larger embedding models produce
> 768-dim or 1024-dim vectors but cost more. 384 dims hits a
> sweet spot for quality vs storage (and was chosen as the v1
> default). Chapter 08 has the full discussion.

---

## What's in the metadata

The fields you'll see referenced across the rest of the docs:

| Field | What it means |
|---|---|
| `memory_id` | The handle Brain uses to refer to this memory. See "The `MemoryId`" below. |
| `agent_id` | Who owns this memory. Memories are isolated per agent (chapter 23). |
| `context_id` | Which "context" / namespace the memory belongs to. Default for most use. |
| `kind` | One of `Episodic`, `Semantic`, `Consolidated`. Chapter 06. |
| `salience` | How "alive" this memory feels, roughly `[0, 1]`. Decays over time; boosts on access. Chapter 07. |
| `salience_initial` | The salience at creation time. Used for decay math. |
| `created_at` | Unix nanosecond timestamp of creation. |
| `last_accessed_at` | When this memory was last recalled. Used for access-boost. |
| `access_count` | Running count of recalls. |
| `embedding_model_fp` | Fingerprint of the embedding model that produced this memory's vector. If the model changes, this flags the memory as needing re-embedding. |
| `flags` | A bitfield of state: active / tombstoned / pinned / stale. |
| `forgot_at` | Set when the memory is forgotten. `None` for live memories. |
| `tombstoned_at` | Set when the memory is tombstoned. |
| `consolidated_at` | Set when a consolidated memory was created from this one. |

This is the *substrate's* view. In knowledge-active mode,
there are additional links from a memory to derived
statements via the `entity_mentions` and `statements_by_evidence`
indexes, but those are tracked separately, not on the memory
row itself.

> **Why does Brain track "salience" at all?**
>
> Because cognitive memory needs gradation. A 6-month-old
> memory you haven't touched should rank below a 2-day-old
> one on the same topic. A memory that's been recalled 50
> times should rank above one that's been recalled once. Pure
> recency-only or pure access-count-only would each be wrong
> in different ways; salience combines them with a decay
> curve so the ranking stays sensible across timescales.
> Chapter 07 covers the math.

---

## The `MemoryId`

The `memory_id` is 16 bytes, but it's not a random UUID. It
encodes three things:

```
MemoryId  (16 bytes)
├── shard_id      2 bytes   ← which shard owns this memory
├── slot_id       6 bytes   ← which storage slot in the arena
├── slot_version  4 bytes   ← bumps every time this slot is reused
└── reserved      4 bytes
```

Most of this is invisible to clients — you treat a `memory_id`
as an opaque token. But the design has a real consequence
worth understanding:

> **Why `slot_version`?**
>
> When you forget a memory, its arena slot eventually gets
> reclaimed after the grace period (default 7 days). The slot
> can then be reused by a future `encode`. If the slot's
> address were the only identifier, the new memory would have
> the same `memory_id` as the old one — and a client still
> holding the old id would get the wrong memory.
>
> Brain solves this by bumping `slot_version` every time the
> slot is reclaimed. The old `memory_id` has `slot_version =
> 3`; the new one has `slot_version = 4`. A lookup with the
> old id finds the slot, sees the version doesn't match, and
> returns `MemoryNotFound`.

The substrate has up to ~281 trillion slots per shard
(48 bits) and each slot can be reused ~4 billion times
(32 bits of version) before retirement. Realistically you
never hit either limit.

---

## The lifecycle of a memory

```
   ┌──────────────────┐
   │  client calls    │
   │  encode(text)    │
   └────────┬─────────┘
            │
            ▼
   ┌──────────────────┐
   │   Live           │  ← memory is queryable
   │                  │     - vector is in the index
   │                  │     - row exists in metadata
   │                  │     - decay worker may lower salience
   │                  │     - access_count rises on recall
   └────────┬─────────┘
            │   client calls forget(memory_id)
            ▼
   ┌──────────────────┐
   │   Tombstoned     │  ← memory is hidden but data is intact
   │                  │     - vector still in arena but tombstone
   │                  │       bit set, search filters it out
   │                  │     - row marked tombstoned in metadata
   │                  │     - waits out the grace period
   └────────┬─────────┘
            │   (grace period elapses, default 7 days)
            ▼
   ┌──────────────────┐
   │   Reclaimed      │  ← the slot is now free
   │                  │     - slot_version bumps
   │                  │     - any old MemoryId now NotFound
   │                  │     - slot can be re-used by a future
   │                  │       encode
   └──────────────────┘
```

Five things happen during a `forget` call:

1. The substrate marks the memory's metadata row as
   `tombstoned`.
2. The vector is **not removed from the arena**; only a flag
   bit is set. (Removing it would mean rewriting the file;
   flipping a bit is much cheaper.)
3. The vector *is* removed from the in-RAM search index, so
   recall can't return it.
4. A `FORGET` record goes to the write-ahead log; fsync;
   acknowledge.
5. If `hard_forget = true` was set, the vector and text bytes
   are zeroed immediately so they can't be recovered from
   disk.

Then time passes. The `slot_reclamation` background worker
([chapter 07](07-salience-decay-consolidation.md) for the
worker family) runs periodically. When it sees a tombstoned
memory older than the grace period, it:

1. Writes a `RECLAIM` WAL record (so recovery knows about it).
2. Bumps the slot's `slot_version`.
3. Clears the flags so the slot is free.

After reclamation, a future `encode` may land in that same
slot — with a new `slot_version`, so the new memory's
`memory_id` differs from the old one.

---

## Memory-to-memory edges

The substrate also lets memories reference each other through
typed edges. An edge is metadata, not a memory itself, with
a type drawn from a small fixed set:

| EdgeKind | Means |
|---|---|
| `DerivedFrom` | This memory came from consolidating those. Set automatically by the consolidation worker. |
| `CausedBy` | This memory describes an event caused by that one. |
| `Contradicts` | This memory contradicts that one. |
| `Supports` | This memory supports a claim made in that one. |
| `Mentions` | This memory mentions that one (used by some extractors). |

Edges are bidirectional in storage (Brain maintains both
`source → target` and `target → source` indexes) so you can
ask "what does this memory cause?" and "what causes this
memory?" with equal cost. Chapter 11 covers edges further
when we get to statements (which also have evidence edges).

You can add edges explicitly with `link(source, edge_kind,
target)`. The consolidation worker adds `DerivedFrom` edges
on its own when it produces a consolidated memory.

---

## What you get back from `recall`

For each memory `recall` returns, the response carries:

```
Hit
├── memory_id      mem_018f2b22…   (16-byte handle)
├── text           "Priya wants to move sprint planning to async."
├── similarity     0.81             (vs the query; cosine)
├── salience       0.74             (current; reflects decay + boosts)
├── kind           Episodic
├── created_at     2024-09-12T14:31:22Z
├── …
```

Whether the response includes the full `text` is a request
option — for cue-heavy workloads where most hits are
recognised by their ID alone, omitting text saves a redb
lookup per result. The default is to include it.

The `similarity` is *only* set for hits returned by vector
search. In knowledge-active mode, hits may come from lexical
or graph retrievers too; for those, `similarity` is `None`
and a different score type applies (chapter 21).

---

## What a memory is *not*

Three quick clarifications:

- **A memory isn't a row in a key-value store.** Looking it up
  by `memory_id` works (it's an O(1) bit-extraction + slot
  lookup), but `memory_id`s are minted by Brain, not by the
  client. You can't pre-pick the id.
- **A memory isn't a document.** No nested JSON, no
  attachments, no per-memory schema. The text is opaque to
  Brain; the embedding represents its meaning.
- **A memory isn't an entity or statement.** Memories are
  the substrate; entities and statements are *derived from*
  memories by extractors. Chapter 10–11 covers the
  distinction.

---

## Where memories live on disk

Briefly, for orientation; full detail in
[chapter 19](19-mmap-and-arenas.md).

A shard's directory holds:

- `arena.bin` — a memory-mapped file with one fixed-size
  slot per memory. The vector and a tiny piece of metadata
  live here.
- `metadata.redb` — a B-tree key/value store with the full
  metadata row, the text, and the edges.
- `wal/` — write-ahead log segments capturing every state
  change.

When you encode a memory, the vector goes to the arena, the
full metadata + text go to redb, and the *change* gets logged
in the WAL. When you forget, the same three stores see the
mutation. The WAL is the durability ground truth; the arena
and metadata are derived from it (chapter 18).

---

## Recap

- A memory in Brain is text + a 384-float vector + metadata.
- The metadata tracks who owns the memory, its kind, its
  salience, when it was created/accessed/forgotten, and its
  state flags.
- The `MemoryId` is a 16-byte handle that encodes which shard
  + slot the memory lives in, plus a version that bumps on
  every slot reuse. That version is what makes stale handles
  safe.
- Lifecycle: live → tombstoned (on forget) → reclaimed (after
  grace period). Tombstones are filter-cheap; reclamation is
  the expensive step done in the background.
- The substrate has memory-to-memory edges with a small fixed
  set of types. The knowledge layer adds typed entity /
  statement / relation graph on top — chapter 10 onward.

---

## Where to go next

- **The three memory kinds:** [chapter 06](06-memory-kinds.md)
  — what Episodic, Semantic, and Consolidated mean.
- **Salience, decay, consolidation:** [chapter 07](07-salience-decay-consolidation.md)
  — the cognitive-science-flavoured part.
- **What the vector is:** [chapter 08](08-embeddings.md).
- **How memories live on disk:**
  [chapter 19](19-mmap-and-arenas.md) — arenas, mmap, slots.
- **What "durable" really means:**
  [chapter 18](18-storage-and-durability.md) — the WAL and
  the fsync story.
