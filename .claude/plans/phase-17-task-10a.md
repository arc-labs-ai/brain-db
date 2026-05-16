# 17.10a — Statement integration tests + lifecycle

End-to-end coverage for the statement layer. Drives every opcode
through the full data-plane stack (TCP → frame codec → connection
layer → shard executor → brain-ops dispatch → brain-metadata) and
asserts behavioural invariants the unit tests can't catch (event
emission, error mapping, multi-op composition).

Mirrors the phase 16.7.9 + 16.9.3 entity test suite layout.

## Spec refs

- `spec/19_statements/*.md` — invariants being verified end-to-end.
- `spec/28_knowledge_wire_protocol/06_statement_frames.md` — wire
  shapes and error codes per opcode.
- `spec/28_knowledge_wire_protocol/02_subscribe_events.md` — event
  emission contract (verified post-commit).

## Reads-only (clone patterns)

- `crates/brain-server/tests/knowledge_entity_wire.rs` — per-op
  wire-smoke template (16.6c).
- `crates/brain-server/tests/knowledge_entity_merge_wire.rs` —
  merge / list / tombstone wire-smoke + error paths (16.7.9).
- `crates/brain-server/tests/knowledge_entities_phase_exit.rs` —
  full-lifecycle composition (16.9.3).
- `crates/brain-server/tests/support_harness/` — test-server start
  helpers.

## Test files

### 1. `crates/brain-server/tests/knowledge_statement_wire.rs`

Per-op wire smoke. One test per opcode, all `#![cfg(target_os = "linux")]`:

- `create_fact_round_trips`.
- `create_preference_auto_supersedes_returns_auto_superseded`.
- `create_event_requires_event_at_returns_invalid_argument`.
- `create_unknown_predicate_returns_invalid_argument`.
- `get_returns_statement`.
- `get_follow_supersession_walks_chain`.
- `supersede_returns_new_id_and_chain_root`.
- `tombstone_returns_timestamp_and_flips_is_current`.
- `retract_returns_will_zero_hint`.
- `history_returns_chain_in_version_order`.
- `list_subject_filter_returns_current_preference`.
- `list_predicate_filter_returns_matches`.
- `list_limit_zero_returns_invalid_argument`.

Each test: open server, complete handshake, issue request frame,
read response frame, decode + assert. Setup: pre-create a Person
entity + register a `test:role` predicate via brain-metadata APIs.

Helper: `fn ensure_predicate(metadata: &mut MetadataDb, namespace,
name, kind_constraint, object_type_constraint_byte) -> PredicateId`
called from the test harness so each test starts with a registered
predicate.

### 2. `crates/brain-server/tests/knowledge_statements_phase_exit.rs`

Full lifecycle. One large `#[tokio::test]`:

```text
1. Create Person entities priya, manager_a, manager_b.
2. Register predicates test:role (Fact, Entity), test:prefers
   (Preference, Value), test:scheduled (Event, any).
3. Create Fact (priya role manager_a) — assert id, chain_root=id,
   confidence preserved.
4. Create another Fact (priya role manager_b) — both stored, both
   reachable via STATEMENT_GET; verify no error.
5. Supersede the second Fact with a new one. Verify chain_root
   inherited, version=2.
6. Create Preference (priya prefers "async meetings").
7. Create a second Preference (priya prefers "written agendas") —
   auto-supersede; assert response.auto_superseded == old.id.
8. List current Preferences for priya — exactly one, version=2.
9. STATEMENT_HISTORY on the Preference chain root — returns 2
   entries in version order.
10. Create Event (priya scheduled "planning session", event_at=now).
11. Tombstone the Event; verify response timestamp, then GET shows
    tombstoned=true.
12. Retract one Fact; verify response carries will_zero hint.
13. Final list: current statements for priya across all kinds.
```

