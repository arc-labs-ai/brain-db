# 07 — Salience, decay, and consolidation

Three pieces of vocabulary that come up over and over:
**salience** is a number representing how "alive" a memory
feels, **decay** is the process that lowers it over time, and
**consolidation** is the process that summarises similar
episodic memories into a semantic-shaped one.

Together they're what makes Brain's memory model *cognitive*
rather than just *storage*. A vector database stores
everything at equal weight forever; Brain has gradations and
graceful aging.

---

## Salience, concretely

Every memory carries a `salience` score in roughly `[0, 1]`.
Higher means more "alive." Three things determine the current
salience:

```
salience_now = decay(salience_initial, age, kind) + access_boosts
```

- **`salience_initial`** is the salience the memory was
  created with. Default is 0.7. The encode call can override
  it.
- **`decay`** is a function of age (how long since the memory
  was created), the memory's kind (different half-lives), and
  the time elapsed since the last access.
- **`access_boosts`** is the cumulative bump from recalls.
  Every time the memory is returned in a recall, the
  access-boost worker adds a small increment.

Salience is **not** a probability. It's not a confidence
score. It's a *ranking signal* — used to rank memories in
recall responses when other signals (vector similarity, BM25)
are close.

> **Why a number, not a flag?**
>
> A flag would be too coarse: "active" or "not active" doesn't
> capture "this memory is still relevant but only barely." A
> number captures gradation, and ranking by a number is
> trivially implementable. We're not trying to mirror human
> cognition exactly — just to give Brain a useful continuous
> signal.

---

## The forgetting curve

Brain's decay function is shaped after the **Ebbinghaus
forgetting curve** — Hermann Ebbinghaus's 1885 observation
that without rehearsal, human memory of new information
declines roughly exponentially with time.

The classic curve:

```
retention(t) ≈ exp(-t / S)
```

where `t` is time since learning and `S` is a stability
parameter that depends on the memory and the person. The
intuition: most forgetting happens early; the curve flattens
out for memories that survive the initial drop.

