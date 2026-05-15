# Schema DSL

## Purpose

Users declare entity types, predicates, relation types, and extractors in a single declarative schema document. The substrate enforces these declarations on writes and uses them to drive extraction.

The DSL is designed for:
- **Readability**: looks like documentation, not code.
- **Stability**: changes are versioned; existing data migrates.
- **Tooling**: parseable for editors, formatters, validators.

## Format

Schema is a single document, typically `schema.brain`, parsed by the substrate at upload time:

```
# Schema for a CRM-like cognitive substrate
# Version 3, 2026-05-13

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
    model: "brain-reporting-line-classifier-v3"
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

The full grammar (with whitespace, comments, escapes) lives in `01_grammar.md` (TBD).

## Validation rules

The substrate validates a schema before accepting it:

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

### Compatibility (when updating existing schema)
- Removing an entity type that has live entities: refuse, require migration.
- Renaming an attribute: allowed; old name moves to alias internally.
- Changing an attribute's type: refuse if incompatible (e.g., text → number).
- Changing predicate's `kind`: refuse (would invalidate existing statements).
- Removing a predicate: warn and require explicit `cascade_tombstone` flag.
- Adding fields: always allowed.

## Schema versioning

Each accepted schema gets a version number. Statements and entities carry the `schema_version` they were extracted under.

On schema upload:
- The substrate parses and validates.
- If valid, increments the version (e.g., from v3 to v4).
- Writes the new schema document to `schema_versions` table.
- Triggers a migration plan: which extractors changed, which need re-running.
- Optionally, runs backfill: re-extract over existing memories under the new schema (configurable; typically run as a worker over weeks).

Existing statements remain queryable. They may become "stale" if the extractor that produced them has been improved. A `stale: bool` flag is set when their extractor version is older than the current.

## Migration semantics

When a schema changes, three actions are possible per affected statement:

| Action | Behavior |
|---|---|
| **keep** | Old statement stays. New extractor doesn't touch it. Flagged stale. |
| **re-extract** | Worker re-runs the new extractor over the source memory; produces new statement. Old supersedes-by-new. |
| **tombstone** | Old statement marked tombstoned. Reason: SchemaInvalidation. |

The user picks the action per migration (in the migration declaration) or accepts the default (`re-extract`).

## Multiple schemas (namespaces)

A deployment can have multiple schemas under different namespaces. They share storage (one entities table, one statements table) but don't conflict because:
- Entity types are namespaced: `acme.Person` and `crm.Person` are different types.
- Predicates are namespaced.
- Relation types are namespaced.

This lets a single substrate serve multiple applications with isolated schemas.

## What's NOT in the DSL

- **Imperative code.** Extractors are declarative; you don't write Rust or Python in the schema.
- **Custom retrievers.** Retrieval is fixed (semantic, lexical, graph + filters). You don't write your own retriever in the schema.
- **Custom indexes.** The substrate decides indexes based on declared types.
- **Joins.** The query engine handles joins; the schema declares structure, not access paths.

For things outside the DSL, users write code that calls the SDK with the schema's types.
