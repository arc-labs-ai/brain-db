# 22.03 Triggers

When an extractor runs. Mirrors `TriggerExpr` from
[`../21_schema_dsl/02_ast.md`](../21_schema_dsl/02_ast.md) §5 +
the §00 narrative on triggering.

## 1. The trigger types

```rust
pub enum TriggerExpr {
    OnEncode,
    OnEncodeWhere(ConditionExpr),
    OnDemand,
    OnSchemaChange,
    Periodic { cron: String },
}
```

| Variant | Semantics | Worker tier |
|---|---|---|
| `OnEncode` | Fires after every successful ENCODE on the same shard. | Foreground (pattern) / near-foreground (classifier) / background (llm). |
| `OnEncodeWhere(c)` | Same as above, filtered by `c`. | Same as above. |
| `OnDemand` | Never fires automatically; admin / API invokes explicitly. | Background. |
| `OnSchemaChange` | Fires once when `schema_active(ns)` advances. Phase 22+ — out of scope for phase 20. | Background. |
| `Periodic { cron }` | Cron-driven. Phase 22+ — out of scope for phase 20. | Background. |

Phase 20 implements `OnEncode` + `OnEncodeWhere` only. `OnDemand` /
`OnSchemaChange` / `Periodic` parse and persist (§21/05) but the
worker loop ignores them — they fire `Skipped { reason:
"trigger not implemented" }` audit rows.

## 2. `ConditionExpr` evaluation

The condition AST (§21/02 §5):

```rust
pub enum ConditionExpr {
    Atom { field: Vec<String>, op: ConditionOp, value: ConditionValue },
    Matches { field: Vec<String>, regex: String },
    And(Box<ConditionExpr>, Box<ConditionExpr>),
    Or(Box<ConditionExpr>, Box<ConditionExpr>),
}
```

Phase 20 evaluator surface area:

| Field path | Meaning | Source |
|---|---|---|
| `memory.text` | The text body of the memory. | `Memory::text`. |
| `memory.kind` | Memory kind: `episodic` / `semantic` / `procedural`. | `Memory::kind`. |
| `memory.salience` | f32 in `[0, 1]`. | `Memory::salience`. |
| `memory.agent_id` | The agent that encoded the memory. | `Memory::agent_id` (hex string). |
| `entity.type` | Set on resolver outputs only. Out of scope for phase 20 (resolver tier 2+). | n/a |

Operators (`=` / `!=` / `<` / `<=` / `>` / `>=` / `in`) apply
type-wise:
- Text fields support `=` / `!=` / `matches` / `in [text, ...]`.
- Numeric fields support all comparison ops.
- `Bool` fields support `=` / `!=`.

Type mismatches at evaluation time are runtime errors: the
extractor skips with `ExtractionAudit.status = Skipped(reason:
"trigger eval error")`.

## 3. Compilation

Trigger conditions are compiled once at schema-apply time:

```rust
pub fn compile_trigger(t: &TriggerExpr) -> CompiledTrigger;
```

`CompiledTrigger` pre-walks `Matches` regexes through the same
size/runtime caps as §22/01 §2. Compilation failures abort the
schema upload with `ExtractorInvalidConfig`.

## 4. Filter-fail ≠ extraction-fail

When `OnEncodeWhere(c)` evaluates `c` and returns `false`, the
worker writes a `Skipped(reason: "filter")` audit row (light, no
output) and moves on. This is **not an error** — it's the success
path for "the extractor correctly chose not to run".

The light-skip audit is configurable via deployment setting;
default is to write the row (cheap; useful for "show me what got
filtered" analysis), but operators with massive ENCODE throughput
may set `audit.skip_filter = false` to silence them.

## 5. Multiple triggers

An extractor declares one `trigger:` field. Compound conditions
go in the `where` clause:

```text
trigger: on encode where (memory.kind = episodic) and
                         (memory.text matches /.*meeting.*/)
```

Internal evaluation is left-to-right, short-circuiting on `and` /
`or`.

## 6. `depends_on` chains

```rust
pub struct ExtractorDef {
    ...
    pub depends_on: Vec<ExtractorId>,
}
```

`depends_on` is consulted by the worker scheduler (§27/01) — an
extractor is dispatched only after all its dependencies have
written their audit rows for the same `(memory_id, schema_version)`
tuple. Circular dependencies are rejected at schema-apply time
(`ExtractorInvalidConfig`).

Phase 20 supports linear chains. Diamond / multi-source
dependencies are accepted but order is arbitrary across the
diamond; deterministic ordering tracked as a §22/07 open question.

## 7. Tests

Phase 20 unit tests cover:

- `OnEncode` fires for every memory.
- `OnEncodeWhere(memory.kind = episodic)` filters non-episodic.
- `Matches` on `memory.text` with a complex regex.
- `And` / `Or` short-circuit semantics.
- `Skipped(filter)` audit row written on filter-fail.
- Type mismatch produces `Skipped(eval error)` not a panic.
