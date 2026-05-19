# 10 — Entities

This chapter is the first of five about the knowledge layer.
We start with **entities** — the typed identity anchors that
memories get linked to once a schema is declared.

If memories are "what happened," entities are "the things
that happen to / involve / are." Priya, Acme Corp, the Atlas
project, the planning session on Tuesday — those are
entities. A memory might mention three of them; an entity
might be mentioned in hundreds of memories.

---

## What an entity is

An entity is a **typed**, **identified**, **persistent** thing
in your domain.

- **Typed.** Every entity has a type — Person, Project,
  Organization, whatever you declared in your schema. The
  type controls what attributes the entity carries and how
  it resolves.
- **Identified.** Every entity has an `EntityId` — a 16-byte
  handle Brain uses to refer to it. The ID is stable across
  the entity's lifetime, no matter how many memories mention
  it.
- **Persistent.** Once an entity exists, it stays. Memories
  that mentioned it can be forgotten; the entity remains.
  This is intentional (more below).

A representative entity record:

```
Entity
├── entity_id        ent_01HJ4Z8M2X…
├── entity_type      Person
├── canonical_name   "Priya Ramesh"
├── normalized_name  "priya ramesh"
├── aliases          [ "Priya", "P. Ramesh", "@priya" ]
├── attributes       { email: "priya@acme.com", role: "manager" }
├── mention_count    47
├── created_at       2024-09-01T10:14:00Z
├── updated_at       2024-12-12T08:32:11Z
└── merged_into      None
```

The `canonical_name` is what you'd display: capitalised, with
diacritics, as the entity should be referred to in the UI.
The `normalized_name` is what Brain uses internally for
matching: lowercased, NFKC-normalised, whitespace-collapsed.
That's the key Brain looks up when an extractor sees
"PRIYA RAMESH" or "priya  ramesh" — both normalise to the
same string.

