# 26 — Contexts: the agent's coarse partition

A **context** is the substrate's coarse partition of an agent's
memories. It's a `u64` ID (and an optional human name) attached
to every encode and consulted on every recall. This chapter
explains what a context is, why it exists, what values you can
pass, how it gets created, and when to use it vs not.

Contexts are agent-scoped: two agents can both have
`ContextId(1)` and they refer to completely unrelated buckets.
Every memory belongs to exactly one context. The shell's
`encode --context N` and `recall --filter-context N` are the
hot-path knobs that pin a request to a specific partition.

---

## Why contexts exist

Without contexts, every `recall` would query across all the
agent's memories. That has three failure modes:

1. **Dilutes ranking.** High-salience memories from one project
   crowd out lower-salience but more relevant memories from
   another. A standup note from the auth project outranks a
   relevant prefs memory from a UX project just because the
   standup got more re-encodes recently.

2. **Wastes work.** The substrate processes irrelevant
   candidates — embedder cache misses, edge walks into
   irrelevant graphs, more HNSW candidates filtered post-hoc.
   Tail latency suffers.

3. **Conflates audiences.** Debug-mode memories surface in
   production-mode queries. Personal memories surface in
   work queries. The agent ends up reasoning over the wrong
   substrate.

Contexts are the structural fix: state "this memory belongs to
bucket X" at encode time, then "give me bucket X's memories"
at recall time. The substrate narrows the search space at the
storage layer (not in a post-filter), and ranking sharpens
because cross-context noise is gone.

```
Without contexts                 With contexts
─────────────────                ────────────────────
        memories                  memories  ── ctx 0 (default)
       /  ALL  \                            └── ctx 7 (project-atlas)
       │       │                            └── ctx 12 (personal)
       │       │
       ▼       ▼                  recall --filter-context 7
   recall "X" → ranks all     →   reads only ctx 7's memories
   candidates against X           (smaller candidate set, sharper rank)
```

This isn't unique to Brain — Postgres has schemas, S3 has
buckets, Redis has logical databases. Contexts are the same
shape: a namespace within the principal's namespace.

---

## What values are allowed

The wire field is `context_id: u64` ([spec §02/03](../../spec/02_data_model/03_identifiers.md)).
The shell accepts a `u64` after `--context`. Three categories:

| Value | Meaning |
|---|---|
| `0` | The **default context** — always exists, can't be deleted, can't be renamed. Used when `--context` is omitted. |
| `1`–`2^64-1` | An agent-scoped context. Allocated lazily on first reference. See "Lazy creation" below. |

The shell can address only positive integer IDs today; the
wire supports human-readable names per spec §02/04 (allocated
to IDs on first use), but the shell doesn't yet expose
name-based lookup. To pre-create a named context with a
specific ID, use the SDK or the admin API. The brain-cli
admin surface may grow `brain-cli context create <name>`
later (tracked in spec §02/04 §3).

Per [`spec/02_data_model/04_context.md`](../../spec/02_data_model/04_context.md)
§7 there's no hard cap below `2^64`; practical agents have
single-digit to a few dozen contexts. If you find yourself
generating contexts programmatically, you probably want a
different abstraction (see anti-patterns below).

---

## The default context (`0`)

Every agent automatically has the default context. Bare
`brain encode "..."` (no `--context` flag) lands the memory
there.

Properties:

- **Always present.** Created at agent first-touch; survives
  for the agent's lifetime.
- **Cannot be deleted.** Even in v2 when context deletion
  arrives, `0` is immutable.
- **Cannot be renamed.** Its name is `"default"`, period.
- **Operations that don't specify a context use it.**

For agents that don't need partitioning — small agents,
prototypes, demos, single-purpose helpers — the default
context is fine. There's no overhead to using it.

---

## Lazy creation

Contexts are created on first reference. The flow:

1. You call `brain encode "..." --context 7`.
2. The substrate looks up `(agent_id, context_id=7)` in the
   per-agent context table.
3. If absent, the substrate allocates a new entry with
   `context_id = 7`, `name = ""`, `created_at = now`. Persisted
   in redb.
4. The operation proceeds.

There's no "create context" step you need to run first. Just
encode into the ID you want; the substrate handles the rest.
This is the same model AWS Cloud uses for tags (lazy creation
on first reference), and is one less concept for the agent
developer to think about.

