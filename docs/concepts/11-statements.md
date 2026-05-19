# 11 — Statements

If entities are *things* (Priya, Atlas), a **statement** is a
*claim about those things* — Priya works on Atlas, Atlas
started in March, the deploy failed at 14:31. Statements are
how the knowledge layer turns "text Brain has seen" into
"things Brain knows."

This chapter covers the common structure of statements. The
*three kinds* — Fact, Preference, Event — each have
different mutation rules and get their own chapter
([chapter 12](12-fact-preference-event.md)).

---

## The shape of a statement

A statement is built from five parts:

```
Statement
├── subject      EntityId  ← the thing the claim is about
├── predicate    PredicateId ← what's being claimed (interned)
├── object       Entity | TextLiteral | … ← the value
├── evidence     [MemoryId, …] ← memories that support the claim
└── confidence   f32       ← how strongly the substrate trusts it
```

Plus a few metadata fields: a `StatementId` (the handle),
its kind (Fact / Preference / Event), timestamps, version
information for supersession chains, the extractor that
produced it.

A concrete example, in pseudocode:

```
Statement {
    statement_id    = stmt_07a4b8c…
    kind            = Preference
    subject         = ent_priya
    predicate       = pred_prefers
    object          = TextLiteral("async sprint planning")
    evidence        = [ mem_018f2b22… ]
    confidence      = 0.84
    extracted_at    = 2024-09-12T14:31:25Z
    extractor_id    = extr_preference_extraction
    version         = 1
    superseded_by   = None
}
```

In words: "Brain claims that the entity `Priya` prefers
`async sprint planning`, with 0.84 confidence, supported by
memory `mem_018f2b22…`, extracted by the preference-extraction
extractor on 2024-09-12."

That's the unit. Statements pile up as memories accumulate.
After a year of conversations, an agent might have thousands
of statements about a dozen entities.

---

## Subject and object

**Subject is always an entity.** A statement is about
*something*; that something must be a typed identity anchor.
Statements without subjects don't exist in Brain's model.

**Object can take a few shapes**, depending on the predicate
and the type system:

- **Another entity.** `Priya manages Devi` — both subject
  and object are Person entities.
- **A text literal.** `Priya prefers async sprint planning`
  — the object is a free-text string. Used when there's no
  good entity to point at.
- **A typed value.** `Atlas started_at 2024-03-15` — the
  object is a Timestamp.
- **A reference to another statement.** Used for things like
  `Statement-X contradicts Statement-Y`. Rare in practice.

The schema (chapter 15) declares what shapes are valid for
each predicate. A predicate `manages` declared as
`(Person, Person)` rejects a text-literal object.

---

## Predicates

A **predicate** is the verb of the statement — the part that
says what relation holds between subject and object.

Predicates are **interned**, meaning each one has a `u32` ID
that's looked up from the schema-declared predicate table.
The wire format and storage carry the ID, not the string. So
`prefers` is stored as a `u32` everywhere internally; only
the human-facing API surfaces the name.

Why intern them?

- **Compact storage.** A typical statement row carries the
  predicate ID, not a string. Saves bytes.
- **Index efficiency.** Range scans by predicate (chapter 17)
  use the ID as a composite-key prefix.
- **Fast equality.** Comparing two `u32`s is one instruction;
  comparing two strings is per-character.

Predicates declared in the schema have a *signature* — what
subject and object types are valid. The signature is checked
when a statement is created.

```
predicate prefers (Person, *)        # any object shape
predicate works_on (Person, Project) # entity-to-entity, both typed
predicate started_at (Project, Timestamp)
```

The schema can also declare *constraints*: confidence
thresholds, allowed kinds, validity periods. Chapter 15
covers the DSL.

---

## Evidence

Every statement carries an **evidence list**: which memories
the substrate cited when it made this claim.

```
evidence = [ mem_018f2b22…, mem_019a1d…, mem_01a3ef… ]
```

