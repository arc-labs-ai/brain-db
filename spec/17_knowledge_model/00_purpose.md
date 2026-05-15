# The Three-Layer Knowledge Model

## Overview

the knowledge layer organizes data in three layers:

```
┌─────────────────────────────────────────────────────────────────┐
│  LAYER 3: STATEMENTS    Facts, Preferences, Events              │
│  Typed claims about entities, with provenance + confidence.     │
└─────────────────────────────────────────────────────────────────┘
                              │  derived from
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  LAYER 2: GRAPH         Entities + Relations                    │
│  Canonical nouns (entities) and typed edges between them.       │
└─────────────────────────────────────────────────────────────────┘
                              │  references / anchored to
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  LAYER 1: SUBSTRATE     Memories                            │
│  Raw episodic / semantic / consolidated memories, embedded.     │
└─────────────────────────────────────────────────────────────────┘
```

Each layer has clear ownership:

- **Memories** are *what was experienced or said*. Always authoritative. the substrate owns this.
- **Entities and Relations** are *who and what exists, and how they connect*. The graph.
- **Statements** are *what is claimed about entities*. Derived projections with provenance.

## Why three layers, not five things

The previous framing (Fact, Preference, Event, Entity, Relation as five "kinds") conflates two distinct ideas: *referents* (nouns) and *statements about them* (claims). The three-layer model separates them:

- **Referents** belong in Layer 2 (the graph). They are stable identities.
- **Claims** belong in Layer 3 (statements). They are revisable, versioned, time-bound.

This separation matters because:

1. **Entities are joined to**, not "retrieved." `entity:priya_42` is an anchor, not a claim. You don't search for it by similarity; you resolve to it.
2. **Statements can be true, false, superseded, or contradicted**. Entities can't — an entity either exists or doesn't.
3. **The mutation rules are different.** Entities are merged or renamed; statements are versioned or invalidated.

## The flow of data

A typical write-side flow:

```
User: ENCODE memory_text="Priya prefers async meetings for engineering syncs"
  │
  ▼
LAYER 1: Memory stored in the substrate substrate.
  - vector embedded
  - WAL recorded
  - HNSW indexed
  │
  ▼  background: extractors run
  │
LAYER 2 + 3 (parallel):
  - Entity extractor: "Priya" → resolve to entity_42 (existing)
                       or create new entity if no match
  - Predicate extractor: "prefers" → matches schema predicate
  - Object extractor: "async meetings" → string literal
                       (or could be resolved to entity if schema says so)
  │
  ▼
  - Statement written: Preference(subject=entity_42,
                                  predicate=prefers,
                                  object="async meetings",
                                  context="engineering syncs",
                                  confidence=0.87,
                                  evidence=[memory_id])
  - Indexed in statement BM25 index (for "prefers")
  - Indexed in graph (entity_42 has a new preference)
```

A typical read-side flow:

```
User: "What does Priya prefer?"
  │
  ▼
QUERY ROUTER:
  - Parse: "Priya" is recognized entity → graph lane
  - Parse: "prefer" is a known predicate → type filter (Preference)
  - Decide: Graph + Type-filter + Semantic (for paraphrase)
  │
  ▼
RETRIEVAL (parallel):
  - GraphRetriever: statements where subject=entity_42, kind=Preference
  - SemanticRetriever: statements semantically similar to "Priya preferences"
  │
  ▼
RRF FUSION + filters:
  - Combine ranks
  - Filter superseded statements (return current versions only)
  - Filter by confidence ≥ 0.5
  │
  ▼
RESULT:
  [
    Preference(predicate=prefers, object="async meetings", confidence=0.87,
               evidence=[memory_x, memory_y]),
    Preference(predicate=prefers, object="written agendas", confidence=0.72,
               evidence=[memory_z]),
    ...
  ]
```

Both flows preserve the layer separation. The substrate is never bypassed; statements are never authoritative on their own.

## Identity and references

Each layer has its own ID space:

| Layer | ID type | Format |
|---|---|---|
| Memory | `MemoryId`  | Packed u128: `(shard, slot, version)` |
| Entity | `EntityId` | UUIDv7, 128 bits |
| Statement | `StatementId` | UUIDv7, 128 bits |
| Relation | `RelationId` | UUIDv7, 128 bits |

Cross-layer references:
- A Statement's `subject` is an `EntityId`.
- A Statement's `object` is a `StatementObject` union: `EntityId | Value(JSON) | MemoryId | StatementId`.
- A Statement's `evidence` is a `Vec<MemoryId>` plus optional `Vec<StatementId>` for derived-from-statement cases.
- A Relation's `from`/`to` are `EntityId`s.
- A Relation's `evidence` is a `Vec<MemoryId>`.
- A Memory may *mention* zero or more Entities (recorded in `memory_entity_mentions` join table).

The cross-layer references are first-class: queries can join across layers freely.

## What lives where: a worked example

Scenario: Priya is the engineering manager. She prefers async meetings. Last Tuesday she scheduled a planning session. Bob reports to her.

| Layer 1: Memories | Layer 2: Graph | Layer 3: Statements |
|---|---|---|
| "Met with Priya, she's running engineering now" | Entity: Priya | Fact(Priya, role, "engineering manager") |
| "Priya doesn't like syncronous standups" | Entity: Bob | Preference(Priya, prefers, "async meetings") |
| "Planning session Tuesday at 2pm with the team" | Relation: Bob → Priya (reports_to, valid_from=...) | Event(Priya, scheduled, "planning session", event_at=Tuesday) |
| ... |  | Fact(Bob, reports_to, Priya) |

Queries answer different questions at different layers:

- "What did Priya say last Tuesday?" → Layer 1 (memory recall by timestamp + entity mention).
- "Who reports to Priya?" → Layer 2 (graph traversal on reports_to relation).
- "What does Priya prefer?" → Layer 3 (statement filter by subject + kind).
- "Why do we think Priya is the manager?" → Layer 3 → provenance → Layer 1 (the supporting memories).

## What this is *not*

We are not building a "semantic memory" in the Semantic Web sense. There is no OWL inferencing, no SPARQL, no reasoner. The statements are operational: they answer queries the user actually asks. If a user wants entailment ("Priya manages engineering" + "Bob is in engineering" ⇒ "Priya manages Bob"), they write a rule or an extractor that produces that statement explicitly. The substrate does not infer.

We are also not building a general-purpose knowledge graph. We are optimized for *cognitive substrate use cases*: an agent has a stream of experiences, structure is latent in them, and we want to surface that structure for typed query.