The trade-off: you can't list contexts before any memory
exists in them, and you can't reserve an ID by allocating it
unused. If those matter, use the admin API to pre-create.

---

## Best practices

### Use `0` (default) when

- The agent has one purpose and one stream of memories.
- The agent is short-lived (one conversation, one task).
- You're prototyping and don't yet know what partitions make
  sense.
- You're a script / ad-hoc tool that just wants to remember
  things.

The default context is not a second-class citizen. It's the
right answer for the majority of agents.

### Use explicit contexts when

- The agent's memories **naturally separate** by domain. A
  developer agent that helps with both "the auth-rewrite
  project" and "the billing project" benefits from
  `--context 7` (auth) vs `--context 12` (billing) so that
  recalling "performance issues" returns the relevant project's
  memories, not the other one's.
- You want **isolation across audiences**. Memories the agent
  produces under `--context 1` (operator notes) shouldn't
  surface in `--context 2` (customer-facing) recalls.
- The agent has **distinct time periods or modes** that should
  be partitioned. Daily vs weekly vs ad-hoc. Production vs
  staging vs debug.

The pattern is: one context per **domain**. Cross-domain
recall is opt-in (pass a set of context IDs); cross-domain
ranking is opt-out (default is single-context).

### How many contexts is reasonable

| Count | Status |
|---|---|
| 1 (just default) | Fine for most agents. |
| 2–10 | Typical "real" agent with a handful of distinct projects / domains / audiences. |
| 10–50 | Power user; verify each context has memories actively flowing in. |
| 50+ | **Likely the wrong abstraction.** See anti-patterns. |

---

## Anti-patterns

### Contexts as tags

```
brain encode "..." --context 123   # "this is about Alex"
brain encode "..." --context 456   # "this is about Project Atlas"
brain encode "..." --context 789   # "this is from the standup"
```

Don't. Contexts are coarse partitions, not fine-grained labels.
For labels, the knowledge layer's entity + statement system is
the right tool: declare a schema, let extractors pull "Alex" /
"Atlas" / "standup" into entities, query via `recall
--filter-subject-entity alex`. See
[`14-extractors.md`](14-extractors.md) +
[`10-entities.md`](10-entities.md).

If you need ad-hoc tags **without** the knowledge layer, the
wire `tags: Vec<String>` field on memory metadata is for that
(SDK exposes it as `.tag("foo")`; the shell doesn't yet
surface tag-filtering, tracked for a later commit). Tags are
free-form, can stack many per memory, and don't pollute
your context space.

### Contexts as access control

```
brain encode "..." --context 99   # "only show to admin agents"
```

Don't. Contexts don't gate access — every operation under one
agent sees every one of that agent's contexts. Access control
is the **agent** boundary, not the context boundary. If you
need to keep memories from one audience separate from another,
use **separate agents** with separate AUTH tokens (per
[`configuration.md`](../reference/shell/configuration.md)).

### Contexts as time partitions

```
brain encode "..." --context 2026   # year
brain encode "..." --context 202605  # month
```

Don't. Time is filterable directly via `recall --max-age`
([`commands/recall.md`](../reference/shell/commands/recall.md))
and via `--event-time` ranges in the knowledge layer's
QUERY. Using contexts as a time axis forces you to maintain
the partition + query both axes manually, and lose Brain's
built-in recency decay.

### Hundreds of contexts per agent

```
for memory in stream: encode(memory, context=hash(memory.topic) % 1000)
```

Don't. The substrate maintains per-context stats and the
admin surface lists every context; many small contexts make
both noisier without buying you sharper retrieval. If you're
auto-generating context IDs, you want either:

- **Entities** (knowledge layer) — the LLM extractor already
  groups memories by the people / projects / topics they
  mention.
- **A separate agent** — if the partitioning is total (no
  cross-context recall ever needed), separate agents give you
  true isolation including the wire surface.

---

## How `--context` interacts with the rest of the system

### Dedup

Dedup is keyed on `(agent_id, context_id, content_hash)`. The
same text encoded into different contexts produces different
memories — the substrate doesn't dedup across contexts (it
would be cross-domain noise). See
[`commands/encode.md`](../reference/shell/commands/encode.md)
on `--allow-duplicate`.