> **Where does this come from?**
>
> Ebbinghaus memorised nonsense syllables and tested himself
> at varying intervals, plotting how much he retained. His
> 1885 monograph *Über das Gedächtnis* is the founding
> reference. Modern psychology has refined the model
> considerably; the rough shape (sharp early drop, gentle
> long tail) holds up.
>
> See [Wikipedia: Forgetting curve](https://en.wikipedia.org/wiki/Forgetting_curve).

Brain uses an exponential decay parametrised by a **half-life**
per memory kind:

```
salience(t) = salience_initial × (1/2) ^ (t / half_life)
```

After one half-life, salience is half. After two, a quarter.
After three, an eighth.

| Kind | Half-life | Salience after 30 days (starting at 0.7) |
|---|---|---|
| Episodic | 30 days | 0.35 |
| Consolidated | 90 days | ~0.56 |
| Semantic | 365 days | ~0.66 |

Same starting salience, very different month-old values.
That's the whole point of having kinds.

---

## The access boost

Recall is what *keeps memories alive*. Every time a memory is
returned in a recall result, the **access-boost worker** runs
shortly after and adds a small increment to the memory's
salience:

```
new_salience = min(1.0, salience_now + boost_factor)
# default boost_factor ≈ 0.05
```

The `min(1.0, …)` clamps salience so it can't exceed the
maximum. A memory that's recalled many times in a short
window plateaus at salience ~1.0 — it's still subject to
decay, but its starting point keeps getting reset.

The boost worker is *separate* from the recall path. The
recall response goes back to the client immediately; the
boost is queued and applied a moment later by a background
worker. This avoids slowing down recall for the boost
bookkeeping.

The boost is cumulative across recalls within a short window
but bounded: even if the same memory is recalled 100 times in
a minute, the in-flight boost queue dedupes so the salience
goes up by roughly one boost increment, not 100.

> **Why a background worker, not a synchronous write?**
>
> Recall reads from many memories (top-K ≈ 10–100). If each
> recall synchronously wrote a salience update for each
> returned memory, you'd be doing K writes per read — turning
> a read-heavy workload into a write-heavy one. The
> access-boost worker batches updates and applies them
> efficiently in bulk.

---

## How decay and boost interact

Picture a memory created on day 0 with salience 0.7, recalled
on days 5, 12, and 20, then never touched again:

```
salience
   1.0 │
       │                              boost on day 12: +0.05
       │     boost on day 5: +0.05    boost on day 20: +0.05
   0.7 ●───╮                    ╭───╮
       │    ╲                  ╱     ╲
       │     ╲    ╭──╮        ╱       ╲
       │      ╲  ╱    ╲      ╱         ╲
       │       ●╱      ●────●          ╲
       │                                ╲    decay continues, untouched
       │                                 ╲
       │                                  ╲___
   0.0 │_______________________________________________________ time (days)
       0       5       10       15       20       …      365
```

(ASCII can't really capture the smoothness — imagine the
declines being curves, not straight lines.)

The pattern: decay all the time, occasional small bumps on
recall, and the bumps eventually stop when the agent moves
on. That last untouched stretch is when the memory drops
fast.

The system isn't trying to predict whether *you* will recall
this memory in the future — it's just keeping a continuous
score that ranks "things the agent is actively using" above
"things the agent has moved on from."

---

## Practical consequences for recall ranking

When `recall` returns results, the candidates come from
vector similarity (or hybrid retrieval in knowledge-active
mode). Among candidates with similar similarity scores, the
ranker uses salience as a tiebreaker:

```
final_rank ≈ f(similarity, salience, …)
```

The exact function matters less than the intuition: if two
memories are about equally similar to your cue, the higher-
salience one comes first. This means:

- A 2-day-old Episodic memory beats a 60-day-old Episodic
  memory on the same topic. Recent things rank higher.
- A 6-month-old Semantic memory beats a 6-month-old Episodic
  memory on the same topic. Persistent knowledge holds up
  longer.
- A heavily-recalled memory beats a never-recalled one. What
  the agent uses, it ranks higher.

You don't have to do anything to get this behaviour — it just
happens, driven by the worker that runs decay and the worker
that runs the access boost.

---

## Consolidation

The third piece of the chapter. Where decay and boost manage
memory's *salience*, consolidation manages its *level of
abstraction*.

The consolidation worker, by default, runs every 5 minutes
per shard. Each cycle:

1. **Scans** recently-created episodic memories (typically
   the last hour or so).
2. **Clusters** them by vector similarity. Two memories whose
   embeddings cosine-match above ~0.6 are candidates to
   cluster together.
3. **For each cluster of 3+ memories** (the default minimum):
   emit a new memory of kind **Consolidated**, whose:
   - Text is either:
     - An LLM-generated summary, if a `Summarizer` is
       configured.
     - Or a deterministic concatenation of the cluster's
       texts, if not.
   - Vector is freshly computed from the consolidated text.
   - `DerivedFrom` edges point to every source memory in the
     cluster.

The cluster's source memories *are not deleted*. They stay
where they are, as Episodic memories, decaying on their own
30-day clock. The new Consolidated memory rides a slower
90-day clock and represents the cluster at a higher level.

### Why consolidate at all

Two reasons:

1. **The agent gets more general over time.** After three
   months of the same kinds of meetings, the substrate has a
   handful of consolidated "the team had multiple meetings
   about X" summaries that the agent can recall, instead of
   relying on 47 individual standup memories that have all
   decayed below useful salience.
2. **Recall stays fast for long-running deployments.** A
   shard with 10M episodic memories and no consolidation
   eventually drowns its own search index in stale near-
   duplicates. Consolidation, paired with eventual
   reclamation, keeps the working set sensible.

The trade-off is cost: consolidation runs an LLM call per
cluster (when configured), which costs money. The worker has
budget controls. A deployment that wants the lower-cost path
configures a deterministic summariser instead of an LLM one.

> **What's a "Summarizer"?**
>
> A small interface inside Brain that takes a list of memory
> texts and produces one summary text. The default
> implementation is `DisabledSummarizer` (concatenation, no
> LLM call). Operators can wire an LLM summarizer
> (Anthropic, OpenAI) by setting the relevant env vars.
> Chapter 14 covers the LLM tier generally.

---

## What an operator can tune

A few knobs, mostly in the worker config:

- **Decay half-lives** (`EPISODIC_HALF_LIFE_DAYS`,
  `SEMANTIC_HALF_LIFE_DAYS`, `CONSOLIDATED_HALF_LIFE_DAYS`)
  — defaults 30 / 365 / 90.
- **Access boost factor** (`DEFAULT_BOOST_FACTOR`) — default
  ~0.05.
- **Consolidation similarity threshold**
  (`DEFAULT_SIMILARITY_THRESHOLD`) — default 0.6. Higher
  means clusters must be tighter; lower means looser
  groupings.
- **Consolidation minimum cluster size**
  (`DEFAULT_MIN_CLUSTER_SIZE`) — default 3. Fewer than 3 and
  no summary is produced.
- **Decay sweep interval** — default 1 hour. The decay
  worker scans memories and updates salience at this
  cadence.

Tuning these is rare. The defaults are reasonable; if
something feels off (memories aging too fast, consolidations
too aggressive), adjust the single knob you suspect and
measure.

---

## What's not part of this story

A few things that *aren't* part of salience / decay /
consolidation:

- **Forgetting** is a separate verb — the client explicitly
  asks to forget. Decay is automatic; forgetting is
  intentional. A salience of 0.0 doesn't trigger automatic
  deletion; it just means the memory ranks last in recall
  results.
- **The vector index doesn't shrink as memories decay.**
  Decay updates a salience number; the vector stays in the
  index. The HNSW maintenance worker handles index pruning
  separately (chapter 20).
- **Salience doesn't propagate.** A memory's salience is
  independent of its derived statements' confidence. If you
  want "confidence over time," that's the knowledge layer's
  job (chapter 11).

---

## Recap

- **Salience** is a per-memory `[0, 1]` score that ranks
  memories in recall responses.
- **Decay** lowers salience over time, parametrised by a
  half-life that depends on the memory's kind (30 / 90 /
  365 days for Episodic / Consolidated / Semantic).
- **The access boost** raises salience on recall, capped at
  1.0. A background worker applies it shortly after the
  recall response goes out.
- **Consolidation** clusters related episodic memories and
  emits a new Consolidated memory summarising the cluster.
  Source memories aren't deleted; they continue decaying on
  their own.
- All of this happens automatically. The client doesn't have
  to do anything special.

---

## Where to go next

- **What a memory contains:** [chapter 05](05-memories.md).
- **Why the three kinds matter:** [chapter 06](06-memory-kinds.md).
- **What an embedding is:** [chapter 08](08-embeddings.md).
- **How similarity is measured:** [chapter 09](09-vector-similarity.md).
- **The full background-worker catalogue:**
  [`../architecture/07-background-workers.md`](../architecture/07-background-workers.md).
