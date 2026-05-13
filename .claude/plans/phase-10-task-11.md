# Phase 10 — Sub-task 10.11 plan

**Task:** `brain-cli worker`, `config`, `audit`, `agent`, `shard`.

**Phase doc target:**
> All spec'd subcommands work. (Stubs from Phase 0 are now real.)

**Spec:** `spec/14_observability_ops/06_admin_ops.md` §6 (worker),
§7 (config), §8 (audit), §10 (agent), §11 (shard).

---

## 1. Scope decision (read this first)

Spec §14/06 lists **five command families** with ~12 sub-actions.
A blunt audit of which actions have server-side primitives ready
today:

| Sub-action | Backed today? | Source of truth |
|---|---|---|
| `worker list` | ✅ | `ShardHandle::scheduler_snapshot()` |
| `worker stop` / `start` / `run-now` | ❌ | `Scheduler` has no pause / resume / trigger plumbing — would need new control plane. |
| `config reload` | ❌ | Config is `Arc<Config>` loaded at startup; no live-reload swap. |
| `config get` | ✅ (partial) | Loaded config is readable; key-path lookup is a thin serde walk. |
| `config set` | ❌ | Requires an editable in-memory store + persistence pathway; spec itself says "not all settings are runtime-tunable". |
| `audit query` / `export` | ❌ | No audit-log primitive exists yet (spec §14/02 logs cover service logs, not the audit stream spec §14/06 §17 references). |
| `agent list` | ⚠️ deferred | Would require a redb secondary scan keyed by agent_id; not exposed via `ShardHandle`. |
| `agent stats <id>` | ⚠️ deferred | Same. |
| `agent delete <id>` | ❌ | Destructive; requires the agent_id index above + cascade delete + audit-log entry. |
| `shard list` | ✅ | `AdminState::shards` (read directly). |
| `shard create` / `delete` | ❌ | Phase-12 cluster-expansion territory; explicitly post-v1. |

**Decision:** ship the **CLI surface for every sub-command** so
operators have a single tool to learn, but back each one according to
its readiness. Specifically:

- **Wired end-to-end (admin HTTP + CLI + tests):**
  `worker list`, `config get`, `shard list`.
- **CLI present + admin route returns `501 Not Implemented` with a
  structured `{ "error": "not_implemented", "deferred_to": "..." }`
  body**, surfaced cleanly by the CLI:
  `worker stop` / `start` / `run-now`, `config reload` / `set`,
  `audit query` / `export`, `agent list` / `stats` / `delete`,
  `shard create` / `delete`.

