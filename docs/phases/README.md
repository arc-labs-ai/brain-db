# Phase Documentation

Detailed implementation plans for each phase of Brain. The high-level summary lives in [`../../ROADMAP.md`](../../ROADMAP.md); this directory has the per-phase breakdowns.

Brain v1.0 ships in two layers:

- **Substrate (phases 0–14)** — vector memory store: WAL, HNSW, wire protocol, cognitive primitives (ENCODE, RECALL, PLAN, REASON, FORGET), HTTP transport, observability, benchmarks, acceptance.
- **Knowledge layer (phases 15–24)** — typed entities, statements, relations, schema DSL, three-tier extractors, hybrid retrieval. Activates when a schema is declared; dormant otherwise.

The v1.0.0 tag lands at the end of Phase 24, after the *combined* acceptance suite passes. Phase 14 produces a substrate-only `v0.9.x` release-candidate tag — a valid deployment posture for users who only need vector retrieval.

## Substrate phases (0–14)

| Phase | Title | File |
|---|---|---|
| 0 | Workspace skeleton | (provided by starter — see [`ROADMAP.md`](../../ROADMAP.md) §Phase 0) |
| 1 | Wire protocol & core types | [`phase-01-wire-protocol.md`](phase-01-wire-protocol.md) |
| 2 | Storage: arena + WAL + recovery | [`phase-02-storage.md`](phase-02-storage.md) |
| 3 | Metadata + graph (redb) | [`phase-03-metadata.md`](phase-03-metadata.md) |
| 4 | ANN index (HNSW) | [`phase-04-ann-index.md`](phase-04-ann-index.md) |
| 5 | Embedding layer | [`phase-05-embedding.md`](phase-05-embedding.md) |
| 6 | Query planner & executor | [`phase-06-planner.md`](phase-06-planner.md) |
| 7 | Cognitive operations | [`phase-07-operations.md`](phase-07-operations.md) |
| 8 | Background workers | [`phase-08-workers.md`](phase-08-workers.md) |
| 9 | Server end-to-end wire-up | [`phase-09-server.md`](phase-09-server.md) |
| 9 (alt) | Glommio port | [`phase-09-glommio-port.md`](phase-09-glommio-port.md) |
| 10 | Rust SDK & CLI | [`phase-10-sdk-cli.md`](phase-10-sdk-cli.md) |
| 11 | `brain-http` foundation (HTTP/WS/SSE) | [`phase-11-brain-http.md`](phase-11-brain-http.md) |
| 12 | Observability | [`phase-12-observability.md`](phase-12-observability.md) |
| 13 | Benchmarks & chaos | [`phase-13-benchmarks.md`](phase-13-benchmarks.md) |
| 14 | Substrate acceptance + `v0.9.x-substrate-rc` tag | [`phase-14-acceptance-release.md`](phase-14-acceptance-release.md) |

## Knowledge-layer phases (15–24)

Estimated total: 58–83 days of focused work. Phases 16–22 can partially overlap once Phase 15 is done.

| Phase | Title | Days | File |
|---|---|---|---|
| 15 | Knowledge storage extensions | 3–5 | [`phase-15-knowledge-storage.md`](phase-15-knowledge-storage.md) |
| 16 | Entity layer | 7–10 | [`phase-16-entities.md`](phase-16-entities.md) |
| 17 | Statement layer | 7–10 | [`phase-17-statements.md`](phase-17-statements.md) |
| 18 | Relation layer | 5–7 | [`phase-18-relations.md`](phase-18-relations.md) |
| 19 | Schema DSL | 5–7 | [`phase-19-schema-dsl.md`](phase-19-schema-dsl.md) |
| 20 | Pattern + classifier extractors | 7–10 | [`phase-20-pattern-classifier-extractors.md`](phase-20-pattern-classifier-extractors.md) |
| 21 | LLM extractor | 7–10 | [`phase-21-llm-extractor.md`](phase-21-llm-extractor.md) |
| 22 | Tantivy / lexical retrieval | 5–7 | [`phase-22-tantivy-lexical.md`](phase-22-tantivy-lexical.md) |
| 23 | Hybrid query engine | 7–10 | [`phase-23-hybrid-query.md`](phase-23-hybrid-query.md) |
| 24 | Sweepers + knowledge acceptance + `v1.0.0` | 5–7 | [`phase-24-acceptance.md`](phase-24-acceptance.md) |

### Dependency DAG

```
15 (storage)
  ├──> 16 (entities)
  │     ├──> 17 (statements)
  │     │     ├──> 18 (relations)
  │     │     ├──> 22 (tantivy)
  │     │     └──> 23 (query engine)
  │     └──> 19 (schema DSL)
  │           └──> 20 (pattern + classifier extractors)
  │                 └──> 21 (LLM extractor)
  │                       └──> 23 (query engine)
  └──> 22 (tantivy)
              └──> 23 (query engine)
                    └──> 24 (sweepers + acceptance + release)
```

## How to use these docs

Each phase doc has the same structure:

1. **Goal** — the one-paragraph outcome.
2. **Prerequisites** — what must be true before starting.
3. **Reading list** — required spec sections, in order.
4. **Outputs** — what code, tests, and tags exist at the end.
5. **Sub-tasks** — numbered, sized for one commit each. Each has a "Reads", "Writes", "Done when" checklist, and "Pitfalls" warnings.
6. **Phase exit checklist** — the gate before tagging.
7. **Decisions log** — record non-trivial decisions made during the phase.

In autonomous mode (per [`AUTONOMY.md`](../../AUTONOMY.md)), Claude works through these in order: lowest unfinished sub-task in the lowest unfinished phase. Each sub-task ends with a commit; each phase ends with a tag.

## Substrate-only deployments are real

A deployment that never calls `SCHEMA_UPLOAD` runs as a pure vector substrate — phases 15–24 are dormant on disk and at runtime. This is a first-class deployment posture, not a legacy mode. The schema-optional regression test (sub-task 15.6) is binding for the lifetime of the project: substrate-only behavior must remain identical to the post-Phase-14 baseline.

## When the spec is ambiguous

Each phase doc lists exact spec files in its "Reads" section. If a sub-task can't be completed because the spec is genuinely silent on a point:

1. Re-read the relevant `*_open_questions.md` in the spec directory.
2. Knowledge-layer-specific open questions live in `spec/30_knowledge_open_questions/`.
3. If still unclear, follow the "STOP and surface" protocol in `AUTONOMY.md` §3.

Don't invent. Don't guess.

## Updating these docs

These docs evolve as the project does. If a sub-task's scope changes during work:

- Document the change in the "Decisions log" of the relevant phase doc.
- Don't silently add or remove sub-tasks — that breaks bisect against the roadmap.

If a whole phase needs restructuring, that's a user decision — surface it.
