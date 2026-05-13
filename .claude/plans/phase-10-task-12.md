# Phase 10 — Sub-task 10.12 plan

**Task:** `brain-cli profile`, `debug-snapshot`.

**Phase doc target:**
> Profile capture works (pprof format); debug snapshot writes JSON.

**Spec:** `spec/14_observability_ops/06_admin_ops.md` §9.

---

## 1. Scope decision (read this first)

Two commands. Honest readiness audit:

### `profile`

Spec: `brain-cli profile --shard <uuid> --duration 30s --output /tmp/profile.pb`
— captures a CPU profile of the shard's Glommio executor in
pprof protobuf format.

Real CPU profiling on a thread-per-core Glommio runtime is hard:
- Signal-based sampling (`pprof-rs`) walks the current thread's
  stack on SIGPROF. To target a specific shard, we'd need to deliver
  the signal to the shard's pinned thread and capture only its
  samples. Glommio doesn't expose a "run this hook on the executor
  thread" API today.
- The `pprof` crate's `ProfilerGuardBuilder` is built around Tokio /
  std threads, not Glommio.
- pprof-protobuf encoding (`pprof::protos::Profile`) adds a `prost`
  + `protobuf-codegen` dep — heavy for one rarely-used command.
- The natural alternative — `perf record --pid <shard_tid>` — is an
  operator-side workflow, not something the CLI shells out to.

**Decision:** ship the CLI + admin surface but return the structured
`501 deferred_to: "phase-11/glommio-profiler"` for now. Real
profile capture lands once Phase 11 work needs it (the operator-
visible substitute today is `perf` against the server PID, which
captures all shards).

### `debug-snapshot`

Spec: dump
- Active tasks.
- Pending requests.
- In-memory state summary.
- Recent errors.
- Worker statuses.

Per-shard readiness:

| Field | Backed today? | Source |
|---|---|---|
| Worker statuses | ✅ | `ShardHandle::scheduler_snapshot()` (already used by `worker list`) |
| In-memory state summary | ⚠️ partial | Arena slot count + free slot count exist; recall/encode counters don't yet have a stable surface |
| Pending requests | ❌ | The shard's `flume::Receiver<ShardRequest>` doesn't expose its current queue depth |
| Active tasks | ❌ | Glommio doesn't expose its task registry |
| Recent errors | ❌ | No ring buffer of recent errors anywhere in the server |

**Decision:** ship the command end-to-end with what's available
today: worker statuses + a `partial: true` flag + a `deferred:`
array listing the missing fields. Operators get something useful
(it summarizes what `worker list`, `stats`, `health` would each
print separately) and the schema is forward-compatible — future
phases just remove items from `deferred[]` as primitives land.

---

## 2. New admin HTTP endpoints

`crates/brain-server/src/admin/`:

```
admin/
├── diagnostics.rs   # NEW (profile + debug-snapshot)
```

Routes:

| Method | Path | Status |
|---|---|---|
| POST | `/v1/diagnostics/profile?shard=N&duration_secs=D` | 501 `deferred_to:"phase-11/glommio-profiler"` |
| GET | `/v1/diagnostics/debug-snapshot?shard=N` | 200 — JSON object |

### `debug-snapshot` response shape (v1)

```json
{
  "shard": 0,
  "captured_at_unix": 1700000000,
  "partial": true,
  "deferred": [
    "active_tasks",
    "pending_requests",
    "recent_errors",
    "in_memory_state_summary"
  ],
  "workers": [
    {"name": "decay", "cycles": 3, "processed": 12, "errors": 0, "last_run_unix": 1699999000},
    ...
  ]
}
```

`partial:true` flags the schema as incomplete; `deferred[]` lists
the spec-required fields not yet populated. v2 (Phase 11) drops
entries from `deferred[]` as they land.

Both routes go through the existing dispatch chain in `admin/mod.rs`
(after agent / shard_route).

### Why fold both into one `diagnostics.rs`?

