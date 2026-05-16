# 18.1 — §20 backfill + bundled spec edits

Doc-only sub-task. Brings `spec/20_relations/` from a 1-file stub
(~7 KB) to §03-substrate depth (8 files), mirroring 17.1's §19
backfill.

## Spec refs

- `spec/20_relations/00_purpose.md` — live; touched only for cross-refs.
- `spec/19_statements/` — structural template (especially §19/01
  supersession, §19/03 storage, §19/05 evidence).
- `spec/18_entities/` — entity-side merge precedent for the "merge
  re-routing" question.
- `spec/28_knowledge_wire_protocol/07_relation_frames.md` —
  already §03-depth; cross-link target.
- `spec/26_knowledge_storage/00_purpose.md` — relation tables in
  the storage catalog.

## Files written

| Path | Purpose |
|---|---|
| `spec/20_relations/01_cardinality.md` | Cardinality variants + supersession rules per cardinality. |
| `spec/20_relations/02_symmetric.md` | Canonical from/to ordering + dual-index reads + dedup. |
| `spec/20_relations/03_storage.md` | redb table layout matching 15.1 scaffolding. |
| `spec/20_relations/04_traversal.md` | BFS algorithm, depth cap, cycle detection, branching factor. |
| `spec/20_relations/05_evidence.md` | Flat Vec<MemoryId>; FORGET cascade; no overflow in v1. |
| `spec/20_relations/06_open_questions.md` | Q1–Q8 (overflow evidence, deep traversal, cross-shard, merge re-route, etc.). |
| `spec/20_relations/07_references.md` | Cross-links to §17, §18, §19, §25, §26, §28/07. |

Bundled edits:

- `spec/16_benchmarks_acceptance/02_latency_targets.md` — add
  §2.4 (relation-layer latency targets at 1M relations / shard) +
  flip §2.5 to include phase-18 perf gate.
- `spec/29_knowledge_sdk/00_purpose.md` — flip phase-scope row 18.x
  to "this phase".

## Plan

Write the 7 new §20 files in one pass (template from §19 file
structure; relation-specific content per §20/00 + §28/07). Then
the two bundled edits. Single commit.

## Verification

```
git diff --stat spec/  # confirm 7 new + 2 modified
# No code changes; nothing to build.
```

## Commit message draft

```
docs(spec): §20 backfill to §03-depth + perf targets + SDK scope (18.1)

Brings spec/20_relations/ from a 1-file stub to 8 files matching
§19's depth:
- 01_cardinality.md   — variants + supersession rules per cardinality.
- 02_symmetric.md     — canonical from/to ordering + dual-index reads.
- 03_storage.md       — redb table layout (matches 15.1 scaffolding).
- 04_traversal.md     — BFS, depth cap, cycle detection, branching.
- 05_evidence.md      — flat Vec<MemoryId>; FORGET cascade.
- 06_open_questions.md — overflow evidence, deep traversal, cross-shard,
                          merge re-route deferrals.
- 07_references.md    — cross-links to §17, §18, §19, §25, §26, §28/07.

Bundled spec edits:
- §16/02 §2.4 — relation-layer perf targets at 1M relations/shard
  (CREATE 3ms/15ms, GET 0.5ms/2ms, LIST_FROM 2ms/10ms,
  TRAVERSE depth-1 5ms/25ms, depth-2 15ms/50ms, depth-3 30ms/100ms).
- §16/02 §2.5 — phase-18 perf gate added (sub-task 18.9).
- §29/00 phase-scope — 18.x flipped from "later" to "this phase".

Plan: .claude/plans/phase-18-task-01.md.
```

## Risks

- **Spec drift between §20 and §28/07.** §28/07 is the authoritative
  wire shape; §20 cross-links rather than redefines. Easy to
  duplicate field lists; the backfill keeps wire shapes in §28 only.
- **Cardinality nuances around symmetric** — symmetric many-to-many
  is the common case; symmetric one-to-one is allowed (e.g.,
  "married_to"). §20/01 + §20/02 split the concerns: §20/01 is
  per-cardinality supersession, §20/02 is per-relation-type storage.

## Out of scope

- Code (18.2+).
- §28/07 changes (already at §03-depth).
- ROADMAP update (lands in 18.9b at phase exit).
