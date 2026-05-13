# Sub-task 9.2 — Tokio/Glommio audit + port shim

**Reads:** every shard-bound crate's source (`brain-core`, `brain-protocol`, `brain-storage`, `brain-metadata`, `brain-index`, `brain-embed`, `brain-planner`, `brain-ops`, `brain-workers`), spec §01/04, spec §10/02.
**Phase doc:** `docs/phases/phase-09-server.md` — this is the **9.2 entry** per the orientation's re-numbering (the doc's old 9.2 "Shard executor" became orientation's 9.4).
**Done when:** A checked-in audit doc enumerates every tokio API used in shard-bound code with a per-use-site disposition (stays / port / move-to-connection-layer / open-question). Any non-trivial port decisions are escalated to the user before code lands.

---

## 1. Why this sub-task exists

Phase 9 introduces Glommio. Glommio's `LocalExecutor` is **single-threaded, `!Send`, and doesn't drive tokio reactors**. Anything in shard code that currently calls `tokio::time::sleep` / `tokio::sync::broadcast` / `tokio::spawn` will silently hang or panic under Glommio because there's no tokio runtime in the shard.

We've been adding tokio-isms freely through Phases 6–8 (e.g. `tokio::sync::broadcast` for the in-process EventBus, `tokio::time::interval` in workers, `Pin<Box<Future>>` everywhere). Some of that is fine (futures are runtime-agnostic). Some is not (anything touching `tokio::runtime` machinery).

**The cost of skipping this audit:** during 9.4–9.7 (the real implementation sub-tasks) we'd discover tokio dependencies one at a time and have to refactor mid-feature. Doing it upfront means later sub-tasks land cleanly without churn.

**The cost of doing it:** ~half a day of grep + classification + a small markdown deliverable. No production code changes (unless the audit reveals a duplicated helper worth extracting).

---

## 2. Output artifacts

1. **`docs/phases/phase-09-glommio-port.md`** (NEW, checked in)
   The audit document. Read by 9.4–9.14 implementers. Lives under `docs/phases/` (not `.claude/plans/`) because it's reference material for the implementation phase, not a one-shot plan.

2. **`crates/brain-server/src/rt.rs`** (NEW, optional)
   A thin runtime-selector module *only if* the audit shows ≥ 3 distinct sites that benefit from a unified `runtime::sleep` / `runtime::yield_now` / `runtime::spawn_local` API. Likely scope: 20–60 LOC. If the audit shows 0–2 sites it's not worth introducing — skip and let each call site do its own glommio import.

3. **No changes to existing crates.** Ports happen in 9.4+; this sub-task only catalogues them.

---

## 3. The audit methodology

For each shard-bound crate, grep for tokio API surface and classify every match. The pattern set:

```
tokio::time::            # sleep, interval, timeout — needs glommio::timer
tokio::sync::            # broadcast, mpsc, watch, oneshot, Notify, Mutex, RwLock
tokio::task::            # spawn, spawn_local, yield_now, JoinHandle
tokio::spawn             # implicit spawn
tokio::select!           # select macro (runtime-agnostic, but uses tokio reactor for tokio futures)
tokio::io                # AsyncRead/Write, BufReader — connection-layer-only
tokio::net               # TcpListener/Stream — connection-layer-only
tokio::fs                # async filesystem — anti-pattern in shard code per CLAUDE.md §9
tokio::runtime           # Runtime / Handle — forbidden in shard code
#[tokio::test]           # test harness — connection-layer / cross-cutting only
#[tokio::main]           # entry point — connection-layer-only
```

Plus a parallel check for **non-tokio futures plumbing** that's already Glommio-compatible:
- `Pin<Box<dyn Future<Output = ...>>>` — runtime-agnostic, **keeps**.
- `async-trait` — runtime-agnostic, **keeps**.
- `futures::*` — runtime-agnostic, **keeps**.
- `parking_lot::Mutex`/`RwLock` — sync primitives, runtime-agnostic, **keeps** (with caveats: a sync lock held across `.await` is a bug under any runtime; the audit flags those).

