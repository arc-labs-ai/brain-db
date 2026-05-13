# Sub-task 10.10 — `brain-cli rebuild-ann`

**Reads:**
- `spec/14_observability_ops/06_admin_ops.md` §4 (`rebuild-ann`).
- `crates/brain-workers/src/workers/hnsw_maint.rs` — existing
  `HnswMaintenanceWorker` + `do_maintenance_cycle` logic.
- `crates/brain-server/src/shard/adapters.rs` —
  `ArenaRebuildSource` already implements `RebuildSource`.
- `crates/brain-server/src/admin/snapshot.rs` — pattern for the
  new admin HTTP route.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.10.

**Done when:** `brain-cli rebuild-ann [--shard N]` triggers an
immediate full HNSW rebuild on the named shard and prints the
new index size. Server side: new admin HTTP route +
`ShardRequest::RebuildHnsw` + `ShardHandle::rebuild_hnsw()`.

---

## 1. Scope

Spec §14/06 §4 says rebuild is async and offers a
`rebuild-ann-status` follow-up. In v1 we run it **synchronously
to the HTTP request**: the caller's HTTP call blocks until the
rebuild completes and the response carries the new index size.
Spec-canonical job-id tracking + status query (§16) lands as a
follow-up sub-task once we have at least one other long-running
op to share infrastructure.

The work is small because the rebuild logic already exists in
`HnswMaintenanceWorker::do_maintenance_cycle` (Action::FullRebuild
arm). 10.10 extracts the same pattern into an immediate-trigger
helper.

---

## 2. Server-side changes

### 2.1 `Shard` struct
Add a field carrying the `RebuildSource` Arc (currently
constructed locally in the spawn closure and only handed to the
scheduler):

```rust
struct Shard {
    …
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
    hnsw_shared: SharedHnsw<{ VECTOR_DIM }>,
}
```

`hnsw_shared` is needed for the `swap()` call. It's already
constructed in the spawn closure; we just stash a clone in the
struct.

### 2.2 New `ShardRequest::RebuildHnsw`

```rust
RebuildHnsw {
    reply_tx: Sender<Result<RebuildReport, String>>,
},

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    pub entries: usize,
    pub elapsed_ms: u64,
}
```

Main-loop arm:
1. `snapshot_vectors()` → `Vec<(MemoryId, [f32; D])>`.
2. `HnswIndex::<{VECTOR_DIM}>::rebuild(params, vectors)`.
3. `hnsw_shared.swap(new_idx)`.
4. Send back `RebuildReport { entries, elapsed_ms }`.

### 2.3 `ShardHandle::rebuild_hnsw()`
Standard send-receive on flume. Returns
`Result<RebuildReport, ShardError>`.

### 2.4 Admin HTTP route
`POST /v1/rebuild-ann[?shard=N]` →
`201 {"entries":N,"elapsed_ms":N,"shard":N}`

Add to `src/admin/`:

```
src/admin/
├── mod.rs                       (extended dispatch)
├── snapshot.rs                  (unchanged from 10.9)
└── rebuild.rs                   NEW
```

`rebuild::dispatch` returns `Some(...)` for `POST /v1/rebuild-ann`.

---

## 3. CLI-side changes

```
src/commands/
├── ...
└── rebuild.rs                   NEW
```

`Command::RebuildAnn { shard: usize }` variant. argv parser:

```
brain-cli rebuild-ann [--shard N]
```

`rebuild::run(server, shard, output) -> Result<String>` POSTs
`/v1/rebuild-ann?shard=N`, parses the response, renders.

---

## 4. Tests

### 4.1 Server-side
A full server-spawn integration test is heavy (mirrors the 9.17
e2e scaffold). The rebuild logic is already covered by
`brain-workers/tests/hnsw_maint.rs`. 10.10 ships:
- A unit test in `src/admin/rebuild.rs::tests` for path / query
  parsing (mirroring `snapshot.rs::tests`).

### 4.2 CLI-side
- `tests/rebuild.rs` integration test:
  - Mock admin HTTP returns canned `{"entries":42,"elapsed_ms":5,"shard":0}`.
  - `rebuild::run` parses, renders both JSON and table.
  - 5xx error path reports a useful message.

---

## 5. Module layout summary

```
crates/brain-server/src/admin/
├── mod.rs
├── snapshot.rs           (unchanged)
└── rebuild.rs            NEW (~120 LOC)

crates/brain-server/src/shard/mod.rs
└── + ShardRequest::RebuildHnsw + RebuildReport + handle method
  + Shard.rebuild_source + Shard.hnsw_shared

crates/brain-cli/src/commands/
├── ...
└── rebuild.rs            NEW (~100 LOC)
```

LOC: ~300 production + ~150 test.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Synchronous rebuild blocks the HTTP request for the duration of the rebuild (could be minutes on 1M-entry shards) | Document. Spec §14/06 §4 says async + status; v1 takes the simpler synchronous path. The operator runs this during a maintenance window. Job-id tracking + `rebuild-ann-status` is the follow-up. |
| `swap()` mid-rebuild while readers are active | `SharedHnsw::swap()` is atomic per SD-4.8-1. Existing readers complete on the old index; new readers see the rebuild. |
| `ArenaRebuildSource` walks the entire arena holding an immutable borrow | The borrow drops between batches inside `snapshot_vectors()`. Existing infrastructure; no new constraints. |
| Adding `Shard.rebuild_source` + `Shard.hnsw_shared` widens the struct | Acceptable — admin ops are first-class and need access to these for v1. |
| Wire-protocol admin path stays stubbed | Same trade-off as 10.8 / 10.9. HTTP suffices. |

---

## 7. Done criteria

- [ ] Server: `src/admin/rebuild.rs` + extended admin router.
- [ ] Server: `ShardRequest::RebuildHnsw` + `RebuildReport` +
  `ShardHandle::rebuild_hnsw()` + main-loop arm.
- [ ] Server: `Shard.rebuild_source` + `Shard.hnsw_shared` fields.
- [ ] CLI: `src/commands/rebuild.rs` + `Command::RebuildAnn` +
  argv routing.
- [ ] 3+ new server-side unit tests (path/query parse).
- [ ] 3+ new CLI integration tests (success path, 500, table+JSON).
- [ ] All 82 pre-10.10 tests still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.10 marked `[x]` in the phase doc.

---

## 8. What 10.10 explicitly defers

- `rebuild-ann-status` follow-up command — needs job-id
  tracking; v2 / Phase 11.
- Async dispatch with a status endpoint (spec §16) — same.
- Cross-shard "rebuild all" mode — operator scripts can loop.
- Dry-run mode for rebuild — defer.
- Auth on the admin route — 11.x.

---

*Implement on approval.*
