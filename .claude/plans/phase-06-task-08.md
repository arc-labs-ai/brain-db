# Sub-task 6.8 — Plan inspection (EXPLAIN-style pretty-printer)

The visible-side polish for Phase 6. Renders an `ExecutionPlan` as a SQL-`EXPLAIN`-style tree so operators (and tests) can see the planner's choices at a glance. No logic, no I/O, pure formatting.

Phase doc says "`impl Debug for Plan` with a tree pretty-printer". Implementing `Debug` for the pretty-tree would overwrite the existing derive-generated `Debug`, which we still use for test diagnostics (`println!("{plan:?}")` in failing assertions). The right idiom is `Display` — `{}` prints the tree, `{:?}` keeps the derive output. Phase doc divergence documented.

Spec §01 §8: "the plan is an immutable value passed from planner to executor. Lets the executor log the plan for observability." Spec §01 §15 + §03 §15: future `ADMIN_EXPLAIN_PLAN` admin opcode (Phase 9) wraps this.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/01 §8 | Plans are values; tests + observability log them |
| §08/01 §15 | Operators query log entries to debug latency anomalies |
| §08/03 §15 | `ADMIN_EXPLAIN_PLAN` returns the plan without executing |
| §08/05 §15 | Same for PLAN / REASON |
| Phase doc 6.8 | "Plans round-trip through Debug readably; useful for diagnostics" |
| Orientation plan §4.12 | ASCII box-drawing for readability |

## 1. Scope

**In scope for 6.8:**
- `crates/brain-planner/src/explain.rs` — new module with the formatting logic.
- `impl Display for ExecutionPlan` + per-plan-variant `Display` impls (RecallPlan, EncodePlan, ForgetPlan, PathPlan, ReasonPlan).
- Helper free functions: `explain(plan: &ExecutionPlan) -> String` (convenience wrapper around `format!("{plan}")`).
- Tree drawing using `├─ │ └─` box-drawing chars per orientation plan §4.12.
- Compact value rendering: text fields show first ~40 chars + ellipsis; vectors / large arrays summarised (e.g. `vector: [f32; 384]`); booleans inline.
- Doc-tests showing one example per plan variant (for crate docs).
- Unit tests: assert key substrings appear in the rendered output for each variant.

**NOT in scope:**
- The `ADMIN_EXPLAIN_PLAN` opcode itself — Phase 9 server territory.
- A parser for the EXPLAIN output (the format is human-only).
- Per-step cost annotations beyond what's already in the plan struct (the `estimated_cost_ms` field is rendered).
- Color / ANSI escape codes — operators may pipe through `less`; keep it plain.

## 2. Module surface

```rust
// crates/brain-planner/src/explain.rs

use std::fmt;

use crate::plan::ExecutionPlan;

impl fmt::Display for ExecutionPlan { /* delegates to per-variant impl */ }

impl fmt::Display for RecallPlan { /* tree */ }
impl fmt::Display for EncodePlan { /* tree */ }
impl fmt::Display for ForgetPlan { /* tree */ }
impl fmt::Display for PathPlan   { /* tree */ }
impl fmt::Display for ReasonPlan { /* tree */ }

/// Convenience: render a plan as a string.
pub fn explain(plan: &ExecutionPlan) -> String {
    format!("{plan}")
}
```

Re-export `explain` from `lib.rs`.

## 3. Output examples

### 3.1 Recall

```
RecallPlan  (est. 7.51 ms)
├─ embedding: "the cat sat on the mat" (cache_lookup=true)
├─ shards (1)
│  └─ ShardSearchStep shard=0
│     ├─ ann_search: ef=64, candidates=80
│     ├─ metadata_lookup: include_extra=false
│     └─ filter_apply: PostFilter, 0 rules
├─ merge: sort_by=Score, final_top=10
├─ text_fetch: None
└─ response: include_text=false, include_metadata=false
```

### 3.2 Encode

