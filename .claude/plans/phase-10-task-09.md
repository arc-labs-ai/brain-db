# Sub-task 10.9 — `brain-cli snapshot` family

**Reads:**
- `spec/14_observability_ops/06_admin_ops.md` §5 (snapshot ops).
- `crates/brain-workers/src/workers/snapshot.rs` —
  `SnapshotSource` trait + `SnapshotDesc`.
- `crates/brain-server/src/shard/adapters.rs:231` —
  `ShardSnapshotSource` implements the trait (9.8 + 9.17).
- `crates/brain-server/src/admin/mod.rs` — admin HTTP router
  (currently `/healthz` + `/metrics`).
- `crates/brain-server/src/shard/mod.rs` — `ShardRequest` enum +
  shard main loop.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.9.

**Done when:**
- `brain-cli snapshot create [--shard SHARD]` triggers a
  snapshot on the named shard (or shard 0 by default), prints
  the snapshot id.
- `brain-cli snapshot list` lists snapshots across all shards.
- `brain-cli snapshot delete <id> [--shard SHARD]` removes a
  snapshot by id.
- `brain-cli snapshot restore <id>` returns "not yet supported;
  restart from cold backup is the v1 workflow" — destructive
  restore needs server downtime and is deferred to a Phase 11
  ops sub-task.

---

## 1. Scope — HTTP, not wire-protocol admin ops

Same trade-off as 10.8: wire-protocol admin ops are stubbed
`NotYetImplemented` in `dispatch.rs`. Wiring them server-side
is its own sub-task. 10.9 adds new HTTP routes to the admin
server and uses the existing brain-cli HTTP client.

When 11.x wires the wire-protocol admin path, the spec-canonical
flow becomes available; the HTTP routes can stay for operator
ergonomics (kubectl-style).

---

## 2. New HTTP routes (server-side)

Add to `brain-server/src/admin/`:

```
POST  /v1/snapshots                    [+ ?shard=N]   → 201 + JSON {"id": <u64>}
GET   /v1/snapshots                                    → 200 + JSON [SnapshotDesc, …]
DELETE /v1/snapshots/<id>              [+ ?shard=N]   → 204
```

All routes are unauthenticated for now (consistent with
`/healthz` + `/metrics`). 11.x layers auth on the whole admin
HTTP surface.

Layout in brain-server:

```
src/admin/
├── mod.rs                              (extended)
└── snapshot.rs                         NEW handlers
```

Each handler fans out to the right shard via a new
`ShardHandle::take_snapshot()` / `::list_snapshots()` /
`::delete_snapshot()` channel call.

---

## 3. New `ShardRequest` variants

`shard/mod.rs::ShardRequest` gains three variants:

```rust
ShardRequest::TakeSnapshot { reply_tx },
ShardRequest::ListSnapshots { reply_tx },
ShardRequest::DeleteSnapshot { id: u64, reply_tx },
```

Each handler in `shard_main_loop` calls into
`snapshot_source.take_snapshot()` etc. The existing
`ShardSnapshotSource` already provides all three.

`ShardHandle` gets three public methods that send the
request + await the reply on a flume oneshot.

---

## 4. CLI surface

```
brain-cli snapshot create  [--shard N]               → 201 → prints id
brain-cli snapshot list                               → table or json
brain-cli snapshot delete <id> [--shard N]           → 204 → prints "deleted"
brain-cli snapshot restore <id>                       → prints stub message,
                                                       exits 0
```

Default `--shard` is `0` for single-shard dev clusters. List
queries all shards in parallel (admin server fans out).

---

## 5. Module layout (brain-cli)

```
crates/brain-cli/src/commands/
├── mod.rs                              (extended — re-export snapshot)
├── health.rs                           (unchanged)
├── stats.rs                            (unchanged)
└── snapshot/                           NEW (folder-per-concern)
    ├── mod.rs                          subcommand dispatcher
    ├── create.rs
    ├── list.rs
    ├── delete.rs
    └── restore.rs                      stub
```

CLI argv parser gains `snapshot <action> [args]` parsing.

---

## 6. Tests

### 6.1 Server-side (`brain-server/tests/admin_snapshots.rs`)
- Spawn in-process server scaffold (same pattern as 9.17's
  `tests/e2e.rs`).
- POST /v1/snapshots → 201 + JSON containing `id`.
- GET /v1/snapshots → 200 + JSON array with that id.
- DELETE /v1/snapshots/{id} → 204.
- GET again → array doesn't contain the deleted id.

### 6.2 CLI-side (`brain-cli/tests/snapshot.rs`)
- Mock HTTP server returns canned responses per endpoint.
- `snapshot::create::run` returns the expected id.
- `snapshot::list::run` parses the array.
- `snapshot::delete::run` succeeds on 204.
- `snapshot::restore::run` prints the stub message and exits
  cleanly without hitting the server.

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Adding ShardRequest variants requires changing both shard/mod.rs and shard_adapters.rs interfaces | The existing snapshot_source already implements the trait. The shard main loop's dispatch just routes the new variants to `snapshot_source.<method>().await`. |
| Cross-shard fan-out for `list` requires multiple round-trips | We do them sequentially in the admin handler; list responses are small. Parallel via `futures::join_all` only matters if N > 10 shards (unlikely in v1). |
| `restore` is intentionally a stub | Documented up-front in §4 and in the command's `--help`. Spec §14/06 §5 calls restore "destructive — current data is lost"; production restore needs the substrate stopped and is a runbook step, not a one-liner. v2. |
| HTTP routes don't have auth | Same as `/healthz` + `/metrics`. 11.x layers auth on the whole admin endpoint. Document. |
| Body parsing on POST/DELETE | We use std-library TCP again on the server side (no hyper). brain-server's admin.rs already hand-rolls HTTP/1.1; adding method-based routing is a small extension. |

---

## 8. Done criteria

- [ ] Server: `src/admin/snapshot.rs` with 3 handlers.
- [ ] Server: 3 new `ShardRequest` variants + main-loop arms.
- [ ] Server: 3 new `ShardHandle` methods.
- [ ] Server: routes wired into the admin HTTP router.
- [ ] CLI: `src/commands/snapshot/` folder with 4 files.
- [ ] CLI: argv parser handles `snapshot <action>`.
- [ ] 4+ new server-side integration tests.
- [ ] 4+ new CLI integration tests.
- [ ] All 72 pre-10.9 tests still pass (50 SDK + 22 CLI).
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.9 marked `[x]` in the phase doc.

---

## 9. What 10.9 explicitly defers

- `restore` server-side wiring — needs server-side downtime
  handling; v2 / Phase 11 runbooks.
- `vacuum` and `gc` commands (spec §14/06 §4) — separate
  sub-task work; 10.10/10.11.
- Wire-protocol admin ops (ADMIN_SNAPSHOT_REQ etc.) —
  separate post-Phase-10 effort; spec-canonical but not
  blocking.
- Authentication for admin HTTP — 11.x.
- TLS — 11.x.
- Dry-run mode (spec §13) — defer.
- Job-id tracking for long-running ops (spec §16) — snapshot
  create is synchronous in v1; long ops surface later.

---

*Implement on approval.*
