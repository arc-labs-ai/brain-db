# Phase-9 post-mortem audit

Pre-Phase-10 audit covering: spec alignment, code organization,
and the deferred-work backlog. Findings only — no changes made.

---

## 1. Spec alignment

### 1.1 Critical misalignment (0 — see correction)

- ~~AUTH-phase timeout not enforced~~ — **false positive**. The
  spec-audit subagent missed `crates/brain-server/src/connection.rs:434`
  which arms a `handshake_deadline` at `serve_connection` start,
  fires `ErrorCode::Unauthenticated` on expiry, and is cleared on
  `ConnPhase::Established`. The implementation is correct; only
  the firing path lacks a dedicated test (one test in
  `tests/connection.rs:127` exists for a passing handshake but
  not the timeout case). Not blocking Phase 10.

### 1.2 No other semantic violations

Connection FSM, handshake, error mapping, frame dispatch,
shard orchestration, shutdown discipline — all spec-faithful.
All 19 SD entries in `docs/spec-deviations.md` remain accurate
(none stale, none stealth-reconciled).

---

## 2. Code organization

The user's observation is correct: most crates pile files at
the `src/` root with no sub-module grouping. The crates that
have done it (brain-storage `arena/`+`wal/`, brain-metadata
`tables/`, brain-planner `executor/`+`plan/`, brain-server
`llm/`) are noticeably easier to navigate.

### 2.1 High-value refactors (low risk, big nav win)

| # | Crate | Move | Why now |
| -- | ----- | ---- | ------- |
| **A** | brain-workers | 12 worker files → `src/workers/` | 20 files in root, no grouping; workers are the textbook "one type per file" cluster |
| **B** | brain-ops | `encode.rs writer.rs forget.rs recall.rs plan.rs reason.rs link.rs subscribe.rs txn.rs` → `src/ops/` | 16 files in root mix op handlers with infra (context, dispatch, error) |
| **C** | brain-ops | Split `writer.rs` (1315 LOC) — extract `do_encode`/`do_forget`/`do_link`/`do_unlink` into `src/ops/writer/{encode,forget,link,unlink}.rs` | Single largest non-protocol file in the workspace; handlers are already independent functions |
| **D** | brain-protocol | Group `request.rs` (1026) + `response.rs` (1497) bodies into `src/requests/` + `src/responses/` sub-modules by op family (cognitive / link / txn / admin / subscribe) | Two of the three biggest files in the workspace; the enums stay at root, only the per-variant structs move |

All four are pure-renames + visibility tweaks; no semantic
changes. Each ships as its own commit; verify-after-each.

### 2.2 Medium-value refactors

| # | Crate | Move | Why later |
| -- | ----- | ---- | --------- |
| **E** | brain-server | Split `shard.rs` (975 LOC) — keep `ShardRequest` enum + `shard_main_loop` at root; extract worker-adapter glue into `src/shard_adapters/{rebuild,snapshot,retention,...}.rs` | Cleaner but riskier — touches the Tokio↔Glommio boundary, where bugs are expensive |
| **F** | brain-planner | Op handlers (`encode/recall/forget/reason/path.rs`) → `src/ops/`; analysis (`cost.rs explain.rs`) stays at root | Mirrors brain-ops naming; less urgent because `executor/` + `plan/` already give some structure |
| **G** | brain-index | Extract snapshot I/O out of `hnsw.rs` (1200 LOC) into `src/persistence/{codec,io}.rs` | The split needs private-struct visibility surgery; correctness-critical (CRC, magic, versioning) |

### 2.3 Skip

- **brain-server's 11 root files** — `<concern>.rs` naming is
  appropriate for a multi-concern server; root layout reads fine.
  Refactor E covers the one outlier (`shard.rs`).
- **brain-embed (9 files), brain-core (5), brain-metadata (4)** —
  small enough that sub-moduling adds ceremony without payoff.
- **Cross-crate duplication** — observed twice (shutdown signal
  pattern, metrics-snapshot wiring) but neither is acute enough to
  hoist into brain-core. Watch for a third occurrence before
  extracting.

### 2.4 Naming inconsistencies

None blocking. The brief surveyed every crate and found
consistent intra-crate conventions (verbs in brain-ops, kinds in
brain-workers, concerns in brain-server). The mix is intentional.

---

## 3. Deferred backlog

### 3.1 SDs closable with a one-line spec PR (S, batch)

- SD-2.3-1, SD-2.4-1 — CRC range typos (`[0..36]→[0..40]`,
  `[0..76]→[0..80]`).
- SD-3.5-1 — document the `IdempotencyEntry.request_hash` field.
- SD-4.5-1 — document the three-file HNSW snapshot layout.
- SD-5.1-1 — tighten §04/03 §11 to "safetensors only".

These are spec-text changes, not code changes. The user owns
spec edits, so this is a "queue these up next time we touch the
spec" item, not a Brain-side TODO.

### 3.2 SDs to keep deferred (structurally correct)

SD-2.8-1 (O_DIRECT + WAL pages), SD-2.8-2-b (two-syscall fsync),
SD-4.5-2 (Box::leak on HnswIo), SD-4.8-1 (RwLock vs ArcSwap on
HNSW), SD-5.1-2 (full-file safetensors), SD-10.6-1 (crossbeam-
epoch). All have load-bearing constraints; revisit only if
benchmarks regress.

### 3.3 Phase 9 code-level punts

Only one real TODO landed in code:

- `crates/brain-server/src/shard_adapters.rs:225` — `hnsw.snapshot`
  in the snapshot worker is a no-op. Blocked on
  `HnswIndex::save_snapshot` (Phase 6 hadn't exposed the API
  when 9.12 shipped). Closing this is a brain-index +
  brain-server change (S). Worth doing before Phase 10 starts if
  Phase 10 touches snapshots; otherwise it's fine to carry.

All other 9.x deferrals (multi-frame streaming, SUBSCRIBE WAL
replay, per-IP rate limits, full admin surface, in-flight drain
accounting, signal-handling tests, multi-shard fan-out, crash
recovery E2E) are intentional v2 / Phase-16 scope.

---

## 4. Recommendation

Before Phase 10, in priority order:

1. **Fix the AUTH-phase timeout** (§1.1) — 15 LOC, spec MUST.
2. **Land refactor A** (brain-workers → `workers/`) — sets the
   pattern; lowest-risk.
3. **Land refactor B** (brain-ops → `ops/`) — same template.
4. **Land refactor C** (split `writer.rs`) — pays off the biggest
   file in the workspace.
5. **Land refactor D** (brain-protocol bodies into sub-modules) —
   tackles the other two giants.

Stop here. Refactors E/F/G are nice but not pre-Phase-10
urgent; carry them as a backlog. The HNSW snapshot TODO and
the spec-text SDs can sit until they intersect new work.

Estimated effort for the top 5: ~1 commit each, ~30 min per
commit including verify. Total ~3 hours.

---

*Awaiting user direction on which items to execute.*