```
EncodePlan shard=0  (est. 9.86 ms)
├─ idempotency_check: request_id=01010101…
├─ embedding: "hello" (cache_lookup=true)
├─ context_resolution: Explicit(ContextId(42))
├─ allocation: arena_grow_if_needed=true
├─ wal_append: kind=Episodic, salience_initial=0.50, fsync=true
├─ apply: arena_write=true, metadata_write=true, hnsw_insert=true
├─ edges (0)
└─ response: persistent_id=true
```

### 3.3 Forget

```
ForgetPlan shard=0  (est. 0.80 ms)
├─ memory_id=00000000000000000000000000000007 mode=Soft
├─ idempotency_check: request_id=01010101…
├─ wal_append: fsync=true, mode=Soft
├─ apply: arena_tombstone=true, metadata_commit=true, hnsw_mark_removed=true
└─ response: include_outcome=true
```

### 3.4 Path

```
PathPlan strategy=Auto  (est. 24.30 ms)
├─ start: ByText("origin")
├─ goal:  ByText("destination")
├─ budget: max_steps=4, max_branches=64, wall=100 ms
├─ starting_recall: Some
├─ goal_recall:    Some
├─ traversal: max_depth=4, max_paths=64, kinds=[Caused, FollowedBy]
├─ scoring: length + edge_weight + salience, top_n=10
└─ response: paths=true, text=false, metadata=false
```

### 3.5 Reason

```
ReasonPlan depth=3 max_inferences=5  (est. 12.40 ms)
├─ observation: ByText("the cat sat")
├─ embedding:  Some
├─ base_recall: Some
├─ supports_traversal: max_depth=3, kinds=[Supports, DerivedFrom]
├─ contradicts_traversal: max_depth=3, kinds=[Contradicts]
├─ aggregation: max_supporting=5, max_contradicting=5, aggregate=true
└─ response: paths=true, text=false, metadata=false
```

Operators read these top-to-bottom; the tree characters carry depth.

## 4. Implementation decisions

### 4.1 `Display` vs `Debug`

Phase doc says "Debug". We pick `Display`:
- Existing `#[derive(Debug)]` on plan types gives `RecallPlan { embedding: EmbeddingStep { ... }, … }` which is fine for `assert!(format!("{plan:?}").contains(...))` panic messages.
- Overriding `Debug` would lose that, force every test to switch.
- `Display` is the idiomatic home for human-readable formatting in Rust.

Document the divergence in the commit message.

### 4.2 Tree characters

ASCII-only would be `+--`, `|`, etc. Box-drawing chars (`├─ │ └─ ─`) read better and have been standard since Unicode was invented. Most terminals + docs render them. Decision: box-drawing chars throughout.

### 4.3 Per-level indentation

Two-space indent per level + the appropriate tree char:
```
RecallPlan
├─ first child
│  └─ second child of first child
└─ last child
```

The vertical bar `│` connects siblings at the parent's indent level until the last sibling, which uses `└─`.

### 4.4 Long-value handling

- Text fields: render literally if ≤ 40 chars, else `"<first 37 chars>..."`.
- Vectors (`[f32; 384]`): never inline. Show shape only — `[f32; 384]`.
- Byte arrays (`[u8; 16]` — fingerprint, request_id): hex first 8 bytes + ellipsis: `01020304… `.
- `MemoryId`: 32-hex (the full big-endian form).
- `EdgeKind` / `MemoryKind` / similar enums: their `Debug` derive output (`Episodic`, `Caused`, …).
- `Option<T>`: `None` literal, or `Some(<T's render>)` — but for compactness, top-level optional fields show `Some` / `None` only; the executor doesn't need the body in EXPLAIN.

### 4.5 `RecallPlan::shards` rendering

In v1 the vec is always length 1 (orientation §4.7). The renderer shows `shards (N)` with N = `len()`. When Phase 12 lights up cross-shard, the same code emits multiple `ShardSearchStep` children.

### 4.6 Cost in the title line

Each top-level plan title includes `(est. X.YZ ms)` so operators see the cost without scanning. Helper: `format_cost(ms: f32) -> String` with 2 decimal places.

