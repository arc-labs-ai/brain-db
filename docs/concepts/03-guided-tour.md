# 03 — A guided tour

The first two chapters were abstract. This one is concrete: a
single agent talks to Brain, encodes a few memories, recalls
some of them, forgets one, and we look at what happened.

Code in this chapter is **pseudocode** — language-neutral,
readable like English. The real Rust SDK lives in
[`../reference/sdk-rust.md`](../reference/sdk-rust.md); other
language SDKs are listed there too. The shapes are real even
when the syntax isn't.

We'll do the tour twice:

1. First in **substrate-only mode** — the simple, vector-only
   experience.
2. Then in **knowledge-active mode** — what changes when a
   schema is declared.

---

## Setting up

```
session = brain.connect(
    addr     = "127.0.0.1:9090",
    agent_id = "alice@acme.com",
)
```

A connection negotiates a wire-protocol version, authenticates
the agent, and stays open across many operations. From here
the agent has a *session* and can call cognitive verbs against
it.

> **What's an agent_id?**
>
> An `agent_id` is a UUID-shaped identifier for who is talking
> to Brain. Every memory the agent encodes is tagged with this
> id. Memories are *isolated by agent_id*: agent A cannot see
> agent B's memories. Brain calls this **per-agent
> isolation** — covered in chapter 23.

For the tour we'll pretend Alice is a personal-assistant agent
talking on behalf of a user.

---

## Substrate-only mode: the tour

### Encode some memories

```
session.encode(text =
    "Met Priya at the offsite, talked about the Atlas project."
)
# → memory_id = mem_018f2b1e…
# → salience = 0.7

session.encode(text =
    "Priya wants to move sprint planning to async."
)
# → memory_id = mem_018f2b22…
# → salience = 0.7

session.encode(text =
    "Joined the Atlas standup. Sprint goals for the week:
     ship the auth migration."
)
# → memory_id = mem_018f2b41…
# → salience = 0.7
```

Each call:

1. Sends the text to Brain.
2. Brain embeds it on the server (the agent never computes a
   vector).
3. Brain writes a record to the write-ahead log, fsyncs it to
   disk, *then* responds. By the time `encode` returns, the
   memory is durable.
4. Brain inserts it into the vector index so it's findable.
5. Brain returns a `memory_id` — a 16-byte handle for this
   specific memory — and an initial salience score.

> **What's a "salience"?**
>
> Salience is a number in roughly `[0.0, 1.0]` that represents
> how "alive" this memory feels. Fresh memories start around
> 0.7; they decay slowly over days/weeks; they get a boost
> every time they're recalled. Recall ranking takes salience
> into account, so older-but-still-relevant memories don't
> drop out as fast. Chapter 07 covers it.

The agent now has three memories on the substrate.

### Recall something

```
hits = session.recall(
    cue   = "What did Priya say about meetings?",
    top_k = 5,
)
```

The agent asked in natural language. Brain:

1. Embeds the cue.
2. Searches the vector index for the most-similar memories.
3. Returns them ranked by similarity.

The result might look like:

```
hits = [
    {
        memory_id  = mem_018f2b22…,
        text       = "Priya wants to move sprint planning to async.",
        similarity = 0.81,
        salience   = 0.7,
    },
    {
        memory_id  = mem_018f2b1e…,
        text       = "Met Priya at the offsite, talked about the Atlas project.",
        similarity = 0.55,
        salience   = 0.7,
    },
    # … the third one ranks lower; it's about standup not meetings
]
```

Two things to notice:

- The top hit didn't contain the *word* "meetings" — it said
  "sprint planning." The embedding captured that they mean
  similar things. This is what vector similarity buys you
  over keyword search.
- The agent got back a list, ranked, with similarity scores.
  Brain doesn't pick "the one true answer" — it returns
  candidates and lets the caller decide.

> **What does "similarity 0.81" mean?**
>
> Brain reports similarity as cosine similarity in `[-1, 1]`,
> where 1.0 means identical, 0 means unrelated, -1 means
> opposite. For L2-normalised embeddings (what BGE produces),
> cosine similarity is equivalent to a dot product. In
> practice, similarities above ~0.7 are "very related";
> 0.5–0.7 is "related"; below 0.4 is mostly noise. Chapter
> 09 covers this.

### Forget something

```
session.forget(memory_id = mem_018f2b41…)
# → soft-forget acknowledged
```

The standup memory is gone — at least from queries. What
Brain actually does:

1. Marks the memory as *tombstoned* in the metadata: still on
   disk, but invisible to recall.
2. Writes a `FORGET` record to the write-ahead log; fsync;
   respond.