> **What's NFKC normalisation?**
>
> Unicode characters can sometimes be encoded multiple ways
> (e.g., `é` can be a single code point or `e` + a combining
> acute accent). NFKC ("Normalization Form Compatibility
> Composition") rewrites text into a canonical form so two
> visually-identical strings compare equal.
>
> See [Wikipedia: Unicode equivalence](https://en.wikipedia.org/wiki/Unicode_equivalence).

---

## Entity vs memory: the key distinction

This is the conceptual move worth pausing on:

```
       memory                              entity
   ┌─────────────────────┐         ┌─────────────────────┐
   │ "Met Priya at the   │ ─────▶  │  ent_01…  Priya     │
   │  offsite, talked    │         │  (Person)           │
   │  about Atlas."      │ ─────▶  │  ent_02…  Atlas     │
   │                     │         │  (Project)          │
   │  mem_018f2b1e…      │         │                     │
   └─────────────────────┘         └─────────────────────┘
        what happened                  who/what was involved
        (substrate)                    (knowledge layer)
```

A memory captures an *occurrence* — a sentence that mentions
Priya and Atlas. The entities `Priya` and `Atlas` are the
*things* that occurrence references. Multiple memories can
mention the same Priya entity; the entity ties them together.

This separation is what makes structured queries possible.
Without entities, you'd be left with "memories whose text
contains the substring `Priya`" — fragile (`Pri Ya` doesn't
match), no aliases, no typing. With entities, you ask "what
do I know about *the entity* Priya?" and get back everything
linked to that EntityId.

---

## Entity types

Entity types are declared in the schema. A small example:

```
entity_type Person {
    attributes {
        email: String?
        role: String?
    }
}

entity_type Project {
    attributes {
        codename: String
        started_at: Timestamp?
    }
}

entity_type Organization {
    attributes {
        domain: String?
    }
}
```

Three things to note:

- **Types are declared, not inferred.** Brain doesn't decide
  for you that "Priya" is a Person — your extractor (chapter
  14) decides. The schema names the types your domain has;
  the extractor pattern matching `\b[A-Z][a-z]+\b` *plus*
  declaring `target = entity Person` produces a Person
  candidate.
- **Attributes have types.** Brain's schema DSL supports a
  small set: `String`, `Integer`, `Float`, `Boolean`,
  `Timestamp`, and `?` to mark a field optional. Chapter 15
  covers the DSL.
- **The type is interned.** Internally each entity type gets
  a small `u32` ID; entity rows carry that, not the string
  name. This makes index keys compact.

The set of types is per-deployment. One deployment might
have only `Person` and `Project`; another might have
hundreds of types for a knowledge-management workflow.

---

## Entity resolution: the deduplication problem

When an extractor sees the text `"Priya"`, Brain has to
decide: is this the existing `Priya` entity, or a new one?

Getting this right is *the* hard part of the knowledge layer.
Get it wrong by being too eager and you merge two people into
one entity; get it wrong the other way and you create
duplicate entities ("Priya," "Priya Ramesh," "P. Ramesh") for
the same person.

Brain's resolver runs a **four-tier match** for each mention
candidate, stopping at the first success:

### Tier 1: exact canonical name match

```
SELECT entity_id FROM entity_by_canonical_name
WHERE entity_type_id = ? AND normalized_name = "priya ramesh"
```

If the extractor produced `"Priya Ramesh"` and an entity with
normalised name `"priya ramesh"` already exists *for the
same type*, that's the match. Fastest, most certain.

### Tier 2: alias match

The alias table holds known alternate names:

```
SELECT entity_id FROM entity_aliases
WHERE entity_type_id = ? AND normalized_alias = "priya"
```

If `"Priya"` is registered as an alias of `"Priya Ramesh"`,
that match succeeds. Aliases are populated by extractors
(some explicitly emit aliases) and by operators (admin RPC to
add an alias).

### Tier 3: trigram fuzzy match

For typos and minor variants ("Priyaa", "Pria Ramesh"), Brain
maintains a trigram index. A trigram is a 3-character window:

> **What's a trigram?**
>
> A sequence of 3 consecutive characters. "priya" has
> trigrams `_pr`, `pri`, `riy`, `iya`, `ya_` (the
> underscores are sentinel boundary markers). Two strings
> share many trigrams ⇔ they're typographically similar.
> Trigram indexes are how Postgres's `pg_trgm` extension
> implements fuzzy `LIKE`.
>
> See [Wikipedia: N-gram](https://en.wikipedia.org/wiki/N-gram).

The trigram tier looks for entities whose trigram set has
>50% overlap with the candidate, picks the closest. Catches
"Priyaa" → Priya, "Pria" → Priya, etc.

### Tier 4: embedding similarity

If no exact, alias, or trigram match wins, Brain falls back
to vector similarity over the **entity HNSW**. Every entity
has its canonical name embedded; the candidate's name is
embedded; if the cosine similarity exceeds a threshold (default
0.85), it's a match.

This catches paraphrase: an extractor pulling out "the
engineering manager" can match the Priya entity *if* the
embeddings are close enough. The threshold is configurable
because false positives here are expensive.

### Tier 5 (optional): LLM disambiguation

If multiple matches above threshold come back from tier 4 —
"P. Ramesh" matches both Priya Ramesh and Pavan Ramesh
ambiguously — Brain can optionally call an LLM to
disambiguate among the top candidates. This is opt-in
because of the cost, and it's audited like any other
LLM call (chapter 14).

### If no tier matches

Brain creates a new entity. The extractor's output gets a
fresh `EntityId`. Future mentions of the same name will
resolve to this new entity.

---

## Why entities outlive their statements

A deliberate design choice: when every statement about Priya
is forgotten, the Priya entity stays.

Why?

1. **Entities are identity anchors.** They're how multiple
   memories cohere into "things Brain knows about." Killing
   the entity every time the last claim about it expires
   means churn — the entity gets recreated next time, with a
   new ID, breaking any references that pointed at the old
   ID.
2. **Other entities can reference an entity.** Through
   relations: `Priya reports_to Devi`. If Priya goes away
   when her last statement decays, the relation pointing at
   her goes dangling.
3. **The cost is small.** An entity row is ~150 bytes plus
   its embedding. Holding a few thousand "orphan" entities
   is trivial.

For deployments that *do* want to clean up orphan entities,
there's an opt-in **`entity_gc`** worker that prunes
entities with no recent references after a long grace period
(default 90 days). Off by default; operators turn it on
explicitly.

---

## Merging entities

Sometimes the resolver gets it wrong. The system creates
two separate entities for what's actually one person — maybe
the extractor was running before alias data was loaded, or
the trigram threshold was too high. The admin RPC fixes
this:

```
entity_merge(
    source = ent_old_priya,
    target = ent_priya,
)
```

What this does:

1. Marks `ent_old_priya` as merged into `ent_priya` (sets
   `merged_into`).
2. Rewrites all statements and relations that pointed at
   `ent_old_priya` to point at `ent_priya`.
3. Updates the alias table so future mentions of
   `ent_old_priya`'s canonical name resolve to `ent_priya`.
4. Writes a row to the `merge_log` audit table so the merge
   is permanent and reviewable.

The merged entity *stays in the entities table* with the
`merged_into` pointer — that's the "forwarding address" so
old references don't break. Forgetting it for real would
require an admin tombstone.

The merge is auditable but **not undoable** in v1. The audit
trail tells you what was merged when by whom; if you need to
unmerge, you'd manually recreate the source entity and
rewrite the affected statements / relations — non-trivial,
not a one-call operation.

---

## Tombstoning an entity

The admin RPC `entity_tombstone(entity_id)` marks the entity
as gone:

- New mentions of the same name won't resolve to it.
- Existing statements and relations that point at it remain
  (with the tombstoned marker on the entity).
- Queries skip the tombstoned entity unless explicitly
  asked to include it.

This is the "GDPR right to be forgotten" path for
knowledge-layer entities. It's separate from `forget` on a
memory: forgetting all memories *about* Priya doesn't
tombstone the Priya entity unless `entity_gc` is on; an
explicit tombstone is decisive.

---

## What's in an entity row, completely

For completeness, the full row shape:

| Field | Purpose |
|---|---|
| `entity_id` | 16-byte handle (UUIDv7-shaped). |
| `entity_type_id` | u32 interned reference to the type table. |
| `canonical_name` | Display name. |
| `normalized_name` | Lowercased + NFKC + whitespace-collapsed; the key for exact lookup. |
| `aliases` | Vec of alternate normalized names. |
| `attributes_blob` | rkyv-encoded blob holding the typed attributes the schema declared. |
| `mention_count` | How many memories have mentioned this entity. |
| `created_at` / `updated_at` | Lifecycle timestamps. |
| `merged_into` | If non-None, points at the entity this was merged into. |
| `embedding_version` | Which version of the entity-embedding model produced its embedding. |
| `flags` | Bitfield: tombstoned / pinned / etc. |

The embedding itself doesn't live in this row — it lives in
the `entity.hnsw` file (a separate HNSW index for entity
names, used by the resolver's tier 4).

---

## Recap

- An entity is a typed, identified, persistent thing in your
  domain — Person, Project, etc.
- Every entity has an `EntityId`, a canonical name, a
  normalised name, aliases, optional typed attributes, and a
  mention count.
- Entities are produced by **extractors** (chapter 14) that
  process memories and emit entity mention candidates.
- The **entity resolver** decides if a mention matches an
  existing entity in five tiers: exact, alias, trigram, vector
  similarity, optional LLM.
- Entities **outlive** their statements. An entity row is
  cheap; the optional `entity_gc` worker prunes orphans on a
  long grace period.
- Entities can be **merged** (forwarding pointer) or
  **tombstoned** (hidden) via admin RPCs.

---

## Where to go next

- **What kinds of claims attach to entities:**
  [chapter 11](11-statements.md) — statements.
- **The three statement kinds:**
  [chapter 12](12-fact-preference-event.md) — Fact /
  Preference / Event.
- **Typed edges between entities:**
  [chapter 13](13-relations.md) — relations.
- **What produces entities from text:**
  [chapter 14](14-extractors.md) — extractors.
- **How types are declared:**
  [chapter 15](15-schemas.md) — schemas.
- **What the entity HNSW is:**
  [chapter 04 in the architecture tier](../architecture/04-hnsw-index.md).
