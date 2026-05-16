# Plan: Phase 22 — Task 08, Phase exit

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1 (one `chore(...): 22.8 — phase 22 exit` commit)

---

## 1. Scope

Close phase 22. Three threads:

1. **Integration test surface** — one focused test file
   exercising the full read pipeline (ENCODE / STATEMENT_CREATE
   → tantivy commit → `LexicalRetriever::retrieve`) end-to-end
   through the shard spawn path. Mirrors the phase-20 /
   phase-21 phase-exit test pattern.
2. **Criterion benches** against §16/02 §2.9 — three benches in
   `crates/brain-index/benches/lexical_retrieve.rs`: single-term
   memory, multi-term + filter memory, statement scope.
3. **Phase exit metadata**:
   - ROADMAP.md Phase 22 entry rewritten in the phase-20/21
     template (Exit / Scope cut / Delivered / Deferred /
     Bench results).
   - `docs/phases/phase-22-tantivy-lexical.md` checkboxes
     flipped; explicit scope-cut callouts.
   - §27/07 open-questions update for items deferred during 22.x.
   - Tag `phase-22-complete` (annotated).

## 2. Spec references

- `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.9 —
  the perf targets the benches validate.
- `spec/23_retrievers/02_lexical_retriever.md` §8 — pins the
  targets that §2.9 references.
- All phase-22 spec files (`§23/02`, `§26/01`, `§27/02`) are
  already at implementation depth from 22.0.

## 3. External validation

Not applicable — phase exit is purely internal. The plan calls
into the now-stable phase-22 surface; no external libraries
introduced.

## 4. Architecture sketch

### Integration test

`crates/brain-server/tests/knowledge_lexical_phase_exit.rs`
(matching the phase-20 / 21 file naming):

```rust
//! Phase 22 exit smoke. Drives the full lexical pipeline.

#[tokio::test(flavor = "current_thread")]
async fn encode_then_lexical_recall() {
    // 1. spawn_shard via the existing test harness.
    // 2. ENCODE a memory with text "ticket ACME-1247 broke prod".
    // 3. Wait for the indexer to commit (poll-with-timeout on
    //    LexicalRetriever::retrieve until first hit, max 500 ms).
    // 4. Query terms=["acme-1247"] → exactly one hit, id matches.
    // 5. Query terms=["broke"] → one hit, score > 0.
}

#[tokio::test(flavor = "current_thread")]
async fn statement_create_then_lexical_recall() {
    // 1. spawn_shard + upload a minimal schema with `lives_in`.
    // 2. Create an Entity "Alice Wong" via ENTITY_CREATE.
    // 3. STATEMENT_CREATE: subject=Alice, predicate=lives_in,
    //    object=Value(Text("Paris")), kind=Fact, confidence=0.9.
    // 4. Wait for commit (same poll pattern).
    // 5. Query against StatementText scope, terms=["paris"] →
    //    one hit; subject_name filter pushed through; expected
    //    id matches.
}

#[tokio::test(flavor = "current_thread")]
async fn forget_removes_from_lexical() {
    // 1. ENCODE + wait for commit.
    // 2. Retrieve confirms hit.
    // 3. FORGET.
    // 4. Wait for commit + retrieve returns zero hits.
}

#[tokio::test(flavor = "current_thread")]
async fn rebuild_on_restart_recovers_index() {
    // 1. Spawn shard, ENCODE memory, wait for commit, drop shard.
    // 2. Corrupt memory_text.tantivy/meta.json on disk.
    // 3. Spawn again — 22.7 recovery rebuilds.
    // 4. Retrieve returns the original memory.
}
```

These reuse the existing `ShardSpawnConfig` + `spawn_shard`
harness in `brain-server::shard::tests` and drive ops via
the wire-op dispatch path (matching phase-21's integration tests).

### Benches

`crates/brain-index/benches/lexical_retrieve.rs`:

```rust
// criterion_main! with three bench groups against §16/02 §2.9.

fn bench_memory_single_term(c: &mut Criterion) {
    // Setup: build a 10K-doc memory index (smaller than the
    // spec's 100K, but enough to detect regressions; full-scale
    // 100K runs are reserved for the phase-23 hybrid bench).
    // Bench: retrieve.terms=["quick"] over the index.
    // Target check: report p50 against §16/02 §2.9 (10 ms p50).
}

fn bench_memory_multi_term_filter(c: &mut Criterion) {
    // Setup: same 10K corpus.
    // Bench: retrieve.terms=["quick", "brown"] +
    //   filters.created_at_ms=Some(range).
    // Target: §2.9 (15 ms p50).
}

