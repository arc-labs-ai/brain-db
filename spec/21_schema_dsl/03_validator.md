# 21.03 Schema Validator

Static validation rules + error model for the DSL. Runs over the
typed AST ([§02](./02_ast.md)) after the parser
([§01](./01_grammar.md)). Validation passes are pure functions —
no I/O, no async.

Migration-time compatibility checks (e.g., "removing a predicate
with live statements") are **explicitly out of scope** for v1 per
the project's no-migration directive. The validator only runs
static structural checks. See [§07](./07_open_questions.md) Q3 for
the deferral.

Cross-references:
- [`./02_ast.md`](./02_ast.md) — AST shapes validation runs over.
- [`./05_versioning.md`](./05_versioning.md) §3 — when the
  validator runs at `SCHEMA_UPLOAD` time.
- [`../28_knowledge_wire_protocol/05_schema_frames.md`](../28_knowledge_wire_protocol/05_schema_frames.md)
  §5 — `SCHEMA_VALIDATE` opcode (parses + validates without
  persisting).

## 1. Surface

```rust
pub fn validate(schema: &Schema) -> Result<ValidatedSchema, ValidationErrors>;
```

`ValidatedSchema` is a thin newtype proving the validator passed —
storage code that accepts only validated schemas takes
`&ValidatedSchema`, not `&Schema`. The `validate()` function is the
only constructor.

`ValidationErrors` is a `Vec<ValidationError>` — validation
returns **all** errors, not the first one. Useful for surfacing
multiple problems to the caller in a single round-trip.

```rust
pub struct ValidationError {
    pub code: ValidationErrorCode,
    pub message: String,
    pub source_span: Option<SourceSpan>,
}

pub struct SourceSpan {
    pub line: u32,        // 1-indexed
    pub column: u32,
    pub length: u32,
}

pub enum ValidationErrorCode {
    NamespaceMissing,
    NamespaceInvalidIdentifier,
    DuplicateDefinition,
    UnresolvedTypeRef,
    PredicateKindObjectMismatch,
    RelationCardinalitySymmetricInvalid,
    ExtractorMissingRequired,
    ExtractorDuplicateField,
    ExtractorInvalidConfig,
    AttributeUniqueOnRefType,
    DefaultIncompatibleWithType,
    NameInvalidIdentifier,
    NameTooLong,
}
```

## 2. The rules

### 2.1 Namespace

- A schema MUST declare a `namespace`. Missing namespace →
  `NamespaceMissing`.
- The namespace identifier MUST match `[a-z][a-z0-9_]*`, max 32
  chars. Same grammar as predicate / relation-type namespaces
  ([§17/03](../17_knowledge_model/) and per phase 17.3 /
  18.3 conventions).
- The `brain:` namespace is **reserved** for the system schema
  (§06). User schemas MUST NOT declare `namespace brain`. →
  `NamespaceInvalidIdentifier`.

### 2.2 Duplicate definitions

- No two `EntityTypeDef` with the same name in one document.
- No two `PredicateDef` with the same name.
- No two `RelationTypeDef` with the same name.
- No two `ExtractorDef` with the same name.

Duplicate → `DuplicateDefinition` with both source spans recorded.

### 2.3 Type-reference resolution

Every entity-type name referenced in a `RelationTypeDef.from_type /
to_type`, `ObjectTypeDecl::Entity { entity_type }`, or
`ExtractorTarget::Entity / Relation { ... }` MUST resolve to either:

- An `EntityTypeDef` in the same document.
- The special `Any` literal (case-sensitive).

Cross-namespace references are NOT supported in v1. References
without explicit qualification resolve only within the current
schema's namespace. → `UnresolvedTypeRef`.

### 2.4 Predicate kind / object consistency

| `kind` | Allowed `object` |
|---|---|
| `Fact` | Any of `Value<T>` / `Entity<T>` / `Memory` / `Statement` / `Any` |
| `Preference` | `Value<T>` / `Any` |
| `Event` | `Value<T>` / `Entity<T>` / `Any` |

Mismatched combinations → `PredicateKindObjectMismatch`.

Rationale: Preferences with `Entity<T>` objects model edges, which
should be a relation_type instead; Statements / Memory references
as Preference objects don't have meaningful semantics for the
auto-supersession path.

### 2.5 Cardinality + symmetric combinations

- `symmetric: true` is invalid for `OneToMany` and `ManyToOne`
  (asymmetric by definition). → `RelationCardinalitySymmetricInvalid`.
- `OneToOne + symmetric: true` is allowed (marriage / paired).
- `ManyToMany + symmetric: true` is the typical "discussed_with"
  case.

### 2.6 Attribute / property rules

- An attribute marked `unique` MUST NOT be `Ref<...>` — uniqueness
  on entity references is the relation cardinality's job, not the
  attribute's. → `AttributeUniqueOnRefType`.
- A `default` literal MUST match the declared `AttrType`. → 
  `DefaultIncompatibleWithType`.
- Attribute names MUST match `[a-z][a-z0-9_]*`, max 64 chars. →
  `NameInvalidIdentifier` / `NameTooLong`.

### 2.7 Extractor rules

- `pattern` extractor MUST have at least one `patterns:` entry.
- `classifier` + `llm` extractor MUST have a `model:` field.
- `llm` extractor MUST have a `prompt:` field.
- Each `ExtractorField` may appear at most once. → `ExtractorDuplicateField`.
- `confidence_threshold` MUST be in `[0, 1]`. → `ExtractorInvalidConfig`.
- `confidence` (pattern) MUST be in `[0, 1]`.

Triggers + cost budgets + cache configs are parsed but not
behaviourally validated in v1; phase 20 extractor work adds the
runtime checks.

### 2.8 Reserved names

The `brain:` namespace reserves the following type names; user
schemas MUST NOT redeclare them under the `brain:` namespace:

- `Person` (entity)
- `related_to`, `reports_to`, `co_authored` (relation types)
- `is_a`, `has_name`, `mentions`, `related_to`, `prefers`,
  `scheduled` (predicates)

Other namespaces may freely use any names (the qname is unique).

## 3. Error reporting

Errors carry source spans when available. The parser supplies
spans during AST construction; programmatic SchemaBuilder uploads
omit them.

```text
ValidationError {
    code: UnresolvedTypeRef,
    message: "relation_type \"reports_to\": from_type \"Persoon\" \
              is not declared",
    source_span: Some(SourceSpan { line: 47, column: 14, length: 7 }),
}
```

The wire-side `SCHEMA_UPLOAD` / `SCHEMA_VALIDATE` responses carry
a `Vec<ValidationErrorWire>` with a wire-friendly representation —
see [§28/05](../28_knowledge_wire_protocol/05_schema_frames.md) §5.

## 4. Severity

All validation errors are **errors** — none are warnings in v1. A
schema that produces any `ValidationError` is rejected; no partial
acceptance. This trades flexibility for clarity (the user sees
"fix everything before re-upload" rather than "we accepted half
of it").

Warnings + advisories (e.g., "this looks like it should be a
relation, not an attribute ref<>") are tracked for post-v1 (§07
Q4).

## 5. Tests (phase 19.4)

Phase 19.4 lands the validator with these test cases:

- Empty schema (no `namespace`) → `NamespaceMissing`.
- Reserved `brain:` namespace → `NamespaceInvalidIdentifier`.
- Two `Person` entities → `DuplicateDefinition` with both spans.
- `from_type: "Persoon"` (typo) → `UnresolvedTypeRef`.
- `Preference` predicate with `Entity<Person>` object →
  `PredicateKindObjectMismatch`.
- `OneToMany + symmetric: true` → invalid combination.
- `unique` modifier on `ref<Person>` → invalid.
- `default 42` for `attr: text` → incompatible.
- `pattern` extractor with no `patterns:` → missing required.
- Duplicate `model:` in one extractor → duplicate field.
- `confidence_threshold: 1.5` → invalid config.
- Valid schema with 10 types + 5 predicates + 3 relation types →
  passes; produces `ValidatedSchema`.

## 6. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q3 — Migration-time compatibility checks (deferred per project
  scope).
- Q4 — Warnings vs errors split.
- Q5 — Custom validation rules / plugins.
