# 13 — Relations

A **relation** is a typed edge between two entities. Where a
statement says "subject has-this-property-or-claim,"
a relation says "this entity connects to that entity by this
edge type."

```
ent_priya  ──reports_to──▶  ent_devi
ent_priya  ──works_on────▶  ent_atlas
ent_atlas  ──depends_on──▶  ent_auth_service
```

Relations are how the knowledge layer models *structure*:
org charts, project memberships, system dependencies, who
mentions whom in what context. Once you have them, queries
like "who reports to Priya?" and "who reports to anyone who
reports to Priya?" become single-graph traversals instead of
prompt-engineering exercises.

---

## What a relation looks like

A relation record:

```
Relation
├── relation_id    rel_018f3c…
├── relation_type  rel_type_reports_to   (interned)
├── from           ent_priya             (subject of the edge)
├── to             ent_devi              (target of the edge)
├── confidence     0.91
├── evidence       [ mem_018a…, mem_019b… ]
├── created_at     2024-09-12T14:31:00Z
├── version        1
├── superseded_by  None
└── flags          (active, …)
```

The shape is similar to a statement (chapter 11). What
differs:

- **Both `from` and `to` must be entities.** A relation can't
  point at a text literal or a typed value — both endpoints
  are required to be `EntityId`s. That's what makes it a
  graph edge.
- **There's a `relation_type`.** Schema-declared, interned to
  a `u32`. The type acts like a predicate but is structurally
  reserved for edge shapes.
- **Direction is significant.** `Priya reports_to Devi` is
  not the same as `Devi reports_to Priya`. Brain stores
  both directions in indexes so traversal is fast either
  way, but the relation itself has a single direction.

---

## Relation types vs predicates

Both relation types and predicates are interned, schema-
declared, and identify a "kind of connection." So what's
the difference?

| | Predicate (on a Statement) | Relation type (on a Relation) |
|---|---|---|
| Subject | `EntityId` | `EntityId` |
| Object | entity / literal / typed value | **must be `EntityId`** |
| Purpose | typed claims with arbitrary object | typed edges between entities |
| Query path | by-subject / by-predicate / by-object-entity indexes | by-from / by-to / traversal |

Two ways to think about it:

- **A statement is a fact about an entity.**
  `Priya prefers async meetings` — the object is a textual
  preference, not an entity.
- **A relation is a connection between entities.**
  `Priya manages Devi` — both endpoints are people. The
  edge type carries the meaning.

When both endpoints are entities, you *could* model the
connection as either: a statement with `predicate = manages`
and `object = ent_devi`, or a relation with
`relation_type = manages` and `to = ent_devi`. The schema
picks. For typical org-chart-shaped edges, the relation
modelling wins:

- Graph traversal queries (multi-hop "everyone Priya
  manages transitively") work over relations, not over
  statements.
- The direction indexes make "who reports to Priya?" a
  single range scan; over statements you'd need a different
  query path.
- Relations don't have a `kind` (no Fact/Pref/Event
  distinction); they're simpler.

If you find yourself adding `predicate = manages` with
`object` always being an entity, that's a sign the modelling
should be a relation type instead. The schema linter can flag
this.

---

## Relation types are declared in the schema

```
relation_type reports_to {
    domain = Person
    range  = Person
}

relation_type works_on {
    domain = Person
    range  = Project
}

relation_type mentioned_with {
    domain = Person
    range  = Person
    symmetric = true       # A mentioned_with B ⇔ B mentioned_with A
}
```

- **`domain`** is the type the `from` endpoint must be.
- **`range`** is the type the `to` endpoint must be.
- **`symmetric`** marks edges where direction is meaningless
  (default is asymmetric).

Type checking happens at relation creation: an extractor
trying to create a `reports_to` from a Project to a Person
gets rejected and audited.

---

## Provenance, again

Like statements, relations carry an evidence list — the
memories that the extractor cited when producing this edge:

```
Relation {
    …
    evidence = [ mem_018a…, mem_019b…, mem_01a3ef… ]
}
```

Three memories all suggested "Priya manages Devi"; the
relation's confidence rises with each piece of supporting
evidence; the evidence list grows.

When a memory is forgotten, the `forget_cascade` worker
visits every relation that cited it and revises:

- Remove the memory from the evidence list.
- Recompute confidence.
- If evidence becomes empty, the relation is superseded
  with `superseded_by = null` (effectively retracted).

This is the same cascade story as statements; it's the
load-bearing piece that makes "forget" mean something even
when claims have been derived from the memory.

---

## Traversal

The whole point of having relations as a separate concept is
to make graph queries cheap. Brain supports bounded-depth
traversal anchored on an entity:

```
session.graph_traverse(
    anchor          = ent_priya,
    relation_types  = [ rel_type_reports_to ],
    direction       = Incoming,         # who reports up to Priya?
    max_depth       = 3,
    max_branching   = 50,
    limit           = 100,
)
```

The result is a set of entities reachable from `anchor`
through edges of the named types, within `max_depth` hops,
respecting `max_branching` per hop.