### Disposition codes

Each tokio use-site gets one tag:

- **STAY-CONN**: lives in the connection layer (Tokio), no port needed.
- **STAY-TEST**: test-only code; keep tokio in dev-deps for now.
- **PORT-GLOMMIO**: needs glommio equivalent. Specify which one.
- **PORT-LOCAL**: needs hand-rolled per-shard primitive (e.g. cross-shard broadcast doesn't have a 1:1 glommio mapping). Specify design.
- **MOVE**: code itself moves to the connection layer (e.g. EventBus fan-out for SUBSCRIBE).
- **DELETE**: code is removed; the abstraction collapses (e.g. RealWriterHandle's tokio::sync::mpsc → direct per-shard call, no message-passing inside a shard).
- **QUESTION**: ambiguous, needs the user to decide before 9.4 starts.

### Per-crate scope (in audit order)

| # | Crate | What we expect to find |
| - | ----- | ---------------------- |
| 1 | `brain-core` | Pure types. Should be zero tokio. Confirm. |
| 2 | `brain-protocol` | Frame codec. May use `tokio::io::AsyncRead/Write` for streaming reads — **STAY-CONN**. |
| 3 | `brain-storage` | mmap + WAL. Synchronous I/O via `libc::pwritev2`. Should be zero tokio. Confirm. |
| 4 | `brain-metadata` | redb sync API. Zero tokio expected. Confirm. |
| 5 | `brain-index` | hnsw_rs sync. Zero tokio expected. Confirm. |
| 6 | `brain-embed` | candle sync. The dispatcher's batching loop might use `tokio::time::sleep` for the `batch_window_ms` — **PORT-GLOMMIO**. |
| 7 | `brain-planner` | Pure logic over ops. Zero tokio expected. Confirm. |
| 8 | `brain-ops` | The big one. `WriterHandle`/`RealWriterHandle` use `tokio::sync::mpsc`. SUBSCRIBE uses `tokio::sync::broadcast`. Worker `OpsContext` may carry tokio handles. Each gets a disposition. |
| 9 | `brain-workers` | Scheduler uses `tokio::time::interval` and `tokio::select!`. Each worker may use `tokio::time::sleep`. **PORT-GLOMMIO** for the scheduler core. |

### Output format inside the audit doc

```markdown
## brain-ops

### `src/writer.rs:42` — RealWriterHandle uses `tokio::sync::mpsc`
Disposition: **DELETE**
Rationale: Single-writer-per-shard means the writer is a local trait
object on the shard's executor — no inter-task channel needed. The
message-passing abstraction was a Phase 6 carry-over from the
multi-runtime sketch. Replace with a direct call inside the shard.

### `src/subscribe.rs:88` — EventBus uses `tokio::sync::broadcast`
Disposition: **MOVE + PORT-LOCAL**
Rationale: SUBSCRIBE is a cross-shard concern. The bus moves to the
connection layer (Tokio). Per-shard event sources push into a Tokio
broadcast channel that lives in the connection layer; subscribers
attach there. Design detail in §5 below.
```

That's the granularity we want — exact file + line + decision + 2-sentence rationale.

---

## 4. Cross-cutting design questions the audit must answer

These are decisions that affect multiple sub-tasks (9.4–9.14). Surface them in the audit doc with **explicit recommendations** plus an "open question" flag if I'm not confident.

1. **Where does the EventBus live?**
   - (a) Per-shard local + cross-shard fan-out in connection layer, **or**
   - (b) Single global bus in connection layer, shards publish via channel
   - **Recommendation: (a).** Per-shard events stay cheap; only SUBSCRIBE-fan-out crosses cores.

2. **Glommio task model for workers**
   - (a) One `Task::local` per worker, all on the shard's executor, **or**
   - (b) Single scheduler future that round-robins
   - **Recommendation: (b)** matches the spec's "deterministic worker tick" framing and is what `brain-workers::Scheduler` already does internally — only the timer needs porting.

3. **WAL group commit under Glommio**
   - The current WAL code (brain-storage §05) uses synchronous `pwritev2(RWF_DSYNC)`. Glommio prefers io_uring for everything. Does the group commit path become io_uring-based?
   - **Recommendation: keep synchronous `pwritev2`** for v1 — the call is blocking but bounded, and the alternative (uring-batched fsync via `submit_and_wait`) is a 9.6 sub-task in its own right. **OPEN QUESTION:** confirm Glommio LocalExecutor tolerates a sync `pwritev2` call. If not, must port immediately.

4. **brain-embed's BGE inference**
   - candle is sync + CPU/GPU bound. Where does it run?
   - **Recommendation: one Glommio "embedder" task per shard, using `glommio::yield_if_needed()` between batches** to keep the executor responsive. **OPEN QUESTION:** is shard-local embedding the right model, or do we want a shared embedder pool? Spec §06/04 implies per-shard, locked in by the orientation's "owns the embedding model" line — but the dispatcher is `Arc<CachingDispatcher>` today, which won't cross shards under Glommio's `!Send` discipline. May need per-shard dispatcher with shared model weights (`Arc<Model>` is `Send`/`Sync`; only the cache is per-shard).

5. **`Send` bounds on trait objects**
   - All our Phase 7/8 traits use `Pin<Box<dyn Future<Output = ...> + '_>>` (no `Send`) — that's correct for Glommio. **Verify no accidental `+ Send` bounds slipped in.** Audit greps `+ Send` in the relevant crates and flags any.

---

## 5. The cross-shard SUBSCRIBE design preview

This is the biggest non-trivial port. Sketch the v1 design in the audit so 9.11 has a target to implement against:

```
                                         ┌─────────────────────────────────┐
                                         │ Connection layer (Tokio)        │
                                         │ ┌─────────────────────────────┐ │
                                         │ │ Global SubscriberRegistry   │ │
                                         │ │ - HashMap<SubId, Sender>    │ │
                                         │ │ - HashMap<Filter, Vec<SubId>>│ │
                                         │ └─────────────────────────────┘ │
                                         │            ▲                    │
                                         │            │ tokio::mpsc::Sender│
                                         └────────────┼────────────────────┘
                                                      │
   ┌──────────────────────────────────────────────────┴──┐
   │ Glommio shard N — local EventBus                     │
   │   on each ENCODE/CONSOLIDATE/FORGET:                 │
   │     bus.publish(Event { shard, memory_id, kind })    │
   │   bus has a single subscriber: a "fan-out task" that │
   │   forwards events to the cross-shard mpsc Sender.    │
   └──────────────────────────────────────────────────────┘
```

- Per-shard local bus stays cheap (in-process, no cross-core sync).
- A single per-shard fan-out task forwards events into a tokio mpsc owned by the connection layer's `SubscriberRegistry`.
- The registry holds active subscribers and dispatches to interested clients.
- Drop on overflow with a per-subscriber counter (already spec'd).

Sub-task 9.11 implements this; 9.2's audit just notes the design so 9.4's shard scaffold reserves the right hooks.

---

## 6. Tooling: how I actually do the audit

```bash
# Per crate. The list is ~9 crates → ~9 minutes of grep + classify.
for c in brain-core brain-protocol brain-storage brain-metadata brain-index \
         brain-embed brain-planner brain-ops brain-workers; do
    echo "=== $c ==="
    rg -n 'tokio::' "crates/$c/src" || echo "  (no tokio uses)"
done
```

Plus a separate pass for the `Send`/`Sync` accidents:

```bash
rg -n '\+ Send|\+ Sync' crates/brain-ops/src crates/brain-workers/src \
                      crates/brain-embed/src crates/brain-planner/src
```

Plus a check that no dev-dep tokio bleeds into a non-test target:

```bash
cargo tree -p brain-ops -e features 2>&1 | rg tokio
```

Each tokio match gets one row in the audit table. Aggressively classify (don't agonise on close calls — flag with QUESTION and move on; user picks).

---

## 7. The audit document outline

```
# Phase 9 — Tokio→Glommio port audit
1. Summary table (one row per crate: total uses, breakdown by disposition)
2. Detailed inventory (per crate, per file, per use-site)
3. Cross-cutting design decisions
   3.1 EventBus topology
   3.2 Worker scheduler model
   3.3 WAL group commit semantics
   3.4 Embedder ownership
   3.5 Send bound audit
4. Open questions for the user
5. Recommended port order (which sub-task owns which port)
```

Section 4 ("Open questions") is the **only** thing that blocks 9.3 from starting. Everything else is reference material. If section 4 ends up empty, we proceed to 9.3 immediately on 9.2's commit. If section 4 has real questions, I STOP and surface them per AUTONOMY §3.

---

## 8. Sizing

| Component | LOC |
| --------- | --- |
| `docs/phases/phase-09-glommio-port.md` | 400–700 (most of it is the inventory table) |
| `crates/brain-server/src/rt.rs` | 0–60 (only if audit warrants) |
| New tests | 0 (this sub-task is documentation; tested by 9.4+) |

Single commit on `feature/brain-server`. Commit subject: `chore(brain-server): Tokio/Glommio port audit (sub-task 9.2)`.

---

## 9. Risks

| Risk | Mitigation |
| ---- | ---------- |
| The audit surfaces a blocker that needs spec clarification | STOP and surface per AUTONOMY §3. Don't paper over. |
| Discover that *most* of brain-ops needs porting → audit balloons into a refactor | The audit is **read-only**. Porting happens in 9.4+. Resist the urge to fix in-place. |
| The audit doc bit-rots between Phase 9 sub-tasks | Live document — 9.4+ implementers update it as they port each item ("done", "skipped"). |
| I miscategorise something (e.g. mark a STAY-CONN as PORT-GLOMMIO) | The classifications are reviewed at 9.4+ point-of-use. Self-correcting; cost of error is low. |

---

## 10. Done criteria

- [ ] `docs/phases/phase-09-glommio-port.md` checked in, all 9 crates audited.
- [ ] Each tokio use-site has a disposition + 1–2 sentence rationale.
- [ ] §3 (cross-cutting decisions) and §4 (open questions) populated.
- [ ] If any open questions, they're surfaced to the user **in the commit message** plus in this thread before declaring 9.2 done.
- [ ] `cargo check --workspace` still green (audit is doc-only; no code changes expected, but verify).
- [ ] Commit on `feature/brain-server`.
- [ ] Update `docs/phases/phase-09-server.md` to mark 9.2 `[x]` and reflect the orientation's renumbering (current doc still says 9.2 = shard executor; clarify with an inline note that the orientation reordered).

---

## 11. Why this isn't deferred

Skipping a dedicated audit and "just porting as we go in 9.4+" feels efficient but fails on:

- **Cross-cutting decisions** (EventBus topology, embedder ownership) span multiple sub-tasks. Deciding them inside 9.4 means 9.5+ either follows blindly or revisits.
- **Time-boxed visibility**: the user reads the audit doc once, signs off on the design, then 9.4+ executes. Without it, every port becomes a mini-design-decision in its own PR.
- **CI gating**: the audit doc lets us write a clippy-equivalent grep check for "no tokio:: outside crates/brain-server/src/connection.rs and tests/" once the port is complete. The doc is the ground truth that test enforces.

~half a day of work to save 2–3 days of churn across 9.4–9.14.

---

*Proceed to implement when approved.*
