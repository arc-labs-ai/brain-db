# Phase 14 — Acceptance & v1.0.0 Release

## Goal

Run every acceptance gate, validate every runbook, finish the docs
pass, and cut `v1.0.0`. Everything before this phase produced
components; this phase declares them, together, production-ready.

## Prerequisites

- [ ] Phase 13 complete (`phase-13-complete` tag). The acceptance suite
      consumes Phase 13's benchmark baselines and chaos results.

## Reading list

1. [`spec/16_benchmarks_acceptance/08_acceptance_test_suite.md`](../../spec/16_benchmarks_acceptance/08_acceptance_test_suite.md) — the 10 gates.
2. [`spec/14_observability_ops/07_runbooks.md`](../../spec/14_observability_ops/07_runbooks.md) — runbooks to validate.

## Outputs

- `acceptance/run.sh` — single entry point that runs gates 1-10 and
  reports pass/fail.
- `docs/runbooks/*.md` — one per runbook in spec §14/07, each tested
  against a chaos scenario produced in Phase 13.
- README + getting-started overhaul; operator guide covers install,
  config, monitor, recover.
- `CHANGELOG.md` covering v0.1 → v1.0.0.
- Tags: `phase-14-complete`, `v1.0.0`.

## Sub-tasks

### Task 14.1 — Acceptance suite runner
**Reads:** spec §16/08.
**Writes:** `acceptance/run.sh` + per-gate test files.
**Done when:** `bash acceptance/run.sh` exits 0 on the reference
environment; output is a clear pass/fail per gate.

### Task 14.2 — Runbook validation
**Reads:** spec §14/07.
**Writes:** `docs/runbooks/{disk-full,wal-corruption,shard-down,hnsw-rebuild,oom,latency-spike,etc}.md`.
**Done when:** each runbook is a working procedure executed against the
corresponding Phase 13 chaos scenario; recovery time recorded.

### Task 14.3 — Documentation pass
**Writes:** README rewrite, `docs/guides/{install,configure,operate,upgrade}.md`, `cargo doc` cleanup.
**Done when:** `cargo doc --workspace` is warning-free; every public
API in the SDK has at least one example; getting-started works from a
clean machine in under 15 minutes.

### Task 14.4 — Release notes
**Writes:** `CHANGELOG.md` covering every feature shipped Phase 1 → Phase 13; `RELEASE-NOTES-v1.0.0.md`; known-limitations section pulled from `spec/*/open_questions.md`.
**Done when:** changelog references every tagged phase; release notes
are written for an operator (not a developer) audience.

### Task 14.5 — Release cut
**Writes:** version bumps across all `Cargo.toml`s; final `v1.0.0` tag.
**Done when:** all 10 acceptance gates green, soak result attached,
release notes published, tag pushed, README points at the release.

The release-cut procedure (run by the release manager on a clean
checkout, after the 48 h soak passes):

```bash
# 1. Run the full acceptance suite. Expected: 10/10 PASS.
bash acceptance/run.sh
cat acceptance/last-run.jsonl    # archive this with the release.

# 2. Confirm soak result exists.
ls docs/performance/soak-*.md

# 3. Bump workspace version: 0.1.0 → 1.0.0.
sed -i 's/^version = "0.1.0"$/version = "1.0.0"/' Cargo.toml
cargo update --workspace          # refresh Cargo.lock with new version
just docker-verify                # final green check

# 4. Commit + tag.
git add Cargo.toml Cargo.lock
git commit -m "release: v1.0.0"
git tag -a phase-13-complete -m "Phase 13 — soak + chaos gates green"
git tag -a phase-14-complete -m "Phase 14 — acceptance gates 1-10 green"
git tag -a v1.0.0 -s -m "Brain v1.0.0 — see RELEASE-NOTES-v1.0.0.md"

# 5. Push.
git push origin main dev phase-13-complete phase-14-complete v1.0.0

# 6. Update README "implementation status" table to mark v1.0.0
#    shipped. Commit + push that as a follow-up.
```

## Phase exit checklist

- [x] Sub-tasks 14.1–14.4 scaffolded.
- [x] CHANGELOG + RELEASE-NOTES-v1.0.0 written.
- [x] Acceptance runner + per-gate tests in place.
- [x] 10 runbooks + 4 operator guides shipped.
- [ ] Operator runs `bash acceptance/run.sh`; gates 1-10 green on
      reference infra (gates 5, 7, 8, 10 need full operator runs
      vs the CI smoke-checks).
- [ ] Operator runs the 48 h soak; result file lands in
      `docs/performance/soak-<date>.md`.
- [ ] Every runbook executed once against a chaos scenario
      (validation matrix in `docs/runbooks/README.md`).
- [ ] `cargo doc --workspace` clean (pre-existing brain-http link
      warnings are not blockers — they're inside doc comments only).
- [ ] Version bump in `Cargo.toml`: `0.1.0` → `1.0.0`.
- [ ] Tags `phase-13-complete`, `phase-14-complete`, `v1.0.0`.

## Notes

This is the last phase. Treat it as a checklist, not a feature stream —
the goal is "everything we already built works together", not "build
more". Resist the urge to fix-by-feature in this phase. Real fixes go
in a follow-up minor release.

The release-cut commit itself is intentionally one-liner small. All
the heavy lifting — runbooks, guides, acceptance runner, release
notes — landed in 14.1–14.4 so the final commit is just "bump
version + tag".