3. Removes the memory from the vector index so it can't be
   returned by future recalls.

A subsequent recall returning the same cue won't surface that
memory:

```
hits = session.recall(cue = "What was the sprint about?")
# → returns the offsite memory, but not the standup one
```

After a configurable **grace period** (default 7 days), a
background worker reclaims the storage slot — that's when the
memory's bytes actually get released. The grace period exists
because the agent might have given out the memory's ID to
something downstream; reclaiming immediately could let a fresh
new memory accidentally hit that same slot ID.

For genuinely sensitive content there's a `hard_forget` option
that zeroes the vector and text bytes immediately, before the
grace period. Chapter 18 covers both modes.

### What the substrate side looks like, end of session

After the three encodes and one forget, Brain holds:

- 2 live memories (the offsite + the preference), each with
  text, vector, metadata, and an entry in the vector index.
- 1 tombstoned memory (the standup), still on disk but
  invisible to queries, awaiting reclamation.
- A write-ahead log with 4 records (3 encodes + 1 forget),
  all fsynced to disk.

If the server crashed right now, recovery would replay the WAL
and the state would come back exactly as above.

---

## Knowledge-active mode: the tour, with schema

Now let's do the same session, but with a schema declared. The
agent calls:

```
session.schema_upload(text = """
    entity_type Person {
        attributes { email: String? }
    }

    entity_type Project {
        attributes { codename: String }
    }

    predicate works_on (Person, Project)
    predicate prefers  (Person, *)         # * = any object shape
    predicate met_at   (Person, Person)

    extractor person_mentions {
        kind     = pattern
        patterns = [ /\b[A-Z][a-z]+\b/ ]
        target   = entity Person
    }

    extractor preference_extraction {
        kind             = llm
        target           = statement Preference
        model            = "claude-haiku-4-5"
        cost_budget      = "$0.001 per memory"
    }
""")
# → schema accepted, schema_version = 1
```

Brain validated the schema, recorded it, and flipped the
shard's schema gate from off to on. From now on, every encode
runs through the declared extractors.

### Encode the same memories

Same three calls as before. Same `memory_id`s back, same
substrate behaviour. **What's different is what happens after
the substrate write:**

```
session.encode(text =
    "Met Priya at the offsite, talked about the Atlas project."
)
# → memory_id = mem_018f2b1e…
# … and behind the scenes, in the same shard:
#   - person_mentions extractor sees "Priya"
#       → emits Entity candidate: Person { name = "Priya" }
#       → Brain resolves it: no existing Priya, creates entity ent_01…
#   - person_mentions extractor sees "Atlas"
#       → emits Entity candidate
#       → Brain resolves it: it's a Project, not a Person — extractor
#         was wrong, this entity is dropped
#   - background queue: preference_extraction (LLM) gets called
#       → no preference here, drops the memory
```

The encode response is unchanged. The extractor work runs
*synchronously* for fast tiers (pattern + classifier) and
*in the background* for slow tiers (LLM). The client never
waits on the LLM tier.

