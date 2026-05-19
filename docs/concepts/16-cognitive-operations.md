# 16 — The cognitive operations

Brain's API has five primary verbs:

- **encode** — store a memory
- **recall** — find memories similar to a cue
- **plan** — propose a sequence of memories that lead to a goal
- **reason** — combine memories into a derived conclusion
- **forget** — remove a memory

There are a few others (`link`, `unlink`, `subscribe`, the
transaction trio, the knowledge-layer-specific `query`),
but those five are the substrate's core vocabulary.

This chapter explains each verb in plain English, with the
question "why these verbs and not `put/get/query`?" answered
up front.

---

## Why these verbs

A SQL database has `INSERT / SELECT / UPDATE / DELETE`. A
key-value store has `put / get / delete`. A vector database
has `upsert / query / delete`. Why does Brain pick different
words?

Two reasons.

**One: the verbs match the agent's mental model.** An AI
agent doesn't think "let me run an UPSERT against my
memory table." It thinks "let me remember this." It
doesn't run a SELECT — it tries to recall. The substrate's
job is to *be the right abstraction for that mental model*.
If the agent has to translate from "I want to remember X"
to "I should call `upsert` on my vector DB," the abstraction
is wrong by one level.

**Two: the verbs carry different semantics than CRUD.**

- `encode` runs an embedding model and a WAL fsync. It's
  not a fast `put`.
- `recall` runs vector search and (in knowledge-active
  mode) fuses three retrievers. It's not a single index
  lookup.
- `forget` tombstones with a grace period and cascades
  through derived statements. It's not a `DELETE`.

Using CRUD names would lie about what the operations do.
The cognitive verbs are *accurate* — they describe what's
actually happening at the substrate level.

---

## encode

```
session.encode(text, kind = Episodic) → MemoryId
```

Stores a new memory. The full path
([chapter 03](03-guided-tour.md), [chapter 05](05-memories.md),
[chapter 18](18-storage-and-durability.md)):

1. The text travels from client to server.
2. Brain embeds it (chapter 08).
3. Brain allocates a storage slot.
4. Brain writes a record to the write-ahead log, fsyncs to
   disk.
5. Brain inserts the embedding into the vector index.
6. Brain updates the metadata store.
7. In knowledge-active mode: pattern and classifier
   extractors run synchronously; LLM extractors queue for
   background processing.
8. `encode` returns the `MemoryId`.

Two important things:

- **Encode is not free.** The embedding step is the slowest
  piece — 5-10 ms on CPU for a cache miss. Brain has an
  LRU cache for repeated texts; hits skip the embedding
  entirely.
- **Encode is durable on return.** Once `encode` has
  acknowledged, the memory is on stable storage. A power
  loss after the ack doesn't lose the memory. (Chapter 18
  for the durability story.)

The optional `kind` parameter picks one of Episodic,
Semantic, or Consolidated (chapter 06). Default is
Episodic, which is right for most encodes.

### Idempotency on encode

Every `encode` carries a client-generated `request_id`. If
you retry the same call with the same `request_id` (within
24 hours), Brain returns the cached response from the
first call — same `memory_id`, same outcome. It does *not*
re-execute and create a second memory.

If you retry with the same `request_id` but *different
text*, Brain returns `IdempotencyConflict`. That's a client
bug; the substrate refuses to silently double-write.

Chapter 25 covers idempotency in depth.

---

## recall

```
session.recall(cue, top_k = 10) → [Hit]
```

Finds memories similar to the cue. The cue can be:

- **A text string.** Brain embeds it on the server. Most
  common case.
- **A pre-computed vector** (rare). Skip the embedding
  step.
- **A `MemoryId`** (substrate-only). Find memories similar
  to *this existing memory*.

The default behaviour is vector search over the memory
index. In knowledge-active mode, recall fans out to three
retrievers (semantic + lexical + graph) and fuses results
with RRF — chapter 17 covers this hybrid path in detail.

The response is a ranked list of `Hit`s, each carrying the
memory's text (optional, configurable), similarity score,
salience, kind, timestamps, and the `MemoryId`. In
knowledge-active mode results can include statements and
entities too.

### Filters

```
session.recall(
    cue              = "what happened in standup yesterday?",
    top_k            = 20,
    filter_kind      = [ Episodic ],
    filter_time      = TimeRange(from = -1d),
    filter_agent     = "alice",
)
```

Filters narrow the candidate set *before* ranking. Common
filters:

- **`kind`** — memory kind (Episodic / Semantic / Consolidated).
- **`time`** — date range, often relative ("last 7 days").
- **`agent`** — restrict to a specific agent (rare; usually
  the session's agent is implicit).
- **`context`** — restrict to a context namespace.
- **`confidence_min`** (knowledge-active) — minimum
  confidence for returned statements.

Filters that map cleanly to indexes (kind, time) become
pre-filters at the retriever layer — cheap. Filters that
require metadata lookups (per-memory salience threshold)
run post-fusion on the survivors — more expensive but
correct.

### `top_k`

`top_k` is the maximum number of results to return. Higher
values let you see more of the tail (but cost more time).
Most workloads use `top_k = 10` to `100`. The substrate caps
this at a hard maximum (configurable; default 200) to
prevent abuse.

`recall` is *read-only*. No durability cost, no WAL record.
The salience update that follows a recall (the access boost,
chapter 07) is asynchronous and batched.

---

## plan

```
session.plan(goal, top_k = 10) → [PlanStep]
```

Returns a sequence of memories the agent could traverse to
*get to* the goal. Think of it as recall with a *direction*.

Plan is what an agent uses when it's not asking "what's
similar to my question?" but "what's a chain of relevant
memories I should consider to act?"

Two algorithms run:

- **Forward traversal.** Starting from highly-salient recent
  memories, follow `caused_by` and `supports` edges towards
  goal-relevant memories. Returns the *forward chain*.
- **Backward search.** From the goal, walk backwards through
  `derived_from` and `mentions` edges to find supporting
  memories. Returns the *backward chain*.

The output ranks the candidates by chain coherence — how
well the chain "tells a story" from start to goal.

> **What does this look like in practice?**
>
> Imagine an agent that's deciding whether to schedule a
> meeting with Priya. The agent calls
> `plan(goal = "Should I schedule a 1:1 with Priya?")`.
> Brain returns chains of memories:
>
> 1. Recent memories about Priya's working preferences (she
>    prefers async meetings).
> 2. Memories about ongoing topics she's involved with
>    (Atlas migration, async sprint planning debate).
> 3. Memories about recent commitments she's made.
>
> The agent uses the chain to inform its decision — not
> "the answer," but the relevant *evidence* shaped as a
> traversal.

`plan` is less commonly used than `recall` because it
requires the agent to know what shape of question it's
asking. Many agents use `recall` exclusively and let the
calling LLM figure out chains downstream.

---

## reason

```
session.reason(operands, depth = 2) → DerivedMemory
```

Combines a set of memories into a *new* derived memory by
running a small reasoning chain. The operands are
`MemoryId`s; `depth` is how many composition steps to
perform.

A simple example: two memories,

> mem_a: "Priya is the Atlas team lead."
> mem_b: "The Atlas migration is scheduled for Q4."

`reason(operands = [mem_a, mem_b])` might produce:

> derived: "Priya is leading the Q4 Atlas migration."

The substrate uses **vector arithmetic** — sums, projections,
bindings — on the operand embeddings, optionally combined
with an LLM if one is configured, to produce a new
embedding. That embedding then gets stored as a new memory
(of kind Consolidated, with `derived_from` edges to the
operands).

`reason` is the most experimental of the verbs. It's useful
when the agent wants to *materialise* a deduction and have
it become recallable like any other memory. It's *not* a
substitute for an LLM call; it's a substrate-level
composition primitive.

Most production deployments use `recall` and let the LLM
do the actual reasoning at prompt-build time. `reason`
exists for the case where the deduction is worth
preserving as a memory for future recall.

---

## forget

```
session.forget(memory_id, hard = false) → Ack
```

Removes a memory. Two modes:

- **Soft forget** (default). Tombstones the memory:
  invisible to recall, vector stays in the arena until the
  grace period elapses. The grace period (default 7 days)
  is what makes `MemoryId`s safe for the period right after
  a forget: a future encode won't accidentally land in a
  reclaimed slot until the grace expires.
- **Hard forget** (opt-in). Zero the memory's vector and
  text bytes immediately, before the grace period. Used
  for compliance / privacy / accidentally-encoded sensitive
  content.

In knowledge-active mode, `forget` cascades:

- Every statement that cited this memory as evidence is
  re-evaluated.
- Statements that lose all evidence get superseded with
  `superseded_by = null` (retracted).
- Relations citing the memory get the same treatment.

The cascade runs in the background (`forget_cascade`
worker). The substrate's WAL record for the forget commits
immediately; the cascade catches up shortly after.