```
brain encode "the build broke" --context 7    # ctx-7 memory m1
brain encode "the build broke" --context 12   # ctx-12 memory m2 (different!)
brain encode "the build broke" --context 7    # dedup hit, returns m1
```

### Recall

`recall --filter-context N` reads only context N. Multiple
context filters stack as OR within the context dimension:
`--filter-context 7 --filter-context 12` reads either. Up to
16 in a single query (wire-side cap). See
[`commands/recall.md`](../reference/shell/commands/recall.md).

### Subscribe

`subscribe --context N` streams events from a context. Same
multiplicity rules (repeatable, up to 16). See
[`commands/subscribe.md`](../reference/shell/commands/subscribe.md).

### Sticky context (REPL)

`\set context N` in the REPL sets a session-level default —
subsequent `encode` / `recall` / `subscribe` calls inherit
that context unless they override with their own
`--context`. The prompt updates to `brain[ctx=N]>` so you can
see the binding. See
[`meta/set.md`](../reference/shell/meta/set.md).

### Salience

Salience is **per-memory**, not per-context. A high-salience
memory in context 7 doesn't get a salience bump when
queried from context 12 — it just doesn't match the filter
at all. Cross-context relevance scoring would require a join
we don't do in v1. ([Spec §02/04 §10](../../spec/02_data_model/04_context.md#10-context-aware-salience).)

### Edges

Edges may cross context boundaries. A memory in context "work"
may have a `DERIVED_FROM` edge into context "personal". The
edge is visible during graph traversals, but if your recall
filters to one context, only the same-context side of the
edge is auto-loaded. ([Spec §02/04 §11](../../spec/02_data_model/04_context.md#11-edges-across-contexts).)

---

## Operations on contexts

| Operation | When | Surface |
|---|---|---|
| Create | Automatic on first reference (encode / recall / subscribe). | implicit |
| Pre-create with a name + description | When you want to populate metadata before any memory exists | `ADMIN_CREATE_CONTEXT` (SDK / admin API) |
| List | Stats, debugging | `brain-cli context list` (planned) / SDK |
| Rename | Operator hygiene | `ADMIN_RENAME_CONTEXT` (admin API; v1) |
| Move memory between contexts | Mis-encoded; refactor | `ADMIN_MOVE_MEMORY` (admin API; v1) |
| Delete | — | **Deferred to v2** (see below) |

Context deletion is **not in v1**. Memories in a context can't
be orphaned, and forgetting all members + then removing the
context is heavy and rarely needed. For now: leave unused
contexts in place. Storage cost is bounded by their memory
count; query cost is zero (nothing filters to them).

---

## Quick reference

```bash
# Bare encode lands in context 0 (default).
brain encode "casual note"

# Explicit context — created lazily if it doesn't exist.
brain encode "atlas project standup" --context 7

# Recall filters to a single context.
brain recall "performance" --filter-context 7

# Cross-context recall — up to 16 IDs.
brain recall "anything tagged urgent" \
  --filter-context 7 \
  --filter-context 12

# REPL sticky context — all subsequent ops default to ctx=7.
brain> \set context 7
brain[ctx=7]> encode "..."
brain[ctx=7]> recall "..."
brain[ctx=7]> \unset context
brain>

# Dedup is per-context. Same text in different contexts → two memories.
brain encode "the build broke" --context 7    # m1
brain encode "the build broke" --context 12   # m2 (different)
brain encode "the build broke" --context 7    # dedup hit → m1
```

---

## See also

- [`spec/02_data_model/04_context.md`](../../spec/02_data_model/04_context.md)
  — the authoritative spec (creation, ID allocation, capacity,
  edges across contexts, persistence layout).
- [`05-memories.md`](05-memories.md) — what a memory contains,
  including its `context_id` slot.
- [`10-entities.md`](10-entities.md),
  [`14-extractors.md`](14-extractors.md) — when you reach for
  the knowledge layer instead of more contexts.
- [`commands/encode.md`](../reference/shell/commands/encode.md),
  [`commands/recall.md`](../reference/shell/commands/recall.md),
  [`commands/subscribe.md`](../reference/shell/commands/subscribe.md),
  [`meta/set.md`](../reference/shell/meta/set.md)
  — flag-level reference.
