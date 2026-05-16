# 19.4 — Schema validator

Static structural validator over the AST from 19.2, consumed at
`SCHEMA_UPLOAD` and `SCHEMA_VALIDATE` time (§21/03).

Migration-time compatibility checks are **explicitly out of scope**
per the v1 no-migration directive (§21/07 Q3). This sub-task ships
structural rules only.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-protocol/src/schema/validator.rs` | `validate(&Schema) -> Result<ValidatedSchema, ValidationErrors>` + supporting types. |
| `crates/brain-protocol/src/schema/mod.rs` | Add `pub mod validator;` + re-exports. |
| `crates/brain-protocol/tests/schema_validator.rs` | Integration tests (12 cases per §21/03 §5). |

## Public surface

```rust
pub fn validate(schema: &Schema) -> Result<ValidatedSchema, ValidationErrors>;

#[derive(Debug, Clone)]
pub struct ValidatedSchema(Schema);
impl ValidatedSchema {
    pub fn as_schema(&self) -> &Schema { &self.0 }
    pub fn into_schema(self) -> Schema { self.0 }
}

pub type ValidationErrors = Vec<ValidationError>;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub code: ValidationErrorCode,
    pub message: String,
    pub source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan { pub line: u32, pub column: u32, pub length: u32 }

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

Rationale: §21/03 mandates `Vec<ValidationError>` (all errors at
once, not first-error). `ValidatedSchema` newtype is a phantom proof
storage takes only validated schemas; constructor is private to
this module so external code can't bypass.

## Rule coverage

Per §21/03 §2:

- **§2.1 Namespace** — present + `[a-z][a-z0-9_]*` + len≤32 +
  not `brain`. Source span: lifted from `Schema.source` if
  available, else `None`.
- **§2.2 Duplicate definitions** — across entity types, predicates,
  relations, extractors. Each duplicate yields one error referencing
  the second occurrence (first is the canonical declaration).
- **§2.3 Type-reference resolution** — collect entity-type names;
  every `Relation.from_type` / `Relation.to_type` / predicate
  `ObjectTypeDecl::Entity { entity_type }` / extractor
  `Entity{entity_type}` resolves to a declared entity OR the
  literal `Any`. Relation-target `ExtractorTarget::Relation { ... }`
  must resolve to a declared relation type.
- **§2.4 Predicate kind/object** —
  - `Fact` → any object kind allowed.
  - `Preference` → `Value<T>` or `Any` only.
  - `Event` → `Value<T>`, `Entity<T>`, or `Any` only.
- **§2.5 Cardinality+symmetric** — `symmetric:true` requires
  `OneToOne` or `ManyToMany`.
- **§2.6 Attribute rules** —
  - `unique` + `Ref{...}` → invalid.
  - `default` literal type matches `attr_type`:
    `Text`↔`LiteralValue::Text`, `Number`↔`Number`,
    `Bool`↔`Bool`, `Date`↔`Date`/`Text` (accept ISO text),
    `Timestamp`↔`Timestamp`/`Number`, `Enum` → text matching a
    variant, `Ref` → text (loosely; better checks belong in 19.5
    storage).
  - Attribute name `[a-z][a-z0-9_]*`, len≤64.
- **§2.7 Extractor rules** —
  - `pattern` requires `Patterns(...)` non-empty.
  - `classifier`/`llm` require `Model(_)`.
  - `llm` requires `Prompt(_)`.
  - Each `ExtractorField` discriminant appears at most once. (The
    AST stores `Vec<ExtractorField>` — we walk and count by
    enum variant.)
  - `Confidence` in `[0,1]`; `ConfidenceThreshold` in `[0,1]`.
- **§2.8 Reserved names** — applies only when `namespace == "brain"`,
  which is already rejected upstream by §2.1. Keep the per-name
  table as a defensive guard but it should never fire in normal
  user flows.

## Source spans

Phase 19.3 parser doesn't currently emit spans onto the AST (AST is
value-typed; spans live in the pest tree at parse time only).
Validator-only error spans: leave as `None`. Adding spans is a
follow-up — tracked in §21/07 as part of Q4. Keep the `SourceSpan`
field for forward-compat.

## Tests (per §21/03 §5)

Integration tests in `tests/schema_validator.rs` exercising each
rule + a happy-path schema. ~14 tests:

1. Missing `namespace` → `NamespaceMissing`.
2. `namespace BRAIN` (uppercase) → `NamespaceInvalidIdentifier`.
3. `namespace brain` → `NamespaceInvalidIdentifier`.
4. Two `Person` → `DuplicateDefinition`.
5. Two predicates with same name → `DuplicateDefinition`.
6. Relation `from_type: "Persoon"` → `UnresolvedTypeRef`.
7. Predicate `kind: Preference object: Entity<Person>` → `PredicateKindObjectMismatch`.
8. Predicate `kind: Event object: Statement` → mismatch.
9. Relation `OneToMany + symmetric:true` → `RelationCardinalitySymmetricInvalid`.
10. `unique` on `ref<Person>` → `AttributeUniqueOnRefType`.
11. `default 42` on `attr: text` → `DefaultIncompatibleWithType`.
12. `pattern` extractor with no `patterns:` → `ExtractorMissingRequired`.
13. Duplicate `model:` in one extractor (parser does last-wins but
    if both literal fields land in AST we still validate the count;
    we'll synthesize via direct AST construction since parser would
    collapse to one) → `ExtractorDuplicateField`.
14. `confidence_threshold: 1.5` → `ExtractorInvalidConfig`.
15. Happy path — the full §21/00 example schema → `Ok(ValidatedSchema)`.

A few `#[test]` cases for the `from_type: "Any"` special case to
confirm it resolves.

## Out of scope

- Migration / compatibility checks across versions (deferred).
- Warnings vs errors split (§21/07 Q4).
- Validating LLM prompt length cap (config-driven, §20).
- Cron-string validation for `Periodic` triggers (defer).

## Single commit

`feat(protocol): 19.4 — schema validator`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-protocol schema::
cargo test -p brain-protocol --test schema_validator
cargo clippy --target x86_64-unknown-linux-gnu -p brain-protocol --all-targets -- -D warnings
```
