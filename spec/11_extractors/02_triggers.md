# 11.02 Triggers

When an extractor runs. Mirrors `TriggerExpr` from
[`../03_schema/02_ast.md`](../03_schema/02_ast.md) §5 +
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
| `OnEncode` | Fires after every successful ENCODE on the same shard. | Enqueued to the per-shard extractor worker; all tiers run async (pattern → classifier → LLM). |
| `OnEncodeWhere(c)` | Same as above, filtered by `c`. | Same as above. |
| `OnDemand` | Never fires automatically; admin / API invokes explicitly. | Background. |
| `OnSchemaChange` | Fires once when `schema_active(ns)` advances. post-v1 — out of scope here. | Background. |
| `Periodic { cron }` | Cron-driven. post-v1 — out of scope here. | Background. |

Brain implements `OnEncode` + `OnEncodeWhere` only. `OnDemand` /
`OnSchemaChange` / `Periodic` parse and persist (see
[`../03_schema/05_versioning.md`](../03_schema/05_versioning.md))
but the worker loop ignores them — they fire `Skipped { reason:
"trigger not implemented" }` audit rows.

## 2. `ConditionExpr` evaluation

The condition AST (see [`../03_schema/02_ast.md`](../03_schema/02_ast.md)):

```rust
pub enum ConditionExpr {
    Atom { field: Vec<String>, op: ConditionOp, value: ConditionValue },
    Matches { field: Vec<String>, regex: String },
    And(Box<ConditionExpr>, Box<ConditionExpr>),
    Or(Box<ConditionExpr>, Box<ConditionExpr>),
}
```

Evaluator surface area:

| Field path | Meaning | Source |
|---|---|---|
| `memory.text` | The text body of the memory. | `Memory::text`. |
| `memory.kind` | Memory kind: `episodic` / `semantic` / `procedural`. | `Memory::kind`. |
| `memory.salience` | f32 in `[0, 1]`. | `Memory::salience`. |
| `memory.agent_id` | The agent that encoded the memory. | `Memory::agent_id` (hex string). |
| `entity.type` | Set on resolver outputs only. Out of scope for `OnEncode` / `OnEncodeWhere` triggers (resolver tier 2+). | n/a |

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
size/runtime caps as §11/01 §2. Compilation failures abort the
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

`depends_on` is consulted by the worker scheduler (see
[`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md)) —
an extractor is dispatched only after all its dependencies have
written their audit rows for the same `(memory_id, schema_version)`
tuple. Circular dependencies are rejected at schema-apply time
(`ExtractorInvalidConfig`).

Brain supports linear chains. Diamond / multi-source
dependencies are accepted but order is arbitrary across the
diamond; deterministic ordering is tracked in
[`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

## 7. Tests

Unit tests cover:

- `OnEncode` fires for every memory.
- `OnEncodeWhere(memory.kind = episodic)` filters non-episodic.
- `Matches` on `memory.text` with a complex regex.
- `And` / `Or` short-circuit semantics.
- `Skipped(filter)` audit row written on filter-fail.
- Type mismatch produces `Skipped(eval error)` not a panic.