This is **provenance**. It's the load-bearing feature of the
knowledge layer for trustworthy AI: when an agent says
"Priya prefers async sprint planning," it can also say
*because* of these specific memories, which it (or you) can
inspect.

Three things to know about evidence lists:

1. **They grow over time.** When a new memory provides
   additional support for a claim that already exists, the
   extractor doesn't create a duplicate statement — it
   appends the new memory's ID to the existing statement's
   evidence list and (typically) bumps the confidence.
2. **They support forget-cascade.** When a memory is
   forgotten, a background worker (`forget_cascade`) finds
   every statement that cited it as evidence and revises:
   - If other memories still support the statement, just
     update its confidence (recompute).
   - If this was the *only* evidence, supersede the
     statement with `superseded_by = null` — effectively
     retracting it.
3. **They can overflow.** If a statement accumulates many
   evidence memories (say, hundreds), the inline list moves
   to a separate `evidence_overflow` table to keep the main
   statement row compact. The row keeps a small inline list
   plus an optional overflow pointer.

The forget-cascade story is *the* operational reason
provenance matters. Without it, forgetting a memory leaves
orphan claims that still appear in query results, with no
backing evidence.

---

## Confidence

The `confidence` field is a `f32` in `[0, 1]` representing
how strongly the substrate trusts the claim. Where does it
come from?

- **Pattern extractors** set a fixed confidence per pattern.
  A regex hit is binary; the extractor's declaration says
  "regex matches in my pattern produce confidence 0.7."
- **Classifier extractors** produce confidence from the
  model's output probability. A logistic regression saying
  the input is a `reports_to` relation with 0.93 probability
  yields a statement with confidence 0.93.
- **LLM extractors** typically have the LLM output a
  confidence field in its JSON, which the extractor uses
  directly (after schema validation).

What confidence is *not*:

- **Not the same as similarity.** Similarity (chapter 09) is
  about how close two embeddings are. Confidence is about
  how strongly the substrate believes the extracted claim.
  They're independent numbers.
- **Not calibrated probability.** A 0.8 confidence doesn't
  mean "80% likely to be true." It's an internal score the
  extractor produces; calibrate per extractor if you need
  one.
- **Not propagated automatically.** If the substrate sees
  three memories all suggesting the same claim, the
  resulting statement's confidence isn't `(c1 × c2 × c3)`
  or similar — it's whatever the *current evidence
  recompute* logic produces, which usually rises with
  more supporting memories but caps short of 1.0.

You use confidence as a **filter**:

```
session.query(
    entity_anchor   = ent_priya,
    confidence_min  = 0.7,
)
```

The query returns only statements at or above that bar.
Statements below threshold still *exist* in storage — they're
just hidden from this particular query.

---

## Statement IDs and versioning

A `StatementId` is 16 bytes — UUIDv7-shaped, same as other
shard-local IDs.

Statements also have:

- **A `version` field.** Increments when the statement is
  superseded (see below).
- **A `superseded_by` pointer.** `None` for current
  statements; set to the next-version `StatementId` for
  superseded ones.
- **A `chain_root` ID.** The original (version 1)
  `StatementId` of the chain. Lets Brain answer "show me
  the full history of this preference" with a single range
  scan.

For Facts and Events, supersession isn't used the same way
(chapter 12 covers the differences); the version stays at 1
in most cases.

---

## A worked example

A short timeline of statements being created and updated:

### Day 1

```
encode("Priya wants to move sprint planning to async.")
# → mem_018f2b22…
```

Pattern extractor sees "Priya" → resolves to existing entity.
LLM extractor processes the memory:

```
Statement {
    statement_id   = stmt_001
    kind           = Preference
    subject        = ent_priya
    predicate      = pred_prefers
    object         = TextLiteral("async sprint planning")
    evidence       = [ mem_018f2b22… ]
    confidence     = 0.81
    version        = 1
    superseded_by  = None
}
```

### Day 5

```
encode("Priya doubled down on the async approach in standup.")
# → mem_019a1d…
```

LLM extractor sees this is supporting evidence for the same
preference. Doesn't create a new statement — *augments* the
existing one:

```
Statement {
    statement_id   = stmt_001
    evidence       = [ mem_018f2b22…, mem_019a1d… ]  ← appended
    confidence     = 0.87                              ← raised
    # everything else unchanged
}
```

### Day 12

```
encode("Priya said async is fine but maybe a weekly sync would help.")
# → mem_01a3ef…
```

LLM extractor sees this as a *new* preference, supersedes
the old one:

```
Statement {
    statement_id   = stmt_002
    kind           = Preference
    subject        = ent_priya
    predicate      = pred_prefers
    object         = TextLiteral("async with a weekly sync")
    evidence       = [ mem_01a3ef… ]
    confidence     = 0.79
    version        = 2
    superseded_by  = None
    chain_root     = stmt_001
}

# And stmt_001 is updated:
Statement {
    statement_id   = stmt_001
    superseded_by  = stmt_002          ← now points at the new version
    # everything else unchanged
}
```

A query for "current preferences of Priya" returns only
`stmt_002` (the one with `superseded_by = None`). A query for
"all preferences of Priya, with history" returns both,
ordered by version.

### Day 13: forgetting the source memory

```
forget(mem_018f2b22…)   # the original memory
```

The `forget_cascade` worker looks at every statement that
cited this memory: `stmt_001`.

- `stmt_001`'s evidence list was `[mem_018f2b22…, mem_019a1d…]`.
- After removing the forgotten memory, evidence becomes
  `[mem_019a1d…]`.
- Confidence gets recomputed (slightly lower).
- The statement isn't deleted; the evidence list shrinks.

If `stmt_001`'s evidence list had been only the forgotten
memory, the cascade would have superseded it with
`superseded_by = null` — a "withdrawn" statement.

---

## Querying statements

Once statements exist, structured queries can target them
directly:

```
session.query(
    entity_anchor   = ent_priya,
    kind_filter     = [ Preference ],
    confidence_min  = 0.7,
    limit           = 10,
)
```

The query routes through the knowledge layer's retrievers
(chapter 17) — semantic over the statement HNSW, lexical over
the statement tantivy index, graph over the
statements-by-subject table — and returns the matching
statements.

Each result carries its evidence list. The agent can use the
statement (the structured claim) and, when accountability
matters, drill into the memories that produced it.

---

## What a statement *isn't*

- **Not a row in a SQL table.** No joins, no foreign keys.
  Statements reference entities and memories by their IDs;
  the substrate enforces the references through workers,
  not through database constraints.
- **Not a graph edge.** Statements *are* claims; relations
  (chapter 13) are typed edges between entities. A
  statement's subject and object may both be entities, but
  that's three columns in a row, not a graph edge.
- **Not deterministic in general.** Two LLM extractor calls
  on the same memory can produce slightly different
  statements (different objects, different confidences).
  Brain's LLM cache (chapter 14) is what tames this in
  practice.

---

## Recap

- A statement is a typed claim: subject (entity) + predicate
  (interned verb) + object (entity / literal / typed
  value).
- Every statement carries an **evidence list** — memories
  that support the claim. This is *provenance*, and it's
  load-bearing for forget-cascade.
- Every statement has a **confidence** in `[0, 1]` from its
  extractor. Use it as a query filter, not a calibrated
  probability.
- Statements have versions and supersession chains, used
  primarily by Preferences (chapter 12).
- Forgetting a source memory triggers a background cascade
  that revises (or supersedes) statements that cited it.

---

## Where to go next

- **The three statement kinds:** [chapter 12](12-fact-preference-event.md)
  — when each applies, the mutation rules.
- **Typed edges between entities:** [chapter 13](13-relations.md)
  — relations.
- **What populates statements:** [chapter 14](14-extractors.md)
  — pattern / classifier / LLM extractors.
- **The verbs that query statements:** [chapter 16](16-cognitive-operations.md)
  — `recall`, `query`.
- **What hybrid retrieval does:** [chapter 17](17-hybrid-retrieval.md).
