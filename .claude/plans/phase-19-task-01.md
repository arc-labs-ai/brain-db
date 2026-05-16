# 19.1 — §21 backfill + bundled spec edits

Doc-only. Brings `spec/21_schema_dsl/` from 2 files (00_purpose,
01_grammar) to 9 files matching §19 / §20's §03-substrate depth.

Migration plan is **explicitly out of scope per user direction** —
documented as deferred in 07_open_questions.md.

## Files written

| Path | Purpose |
|---|---|
| `spec/21_schema_dsl/02_ast.md` | Typed AST consumed by parser → validator → store → SDK. |
| `spec/21_schema_dsl/03_validator.md` | Static validation rules + error model. |
| `spec/21_schema_dsl/04_namespaces.md` | Namespace isolation + cross-namespace reads. |
| `spec/21_schema_dsl/05_versioning.md` | Version counter + redb row + schema_version on writes. |
| `spec/21_schema_dsl/06_system_schema.md` | Built-in `brain:` types parsed from a static schema string at `MetadataDb::open`. |
| `spec/21_schema_dsl/07_open_questions.md` | Deferrals (migration plan, derive macros, cross-namespace, schema diff). |
| `spec/21_schema_dsl/08_references.md` | Cross-links + code path table. |

Bundled edits:

- `spec/16_benchmarks_acceptance/02_latency_targets.md` — add §2.5
  schema-layer perf targets at typical 50-definition schema. Renumber
  deferred + phase-gate sections.
- `spec/29_knowledge_sdk/00_purpose.md` — flip phase-19 row to "this
  phase".

## Single commit

Doc-only; one commit summarising the §21 expansion.
