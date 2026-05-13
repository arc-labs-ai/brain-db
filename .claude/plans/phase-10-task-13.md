# Phase 10 — Sub-task 10.13 plan

**Task:** SDK + CLI integration tests (Phase 10 exit gate).

**Phase doc target:**
> Test harness spins up server, drives via SDK + CLI, asserts outputs.

**Spec:** §13 (SDK) + §14/06 (CLI) — already tested at the unit /
mock level; 10.13 is the integration gate that runs the real
server + real SDK + real CLI together.

---

## 1. Scope decision (read this first)

The phase doc says "workspace-level fixture project". Cargo
doesn't have a workspace-level `tests/` dir, so this resolves to
one of three options:

**Option A — new crate `crates/brain-e2e/`.** Hard, because
`brain-server` is a bin-only crate today; another crate can't
bring its internals up in-process. Would force brain-server to
gain a `[lib]` target — substantial refactor outside this
sub-task.

**Option B — subprocess the brain-server binary.** True black-box
e2e. Costs 10–20s per spin-up (Glommio init + BGE-small model
load). Tests become slow and flaky on CI; not the right v1 target.

**Option C — colocate in `crates/brain-server/tests/`.** Reuses
the existing `#[path]` mount harness pattern already shipped in
`tests/e2e.rs` (554 LOC; brings up shards + connection layer +
admin server in-process). Drive *via SDK and CLI library APIs* —
i.e. `brain_sdk_rust::Client` against the data plane, and
`brain_cli::commands::*::run()` against the admin port.

**Decision: Option C.** Library-level integration is what we
actually want — the value of this gate is "all three layers
(server, SDK, CLI) speak the same protocol against the same
runtime", not "argv parsing works" (already covered by brain-cli's
own unit tests). Option A is for a future phase that gives
brain-server a `[lib]`; Option B can layer on later as a smoke
test if needed.

Trade-off explicitly accepted: argv-parsing roundtrip via the real
`brain-cli` binary is NOT tested here. That gap is small —
brain-cli's own `tests/cli.rs` already covers parsing — but worth
flagging.

---

## 2. New files

```
crates/brain-server/tests/
├── e2e.rs                       (unchanged — keeps raw-frame coverage)
├── support_harness.rs           NEW — shared Bringup factored out
├── sdk_e2e.rs                   NEW — drives via brain_sdk_rust::Client
└── cli_e2e.rs                   NEW — drives via brain_cli::commands::*
```

`support_harness.rs` factors out the `Server`/`Bringup` scaffold
from the existing `e2e.rs` so all three integration files share
one definition. (e2e.rs currently has 554 LOC; the bringup
portion is ~80 LOC.) Each integration file `#[path]`-mounts the
harness.

---

## 3. Cargo wiring

Add to `crates/brain-server/Cargo.toml`:

```toml
[target.'cfg(target_os = "linux")'.dev-dependencies]
brain-sdk-rust = { path = "../brain-sdk-rust" }
brain-cli = { path = "../brain-cli" }
```

Linux-only — same gate as the rest of brain-server's runtime.

---

## 4. `sdk_e2e.rs` test surface

Five scenarios that exercise the full data-plane stack via the
SDK:

1. `sdk_health_handshake_succeeds` — `Client::connect`, run a no-op
   round trip via the SDK's lower-level path; assert the handshake
   completes.
2. `sdk_encode_recall_roundtrip` — `client.encode("hello world").send().await`,
   then `client.recall("hello").send().await`; assert at least one
   result with the encoded memory_id.
3. `sdk_encode_forget_recall_absent` — encode → forget → recall;
   assert the memory_id is absent from results.
4. `sdk_pool_reuses_connections` — issue ~10 encodes; assert the
   connection pool's `MetricsSnapshot::reused_total` increments.
5. `sdk_subscribe_stream_observes_writes` — subscribe in one task,
   encode in another, assert the subscriber's stream sees the event.

These map 1:1 to existing SDK unit-test coverage but run against
the real server instead of a mock socket. The shared harness
makes spin-up cheap.

---

## 5. `cli_e2e.rs` test surface

Six scenarios that exercise the admin plane via brain-cli's lib
API. Each test calls a `run()` function with the harness's admin
addr; no argv parsing happens (covered by brain-cli's own tests).

1. `cli_health_returns_ok` — `brain_cli::commands::health::run`.
2. `cli_stats_emits_brain_up` — `commands::stats::run`; assert the
   `brain_up` line.
3. `cli_shard_list_matches_topology` — `commands::shard::list::run`
   returns the harness's shard count.
4. `cli_worker_list_includes_decay` — `commands::worker::list::run`;
   assert `decay` worker appears.
5. `cli_debug_snapshot_partial_schema` — `commands::diagnostics::
   debug_snapshot::run`; assert `partial:true` and `deferred`
   contains the four 10.12 entries.
6. `cli_snapshot_create_list` — `commands::snapshot::create::run`,
   then `commands::snapshot::list::run`; assert the created
   snapshot's id is present.

---

## 6. Failure modes guarded

- **Harness leaks:** each test takes a fresh `Bringup` and calls
  `bringup.stop().await` — same pattern as existing e2e.rs.
- **Port collisions:** all listeners bind `127.0.0.1:0` (already
  the existing pattern).
- **Embedder cold-start:** the existing e2e.rs already exercises
  ENCODE → so the embedder is hot by the time these tests run.
  No extra warmup needed.

---

## 7. Phase exit checklist

After this sub-task:
- [ ] All five phase-exit items in `docs/phases/phase-10-sdk-cli.md`
      §"Phase exit checklist" can be ticked.
- [ ] Tag `phase-10-complete`.

Done in this sub-task itself: items 1–4 of the exit checklist
(sub-tasks complete; verify green; SDK + CLI driven against a
running server). Item 5 (the tag) lands after the user approves
the final commit.

---

## 8. Done when

- [ ] `crates/brain-server/tests/sdk_e2e.rs` — 5 tests, all passing
      against the in-process harness.
- [ ] `crates/brain-server/tests/cli_e2e.rs` — 6 tests, all passing.
- [ ] Shared harness factored to `support_harness.rs`; existing
      `e2e.rs` rewired to use it (no behavior change).
- [ ] `just docker-verify` green.
- [ ] Phase doc §10.13 ticked; phase-exit checklist ticked.

---

## 9. Risks / open questions

- **Risk:** factoring the existing `Bringup` out of `e2e.rs` is a
  shape-preserving refactor of 80+ lines; risk of accidentally
  changing behavior. Mitigation: run e2e.rs's existing tests
  pre- and post-refactor to confirm parity.
- **Risk:** the SDK's `Client::connect` expects a specific
  HELLO/AUTH negotiation. The harness builds `ServerCapabilities`
  with `AuthMethod::None`; the SDK's default client config must
  also use `Auth::None`. Existing brain-sdk-rust tests confirm
  this combination works against mocks — should work against the
  real server too.
- **Open Q:** the SDK's `Client::new` uses `Pool` with a default
  config. Tests will accept whatever default that is — the
  scenarios don't depend on pool size > 1 except for test #4 (pool
  reuse), which will override `PoolConfig::max_size` if needed.
- **Risk:** the BGE embedder's first call may be slow (~hundreds of
  ms) on first invocation. Tests must have generous timeouts.
  Existing e2e.rs already deals with this; we follow its pattern.
