# Phase 9 — `brain-server`: End-to-End Wire-Up (orientation)

> Orientation plan only. Per-sub-task plans land separately under `.claude/plans/phase-09-task-NN.md`.

---

## 1. What Phase 9 actually delivers

A **runnable substrate**. Up to now every crate has lived in isolation, tested with single-process in-memory fixtures (`tempdir + MetadataDb + SharedHnsw + RealWriterHandle`). Phase 9 turns that into:

- `cargo run --bin brain-server` boots, reads `config/dev.toml`, listens on TCP.
- A client can connect, encode a memory, recall it, get bytes back over the wire.
- Multiple shards run concurrently, each owning its own arena + WAL + metadata + HNSW.
- The Tokio connection layer dispatches to per-shard Glommio executors.
- `SIGINT` drains in-flight requests, fsyncs the WAL, exits clean.
- An end-to-end smoke test asserts the whole loop works.

This is the phase that closes the **largest stack of v1 deviations** we've been carrying through Phases 7 + 8. The pluggable-seam workers (Summarizer, RebuildSource, WalRetentionSource, CacheEvictionSource, SnapshotSource) all get wired here. The PLAN/REASON tombstone filter we deferred from §16/01 §12 lands here. The shard-architecture assumption baked into spec §10 finally becomes real.

It's also the **highest-risk phase** because it introduces Glommio — every existing crate has to be re-validated under Glommio's discipline (`!Send` types in shard code, no `tokio::fs`, no shared Mutex across shards).

---

## 2. The architectural pivot

### Two runtimes, not one

| Component | Runtime | Reason |
| --------- | ------- | ------ |
| TCP accept + auth + frame parse | **Tokio** (multi-thread) | Cross-shard concerns, connection management, fan-out work |
| Per-shard data & execution | **Glommio** (one `LocalExecutor` per shard, pinned to a core) | Spec §01/04: thread-per-core, no work-stealing, no cross-core sync on the request path |

The boundary is a **per-shard channel**. Tokio accept loop reads a frame → routes to shard N → sends an `(opcode, body, response_tx)` message into shard N's mpsc queue. Shard N's Glommio executor drains the queue, dispatches via brain-ops, sends the response back through `response_tx`.

```
   Tokio thread                           Glommio core 0
   ┌─────────────────┐  per-shard mpsc   ┌──────────────────────────┐
   │ accept(socket)  │ ────────────────▶ │ shard 0 executor         │
   │ parse frame     │                    │ ├─ dispatcher (brain-ops)│
   │ route(agent_id) │                    │ ├─ writer task           │
   │ send to shard   │ ◀────── reply ──── │ ├─ workers               │
   │ write to socket │                    │ └─ Arena+WAL+Metadata+HNSW│
   └─────────────────┘                    └──────────────────────────┘
                                          (one of these per shard)
```

### Why this matters for existing code

The Phase 7 + 8 code was designed against `OpsContext` as a single global. Phase 9 makes that **per-shard**:

- Each shard owns its own `OpsContext`.
- Workers register per-shard against per-shard schedulers.
- The connection layer holds an `Arc<RoutingTable>` + `HashMap<ShardId, ShardHandle>`.

**The good news:** the Pin<Box<Future>> trait pattern we used everywhere (`WriterHandle`, `Summarizer`, `RebuildSource`, ...) is Glommio-compatible. Trait objects don't require `Send`, and Glommio's executor doesn't require `Send` futures.

**The bad news:** anything that uses `tokio::time::sleep` or `tokio::sync::broadcast` in shard code needs porting to Glommio equivalents. We've used both in brain-ops's subscribe + worker scheduler. Replace:

| tokio in shard code | Glommio replacement |
| ------------------- | ------------------- |
| `tokio::time::sleep` | `glommio::timer::sleep` |
| `tokio::task::yield_now` | `glommio::executor().yield_if_needed()` |
| `tokio::sync::broadcast` | Per-shard local broadcast (need to build) — or move SUBSCRIBE entirely to the connection layer |
| `tokio::sync::watch` | `glommio::channels::local_channel` (single-consumer) — or build a local watch |
| `tokio::spawn` | `glommio::Task::local` |

The connection layer (Tokio) stays on tokio.

---

## 3. The sub-task dependency graph

```
9.1 (config)
 ├─▶ 9.2 (shard executor)
 │    ├─▶ 9.6 (ArcSwap state)
 │    └─▶ 9.7 (crossbeam-epoch)
 ├─▶ 9.5 (routing — pure fn)
 └─▶ 9.8 (health/metrics)
       │
9.2 + 9.3 (connection) + 9.5 ─▶ 9.4 (frame dispatcher)
                                  └─▶ 9.9 (graceful shutdown)
                                       └─▶ 9.10 (e2e smoke)
```

