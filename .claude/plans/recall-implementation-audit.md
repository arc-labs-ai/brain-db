# Recall implementation — audit fixes

**Status**: in flight
**Scope**: substrate + hybrid recall path, the SDK + CLI surface, the
renderer's score and cluster-warning logic, the help card
**Goal**: every flag the help card lists actually works; every flag
that works appears on the card; scores carry consistent semantics on
both substrate and hybrid paths.

---

## 1 · Findings (from the L5 → M0 audit)

| # | Issue | File:line | Severity |
|---|---|---|---|
| 1 | `RecallResp.similarity_score` is overwritten with `fused_score` on the hybrid path. Help says they're distinct. | `crates/brain-ops/src/ops/recall.rs:432-433` | 🔴 |
| 2 | Cluster warning runs client-side and reads `similarity_score`, so it misfires on the hybrid path. | `crates/brain-explore/src/render/memory.rs:88-106` | 🟡 |
| 3 | `--include-graph` accepts the flag, returns empty enrichment, log-warns. | `crates/brain-shell/src/commands/recall.rs:57-69` | 🟡 |
| 4 | SDK `salience_floor()` builder exists; no CLI flag. | `crates/brain-sdk-rust/src/ops/recall.rs:78-80` | 🟡 |
| 5 | SDK accepts `age_bound_unix_nanos`; no builder, no CLI flag. | `crates/brain-sdk-rust/src/ops/recall.rs:24` | 🔴 (dead) |
| 6 | `include_edges` / `include_vectors` builders exist; server stubs to None. | `crates/brain-sdk-rust/src/ops/recall.rs:84-93` | 🟡 |
| 7 | `--confidence 0.0` returns everything; help doesn't explain. | `crates/brain-ops/src/ops/recall.rs:106-108` | 🟢 |

## 2 · Resolved questions

- **`--include-graph`**: hide from clap entirely until the wire field
  lands. The renderer's empty fallback stays — it'll auto-light-up
  when the wire field exists.
- **`--include-edges` / `--include-vectors`**: not exposing. Stubs
  stay in the SDK for the typed-SDK consumer; CLI side gets nothing.

## 3 · Commit plan

| # | Commit | LoC | Touches |
|---|---|---|---|
| **M1** | Fix score conflation: hybrid path keeps `similarity_score` as the top-contributing-retriever's raw score; `fused_score` is its own field (already exists); response stops conflating them. | ~80 | ops/recall.rs, response types if needed |
| **M2** | Add `--salience-floor F` and `--max-age <duration>` to `RecallArgs`. Thread through `commands::recall::run` → SDK builder → wire request. Update `help_recall()` fixture. | ~80 | parser/command.rs, commands/recall.rs, sdk/recall.rs (builder for age_bound), repl/help.rs |
| **M3** | Hide `--include-graph` from clap with `hide = true` until the wire field lands. Update `help_recall()` fixture to drop the row. The renderer's empty-fallback path stays untouched. | ~30 | parser/command.rs, repl/help.rs |
| **M4** | Cluster-warning correctness: the heuristic now reads the right score post-M1; add a `retrievers=N/2` footer column on hybrid hits so single-retriever results (the "Alice merged…" failure you saw) are visible at-a-glance. Update help notes to call out that cluster-warning is a client-side hint. | ~60 | render/memory.rs, repl/help.rs |
| **M5** | Drift tests + verify gate. Extend `help_drift.rs` to cover the new flags. `cargo fmt && cargo clippy && cargo test` on brain-shell, brain-explore, brain-ops, brain-sdk-rust. | ~30 | tests/help_drift.rs |

Total ~280 LoC, single branch.

## 4 · Done when

- `brain recall "lorem"` on the two-memory store hits "Alice merged…"
  shows `retrievers=1/2` so it's obvious only one retriever ranked it.
- `brain recall "lorem" --confidence 0.025` filters out the
  single-retriever match cleanly on the hybrid path.
- `--salience-floor` and `--max-age` flags work end-to-end (CLI →
  server filter applied).
- `--include-graph` no longer appears in `recall --help` or
  `help recall`.
- All seven L-tier drift tests + new ones pass.
- `cargo clippy -D warnings` clean across all four touched crates.

## 5 · Risks / pushback

- **Wire-protocol semantics change.** M1 changes what
  `similarity_score` means on the hybrid path. The protocol is at
  V2 today (we bumped during the strategy-collapse work). A future
  consumer reading `similarity_score` expecting fused-score values
  would break. Mitigation: the responses already carry `fused_score`
  as a separate field — anyone using it correctly is already
  reading the right thing.
- **`--max-age <duration>` parsing.** clap's `humantime` integration
  is the cleanest way; falls back to "seconds as u64" if humantime
  feels heavy. Either is fine.
- **Cluster warning false-positive rate.** On substrate with N=2
  near-duplicate memories the warning is genuine; on hybrid with
  N=2 from different retrievers it currently fires wrong. M4 fixes
  the latter at the source.
