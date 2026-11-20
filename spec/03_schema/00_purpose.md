# 03. Schema

> **TL;DR.** A single declarative `.brain` document where users define entity types, predicates, relation types, and extractors. Brain parses it at upload time, validates syntax and semantics, versions it, and enforces declarations on writes. The DSL is readable, stable across versions, and parseable for tooling. Every shard ships with the seeded `brain:` system namespace from byte zero; user `SCHEMA_UPLOAD` calls **merge** new declarations into the active state. A schema declaration is not a mode toggle — extractors, retrievers, and the typed-graph wire path are always wired; declarations narrow what `STATEMENT_CREATE` / `RELATION_CREATE` accept and which extracted rows persist.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Operators defining a schema; implementers of the parser + validator; client implementers |
| Voice | Hybrid (rationale + normative MUST/SHOULD) |
| Depends on | [02. Data Model](../02_data_model/00_purpose.md) |
| Referenced by | [11. Extractors](../11_extractors/00_purpose.md), [13. Retrievers](../13_retrievers/00_purpose.md), [04. Wire Protocol](../04_wire_protocol/00_purpose.md) |

## What this spec defines

The `.brain` schema DSL: the language operators use to declare entity types, predicates (statement vocabulary), and relation types for a deployment. Covers the grammar, the AST, the validator, namespacing, the merge semantics of `SCHEMA_UPLOAD`, the destructive counterpart `SCHEMA_REPLACE`, schema versioning, and the system schema (Brain's built-in `brain:` namespace).

Schema is **always on**. Every shard boots with the seeded `brain:` system namespace. User `SCHEMA_UPLOAD` calls **merge** new declarations into the existing active state per namespace; the pipeline does not branch on "is a schema declared" because the answer is always yes.

### What user declarations control

User declarations narrow two things:

| Surface | Effect of a declared type / predicate |
|---|---|
| `STATEMENT_CREATE`, `RELATION_CREATE` (explicit wire ops) | Resolve a qname against the schema version active for the scope the caller writes into. With a version active, a qname that scope does not declare is rejected with `PredicateNotInSchema` (0x004B) or `RelationTypeNotInSchema` (0x004C). With no version active the qname is interned and the write is accepted. |
| `ENTITY_CREATE` (explicit wire op) | Resolves a numeric `entity_type_id` against the type registry, not a qname against a schema version, so there is no entity-type analogue of the two codes above. An id no registry row answers to is a bad argument: `InvalidArgument` (0x0040), carrying the id in the message. |
| Extractor outputs | The pipeline runs on every ENCODE; persistence to the typed-graph tables happens only when the extracted entity / statement / relation references a declared type. Undeclared types are dropped silently (extraction is best-effort). |

Read-side surfaces (`RECALL`, retrieval fan-out, lexical and semantic retrievers) run regardless of which user namespaces are present — every shard wires all three retrievers at spawn.

## Purpose

Users declare entity types, predicates, relation types, and extractors in a single declarative schema document. Brain enforces these declarations on writes and uses them to drive extraction.

The DSL is designed for:
- **Readability**: looks like documentation, not code.
- **Stability**: changes are versioned; existing data migrates.
- **Tooling**: parseable for editors, formatters, validators.

## Format

Schema is a single document, typically `schema.brain`, parsed by Brain at upload time:

```
# Schema for a CRM-like memory database
# Version 1, 2026-05-23

namespace acme

# ─── Entity types ─────────────────────────────────────────────

define entity_type Person {
    attributes {
        email:       text optional unique
        role:        text optional
        team:        text optional
        timezone:    text optional
    }
}

define entity_type Project {
    attributes {
        slug:        text required unique
        repo_url:    text optional
        active:      bool default true
    }
}

# ─── Predicates ───────────────────────────────────────────────

define predicate prefers {
    kind: Preference
    object: Value<text>
}

define predicate role {
    kind: Fact
    object: Value<text>
}

define predicate scheduled {
    kind: Event
    object: Value<text>
}

# ─── Relations ────────────────────────────────────────────────

define relation_type reports_to {
    from: Person
    to: Person
    cardinality: many-to-one
}

define relation_type owns {
    from: Person
    to: Project
    cardinality: many-to-many
    properties {
        since: date optional
    }
}

# ─── Extractors ───────────────────────────────────────────────

define extractor person_mentions {
    kind: pattern
    target: entity Person
    patterns [
        # Person names (English): two or three capitalized words
        /\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2})\b/
    ]
    confidence: 0.7
}

define extractor preferences {
    kind: llm
    target: statement Preference
    trigger: on encode where memory.kind = episodic
    prompt: """
        Extract any preferences stated about a person.
        Format: JSON array of {subject, predicate, object, confidence}.
        Only extract preferences with clear expressions of preference.
    """
    examples: [
        {
            input: "Priya likes async meetings"
            output: [{"subject": "Priya", "predicate": "prefers", "object": "async meetings", "confidence": 0.9}]
        }
    ]
    model: "claude-haiku-4-5"
    confidence_threshold: 0.7
    cache: enabled
}

define extractor reporting_lines {
    kind: classifier
    target: relation reports_to
    trigger: on encode where memory.text matches ".*report.*to.*"
    model: "brain-reporting-line-classifier"
    confidence_threshold: 0.8
}
```

## Grammar (simplified)

```
schema       := (namespace_decl | definition)*
namespace_decl := "namespace" identifier

definition   := entity_type_def
              | predicate_def
              | relation_type_def
              | extractor_def

entity_type_def := "define" "entity_type" identifier "{" attributes? "}"

attributes   := "attributes" "{" attribute_decl* "}"
attribute_decl := identifier ":" attr_type attr_modifier*
attr_type    := "text" | "number" | "bool" | "date" | "timestamp"
              | "enum" "[" identifier_list "]"
              | "ref<" identifier ">"
attr_modifier := "required" | "optional" | "unique" | "indexed"
              | "default" literal

predicate_def := "define" "predicate" identifier "{"
                    "kind:" statement_kind
                    "object:" object_type
                 "}"
statement_kind := "Fact" | "Preference" | "Event"
object_type  := "Value<" attr_type ">"
              | "Entity<" identifier ">"
              | "Memory"
              | "Statement"

relation_type_def := "define" "relation_type" identifier "{"
                       "from:" identifier
                       "to:" identifier
                       ("cardinality:" cardinality)?
                       ("symmetric:" bool)?
                       ("properties" "{" attribute_decl* "}")?
                     "}"
cardinality  := "one-to-one" | "one-to-many" | "many-to-one" | "many-to-many"

extractor_def := "define" "extractor" identifier "{"
                    "kind:" extractor_kind
                    "target:" target_decl
                    (extractor_specific_fields)*
                 "}"
extractor_kind := "pattern" | "classifier" | "llm"
```

The full grammar (with whitespace, comments, escapes) lives in [`01_grammar.md`](01_grammar.md).

## Validation rules

Brain validates a schema before accepting it:

### Syntactic
- Parses without error.
- All referenced types resolve (e.g., `from: Person` requires `Person` to be defined).
- Predicate object types are consistent with the predicate's kind.
- Extractor targets reference defined entity types, predicates, or relation types.

### Semantic
- No two entity types with the same name in the same namespace.
- No duplicate predicates.
- No circular `ref<>` chains in entity attributes (e.g., Person.boss: ref<Person>.boss... is OK; A.refs B and B.refs A in attributes is OK but should be relations, warning).
- Extractor model references resolve (e.g., `model: "claude-haiku-4-5"` must be a registered model).
- LLM extractor prompts under length cap (default 4096 chars).

### Merge classification (when a declaration already exists)

`SCHEMA_UPLOAD` is additive-merge, so the checks here are not a diff against
the prior version — the validator never reads it. They are a per-declaration
pre-flight against the persisted registry, run before any write transaction
opens:

- A declaration that doesn't exist yet: **insert**.
- A declaration that exists with matching constraints: **idempotent**. An
  upload where every item is idempotent returns the current version without
  bumping it.
- A declaration that exists with different constraints — a predicate's `kind`
  or object range, for instance: **conflict**. `SCHEMA_CONFLICT` aborts the
  whole upload and the previous active version stays live.

Removal is not expressible through `SCHEMA_UPLOAD` at all: a type absent from
a document is simply not mentioned, not dropped. The destructive path is the
separate `SCHEMA_REPLACE` opcode ([`05_versioning.md`](05_versioning.md) §9).

Adding declarations is therefore always allowed, and changing one is always
either a no-op or a refusal.

## Schema versioning

Each accepted schema gets a version number. Statements and entities carry the `schema_version` they were extracted under.

On schema upload:
- Brain parses and validates.
- Classifies every declaration against the registry; one conflict aborts the upload.
- If anything is new, increments the version (e.g., from version 3 to version 4) and writes the document to the `schema_versions` table, in one transaction with the definition rows.
- Enqueues a flag sweep, post-commit, so the upload's ack is not coupled to a full statement scan.

Existing rows remain queryable, always. What the sweep does is set
`OUTSIDE_ACTIVE_SCHEMA` on each statement and relation whose predicate or type
is missing from the new active version, and clear it on each one the new
version adopts. Readers keep returning flagged rows; a schema-strict query is
the only thing that uses the flag, to drop them deliberately.

## What a schema change does not do

A schema change reconciles nothing. There is no migration plan, no automatic
backfill, and no per-type action to choose: the new version becomes active, the
flag sweep marks what now falls outside the vocabulary, and every existing row
stays exactly as it was written. A statement's own `schema_version` records the
version it was extracted under, which is what makes the old row interpretable
rather than merely tolerated.

Re-aligning the memories behind those rows is a separate, opt-in act.
`reextract_on_schema_change` is **off** by default — spending a tenant's model
budget is not a default — so the migration worker reports how many memories
fall outside the new vocabulary and leaves the decision with the operator, who
runs the admin `EXTRACT_BACKFILL` op. Turning the knob on has the worker
enqueue them itself. Flagging alone never re-aligns anything.

The reasoning behind that, and the action vocabulary a future reconciler would
need, are in [`05_versioning.md`](05_versioning.md) §3.

## Multiple schemas (namespaces)

A namespace is the **tenant (company) boundary** — it isolates *both* the schema/type vocabulary *and* the data. A deployment hosts many namespaces:
- Entity types are namespaced: `acme:Person` and `crm:Person` are different types (plus the shared `brain:` system vocabulary every namespace may reference).
- Predicates and relation types are namespaced.
- **Data is owned by a namespace too:** every entity / statement / relation / memory carries an owner `namespace_id` (and owning `space_id`); rows are partitioned by `(namespace, space)` and cross-namespace reads are forbidden. The owner namespace is distinct from the *type's* namespace — a row owned by `acme` may reference a `brain:` system type.

This lets a single deployment serve multiple tenants with **isolated data and isolated schemas**. See [`./04_namespaces.md`](./04_namespaces.md) for the operational contract (the data-boundary semantics, fail-closed rules, per-`(namespace, space)` entity resolution).

## Progressive enhancement, single pipeline

Every shard boots with the seeded `brain:` system schema; user namespaces add to it via `SCHEMA_UPLOAD`. The storage path, embedding layer, HNSW indexes (memory, entity, statement), both tantivy indexes (memory text, statement text), graph retriever, audit tables, and extractor pipeline are always wired — they don't activate on schema upload. ENCODE accepts text without a user schema; RECALL fans out to all three retrievers. Adding a user namespace makes its predicates / relation_types / entity_types **available** for explicit typed-graph writes and lets extracted rows referencing those types persist.

This is progressive enhancement around one pipeline, not a mode flip. Agents that just want "store this string, give me similar strings back" never need to upload a schema — the seeded `brain:` namespace already covers the common entity types (Person, Place, Organization) that the built-in extractors target. Agents that want richer typed claims about typed entities declare additional namespaces and get them.

The merge model matters because the cost of schema authoring is real (validation, versioning) and operators routinely co-evolve a schema over many small uploads. Merge semantics let those uploads compose without forcing a destructive replace each time.

## What's NOT in the DSL

- **Imperative code.** Extractors are declarative; you don't write Rust or Python in the schema.
- **Custom retrievers.** Retrieval is fixed (semantic, lexical, graph + filters). You don't write your own retriever in the schema.
- **Custom indexes.** Brain decides indexes based on declared types.
- **Joins.** The query engine handles joins; the schema declares structure, not access paths.

For things outside the DSL, users write code that sends wire requests carrying the schema's types.