Includes a verification that `SUBSCRIBE` receives `StatementCreated`,
`StatementSuperseded`, `StatementTombstoned` events at the right
moments (mirrors 16.9.3's event verification).

### 3. `crates/brain-sdk-rust/tests/knowledge_statement.rs`

SDK builder integration. Uses `support_harness::start` (the same
TCP harness used by `tests/knowledge_entity.rs`):

- `fact_builder_round_trip` — client.fact()...create() returns
  `StatementHandle`; get_current returns same.
- `preference_builder_auto_supersedes` — two preferences with
  same (subject, predicate); second handle has supersedes set to
  first id.
- `event_builder_requires_event_at` — surfaces InvalidRequest.
- `statements_list_chain` — list with current_only returns one.
- `statements_history_returns_chain`.
- `statements_tombstone_then_get_shows_tombstoned`.
- `statements_retract_returns_will_zero_hint`.

Reuses the entity SDK test's bootstrap (start server, connect
Client, register test predicate via direct brain-metadata access
on the test server's MetadataDb).

### 4. Helper additions

`support_harness/` gains a small helper to expose the test server's
`MetadataDb` lock so tests can call `predicate_intern` / `entity_put`
directly without going through the wire. Already needed for the
entity tests in 16.9.3; reused as-is or lightly extended.

## Plan

### Step 1 — Wire smoke (`knowledge_statement_wire.rs`)

Clone `knowledge_entity_merge_wire.rs` preamble (mod includes,
handshake helper, frame helpers, send/decode pair, `harness::start`).
Add one `#[tokio::test]` per opcode per the list above. Run
sequentially in one process (each test starts its own server).

### Step 2 — Full lifecycle (`knowledge_statements_phase_exit.rs`)

One large test exercising the 13-step lifecycle. Subscribes early
in the test to capture events emitted by subsequent ops. Asserts
event sequence at end.

### Step 3 — SDK integration (`crates/brain-sdk-rust/tests/knowledge_statement.rs`)

Clone the entity SDK test bootstrap. ~8 tests covering the builder
surface end-to-end.

### Step 4 — Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
# (host can't run the brain-server tests due to glommio; tests run
# in Linux CI / on the user's Linux box.)
cargo test -p brain-sdk-rust knowledge_statement
cargo clippy --workspace --tests --target x86_64-unknown-linux-gnu -- -D warnings
```

The brain-sdk-rust SDK tests use the `support_harness` from
brain-server (Linux-only); the host pass is `cargo test
-p brain-sdk-rust --lib` covering builder logic only (already
landed in 17.8). Full end-to-end runs on Linux.

## Files written

| Path | Change |
|---|---|
| `crates/brain-server/tests/knowledge_statement_wire.rs` | New. 13 wire-smoke tests + preamble. |
| `crates/brain-server/tests/knowledge_statements_phase_exit.rs` | New. 1 lifecycle test + event-subscription verification. |
| `crates/brain-sdk-rust/tests/knowledge_statement.rs` | New. 7 SDK builder tests against a real server. |

## Commit message draft

```
test(brain-server,brain-sdk-rust): statement integration tests (17.10a)

Three new test files cover the statement layer end-to-end:

- knowledge_statement_wire.rs (brain-server): 13 wire-smoke tests
  one per opcode + per error path. Drives the full TCP → frame →
  dispatch → statement_ops stack.
- knowledge_statements_phase_exit.rs (brain-server): 13-step
  lifecycle (create Fact + contradictory Fact / supersede / Preference
  auto-supersede / list current / history / Event create / tombstone
  / retract) with concurrent SUBSCRIBE verifying StatementCreated /
  Superseded / Tombstoned events fire at the right moments.
- knowledge_statement.rs (brain-sdk-rust): 7 builder integration
  tests over Client::fact / .preference / .event / .statements.

Linux-only via `#![cfg(target_os = "linux")]` (the server's glommio
shard executor pins these tests to Linux); cross-compile verified
on macOS via cargo zigbuild --target x86_64-unknown-linux-gnu.

Closes the test gaps documented in 17.7 ("integration tests +
event-emission assertion lands in 17.10") and 17.8 ("end-to-end
mock-server tests land in 17.10").

Plan: .claude/plans/phase-17-task-10a.md.
```

## Risks

- **Tests don't run on macOS.** Same constraint as entity-layer
  integration tests since 16.6c — glommio's io_uring requires
  Linux. Verification on macOS is `cargo zigbuild --tests` (compiles
  the binary; doesn't execute). Real runs happen on the user's
  Linux box / CI.
- **Event-subscription race**: the test must SUBSCRIBE before
  issuing the mutating ops. Use the same handshake-then-subscribe
  pattern as 16.9.3 / entity tests; drain channel at end with a
  short timeout.
- **Predicate registration via brain-metadata direct access** —
  some tests need to set up predicates before the server starts
  serving wire requests; `support_harness` helper handles this
  the same way entity tests register entity types.

## Out of scope

- Bench (17.10b).
- ROADMAP update + Cargo.lock + phase exit tag (17.10b).
- Statement HNSW semantic-search wire tests — phase 21 when the
  embedding worker ships.
- Cross-shard statement queries — phase 23.
