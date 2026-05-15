# Spec ↔ implementation audit

Final pre-release pass. For each spec section, every MUST clause is
tracked to its implementation evidence and classified
**matched / deferred / deviation / drift**.

- **matched** — impl honors the clause.
- **deferred** — clause not yet implemented; carries a
  `phase-NN/<slug>` tracker inline in the source.
- **deviation** — impl differs from the spec; has an SD entry in
  [`../spec-deviations.md`](../spec-deviations.md).
- **drift** — impl differs and there's *no* tracker / SD entry.
  These are bugs; **a clean release has zero drift**.

The plan + methodology lives in
[`.claude/plans/phase-15-spec-audit.md`](../../.claude/plans/phase-15-spec-audit.md).

## Status

| § | Section | Files | MUSTs (~) | Tier | Audit | Drift |
|---|---|---|---|---|---|---|
| 00 | master overview | 5 | 9 | C | pending | — |
| 01 | system architecture | 11 | 35 | C | pending | — |
| 02 | data model | 11 | 26 | C | pending | — |
| 03 | wire protocol | 12 | 77 | **A** | [s03-wire-protocol.md](s03-wire-protocol.md) | 0 |
| 04 | embedding layer | 11 | 16 | C | pending | — |
| 05 | storage / arena / WAL | 12 | 42 | **A** | [s05-storage.md](s05-storage.md) | 0 |
| 06 | ANN index (HNSW) | 11 | 19 | C | pending | — |
| 07 | metadata + graph (redb) | 12 | 13 | C | pending | — |
| 08 | query planner | 12 | 10 | C | pending | — |
| 09 | cognitive operations | 16 | 17 | C | pending | — |
| 10 | concurrency + epochs | 11 | 25 | C | pending | — |
| 11 | background workers | 17 | 5 | C | pending | — |
| 12 | sharding + clustering | 11 | 10 | C | pending | — |
| 13 | SDK design | 12 | 11 | C | pending | — |
| 14 | observability + ops | 12 | 8 | **A** | [s14-observability.md](s14-observability.md) | 0 |
| 15 | failure recovery | 12 | 11 | C | pending | — |
| 16 | benchmarks + acceptance | 12 | 47 | C | pending | — |
| — | overall | 218 | ~381 | — | 3 of 17 sections | 0 / audited |

**MUSTs (~)** is a heuristic — `grep -i 'must\|required\|always\|never'`
over the section. Some hits are non-normative ("you must understand
X" prose); the per-section audit pages filter visually.

## Tiers

- **Tier A** — full audit in this loop. Chosen for being either
  the v1.0 stability surface (§03), the durability-critical core
  (§05), or the most-recently-shipped piece (§14).
- **Tier B** — light scan + worklist for operator drive-by.
  Currently empty; promote Tier-C sections here as priorities
  emerge.
- **Tier C** — pending. See [`pending.md`](pending.md) — every
  remaining section has its owning crate, MUST count, audit
  priority (P1/P2/P3), and a recommended-order list.

## Fix plan

[`fix-plan.md`](fix-plan.md) triages every audit finding into
**must-fix v1.0 / should-fix v1.x / defer to v2 / spec-side /
closed**, with scope, files touched, effort estimate, and a
sequencing recommendation.

**Headline: zero substrate-v0.x blockers remaining.** F-1 closed
via operator-run `sed`; F-2 / F-3 / F-7 / F-13 closed in commit
`8b78de1`. The substrate is at `v0.9.x-substrate-rc` candidacy
(Phase 14). The `v1.0.0` tag is now deferred to **Phase 24**,
after the knowledge-layer phases (15–24) deliver entities,
statements, relations, schema DSL, three-tier extractors, and
hybrid retrieval. Knowledge-layer §17–§31 will be audited
post-implementation (see [`pending.md`](pending.md)). The
remaining open substrate findings are v1.x tightenings + v2
wire amendments.

## Cross-references

- [`docs/spec-deviations.md`](../spec-deviations.md) — 19 conscious
  deviations recorded during Phases 2-4. The audit pages cite SD
  IDs verbatim.
- [`crates/brain-server/src/metrics/mod.rs`](../../crates/brain-server/src/metrics/mod.rs)
  — inline deferred-set with `phase-NN/<slug>` trackers for the
  observability surface.
- Per-phase docs in [`docs/phases/`](../phases/) — each phase's
  exit checklist is the per-phase audit; this directory is the
  spec-section-axis rollup.

## How to add an audit

1. Read the spec section end-to-end.
2. Grep / visually extract every MUST clause.
3. For each clause: find the impl evidence (`<crate>/<file>:<lines>`
   or a fn name) and classify status.
4. Write `s<NN>-<name>.md` per the template (see existing Tier-A
   pages).
5. Update the status table above (Tier, Audit link, Drift count).
6. If you find drift: open an SD entry or a tracker. Don't silently
   resolve.