Rationale: the alternative — building the worker control plane, live
config reload, an audit-log primitive, an agent secondary index, and
cluster expansion — is multiple phases of work, not one sub-task. The
spec acknowledges this implicitly (§14/06 §13 "dry-run", §16 "job
tracking" are themselves marked as separate concerns). The 501-with-
structured-marker pattern lets us land the operator-facing surface
now without inventing fake data.

Each 501 carries a `deferred_to` slug pointing at the future phase /
sub-task that will back it. The CLI prints the marker so users see
the same shape and know what to expect.

If the user prefers we build the missing primitives now, that's a
multi-week pivot — flag before "go".

---

## 2. New admin HTTP endpoints

`crates/brain-server/src/admin/`:

```
admin/
├── mod.rs              # add `mod {worker,config,audit,agent,shard};` + dispatch
├── snapshot/...        # (unchanged)
├── rebuild.rs          # (unchanged)
├── worker.rs           # NEW
├── config.rs           # NEW
├── audit.rs            # NEW
├── agent.rs            # NEW
└── shard.rs            # NEW
```

Routes (all under `/v1/`, all return `application/json`):

| Method | Path | Backed? | Status |
|---|---|---|---|
| GET | `/v1/workers[?shard=N]` | ✅ | 200 `[{shard,name,kind,cycles,processed,errors,last_run_unix}]` |
| POST | `/v1/workers/{name}/stop`, `/start`, `/run-now` | ❌ | 501 + `{error,deferred_to:"phase-11/scheduler-control"}` |
| GET | `/v1/config[?key=k.path]` | ✅ | 200 — full or sub-tree JSON |
| POST | `/v1/config/reload` | ❌ | 501 |
| POST | `/v1/config?key=…` (body=value) | ❌ | 501 |
| GET | `/v1/audit?since=…&until=…&agent=…` | ❌ | 501 |
| GET | `/v1/audit/export` | ❌ | 501 |
| GET | `/v1/agents[?shard=N]` | ❌ (⚠️) | 501 + `deferred_to:"phase-10.13/agent-index"` |
| GET | `/v1/agents/{id}` | ❌ (⚠️) | 501 |
| DELETE | `/v1/agents/{id}` | ❌ | 501 |
| GET | `/v1/shards` | ✅ | 200 `[{index,shard_id}]` |
| POST | `/v1/shards`, DELETE `/v1/shards/{i}` | ❌ | 501 |

**Dispatch** in `admin/mod.rs::serve_request` follows the existing
fall-through chain: snapshot → rebuild → (new) worker → config →
audit → agent → shard → /healthz → /metrics.

### Config serialization

`AppConfig` (Phase 9.1) derives `Serialize` already (config is loaded
from TOML via `toml::from_str`). For `GET /v1/config`:
- No `key` → entire config as JSON.
- With `key=workers.decay.interval` → walks the `serde_json::Value`
  by dotted path; 404 if any segment missing. (Spec uses dotted
  paths in its examples.)

Need to expose the `Arc<AppConfig>` through `AdminState`. Add a new
field `config: Arc<AppConfig>` on `AdminState` and populate from the
server's existing config Arc.

Touchpoints:
- `crates/brain-server/src/admin/mod.rs::AdminState` — add field +
  ctor param.
- `crates/brain-server/src/main.rs` (or wherever `AdminState::new` is
  called) — pass the config Arc.

### `not_implemented` helper

A small helper in `admin/mod.rs`:

```rust
pub(super) async fn write_not_implemented<W>(
    stream: &mut W,
    deferred_to: &str,
    detail: &str,
) -> io::Result<()>
where W: AsyncWrite + Unpin
{
    let body = format!(
        "{{\"error\":\"not_implemented\",\"deferred_to\":\"{deferred_to}\",\"detail\":\"{detail}\"}}\n",
    );
    write_response(stream, 501, "Not Implemented",
        "application/json; charset=utf-8", &body).await
}
```

Used by every 501 endpoint.

---

## 3. CLI changes

### `crates/brain-cli/src/cli/args.rs`

Extend `Command` enum:

```rust
pub enum Command {
    Help, Version, Health, Stats,
    Snapshot(SnapshotAction),
    RebuildAnn { shard: usize },
    Worker(WorkerAction),     // NEW
    Config(ConfigAction),     // NEW
    Audit(AuditAction),       // NEW
    Agent(AgentAction),       // NEW
    Shard(ShardAction),       // NEW
}
```

Action enums live in their respective `commands/` modules (mirroring
`SnapshotAction`'s pattern):

```
commands/
├── worker/
│   ├── mod.rs          # WorkerAction enum + parse + run dispatcher
│   ├── list.rs
│   ├── stop.rs         # POST /v1/workers/{name}/stop
│   ├── start.rs
│   └── run_now.rs
├── config/
│   ├── mod.rs
│   ├── get.rs
│   ├── reload.rs
│   └── set.rs
├── audit/
│   ├── mod.rs
│   ├── query.rs
│   └── export.rs
├── agent/
│   ├── mod.rs
│   ├── list.rs
│   ├── stats.rs
│   └── delete.rs
└── shard/
    ├── mod.rs
    ├── list.rs
    ├── create.rs
    └── delete.rs
```

(Adheres to pinned `feedback_src_folder_layout.md`: each concern in
its own folder, only mod.rs at the root of each.)

### New POST helpers

`commands/rebuild.rs` already has `post_no_body`; sub-task 10.11
needs `post_with_body` (for `config set --value`, `audit query`
filters). Promote both into `crates/brain-cli/src/http/post.rs` so
the new commands share one implementation. Keep `commands/rebuild.rs`
delegating to it (one-line `use`).

### 501 surfacing

When the admin returns 501, the CLI's POST/GET helpers receive a JSON
body shaped like `{"error":"not_implemented","deferred_to":"...","detail":"..."}`.
Render in both `--output json` (passthrough) and `--output table`:

```
Not yet implemented.
Deferred to: phase-11/scheduler-control
Detail:      live worker control plane
```

Exit code: non-zero so scripts can detect.

---

## 4. ShardHandle extension (small)

Only one new ShardHandle method needed for the *backed* paths:

- `worker_list()` — already provided by `scheduler_snapshot()`. ✅
  No new method.

`config get` reads from `AdminState`, not a shard. `shard list` reads
from `AdminState`. No new ShardHandle methods.

So **zero changes** to `crates/brain-server/src/shard/mod.rs` in this
sub-task. Big plus for review surface.

---

## 5. Tests

Per existing pattern: tokio-driven mock admin server on
127.0.0.1:0; assert request shape + response render.

`crates/brain-cli/tests/`:
- `worker.rs` — `list` JSON+table, 501 paths for stop/start/run-now.
- `config.rs` — `get` (full + by-key), 501 paths for reload/set.
- `audit.rs` — both subcommands hit 501 with expected `deferred_to`.
- `agent.rs` — same.
- `shard.rs` — `list` JSON+table, 501 for create/delete.

Unit tests for argv parsing in `commands/<family>/mod.rs::tests`.

Admin-side unit tests in each new `admin/<family>.rs`:
- 200 path with a synthetic AdminState.
- Bad `shard=` → 400.
- Path normalization (`/v1/workers/decay/stop` parses).

---

## 6. Done when

- [ ] All 5 command families parseable from argv.
- [ ] `worker list`, `config get`, `shard list` return real data from
      the admin server.
- [ ] All other sub-actions return a uniform 501 body and the CLI
      renders the marker + exits non-zero.
- [ ] Phase doc 10.11 ticked with a note listing each deferred slug.
- [ ] `just docker-verify` green.

---

## 7. Risks / open questions

- **Risk:** `AppConfig` may not currently derive `Serialize`. If it
  doesn't, this sub-task adds the derive — confirm before commit.
- **Open Q:** the 501 `deferred_to` slugs are my best guess; some
  ("phase-11/scheduler-control") name future work that doesn't yet
  have a sub-task ID. Will leave as descriptive strings, not real
  IDs, until those phases land.
- **Risk:** path-matching for `/v1/workers/{name}/stop` is ad-hoc
  string parsing in `admin/worker.rs`. Spec'd worker names are a
  closed set (`decay`, `consolidation`, …) per §11/01. Will validate
  against that set in the route handler and reject unknown names
  with 400 even though the route is otherwise 501. (Helps catch typos
  early once the action is wired.)