Both routes share the "diagnostic" semantic + 1 helper (shard
parser). Two ~30-LOC handlers in one file beats two thinly-spread
files, and matches the per-family layout used by `worker.rs` /
`config_route.rs`.

---

## 3. CLI changes

### `crates/brain-cli/src/cli/args.rs`

Extend `Command`:

```rust
pub enum Command {
    ...
    Profile { shard: usize, duration_secs: u32, output_path: Option<String> },
    DebugSnapshot { shard: usize, output_path: Option<String> },
}
```

Add `--duration-secs <N>` global flag (CLI surface convenience; v1
defaults to 30 if not passed). The spec's `--duration 30s`
human-friendly syntax can wait for v2 — for now we accept seconds.

The existing `--output <json|table>` is the *render-format* flag.
For these two commands, spec uses `--output /tmp/profile.pb` (a
path). Resolution: reuse the existing `--value <path>` flag (per
10.11's `audit export` precedent) and document it. v2 disambiguates.

### `crates/brain-cli/src/commands/diagnostics/`

Folder-per-concern (per pinned `feedback_src_folder_layout.md`):

```
diagnostics/
├── mod.rs                  # DiagnosticsAction (not needed — two distinct Command variants suffice)
├── profile.rs              # POST /v1/diagnostics/profile; surface 501
└── debug_snapshot.rs       # GET  /v1/diagnostics/debug-snapshot; pretty-print + optional file write
```

`debug_snapshot.rs` writes the JSON body to `--value PATH` if
provided, else prints it. Pretty-prints in `--output json` (passthru
+ optional file); `--output table` renders the worker rows + a
banner that lists the `deferred[]` fields.

`profile.rs` POSTs to the admin endpoint, expects 501, and surfaces
via `commands::worker::common::surface_status` (already exists from
10.11).

### main.rs dispatch

Two new arms, mirroring 10.11 wiring; each calls `run_result(...)`.

---

## 4. ShardHandle extension

Zero new methods. `debug-snapshot` reuses `scheduler_snapshot()`.

---

## 5. Tests

`crates/brain-cli/tests/`:

- `diagnostics.rs` — 4–5 tests:
  - `debug_snapshot_json_round_trip` — mock returns JSON, CLI prints it.
  - `debug_snapshot_writes_to_path` — `--value /tmp/x.json` writes body to disk; asserts file contents.
  - `debug_snapshot_table_lists_deferred` — table output names each `deferred[]` entry.
  - `profile_surfaces_501` — mock returns the 10.11-style 501, CLI exit non-zero.

Admin-side unit tests in `admin/diagnostics.rs`:
- shard parse default = 0.
- duration parse default = 30 if absent.

---

## 6. Done when

- [ ] `brain-cli debug-snapshot --shard N [--value PATH]` returns
      JSON from the server, optionally writes to disk.
- [ ] `brain-cli profile ...` returns the 501 marker with
      `deferred_to: phase-11/glommio-profiler`.
- [ ] Admin server has `/v1/diagnostics/{profile,debug-snapshot}`.
- [ ] Phase doc 10.12 ticked + deferred slug noted.
- [ ] `just docker-verify` green.

---

## 7. Risks / open questions

- **Risk:** the `--value` flag overload (file path *and* config-set
  value) is ugly. v2 will introduce a dedicated `--output-file PATH`
  flag once we have more file-write commands. Documented in the
  CLI help text.
- **Open Q:** the spec example for `debug-snapshot` uses an
  agent/shard UUID; we use the numeric shard index everywhere else
  in the 10.x admin endpoints. Continuing with numeric — UUID
  routing is Phase-12 territory.
- **Risk:** "partial: true" + "deferred[]" is a v1 schema choice
  that downstream tooling may parse. We bake it in now to keep the
  spec promise of forward compatibility ("future phases just
  remove items from deferred[]"). Worth confirming the schema
  before any external tooling consumes it.