Like `encode`, `forget` is idempotent — retrying with the
same `request_id` is safe.

---

## query (knowledge-active only)

```
session.query(
    entity_anchor    = ent_priya,
    kind_filter      = [ Fact ],
    predicate_filter = [ pred_role ],
    limit            = 10,
) → [Statement]
```

A *structured* query over the knowledge layer. Unlike
`recall` (which takes text and returns ranked candidates),
`query` takes typed criteria and returns statements,
entities, or relations that match.

Available criteria:

- **`entity_anchor`** — start from this entity.
- **`kind_filter`** — only these statement kinds (Fact /
  Preference / Event).
- **`predicate_filter`** — only these predicates.
- **`time_filter`** — only within this date range.
- **`confidence_min`** — only above this confidence.
- **`include_tombstoned`** / **`include_superseded`** —
  defaults are *don't*; include them with these flags.
- **`limit`** — max results.

Chapter 17 covers the router that turns a `query` into a
plan.

`query` is the knowledge-layer counterpart to
substrate-side `recall`. They coexist: an agent can use
`recall` for "what memories are similar to this question"
and `query` for "what does Brain *know* about this entity."

---

## link / unlink

```
session.link(source_memory, edge_kind, target_memory) → Ack
session.unlink(source_memory, edge_kind, target_memory) → Ack
```

Add or remove a typed edge between two memories. The
substrate maintains a small set of edge kinds (`DerivedFrom`,
`CausedBy`, `Contradicts`, `Supports`, `Mentions`).

Most edges get added automatically by the consolidation
worker; `link` is for explicit cases where the agent (or
the application) knows about a connection.

---

## subscribe

```
session.subscribe(filter) → Stream
```

Open a stream that receives events about new memories or
statements matching the filter. Used for "react to incoming
data" workflows — for example, an agent that watches for
new memories about a specific entity and runs some logic.

`subscribe` is the only verb that returns a *stream* rather
than a single response. Behind the scenes the substrate
sends a sequence of frames over the same connection until
the client calls `unsubscribe` or closes the stream.

---

## transactions

```
session.txn_begin() → TxnId
session.encode(text, txn_id = ...) → ...
session.link(..., txn_id = ...) → ...
session.txn_commit(txn_id) → Ack
```

Group multiple state mutations into one atomic unit. Either
all of them commit together (one WAL fsync) or none do
(rollback on `txn_abort`).

Used for "this set of three encodes must succeed
together-or-not-at-all" cases. The substrate writes a
`TxnBegin` record, then the body records, then a
`TxnCommit`; recovery treats partial transactions
(without a matching commit) as aborts.

Most code doesn't need transactions. They're for the
unusual cases where atomicity across mutations matters.

---

## Substrate-only vs knowledge-active

The substrate-side verbs (`encode`, `recall`, `forget`,
`link`, `subscribe`) work the same in both modes. What
differs is:

| Verb | Substrate-only | Knowledge-active |
|---|---|---|
| `encode` | substrate write only | + sync pattern/classifier + background LLM |
| `recall` | vector search → ranked memories | three retrievers → fused ranking |
| `forget` | tombstone + remove from index | + cascade through statements/relations |
| `query` | *not available* | structured query over knowledge layer |
| `plan` | substrate-only graph (memory edges) | + knowledge-layer relations |
| `reason` | substrate vector composition | + optional LLM-aided composition |

The wire shape doesn't change. Same opcodes, same SDK
calls. The substrate just does more work behind the verbs
when the schema gate is on (chapter 02).

---

## Recap

- Brain's verbs are **cognitive**, not CRUD: `encode`,
  `recall`, `plan`, `reason`, `forget`, plus `query` for
  knowledge-active mode.
- Each verb maps to a specific cognitive operation, not to
  a single database primitive.
- `encode` is durable on return; `forget` tombstones with
  a grace period; `recall` is read-only and asynchronously
  boosts salience.
- Every state-mutating verb takes a `request_id` for
  idempotency.
- The verbs behave the same in substrate-only mode and
  knowledge-active mode; what differs is what they do under
  the hood.

---

## Where to go next

- **Hybrid retrieval in detail:** [chapter 17](17-hybrid-retrieval.md).
- **What durability really means:** [chapter 18](18-storage-and-durability.md).
- **Idempotency and replay:** [chapter 25](25-determinism-idempotency-replay.md).
- **The exact wire signatures:**
  [`../reference/cognitive-operations/`](../reference/cognitive-operations/).