### 4.7 No newline at end of output

`format!("{plan}")` returns without a trailing `\n`. Callers add one (`println!`) or not (`assert!`).

### 4.8 Helper for tree-building

Implementing this longhand for each plan type would be repetitive. We write one helper:

```rust
fn tree_line(f: &mut fmt::Formatter, indent: &str, is_last: bool, body: &str) -> fmt::Result {
    let connector = if is_last { "└─ " } else { "├─ " };
    writeln!(f, "{indent}{connector}{body}")
}
```

…and use it from each `Display::fmt`. Per-step rendering is one-line; the helper keeps the formatting consistent.

### 4.9 Tests

`tests/explain.rs` integration tests + unit tests inside `explain.rs` are both viable. Unit tests live closer to the code; they go in `explain.rs`. Each test:

- Constructs a plan (using the same helpers we use in planner-side tests).
- Calls `format!("{plan}")`.
- Asserts key substrings:
  - The plan-type header.
  - The `est. …` cost.
  - Step names.
  - Box-drawing chars (`├─` or `└─`).

Snapshot tests with `insta` would be ideal but add a workspace dep — not worth it. Substring asserts cover the regression-detection use case.

## 5. Files written / changed

```
crates/brain-planner/src/explain.rs                     [new]
crates/brain-planner/src/lib.rs                         [edit: pub mod explain; pub use explain::explain]
```

No new external deps. No `Cargo.toml` change.

## 6. Verify checklist

- `cargo build -p brain-planner` clean (dev container).
- `cargo test -p brain-planner` — 93 existing + ~5 new explain unit tests.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 7. Commit message (draft)

```
feat(brain-planner): Plan EXPLAIN pretty-printer (sub-task 6.8)

The visible-side polish for Phase 6. Renders an ExecutionPlan as a
SQL-EXPLAIN-style tree using ASCII box-drawing chars. Operators see
the planner's choices at a glance; Phase 9's ADMIN_EXPLAIN_PLAN
opcode (when it lands) just wraps `format!("{plan}")`.

Implementation:
- impl Display for ExecutionPlan + per-variant impls on RecallPlan,
  EncodePlan, ForgetPlan, PathPlan, ReasonPlan.
- Free function `explain(plan: &ExecutionPlan) -> String` as a
  convenience.
- Tree uses ├─, │, └─ characters. Two-space indent per level.
- Long text is truncated to first 37 chars + "...".
- Vectors are shape-summarised; byte arrays show first 8 hex bytes.
- Title line for each plan carries `(est. X.YZ ms)` so cost is
  visible without scanning.

Phase doc said "impl Debug for Plan"; we pick Display because the
derive-generated Debug is still used in test panic messages
(`assert!(format!("{plan:?}").contains(...))`). Overriding it
would break that. Display is the idiomatic home for human formatting
in Rust. Documented divergence; commit body for posterity.

Tests: ~5 unit tests assert key substrings (header, cost, step
names, box-drawing chars) appear in the rendered output for each
variant. Snapshot testing (insta) avoided — substring asserts cover
the regression-detection need without adding a workspace dep.

No new external deps. Total 98 tests passing in dev container.
```

## 8. Risks

- **Unicode rendering on weird terminals.** Box-drawing chars are well-supported (since the 1990s). Edge case: Windows command-prompt without UTF-8. Operators on Windows use the dev container or PowerShell; not our concern.
- **Long text in `EmbeddingStep`.** The cue might be 1 KB. We truncate at 40 chars; tests pin the truncation behaviour.
- **`Display` recursion**: A `Display` impl could call itself if we wired it wrong. We use field-level rendering, not `impl Display for SubStep` types directly — so no recursion.

## 9. Out-of-scope flags

- No JSON / parseable format.
- No diff between two plans.
- No execution overlay (e.g. "this step took X ms, est. was Y ms" — that's Phase 11 observability).
- No EdgeStep / Edge weight rendering beyond a count in `EncodePlan` (the per-edge details aren't useful for operators reading EXPLAIN).

---

PLAN READY.