After the second encode ("Priya wants to move sprint planning
to async"):

```
# Synchronously:
#   - person_mentions: "Priya" → resolves to the existing entity ent_01…
#
# In the background:
#   - preference_extraction (LLM) processes the memory text
#       → emits Statement {
#             kind:      Preference,
#             subject:   ent_01…  (Priya),
#             predicate: "prefers",
#             object:    "async sprint planning",
#             evidence:  [mem_018f2b22…],
#             confidence: 0.84,
#         }
#       → Brain stores it in the statements table
```

Some seconds later the statement is queryable. The original
memory is still right where it was — the statement is *derived
from* it.

### Recall — substrate-style vs knowledge-style

The substrate-style recall still works:

```
hits = session.recall(cue = "What did Priya say about meetings?")
# → same as before: ranked memories by vector similarity
```

But there's now a *second* shape of recall available — a
**structured query** that uses the typed knowledge layer:

```
results = session.query(
    entity_anchor = ent_01…,           # Priya
    kind_filter   = [ Preference ],
    limit         = 5,
)
```

This asks: "give me Preferences about Priya." Brain:

1. Hits the graph retriever to find statements anchored on
   Priya.
2. Filters to Preferences.
3. Returns the matching statements, ranked by recency +
   confidence.

The response now includes *typed* objects:

```
results = [
    {
        statement_id = stmt_07a…,
        kind         = Preference,
        subject      = "Priya",
        predicate    = "prefers",
        object       = "async sprint planning",
        confidence   = 0.84,
        evidence     = [
            { memory_id = mem_018f2b22…, text = "Priya wants to …" },
        ],
        extracted_at = 2024-09-12T14:31:22Z,
    },
]
```

Notice the `evidence` list — that's the **provenance** the
knowledge layer maintains. The statement points back to the
memory that supported it. If the agent asks "*why* do you
think she prefers async?", Brain hands back the exact memory.

### Recall with the hybrid path

Even the simple `recall` verb behaves differently now. Same
call:

```
hits = session.recall(cue = "What did Priya say about meetings?")
```

Under the hood:

1. Brain notices "Priya" is a known entity (the recall
   classifier detects an entity mention).
2. Three retrievers run in parallel:
   - **Semantic** — vectors similar to the cue, like before.
   - **Lexical** — BM25 text search for "Priya" / "meetings"
     in the tantivy index.
   - **Graph** — statements anchored on the Priya entity.
3. The three ranked lists are merged with **RRF** (chapter
   21).
4. The fused ranking is returned.

The response shape is the same — the agent just gets back a
better-ranked list because the three retrievers cover each
other's blind spots. Chapter 17 covers this in detail.

### Forget — and what cascades

```
session.forget(memory_id = mem_018f2b22…)   # the preference memory
```

The substrate side is identical: tombstone, WAL record,
remove from vector index.

But there's now a knowledge-side consequence. The statement
`Priya prefers async sprint planning` lists this memory as
evidence. A background worker (`forget_cascade`) notices the
evidence is gone and:

- Recomputes the statement's evidence list.
- If other memories still support the statement, just updates
  confidence.
- If this was the only memory supporting it, supersedes the
  statement with `superseded_by = null` — the statement is
  effectively retracted but the chain remains queryable for
  audit.

That cascade is what "provenance" buys: forgetting a memory
doesn't leave orphan claims hanging around.

---

## Side-by-side

The same code path, both modes:

| Step | Substrate-only | Knowledge-active |
|---|---|---|
| `encode(text)` | embed + WAL + arena + vector index | + sync pattern/classifier + background LLM |
| `recall(cue)` | vector search → ranked memories | three retrievers (semantic + lexical + graph) → RRF fused ranking |
| `query(...)` | not available | structured query over entities/statements/relations |
| `forget(memory_id)` | tombstone + remove from vector index | + cascade: recompute affected statements, supersede if no evidence remains |
| Response shape | `Memory` objects | `Memory` *or* `Statement` *or* `Entity` objects, depending on what matched |

The client's code is mostly the same. The *value* of the
response is what changes.

---

## What we didn't cover

This tour skipped several things on purpose — they each get
their own chapter:

- **Plan / Reason** — two more cognitive verbs the agent has
  available. Chapter 16.
- **Subscriptions** — a stream you can open to get notified
  when new memories or statements arrive. Reference.
- **Transactions** — multi-record operations that commit
  atomically. Reference.
- **Idempotency** — every state-mutating call takes a
  `request_id` and retrying with the same id is safe. Chapter
  25.
- **The actual wire bytes** — TLS handshake, frame format, the
  binary protocol. Architecture chapters 01–02 and
  [`../reference/wire-protocol/`](../reference/wire-protocol/).
- **What happens when shards are involved** — for one agent
  with low traffic the tour above is the whole picture.
  Multi-agent or high-throughput deployments shard across
  many cores; chapter 23 covers it.

---

## Recap

- An agent connects, authenticates, and calls cognitive verbs
  in a session.
- `encode(text)` durably stores a memory (text + embedding +
  metadata) and returns a `memory_id`.
- `recall(cue)` returns memories whose embeddings are similar
  to the cue. In knowledge-active mode it fuses three
  retrievers and may return statements/entities too.
- `forget(memory_id)` tombstones the memory immediately and
  reclaims storage after a grace period. In knowledge-active
  mode, forgetting cascades into statements that cited the
  memory as evidence.
- The wire shape doesn't change between modes; what's *in*
  the responses does.

---

## Where to go next

- **More context on the verbs:** [chapter 16](16-cognitive-operations.md).
- **Comparing Brain to alternatives:** [chapter 04](04-vs-other-systems.md).
- **The memory side, in detail:**
  - [chapter 05](05-memories.md) — what's in a memory.
  - [chapter 08](08-embeddings.md) — what an embedding is.
- **The knowledge side, in detail:**
  - [chapter 10](10-entities.md) — entities.
  - [chapter 11](11-statements.md) — statements (with the Fact
    / Preference / Event distinction in chapter 12).
- **The verbs themselves:** [chapter 16](16-cognitive-operations.md).
- **What `recall` actually does:** [chapter 17](17-hybrid-retrieval.md).
