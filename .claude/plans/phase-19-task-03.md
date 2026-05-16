# 19.3 — DSL parser (pest 2.x)

Implements the surface syntax in `spec/21_schema_dsl/01_grammar.md`
end-to-end into the `ast.rs` types from 19.2.

## Crate / dep changes

- Workspace deps: add `pest = "2.7"` and `pest_derive = "2.7"`.
- `brain-protocol`: depend on both.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-protocol/src/schema/parser.rs` | Pest-driven parser; `parse_schema(&str) -> Result<Schema, ParseError>`. |
| `crates/brain-protocol/src/schema/grammar.pest` | PEG grammar mirroring the EBNF. |
| `crates/brain-protocol/src/schema/parse_error.rs` | `ParseError` enum with line/col + diagnostic body. |
| `crates/brain-protocol/src/schema/mod.rs` | Re-exports + new submodules. |
| `crates/brain-protocol/Cargo.toml` | `pest` + `pest_derive` deps. |
| `Cargo.toml` (root) | `pest`/`pest_derive` in `[workspace.dependencies]`. |

## Public surface

```rust
pub fn parse_schema(input: &str) -> Result<Schema, ParseError>;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("syntax error at {line}:{col}: {message}")]
    Syntax { line: usize, col: usize, message: String },
    #[error("invalid number at {line}:{col}: {value:?}")]
    InvalidNumber { line: usize, col: usize, value: String },
    #[error("invalid JSON at {line}:{col}: {message}")]
    InvalidJson { line: usize, col: usize, message: String },
    #[error("invalid duration at {line}:{col}: {value:?}")]
    InvalidDuration { line: usize, col: usize, value: String },
    #[error("invalid cost expression at {line}:{col}: {message}")]
    InvalidCost { line: usize, col: usize, message: String },
    #[error("missing required field {field:?} at {line}:{col}")]
    MissingField { line: usize, col: usize, field: String },
}
```

`Schema::source` is **always** populated from the input text (parser
records the verbatim source). The `SchemaBuilder` API later passes
`None`, but text-form `SCHEMA_UPLOAD` carries the source through.

## Grammar design notes

Pest grammar (`grammar.pest`) decisions:

1. **WHITESPACE + COMMENT rules** are silent — pest auto-skips them
   between tokens once configured. Inside strings / regex / heredoc
   they're literal.
2. **Identifiers** are `[A-Za-z_][A-Za-z0-9_]*`. Reserved keywords
   (`define`, `entity_type`, `predicate`, ...) match before
   identifier where the EBNF disambiguates by position.
3. **Heredoc strings** (`"""..."""`) match greedily up to the first
   closing `"""`. Captured raw — the parser strips the triple-quotes
   and returns the inner text.
4. **JSON values** — captured as raw `{...}` / `[...]` runs with
   balanced braces / brackets, then handed to `serde_json::from_str`.
   Failures bubble up as `ParseError::InvalidJson` with the position
   of the opening delimiter.
5. **Regex literals** — `/.../` with `\/` escape supported; passed
   to the AST verbatim (no compilation here — validator may compile
   for sanity in 19.4 but parser stays cheap).
6. **Duration suffix** — `s|m|h|d` mapped to `DurationUnit::Seconds`
   / `Minutes` / `Hours` / `Days`.
7. **Cost expression** — `$NUM per (memory|request|day)`.
8. **Trailing commas** in `identifier_list`, `patterns`, JSON
   arrays — accepted and ignored by the grammar.
9. **Comments** (`# ...\n`) are skipped everywhere except inside
   strings / heredocs / regex.

## Mapping rules

- `optional` modifier maps to "field is allowed to be absent" and
  is NOT stored in the AST (default `required = false`). `required`
  flips it to `true`. Both can appear; later wins. (Spec is silent;
  validator can flag duplicates if we want — defer to 19.4.)
- `attr_type` mapping:
  - `text` → `AttrType::Text`
  - `number` → `AttrType::Number`
  - `bool` → `AttrType::Bool`
  - `date` → `AttrType::Date`
  - `timestamp` → `AttrType::Timestamp`
  - `enum [a, b, c]` → `AttrType::Enum { variants }`
  - `ref<Person>` → `AttrType::Ref { target }`
- `object_type` mapping:
  - `Value<text>` → `ObjectTypeDecl::Value { value_type }`
  - `Entity<Person>` → `ObjectTypeDecl::Entity { entity_type }`
  - `Memory` → `ObjectTypeDecl::Memory`
  - `Statement` → `ObjectTypeDecl::Statement`
  - `Any` → `ObjectTypeDecl::Any`
- `target_decl`:
  - `entity Person` → `ExtractorTarget::Entity`
  - `statement Preference` → `ExtractorTarget::Statement`
  - `relation reports_to` → `ExtractorTarget::Relation`
  - `entity_or_statement` → `ExtractorTarget::EntityOrStatement`
- `trigger:` clauses → `TriggerExpr` variants. `condition_expr`
  walks recursively producing `ConditionExpr::And` / `Or` left-
  associative.

## Test fixtures

`crates/brain-protocol/tests/schema_parser.rs` integration tests:

1. **Empty schema** — `namespace acme` → `Schema { namespace: "acme", items: [], source: Some(_) }`.
2. **Entity type with attributes** — exact example from §21/00 lines
   24–31 parses; assert attribute names, types, modifiers.
3. **Predicate def** — `Fact` / `Preference` / `Event` × `Value<>` /
   `Entity<>` / `Memory` / `Statement` / `Any` variants.
4. **Relation type with properties + symmetric** — exact example
   from §21/00 lines 60–73.
5. **Pattern extractor** — example from §21/00 lines 77–85; regex
   literal preserved as-is.
6. **LLM extractor with heredoc prompt + JSON examples + JSON
   schema** — §21/00 lines 87–105.
7. **Classifier extractor with `trigger: on encode where ... matches ...`** —
   §21/00 lines 107–113.
8. **Full §21/00 example** — end-to-end parse; assert item count,
   namespace, and a few spot-checks.
9. **Comments + trailing commas + CRLF** — round-trip.
10. **Syntax error positions** — malformed input surfaces the
    expected line/col in `ParseError::Syntax`.

Unit tests in `parser.rs` cover small grammar pieces (literals,
duration, cost, condition_expr).

## Out of scope

- Validation (unresolved type refs, kind/object mismatch) — 19.4.
- `use` directive — grammar admits it (per §21/01), but 19.3 parses
  + discards (no AST node yet). Multi-document is post-v1
  (§21/04 + §21/07 Q6). The parser MAY surface `ParseError::Syntax`
  here, but the simpler path is to accept the token and ignore.
  Decision: accept + ignore, leaving a TODO note.
- `namespace` redeclaration in a single document — accepted; last
  wins. Validator handles.

## Single commit

`feat(protocol): 19.3 — schema DSL parser (pest)`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-protocol schema::
cargo clippy --target x86_64-unknown-linux-gnu -p brain-protocol --all-targets -- -D warnings
```