> **Why bounded?**
>
> Unbounded graph traversal can explode — a node with
> thousands of edges, traversed 5 hops deep, generates
> astronomical numbers of paths. The substrate caps depth
> (default 4 hard) and branching per hop (default 200
> hard) so the query has a worst-case bound. If your real
> question needs an unbounded traversal, that's a sign for
> a different tool (a graph database; see chapter 04).

Direction options:

- `Outgoing` — follow edges away from the anchor.
  *"Who does Priya manage?"*
- `Incoming` — follow edges towards the anchor.
  *"Who manages Priya?"*
- `Both` — follow in either direction. Useful for symmetric
  relations, or for collecting an entity's full
  neighbourhood.

The traversal scores entities by **proximity** — closer
hops rank higher, with edge confidence as a tiebreaker. So
"direct report" outranks "report of a report."

---

## Two example uses

### An org chart

Schema:

```
entity_type Person { … }
relation_type reports_to {
    domain = Person
    range  = Person
}
```

Memories accumulate over months:

> "Devi said in standup that Priya joined her team."
> "Anika mentioned she had her one-on-one with Priya."
> "Priya asked Devi for sign-off on the migration."

Extractors produce:

```
Relation { from=ent_priya, to=ent_devi, type=reports_to,
           evidence=[mem_a, mem_c], confidence=0.93 }
Relation { from=ent_anika, to=ent_priya, type=reports_to,
           evidence=[mem_b], confidence=0.78 }
```

Now Brain can answer:

- *"Who reports to Priya?"*
  `traverse(anchor=ent_priya, relation_types=[reports_to], direction=Incoming, max_depth=1)`
  → `[ent_anika]`
- *"What's Priya's reporting tree?"*
  Same call with `max_depth=3` → everyone transitively
  reporting up to her.

### Project membership and dependencies

```
entity_type Project { … }
relation_type works_on {
    domain = Person
    range  = Project
}
relation_type depends_on {
    domain = Project
    range  = Project
}
```

Now you can ask "which projects does Priya touch?" and "what
does Atlas depend on?" with the same primitive.

---

## What relations *aren't*

A few clarifications:

- **Not unstructured edges.** Every relation has a declared
  type. You can't add a "miscellaneous" edge with arbitrary
  text labels.
- **Not bidirectional storage decisions made at query time.**
  Brain stores both `from → to` and `to → from` indexes when
  the relation is created. Asking the reverse direction is
  not "ask the question backwards"; it's a different index
  lookup of equal cost.
- **Not transactional consistency across many relations.**
  Each relation create/update is one row's worth of write.
  You can group several into a Brain transaction if you
  need atomicity (reference).
- **Not weighted edges in the graph-algorithms sense.**
  Brain doesn't run shortest-path-by-weighted-edge or
  PageRank. The `confidence` field exists, but it's a
  ranking aid, not an edge weight for graph algorithms.

---

## Relations and the hybrid retriever

Chapter 17 covers hybrid retrieval in detail; here's the
preview as it relates to relations:

When you run a `recall` or `query` and Brain detects an
entity anchor (e.g., "Priya"), the **graph retriever** is
one of three retrievers that fans out:

```
recall("anyone who reports to Priya?")
    │
    ├─ semantic: vectors similar to the question
    │
    ├─ lexical:  text search over memory_text.tantivy
    │
    └─ graph:    starting from ent_priya, walk reports_to
                 edges in the Incoming direction
                 ↓
                 returns ranked entities
```

The three retriever outputs get merged into one ranked
result (with RRF, chapter 21). For the question above, the
graph retriever does most of the work — semantic and
lexical contribute weak signal. For other questions
(paraphrase-heavy, no clear entity), graph contributes
little and semantic / lexical dominate. The fusion lets
all three pull their weight.

---

## A note on graph algorithms vs Brain's traversal

Brain's traversal is the right tool for "what does the
agent know that's *near* this entity" — a small, bounded,
type-filtered walk. It is **not** the right tool for:

- Shortest path between arbitrary nodes.
- PageRank, betweenness centrality, clustering coefficients.
- Community detection on arbitrary subgraphs.
- Anything that needs the whole graph in working memory.

If you need those, you have a graph workload, and a real
graph database (Neo4j, JanusGraph, …) is appropriate. The
two systems coexist fine: Brain is the substrate for the
agent's knowledge, the graph DB is for the structural
algorithms. Chapter 04 covered the comparison.

---

## Recap

- A relation is a typed edge between two entities, with a
  schema-declared type, direction, and confidence.
- Relations have an evidence list; forgetting a source
  memory triggers a cascade that revises (or supersedes)
  affected relations.
- Brain stores both directions of every relation in
  indexes; traversal in either direction is cheap.
- The **graph retriever** uses relations to answer
  entity-anchored queries during hybrid recall.
- Relations are bounded — depth-capped, branching-capped.
  For arbitrary graph algorithms, use a graph database.

---

## Where to go next

- **What produces relations from text:** [chapter 14](14-extractors.md)
  — extractors.
- **How types are declared:** [chapter 15](15-schemas.md)
  — schemas.
- **The graph retriever in detail:** [chapter 17](17-hybrid-retrieval.md).
- **The architecture-tier deep dive:**
  [`../architecture/09-knowledge-layer.md`](../architecture/09-knowledge-layer.md).