**Recommended order:**
1. **9.1** — config loading (decouples everything that follows from hard-coded constants).
2. **9.5** — routing (pure function, can land any time; 9.4 needs it).
3. **9.2** — shard executor scaffold. **Biggest sub-task** — owns the Phase 8 worker wire-up, real WAL hookup, real arena.
4. **9.3** — connection layer (Tokio accept loop, frame I/O).
5. **9.4** — frame dispatcher (the Tokio↔Glommio boundary).
6. **9.8** — health/metrics endpoints (small, independent, useful early).
7. **9.9** — graceful shutdown (ties 9.2 + 9.3 + 9.4 together).
8. **9.6** + **9.7** — ArcSwap & crossbeam-epoch (mostly integrated into 9.2's storage; can land alongside).
9. **9.10** — e2e smoke (acceptance gate).

Sub-tasks **9.2 is almost certainly worth splitting**. The phase doc lists it as one entry, but it covers:

- (a) Per-shard `Shard` struct
- (b) Glommio `LocalExecutor` plumbing
- (c) Real arena hookup (currently no arena exists)
- (d) Real WAL hookup (currently `RealWriterHandle` doesn't write WAL)
- (e) Per-shard `OpsContext` construction
- (f) Per-shard `WorkerScheduler` registration
- (g) Wiring the Phase 7/8 pluggable seams to real impls (Summarizer, RebuildSource, WalRetentionSource, CacheEvictionSource, SnapshotSource)

Realistically that's **3-5 sub-tasks of its own**, not one. I'll surface a split when we get to the per-sub-task plan for 9.2 and ask before fragmenting.

---

## 4. The seams we close in Phase 9

| From | Seam | What gets wired |
| ---- | ---- | --------------- |
| 7.10 | `EventBus` cross-shard | Currently per-process; Phase 9 needs per-shard buses + cross-shard fan-out for `SUBSCRIBE` |
| 8.4 | `Summarizer` trait | LLM-backed adapter (optional — `auth.mode = "none"` style: ship `DisabledSummarizer` as default; document the plug-point) |
| 8.5 | `RebuildSource` trait | Arena-backed `(MemoryId, vector)` snapshot. Real rebuild possible once vectors are keyable by id |
| 8.7 | Slot reclamation free-list | Currently a no-op (no arena). Phase 9 wires arena → free-list push |
| 8.8 | `WalRetentionSource` trait | `brain_storage::Wal` exposes `list_segments` + `delete_segment` (small brain-storage addition) |
| 8.12 | `CacheEvictionSource` trait | `CachingDispatcher::prune_older_than` (small brain-embed addition) + `Arc<CachingDispatcher>` on shard context |
| 8.13 | `SnapshotSource` trait | Orchestrate `WAL checkpoint → arena snapshot → HNSW save_snapshot → metadata copy` |

**Side bug** to fix here: §16/01 §12's PLAN/REASON tombstone filter (deferred to Phase 8 → still pending). Once `MemoryMetadata.tombstoned_at_unix_nanos` is reliable (it is, since 8.7), the executors should consult it. Small brain-planner change.

---

## 5. What stays out of scope in Phase 9

Per spec, deferred to v2 / Phase 10+ / future:

- **Replication** (spec §12 OQ-2) — single-replica per shard. Loss of node = loss of agents until restored from snapshot.
- **Cross-region active-active** — out of scope; cross-region DR via snapshot only.
- **Cross-shard transactions** — single-shard TXNs suffice for v1.
- **Auth** — `auth.mode = "none"` default is fine; basic API-key auth is the most we'll wire.
- **TLS** — `rustls` integration listed in 9.3 but plain-TCP-only is acceptable for the smoke test.
- **`ADMIN_*` handlers** — config, snapshot create/restore, worker stop/restart. Spec mentions these throughout; minimum viable is `ADMIN_STATS`.
- **Real LLM integration** — `Summarizer` stays a seam; operators inject an adapter at their layer.
- **Continuous reconfiguration** — restart-only is fine for v1 (spec §01/04 §15).

These are **explicit non-goals** for Phase 9. Surface them again if pushback comes during sub-task planning.

---

## 6. Risks specific to Phase 9

| Risk | Mitigation |
| ---- | ---------- |
| Glommio runtime requirements (io_uring, memlock) break tests on CI / dev laptops | Brain-dev container is mandatory now. Document the constraint. Some tests stay tokio-only (no shard executor); end-to-end tests use the container |
| Existing brain-ops/brain-workers code uses tokio primitives | Audit + port. Pin<Box<Future>> stays; tokio::time / tokio::sync get replaced. Document the inventory before 9.2 starts |
| Real WAL hookup is a big lift (recovery, crash semantics, group commit) | Surfacing during 9.2 planning. May warrant its own sub-task |
| Real arena is a big lift (mmap, slot allocation, free list) | Same — likely a dedicated sub-task |
| Per-shard topology changes every existing test fixture | The single-shard tests stay valid as the shard=0 fixture. New multi-shard tests for routing + cross-shard SUBSCRIBE |
| Tail-latency targets in spec §16/02 are unreachable in CI | The 9.10 smoke test is "does it work end-to-end", not "does it meet p99". Phase 11 (observability) owns real perf measurement |

---

## 7. The dev.toml schema we're committing to

`config/dev.toml` already exists. Phase 9 picks it up as canonical. Notable shape:

```toml
[server]      listen_addr, metrics_addr, admin_addr
[storage]     data_dir, shard_count
[shard]       arena_capacity_bytes, wal_segment_size_bytes, wal_retention_segments
[hnsw]        m, ef_construction, ef_search
[embedder]    model, cache_size, batch_size, batch_window_ms
[workers]     per-worker interval_sec
[logging]     level, output, format
[tracing]     enabled, sampler, sample_ratio
[auth]        mode = "none"
```

9.1 builds a typed `Config` struct, validates the file, supports `BRAIN__SERVER__LISTEN_ADDR`-style env-var overrides per spec §01/04 §15.

---

## 8. The acceptance gate

`docs/phases/phase-09-server.md` exit checklist:

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] `cargo run --bin brain-server` accepts a connection from a sample client.
- [ ] E2E smoke test passes 100 iterations.
- [ ] `just run-server` boots in < 5 seconds with empty data.
- [ ] Tag `phase-9-complete`.

The "100 iterations" + "5 second boot" are real constraints. The smoke test should run in a loop until either count or timeout — that's the closest thing to a real flake test we'll have until Phase 11.

---

## 9. Recommended kick-off

After this orientation is approved:

1. **9.1 plan first** (config + env-override). Smallest, unblocks everything.
2. Then audit the tokio-vs-glommio inventory across brain-ops, brain-workers, brain-protocol's response framing — surface as a quick "what needs porting" doc before 9.2.
3. Then **9.5 plan** (routing pure-fn — easy win).
4. Then **9.2 plan**, which will probably split into 3-5 sub-tasks (shard scaffold / arena / WAL / per-shard ops / per-shard workers).

Total Phase 9 sub-task count probably ends at **12-15 plans**, not the doc's 10.

Single feature branch: `feature/brain-server` (already created).

---

## 10. Decisions locked in

1. **Full Glommio.** Phase 9 ships Glommio shards + Tokio connection layer. macOS dev = container-only from now on. No LocalSet stepping stone.
2. **Cross-shard SUBSCRIBE.** Fan-out included in Phase 9, not deferred. Expect +1-2 sub-tasks for the cross-shard event bus.
3. **Bundled OpenAI / Ollama Summarizer adapter.** Behind a Cargo feature flag. The seam stays — operators can still inject custom impls — but Phase 9 ships a working adapter so consolidation works out of the box. Defaults stay disabled; feature flag opt-in.
4. **TLS in 9.3.** `rustls` + `tokio-rustls` integration in the connection layer, behind a feature flag. Off by default; `[server]` config gains `tls.enabled / tls.cert / tls.key`.

These choices grow Phase 9 — the realistic sub-task count is now **15-18 plans** (not the doc's 10). Each gets its own `.claude/plans/phase-09-task-NN.md` per the established workflow.

---

## 11. Updated sub-task projection

With the locked-in decisions, the working list (still subject to per-plan revision):

| Sub-task | Working title |
| -------- | ------------- |
| 9.1  | Config loading (typed + env overrides) |
| 9.2  | Tokio/Glommio audit + port shim |
| 9.3  | Routing (pure-fn) |
| 9.4  | Shard scaffold (`Shard` struct + lifecycle, no real arena/WAL yet) |
| 9.5  | Real arena hookup |
| 9.6  | Real WAL hookup (+ recovery) |
| 9.7  | Per-shard `OpsContext` + per-shard `WorkerScheduler` |
| 9.8  | Wire Phase 8 seams to real impls (RebuildSource, WalRetentionSource, CacheEvictionSource, SnapshotSource) |
| 9.9  | Connection layer (Tokio accept + TLS via rustls behind feature flag) |
| 9.10 | Frame dispatcher (Tokio↔Glommio boundary) |
| 9.11 | Cross-shard SUBSCRIBE fan-out |
| 9.12 | ArcSwap shared state + crossbeam-epoch reclamation |
| 9.13 | Health + metrics endpoints (Prometheus) |
| 9.14 | Graceful shutdown |
| 9.15 | OpenAI/Ollama Summarizer adapter (feature-gated) |
| 9.16 | PLAN/REASON tombstone filter (§16/01 §12 carry-over) |
| 9.17 | End-to-end smoke test (`tests/e2e.rs`) |
| 9.18 | Phase exit: docs/phases checklist, tag |

Each lands as a separate commit on `feature/brain-server`.

---

*Proceed to sub-task 9.1 (config loading) plan when ready.*