fn bench_statement_single_term(c: &mut Criterion) {
    // Setup: 10K statements with synthetic entity + predicate names.
    // Bench: terms=["paris"] over StatementText scope.
    // Target: §2.9 (10 ms p50).
}
```

10K (not 100K / 1M) is the bench scale; spec targets are stated
at production scale but the per-query work is dominated by
posting-list lookups, which scale logarithmically. The bench is a
regression detector, not a production validation — the latter
lands as part of phase 14's acceptance suite (`§16/02 §3` phase
gate table notes this).

### ROADMAP entry

Follows the phase-20 / phase-21 template. Sections:
- One-line.
- Detailed plan link.
- Crates touched.
- Sub-tasks count + Exit.
- Scope cuts.
- Delivered (bullet per crate / capability).
- Deferred (each item links to its open-question §).
- Bench results (filled in from 22.8's criterion run, or marked
  "captured in phase-14 acceptance" if we follow 21.7's pattern
  of deferring the wall-time numbers).

### Phase-doc updates

`docs/phases/phase-22-tantivy-lexical.md`:
- Status: ✓ tag `phase-22-complete`.
- Sub-tasks 22.1–22.8 each flip `[ ]` → `[x]` with a one-line
  "Landed in: `.claude/plans/phase-22-task-0N.md`" link.
- Done-when bullets ticked; phase-exit section added.
- Scope cuts called out (matching ROADMAP).

### §27/07 open-questions

Add entries for items deferred during 22.x:
- Q9 — partial WAL replay on shard recovery (deferred from 22.7).
- Q10 — hot rebuild while the live writer is running (deferred from 22.6).
- Q11 — segment-merge windowing during low-traffic windows
  (deferred from 22.0 + 22.3/4).
- Q12 — `ADMIN_TANTIVY_REBUILD` wire op (deferred to §28/05).

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| 10K-scale benches (this plan) | Fast bench run; catches regressions; matches 21.7's "informational" stance | Doesn't prove §2.9 targets at production scale | ✓ — phase 14 acceptance covers production scale |
| 100K-scale benches | Validates spec targets directly | Bench corpus build is ~30 s; CI cost; tantivy 100K is heavyweight | rejected for v1 phase exit |
| One mega-bench file with all three scopes | Single criterion run | Hard to interpret; benches drift apart over time | rejected |
| Defer ROADMAP rewrite to v1.0 cut | Less churn now | Phase-22 entry becomes stale; harder to track scope cuts | rejected |
| Single commit for everything (this plan) | Atomic phase exit | Touches ~6 files | ✓ — same shape as 21.7 |

## 6. Risks / open questions

- **Risk:** Integration tests are flaky because the indexer commits asynchronously. **Mitigation:** wait with a bounded poll-and-sleep loop (max 500 ms, 25 ms intervals), failing the test if the hit doesn't appear. Same pattern phase-20's classifier integration test uses.
- **Risk:** Bench corpus generation is slow on CI. **Mitigation:** 10K @ ~50 µs per add = ~500 ms setup; criterion's `iter_batched` with a pre-built corpus pulls it out of the timed loop.
- **Open question:** Should phase-22 lift the §22.8 perf gate to a CI-blocking assertion (`p50 < 10 ms or fail`)? **Resolution:** report-only in v1 (matching 21.7); phase 14's acceptance suite is the gate. The bench file emits numbers in ROADMAP's "Bench results" section after the user runs them.

## 7. Test plan

The four integration tests in §4. Plus:

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- `cargo clippy --workspace --tests -- -D warnings` clean
  (restricted to the touched crates if pre-existing pedantic
  warnings elsewhere).
- `cargo bench -p brain-index --bench lexical_retrieve -- --quick`
  runs green; results captured in ROADMAP.

Per the 21.7 precedent, **bench wall-time capture is optional**
at the tag — the harness lives in the tree so phase 23 can hit
it on day one. If the user wants concrete numbers in ROADMAP
before tagging, they can run the bench and paste; otherwise the
"§2.9 targets validated in phase 14" line stays.

## 8. Commit shape

Single commit:

```
chore(server,index,docs): 22.8 — phase 22 exit (tests + bench + ROADMAP + tag)

Closes phase 22.

- crates/brain-server/tests/knowledge_lexical_phase_exit.rs
  (new): 4 integration tests driving the full lexical pipeline
  via the wire-op dispatcher.
- crates/brain-index/benches/lexical_retrieve.rs (new): three
  criterion benches against §16/02 §2.9.
- crates/brain-index/Cargo.toml: `[[bench]] name =
  "lexical_retrieve" harness = false`.
- ROADMAP.md: Phase 22 entry rewritten in the phase-20 / phase-21
  template (Exit / Scope cut / Delivered / Deferred / Bench).
- docs/phases/phase-22-tantivy-lexical.md: checkboxes flipped;
  phase-exit section added.
- spec/27_knowledge_workers/07_open_questions.md: Q9–Q12 for
  partial WAL replay, hot rebuild, segment-merge windowing,
  admin rebuild wire op.
```

Plus an annotated tag:

```
git tag -a phase-22-complete -m "Phase 22 — Tantivy / lexical retrieval: \
  TantivyShard + brain analyzer + MemoryTextIndexer + \
  StatementTextIndexer + LexicalRetriever + atomic-swap rebuild + \
  startup recovery."
```

## 9. Confirmation

Please confirm:

1. **Four integration tests** are the right surface (vs. more thorough wire-op coverage). Each targets one scenario.
2. **10K-scale benches** (vs. 100K / 1M) — regression detection in CI, with production validation deferred to phase 14.
3. **Bench wall-time capture optional at tag** — same call as 21.7. Numbers can land later in ROADMAP if you want them before tagging.
4. **Single commit** for the phase exit, matching 21.7's shape.
5. **Tag `phase-22-complete` cut after the commit lands** (annotated).

After approval: implement → verify (workspace zigbuild + targeted clippy) → commit → tag.
