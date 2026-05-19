# Schema DSL grammar

Formal grammar for the Brain schema DSL — the language used to
declare entity types, predicates, relations, and extractors that
flip a deployment from substrate-only mode into the knowledge
layer.

**Spec:** §21/01 (grammar), §21/02 (semantics), §21/03 (validation).
**Source:** `brain_protocol::schema::{parse_schema, validate}`.

A schema document is uploaded via `SchemaUploadReq (0x0120)`. Max
size 1 MiB (spec §21/03 §34).

## File shape

```
namespace acme

define entity_type Person {
  attributes {
    email: text optional unique
  }
}

define predicate prefers {
  kind: Preference
  object: Value<text>
}

define relation_type reports_to {
  from: Person
  to: Person
  cardinality: many-to-one
}

define extractor person_mentions {
  kind: pattern
  target: entity Person
  patterns [ /\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2})\b/ ]
  confidence: 0.7
}
```

One `namespace` declaration; zero-or-more of the four `define`
constructs. Order doesn't matter — references resolve after the
whole document is parsed.

## Grammar (EBNF)

```ebnf
schema           = ws? (statement ws?)*
statement        = namespace_decl
                 | entity_type_def
                 | predicate_def
                 | relation_type_def
                 | extractor_def
                 | comment

namespace_decl   = "namespace" ws identifier
identifier       = /[a-z_][a-z0-9_]*/    (* max 32 chars *)
ws               = (whitespace | comment)+
comment          = "//" ... newline
```

## `define entity_type`

```ebnf
entity_type_def  = "define" ws "entity_type" ws identifier ws? "{" ws?
                       "attributes" ws? "{" ws? (attribute_decl ws?)* "}"
                   ws? "}"

attribute_decl   = identifier ":" ws? attr_type (ws modifier)*

attr_type        = "text"
                 | "number"
                 | "bool"
                 | "date"
                 | "timestamp"
                 | "enum" ws? "[" ws? identifier_list ws? "]"
                 | "ref" ws? "<" ws? identifier ws? ">"

modifier         = "required"
                 | "optional"
                 | "unique"
                 | "indexed"
                 | "default" ws literal

identifier_list  = identifier ("," ws? identifier)*
literal          = string_literal | number_literal | bool_literal | date_literal
```

Validation (spec §21/03):
- `unique` is invalid on `ref<…>` types.
- `default <literal>` must match the declared `attr_type`.
- An attribute is exactly one of `required` or `optional`; defaulting to `optional` if neither given.

## `define predicate`

```ebnf
predicate_def    = "define" ws "predicate" ws identifier ws? "{" ws?
                       "kind:"   ws? statement_kind ws?
                       "object:" ws? object_type    ws?
                   "}"

statement_kind   = "Fact" | "Preference" | "Event"

object_type      = "Value"     ws? "<" attr_type   ">"
                 | "Entity"    ws? "<" identifier  ">"
                 | "Memory"
                 | "Statement"
                 | "Any"
```

Validation (spec §21/03 §103–111):
- `kind = Preference` rejects `Entity<…>` and `Statement` object types.
- `kind = Event` rejects `Statement` object type.

## `define relation_type`

```ebnf
relation_type_def = "define" ws "relation_type" ws identifier ws? "{" ws?
                       "from:"        ws? identifier   ws?
                       "to:"          ws? identifier   ws?
                       ("cardinality:" ws? cardinality ws?)?
                       ("symmetric:"   ws? bool_literal ws?)?
                       ("properties"  ws? "{" (attribute_decl ws?)* "}")?
                    ws? "}"

cardinality      = "one-to-one"  | "one-to-many"
                 | "many-to-one" | "many-to-many"
```

Validation (spec §21/03 §118–124):
- `symmetric: true` is invalid for `one-to-many` and `many-to-one`.
- `from` and `to` must resolve to declared entity types (or the
  literal `Any`).

## `define extractor`

```ebnf
extractor_def    = "define" ws "extractor" ws identifier ws? "{" ws?
                       "kind:"   ws? extractor_kind ws?
                       "target:" ws? target_decl    ws?
                       (extractor_field ws?)*
                   "}"

extractor_kind   = "pattern" | "classifier" | "llm"

target_decl      = "entity"             ws identifier
                 | "statement"          ws statement_kind
                 | "relation"           ws identifier
                 | "entity_or_statement"

extractor_field  = "patterns"             ws? "[" regex_list "]"
                 | "model:"               ws? string_literal
                 | "prompt:"              ws? string_literal
                 | "feature_extraction:"  ws? string_literal
                 | "examples:"            ws? json_literal
                 | "schema:"              ws? json_literal
                 | "cache:"               ws? ("enabled" | "disabled")
                 | "cache_ttl:"           ws? duration_literal
                 | "confidence:"          ws? number_literal
                 | "confidence_threshold:" ws? number_literal
                 | "cost_budget:"         ws? cost_expr
                 | "trigger:"             ws? trigger_expr
                 | "depends_on:"          ws? "[" identifier_list "]"
                 | "resolver:"            ws? "{" resolver_body "}"

trigger_expr     = "on encode" (ws "where" ws condition)?
                 | "on demand"
                 | "on schema_change"
                 | "periodic" ws "at" ws string_literal      (* cron *)
```

Per-kind required fields (spec §21/03 §161–177):
- `pattern` — requires `patterns:` and `confidence:`.
- `classifier` — requires `model:` and `confidence_threshold:`.
- `llm` — requires `model:`, `prompt:`, and `confidence_threshold:`.
- All — `confidence` and `confidence_threshold` clamp to `[0, 1]`.

## Identifiers + reserved names

- Identifier regex: `[a-z_][a-z0-9_]*`, max 32 characters.
- The `brain:` namespace is reserved. Built-in identifiers cannot
  be redefined: `Person`, `related_to`, `reports_to`, `co_authored`,
  `is_a`, `has_name`, `mentions`, `prefers`, `scheduled`.

## Validation summary

Run by `brain_protocol::schema::validate` before persist. Errors
return as `SchemaValidationErrorWire` items in the
`SchemaUploadResp` payload; no version bump on validation failure.

Checks performed:

1. Namespace present, identifier-shaped.
2. No duplicate definitions (same identifier twice in the same
   document).
3. Every type reference resolves within the document.
4. Per-kind validity (predicate / relation / extractor rules above).
5. Confidence values in `[0, 1]`.
6. `unique` / `default` compatibility on entity attributes.

**Not** performed in v1 (deferred to phase 22+):

- Backwards-compatibility checks against the previously-uploaded
  schema for the same namespace.
- Diffing for migration plans.
- Warnings (everything that isn't an error is a pass).

## See also

- [`examples.md`](examples.md) — worked end-to-end schemas.
- [`../wire-protocol/opcodes.md`](../wire-protocol/opcodes.md) — `SchemaUpload*` opcodes.
- [`../../concepts/substrate-vs-knowledge.md`](../../concepts/substrate-vs-knowledge.md) — when and why to declare a schema.

**Spec:** §21/01 (grammar), §21/02 (semantics), §21/03 (validation).
