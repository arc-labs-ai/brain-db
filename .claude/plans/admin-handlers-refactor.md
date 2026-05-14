# Admin handlers refactor

**Task:** Restructure `crates/brain-server/src/admin/` so route
handlers live under `admin/handlers/` and follow the pinned
folder-per-concern convention (one folder per handler family, one
file per route action).

**Reads:** current `crates/brain-server/src/admin/`.

---

## 1. Why

Three observations about the current admin/ layout:

1. **Flat file list mixes concerns.** `mod.rs` (server lifecycle),
   `router.rs` (route table), `util.rs` (response helpers),
   `metrics.rs` (Prometheus body), plus 8 handler files all sit at
   the same level. The lifecycle types and the per-route handlers
   are visually identical despite being different concerns.

2. **Each handler family bundles multiple sub-actions into one file.**
   `snapshot.rs` has create + list + delete in one ~170-LOC file.
   `worker.rs` has list + control. `config_route.rs` has get +
   reload + set. The pinned `feedback_src_folder_layout.md`
   memory says: every concern in its own folder, only `mod.rs` /
   `main.rs` at the root.

3. **Stutter in names.** `shard_route.rs` and `config_route.rs` carry
   the `_route` suffix because they would collide with the existing
   `crate::shard` / `crate::config` modules at the brain-server
   crate root. Under a `handlers/` subfolder there's no collision —
   `handlers::shard` and `handlers::config` work.

Plus three small improvements worth folding in:

4. **`parse_shard` is duplicated** in `snapshot.rs`, `rebuild.rs`,
   `worker.rs`, `diagnostics.rs`. Slightly different signatures (one
   returns `Option<usize>` for the optional case, others return
   `usize` defaulting to 0). One shared `admin/query.rs` consolidates.

5. **`/healthz` is inlined in `router.rs`** as a closure. Every other
   route has its own handler file; `healthz` should too for
   consistency.

6. **`metrics.rs` is a route handler** (responds to `GET /metrics`)
   but sits alongside `util.rs` (a shared helper) and `router.rs` (a
   server-level concern). It belongs under `handlers/` with the
   others.

---

## 2. Target layout

```
crates/brain-server/src/admin/
├── mod.rs                          # AdminState, AdminServer, BoundAdminServer (unchanged)
├── router.rs                       # build(state) -> Router (re-imports from handlers/)
├── util.rs                         # json_response, text_response, not_implemented (unchanged)
├── query.rs                        # NEW — shared query-string parsers
└── handlers/
    ├── mod.rs                      # pub re-exports + family doc
    ├── healthz.rs                  # NEW (moved from router.rs closure)
    ├── metrics.rs                  # MOVED from admin/metrics.rs
    ├── snapshot/
    │   ├── mod.rs                  # handle(req, state) entry + dispatch
    │   ├── create.rs               # POST /v1/snapshots
    │   ├── list.rs                 # GET /v1/snapshots
    │   └── delete.rs               # DELETE /v1/snapshots/{id}
    ├── rebuild.rs                  # POST /v1/rebuild-ann (single action)
    ├── worker/
    │   ├── mod.rs                  # pub list + control
    │   ├── list.rs                 # GET /v1/workers
    │   └── control.rs              # POST /v1/workers/{name}/{action} (501)
    ├── config/                     # renamed from config_route
    │   ├── mod.rs
    │   ├── get.rs                  # GET /v1/config
    │   ├── reload.rs               # POST /v1/config/reload (501)
    │   └── set.rs                  # POST /v1/config (501)
    ├── audit/
    │   ├── mod.rs
    │   ├── query.rs                # GET /v1/audit (501)
    │   └── export.rs               # GET /v1/audit/export (501)
    ├── agent/
    │   ├── mod.rs
    │   ├── list.rs                 # GET /v1/agents (501)
    │   └── by_id.rs                # /v1/agents/{id} prefix; GET / DELETE both 501
    ├── shard/                      # renamed from shard_route
    │   ├── mod.rs
    │   ├── list.rs                 # GET /v1/shards
    │   ├── create.rs               # POST /v1/shards (501)
    │   └── delete.rs               # DELETE /v1/shards/{idx} (501)
    └── diagnostics/
        ├── mod.rs
        ├── profile.rs              # POST /v1/diagnostics/profile (501)
        └── debug_snapshot.rs       # GET /v1/diagnostics/debug-snapshot
```

**What stays at `admin/` root:** lifecycle types (`AdminServer`,
`BoundAdminServer`, `AdminState`, `BuildInfo`), the router builder,
the response-helpers module, the new query-parser module. Four
files plus the `handlers/` directory.

**What moves into `handlers/`:** every route handler module.

---

## 3. Specific renames

| Before | After |
|---|---|
| `admin/snapshot.rs` (170 LOC, 3 actions) | `admin/handlers/snapshot/{mod,create,list,delete}.rs` |
| `admin/rebuild.rs` (single action) | `admin/handlers/rebuild.rs` (no folder; one action) |
| `admin/worker.rs` (2 actions) | `admin/handlers/worker/{mod,list,control}.rs` |
| `admin/config_route.rs` (3 actions) | `admin/handlers/config/{mod,get,reload,set}.rs` |
| `admin/audit.rs` (2 actions) | `admin/handlers/audit/{mod,query,export}.rs` |
| `admin/agent.rs` (2 actions) | `admin/handlers/agent/{mod,list,by_id}.rs` |
| `admin/shard_route.rs` (3 actions) | `admin/handlers/shard/{mod,list,create,delete}.rs` |
| `admin/diagnostics.rs` (2 actions) | `admin/handlers/diagnostics/{mod,profile,debug_snapshot}.rs` |
| `admin/metrics.rs` | `admin/handlers/metrics.rs` |
| `router.rs::healthz` closure | `admin/handlers/healthz.rs` |

