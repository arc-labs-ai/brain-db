# Phase Documentation

Detailed implementation plans for each phase of Brain. The high-level summary lives in [`../../ROADMAP.md`](../../ROADMAP.md); this directory has the per-phase breakdowns.

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
| 9 | Server (end-to-end wire-up) | [`phase-09-server.md`](phase-09-server.md) |
| 10 | Rust SDK & CLI | [`phase-10-sdk-cli.md`](phase-10-sdk-cli.md) |
| 11 | Observability, benchmarks, acceptance | [`phase-11-observability.md`](phase-11-observability.md) |

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

## When the spec is ambiguous

Each phase doc lists exact spec files in its "Reads" section. If a sub-task can't be completed because the spec is genuinely silent on a point:

1. Re-read the relevant `*_open_questions.md` in the spec directory.
2. If still unclear, follow the "STOP and surface" protocol in `AUTONOMY.md` §3.

Don't invent. Don't guess.

## Updating these docs

These docs evolve as the project does. If a sub-task's scope changes during work:

- Document the change in the "Decisions log" of the relevant phase doc.
- Don't silently add or remove sub-tasks — that breaks bisect against the roadmap.

If a whole phase needs restructuring, that's a user decision — surface it.
