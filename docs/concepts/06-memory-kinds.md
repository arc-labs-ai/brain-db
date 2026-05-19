# 06 — Memory kinds

Every memory in Brain belongs to one of three kinds:
**Episodic**, **Semantic**, or **Consolidated**. This chapter
explains what each one means, why three kinds and not more,
and what difference the kind makes in practice.

The terminology is borrowed from cognitive psychology — the
distinction between "I remember meeting Priya at the offsite"
(episodic) and "I know Priya is a manager" (semantic) is a
real cognitive distinction with a long literature.

> **Where does this terminology come from?**
>
> Endel Tulving in the early 1970s distinguished between
> *episodic memory* (specific experiences tied to time and
> place) and *semantic memory* (general world knowledge).
> Brain adopts the same names because they map cleanly onto
> what an AI agent's memory actually contains.
>
> See [Wikipedia: Episodic memory](https://en.wikipedia.org/wiki/Episodic_memory)
> and [Wikipedia: Semantic memory](https://en.wikipedia.org/wiki/Semantic_memory).

---

## The three kinds, in one table

| Kind | Captures | Decay half-life | Example |
|---|---|---|---|
| **Episodic** | A specific moment, event, observation | 30 days | "Met Priya at the offsite, Sept 12." |
| **Semantic** | Generalised knowledge, beliefs, persistent facts | 365 days | "Priya is an engineering manager." |
| **Consolidated** | A summary derived from multiple memories | 90 days | "The team had three planning sessions about Atlas this quarter." |

The half-life is roughly *how long it takes a memory's
salience to drop to half its initial value if nothing touches
it*. Chapter 07 covers the math; the takeaway here is that
the kind directly determines how fast a memory fades.

---

## Episodic: specific moments

An Episodic memory captures a particular event at a particular
time. The agent observed something, said something, did
something — and that *occurrence* is what's being stored.

Examples:

- "User asked about login flow."
- "Deploy of `auth-service` failed at 14:31."
- "Sales call with Acme — they want quarterly billing."
- "Met Priya at the offsite, talked about Atlas."

Episodic is the **default kind** for most things an agent
encodes during a session. It's what `encode(text)` produces
unless you explicitly pick a different kind. The bias is
intentional: most things an agent stores in flight are
moment-shaped, and the agent shouldn't have to think hard
about classification.

### Why a 30-day half-life

Episodic memories are about *the past*. Their value usually
declines: yesterday's events are highly relevant today, but
last quarter's lunch chat probably isn't. A 30-day half-life
means an unrecalled episodic memory's salience drops to ~0.35
after a month, ~0.18 after two, and so on.

Recalling it boosts the salience back up — the agent's
ongoing attention keeps a memory alive. The decay is what
happens when nothing's paying attention.

---

## Semantic: persistent knowledge

A Semantic memory captures a generalised fact or belief — not
about a specific moment, but about how the world is (or how
the agent believes it is).

Examples:

- "Priya is an engineering manager."
- "Atlas is the codename for the authentication migration."
- "The user prefers JSON output over YAML."
- "Quarterly billing is offered to customers above $50K ARR."

A semantic memory should outlive most episodic ones. The half-
life is 365 days — a year of inactivity halves its salience.
That's roughly the right scale for "general knowledge about
the agent's domain."

### When to mark something Semantic

The agent (or the application code calling `encode`) decides.
The substrate provides an `encode(text, kind = Semantic)`
shape:

```
session.encode(
    text = "Priya is the engineering manager for the Atlas project.",
    kind = Semantic,
)
```

A few heuristics:

- **If you'd phrase it as a present-tense, declarative
  statement of fact** about a person, place, system, or
  concept — it's probably Semantic.
- **If it refers to a specific moment** ("Priya said she's
  the engineering manager *in the offsite*") — it's
  Episodic.
- **If it's an instruction or preference that should persist
  across sessions** — Semantic. Per-session ephemera goes
  Episodic.

Many agent frameworks call out "long-term vs short-term
memory." Brain's Semantic vs Episodic is a sharper version of
that distinction, with the decay math reflecting the
intuition.

---

## Consolidated: Brain-generated summaries

A Consolidated memory is one that Brain itself produced by
summarising multiple existing memories. It's the output of
the **consolidation worker** that runs in the background.

Example:

```
# Episodic memories accumulated over a quarter:
mem_018a…  "Atlas standup, week 1: discussing token rotation."
mem_018b…  "Atlas standup, week 2: 401s rising during deploy."
mem_018c…  "Atlas standup, week 3: rolled back the rotation change."
mem_018d…  "Atlas standup, week 4: shipped a smaller rollout."
…

# Consolidation worker clusters these and produces:
mem_018X…  Kind: Consolidated.
           Text: "The Atlas team had multiple discussions about
                  token rotation during Q3, with several rollbacks
                  before a smaller incremental rollout shipped."
           DerivedFrom: [mem_018a, mem_018b, mem_018c, mem_018d, …]
```

The consolidated memory:

- Is a memory just like any other — it has text, an embedding,
  metadata, and a `MemoryId`. Recall finds it like any other.
- Carries `DerivedFrom` edges to every source memory it
  consolidated.
- Has its own decay half-life (90 days — slower than episodic,
  faster than semantic, reflecting that summaries should hold
  up longer than individual moments but aren't permanent
  facts).

> **What's the "consolidation worker"?**
>
> One of twelve background workers Brain runs per shard
> (chapter 22 of the architecture tier for the catalogue;
> chapter 07 here for what consolidation does). It
> periodically:
>
> 1. Scans recently-created episodic memories.
> 2. Clusters them by vector similarity.
> 3. For each cluster above some size threshold, asks an LLM
>    to summarise the cluster (if a summarizer is
>    configured) or concatenates them deterministically (if
>    not).
> 4. Stores the result as a Consolidated memory.

This is loosely modelled on the cognitive-science theory
that human memory consolidates during sleep — repeated
related experiences become more abstract, more general,
detached from the specific moments. The Brain analogue isn't
biologically accurate; it just gives the substrate a way to
*get more general* over time.

### Why have a separate kind for consolidations

Two reasons:

1. **Different decay.** Consolidations are summaries, not
   facts. They shouldn't fade as fast as the moments they
   summarise (you'd lose the abstraction), but they shouldn't
   be permanent (the summary becomes stale as new memories
   arrive). 90 days is a middle ground.
2. **Filtering in queries.** Sometimes you want only "real"
   memories the agent observed, not Brain's own summaries.
   The kind lets you filter.

---

## Why three kinds and not more

Other plausible kinds got rejected:

- **Goal** ("I want X to happen"). Can be expressed as a
  Preference on the knowledge-layer side, or as a Semantic
  memory on the substrate side. Doesn't need its own kind.
- **Observation** ("I saw X"). That's just Episodic.
- **Hypothesis** ("X might be true"). That's a low-confidence
  knowledge-layer statement, not a memory kind.
- **Rule** ("If X then Y"). Rules aren't memories; they're
  policies or programs. Brain doesn't store them.

Three kinds is the right count for the substrate because:

- They map to a real cognitive distinction users understand.
- Each one carries a useful, different decay rate.
- More kinds would multiply API surface (more filter values,
  more `encode` shapes) without adding much.

If you have a domain that genuinely needs another kind, the
knowledge layer's Statement kinds (Fact / Preference / Event,
chapter 12) probably already cover it — at a layer better
suited for typed claims.

---

## How the kind affects what happens

A memory's kind influences three things:

1. **Decay rate.** Different half-lives, as in the table
   above. Two memories created on the same day with the same
   initial salience will have different salience a month
   later if their kinds differ.
2. **Recall ranking.** All else equal, the ranker prefers
   higher-salience memories. A 6-month-old Semantic memory
   (salience ~0.45) beats a 6-month-old Episodic memory
   (salience ~0.07) on the same topic.
3. **Filtering in queries.** A query can ask for only
   memories of a specific kind: "give me all Semantic
   memories about Priya," which uses the kind as a
   pre-filter at the index layer.

The kind does *not* affect:

- How the memory is stored (same arena slot shape, same
  metadata row).
- How it's embedded (same model, same vector dimension).
- Whether it's queryable (all kinds are queryable, modulo
  filters).
- Forgetting semantics (same lifecycle for all three).

---

## Picking the kind, in practice

A simple pattern for agent code:

```
def encode_observation(text):
    # An observation of something that just happened.
    return brain.encode(text, kind = Episodic)

def encode_fact(text):
    # A general claim worth remembering long-term.
    return brain.encode(text, kind = Semantic)

# The agent never directly creates Consolidated memories —
# Brain's worker does that automatically.
```

If you don't pass `kind`, Brain defaults to Episodic. That
default is right far more often than not.

You can also use the knowledge layer instead of (or alongside)
the kind distinction for storing typed claims with
provenance. A Semantic memory says "I, as the substrate, have
text claiming Priya is a manager"; a Statement of kind Fact
says "Brain has resolved entity Priya, asserts she has the
predicate `role` with object `engineering manager`, supported
by these memories." The two coexist (chapter 11).

---

## Recap

- A memory has a `kind`: Episodic, Semantic, or Consolidated.
- **Episodic** = specific moments. Default. 30-day half-life.
- **Semantic** = persistent knowledge. Caller marks it.
  365-day half-life.
- **Consolidated** = Brain's own summaries of related
  episodic memories. Created by the consolidation worker.
  90-day half-life.
- The kind affects decay, ranking, and filtering — not
  storage or embedding.
- Three kinds is sharp enough to be useful without becoming a
  taxonomy.

---

## Where to go next

- **The decay math:** [chapter 07](07-salience-decay-consolidation.md)
  — half-lives, the access boost, the consolidation worker
  in detail.
- **What's actually in a memory:** [chapter 05](05-memories.md).
- **The knowledge-layer counterpart for typed claims:**
  [chapter 11](11-statements.md) (statements) and
  [chapter 12](12-fact-preference-event.md) (Fact /
  Preference / Event kinds).