**Single-action handlers** (`rebuild.rs`, `metrics.rs`, `healthz.rs`)
stay as flat files inside `handlers/` rather than getting their own
folder — folder-per-concern means folder-per-*family*, not
folder-per-file. One action = one file at the family level.

---

## 4. The new `admin/query.rs`

```rust
//! Shared query-string parsers for admin handlers.

/// Parse `?shard=N` from a URI query string. Defaults to `0` if
/// absent. Used by handlers that always target a specific shard
/// (snapshot, rebuild, debug-snapshot).
pub fn shard_required(query: &str) -> Result<usize, String> { … }

/// Parse `?shard=N` returning `None` if absent. Used by handlers
/// that filter optionally (worker list).
pub fn shard_optional(query: &str) -> Result<Option<usize>, String> { … }

/// Parse `?key=dotted.path` returning `None` if absent or empty.
/// Used by config-get.
pub fn config_key(query: &str) -> Option<&str> { … }
```

Replaces ~4 copies of `parse_shard` and `parse_key` across the
existing handler files. ~50 LOC consolidated.

---

## 5. Router changes

`router.rs::build` updates every `crate::admin::FAMILY` import to
`crate::admin::handlers::family`. Routing entries unchanged.
`healthz` closure becomes a normal `handlers::healthz::handle`
reference.

Net router.rs diff: ~10 imports change; the route-registration calls
that already use `worker::list` / `worker::control` stay the same.

---

## 6. Test impact

Looking at `crates/brain-server/tests/admin.rs`,
`crates/brain-server/tests/cli_e2e.rs`, and the brain-cli integration
tests — none import admin handler modules directly. They all hit
the admin server over HTTP. Wire behaviour is byte-identical
after the refactor.

Unit tests colocated in each handler file (e.g.
`snapshot::tests::parse_shard_default`) move WITH their handler.

---

## 7. Commit shape

```
refactor(brain-server): admin handlers into handlers/ subfolder

Restructures crates/brain-server/src/admin/ to follow the pinned
folder-per-concern convention. Every route handler family now lives
under admin/handlers/, and every multi-action family splits into one
file per action.

Renames:
- snapshot.rs → handlers/snapshot/{mod,create,list,delete}.rs
- worker.rs → handlers/worker/{mod,list,control}.rs
- config_route.rs → handlers/config/{mod,get,reload,set}.rs
  (drops the _route suffix — no collision under handlers/)
- audit.rs → handlers/audit/{mod,query,export}.rs
- agent.rs → handlers/agent/{mod,list,by_id}.rs
- shard_route.rs → handlers/shard/{mod,list,create,delete}.rs
  (drops the _route suffix)
- diagnostics.rs → handlers/diagnostics/{mod,profile,debug_snapshot}.rs
- metrics.rs → handlers/metrics.rs
- router.rs healthz closure → handlers/healthz.rs

Single-action handlers (rebuild.rs, metrics.rs, healthz.rs) stay as
flat files inside handlers/ — folder-per-family, not folder-per-file.

New: admin/query.rs consolidates the 4 copies of parse_shard /
parse_key into shared helpers (~50 LOC out, replaced by ~30 LOC
shared).

Router updates: every `crate::admin::FAMILY` import becomes
`crate::admin::handlers::family`. Routing entries unchanged; wire
behaviour byte-identical.

No public API change to AdminState / AdminServer / BoundAdminServer.
All 47 existing admin / cli / e2e tests pass unchanged.
```

---

## 8. Done when

- [ ] `admin/handlers/` directory and the 9 family modules created.
- [ ] All existing handler logic moved verbatim into the new layout
      (no behavioural change).
- [ ] `admin/query.rs` consolidates parse_shard / parse_key.
- [ ] `router.rs` imports updated; healthz closure replaced.
- [ ] All existing admin / cli / e2e tests pass unchanged.
- [ ] `just docker-verify` green.
- [ ] One commit per the message above.

---

## 9. Risks

- **Per-handler unit tests need to move with their handler.** Each
  `parse_shard_default` / `parse_key_extracts_dotted` etc. unit test
  lives in the same file as the function it tests. After the split,
  those tests move into the per-action files (e.g.
  `handlers/snapshot/mod.rs` keeps the parse_shard test if that's
  where `parse_shard` lands — or moves into `admin::query::tests`
  since the function consolidates there).

- **The `config_route` → `config` rename**: imports outside admin/
  also reference `crate::admin::config_route::dispatch` historically.
  After M3 (admin migration), no external code does — but worth a
  grep to confirm before commit.

- **`shard_route` → `shard` rename**: same. `crate::shard` is the
  shard executor at the brain-server crate root; we're renaming to
  `crate::admin::handlers::shard`, no path collision.

- **mod-not-found Rust errors during the move.** I'll lean on
  `cargo check` after each batch of edits to catch them early
  instead of moving all 8 families at once and chasing a wall of
  errors.

- **Existing colocated tests in the per-handler files.** They
  reference helper functions in the same file. After splitting,
  some tests cross-reference functions that moved to siblings. Need
  to update imports.
