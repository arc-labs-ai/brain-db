# Sub-task 9.17 — End-to-end smoke test

**Reads:**
- Phase doc 9.17 (orig 9.10 in `docs/phases/phase-09-server.md`):
  "Test spins up the server in-process (or via subprocess). Uses
  `brain-sdk-rust` to drive: encode → recall → forget → recall.
  Verifies expected results. Done when: Smoke passes reliably."
- `crates/brain-sdk-rust/src/lib.rs` — currently a placeholder
  (spec/13 is a separate, post-Phase-9 effort).
- `crates/brain-server/tests/dispatch.rs` + `tests/subscribe.rs` —
  the wire-driving pattern we already use in 9.10 / 9.11 tests.

**Phase doc:** orientation §11 sub-task **9.17**.

**Done when:** an in-process E2E test spins up shards + connection
listener + admin server; drives a full client lifecycle — handshake
→ ENCODE → RECALL → FORGET → RECALL → BYE — across the wire; and
asserts wire-shape correctness at each step. Multi-iteration
stability loop confirms the binary survives a thousand round-trip
operations without hanging.

---

## 1. What 9.17 actually verifies

The phase doc says "verify expected results". With the current dev
stack — `NopDispatcher` returns zero vectors, no real LLM, no real
auth — we **cannot** verify content-level correctness (e.g.
"RECALL returns the memory I encoded with relevance 0.95"). The
embedder produces the same vector for every text; cosine similarity
is degenerate; RECALL returns memories essentially at random.

What we *can* verify in v1:

- **Wire shape**: each request opcode → matching response opcode,
  CRCs valid, payload decodes, EOS bit set where expected.
- **Routing**: FORGET targeted at a memory routes to that memory's
  shard, ENCODE routes to the agent's bound shard.
- **Tombstone visibility** (sub-task 9.16): after FORGET, follow-up
  ops on the forgotten memory id return appropriate errors / empty
  results.
- **Shutdown discipline**: the client BYE flow + server shutdown
  signal both lead to a clean exit.
- **Stability under repeated ops**: 1000 encode/recall round-trips
  don't leak file descriptors, deadlock, or panic.

Content correctness is the spec §16/01 acceptance suite's job, and
those tests live in the respective crates (brain-ops, brain-planner)
with carefully-curated fixtures. 9.17 is the *wire* smoke test.

This re-scoping is honest: the phase-doc wording predates the
NopDispatcher decision. Documented up front in the test file's
header comment so a future contributor doesn't try to add semantic
assertions that can't hold.

---

## 2. Implementation choice — in-process, not subprocess

**Subprocess option**: `cargo run --bin brain-server -- --config …`
in a child process; client connects to its bound port.

- ✓ Closer to "real" deployment.
- ✗ Sluggish (start-up cost ~3 s including arena/wal init), brittle
  (port collision risk, process leak on test failure, log noise in
  CI), and needs a separate `cargo build --bin brain-server` step.
- ✗ Subprocess exit-code observation needs `Command` plumbing
  that's overkill for "did the server crash?".

**In-process option**: spin up `ConnectionListener` + shards + admin
server on `127.0.0.1:0` inside the test, exactly like 9.10's
`tests/dispatch.rs`. The new sub-task just exercises a richer
client flow against the same scaffold.

**Choice: in-process.** Sub-tasks 9.10 / 9.11 / 9.13 / 9.14 already
proved the in-process pattern is reliable and fast (~5 s per test).
A separate "subprocess smoke" can be added as a follow-up if
operators want it; for the Phase 9 exit gate, in-process is
sufficient.

---

## 3. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/tests/e2e.rs` | new — single binary with one big happy-path test + a stability-loop test | ~600 |

We do *not* extract a shared `tests/util/` helper crate for the
scaffold yet — sub-tasks 9.10–9.14 each duplicated their own
`start_with_shards` flavour, and unifying them would be a separate
refactor sub-task. 9.17 follows the same pattern: duplicate the
fixture code, deal with the duplication in a follow-up.

---

## 4. Tests

### 4.1 `encode_recall_forget_recall_round_trip`

The headline E2E. One client over one connection drives:

1. TCP connect → HELLO → WELCOME → AUTH → AUTH_OK.
2. ENCODE_REQ("the cat sat on the mat") on stream 1 →
   ENCODE_RESP or ERROR. If ENCODE_RESP, capture `memory_id`.
3. RECALL_REQ("cat") on stream 3 → RECALL_RESP with EOS. Body may
   be empty (NopDispatcher) or contain entries; we don't assert
   content.
4. (If ENCODE_RESP succeeded) FORGET_REQ(memory_id) on stream 5 →
   FORGET_RESP or ERROR. Routing is exercised: the memory_id
   carries shard=0 (the agent's bound shard); FORGET lands on the
   right shard.
5. RECALL_REQ("cat") on stream 7 → RECALL_RESP. Body may differ
   from step 3 (the encoded memory is no longer there) — we
   don't insist on a specific delta because the NopDispatcher
   returns identical embeddings.
6. BYE on stream 0 → server BYE → connection close.

All assertions are wire-shape: opcode matches, EOS where expected,
CRC valid (implicit via `Frame::decode_with_max`), decode parses
without error.

### 4.2 `repeated_encode_recall_is_stable`

Spec-doc "smoke passes reliably" → re-run a tight loop:

1. Handshake.
2. 100 × (ENCODE + RECALL) on rotating stream ids (1, 3, 5, …
   mod 1024).
3. BYE.

Assertion: every round-trip completes without timeout, error, or
panic. The test exits within 30 s. Combined with the standard
test-binary cleanup, this catches FD leaks, slow-path memory
growth, and the long-tail subscriber-style hangs we saw in 9.11.

100 iterations × 2 ops = 200 round-trips. At ~5 ms per op on the
NopDispatcher path that's ~1 s real time. Generous 30 s timeout
absorbs CI variance + arena init.

### 4.3 `metrics_endpoint_reflects_traffic`

The admin server should expose the new traffic via the connection
counters introduced in 9.13:

1. Handshake + a few ENCODEs.
2. `GET /metrics` on the admin port.
3. Assert `brain_connections_total >= 1` and
   `brain_connections_active >= 1` (or `== 0` if the data-plane
   client closed before the scrape).

Smoke-checks that the wire + admin integrations don't drift in
silence.

### 4.4 `bye_and_shutdown_drain_cleanly`

End-of-test discipline: the data-plane client sends BYE, the test
calls `server.stop()`, observes:

1. `BYE` opcode echoed back on the data-plane connection.
2. `serve()` task returns within 2 s.
3. Shard joiners return within `DEFAULT_SHARD_DRAIN_BUDGET`.
4. The test binary itself exits without leaking sockets.

This is essentially a re-run of 9.14's `shutdown_shards_returns_within_budget`
but in the full-server fixture; it catches integration regressions
where one of 9.10's per-conn task / 9.11's bridge task / 9.13's
admin server didn't observe the shutdown signal in time.

---

## 5. What goes in the e2e.rs scaffold

The scaffold mirrors `tests/dispatch.rs`'s `start_with_shards` +
helper soup. Specifically:

- `spawn_shard` × N shards.
- Build `Topology` + `Arc<ArcSwap<RoutingTable>>`.
- `Arc<ConnectionMetrics>`.
- `ConnectionListener::new(...).bind()?.serve()` on a `tokio::spawn`.
- `AdminServer::new(...).bind()?.serve()` on another `tokio::spawn`.
- A teardown `Server::stop()` that signals shutdown, awaits both
  servers' JoinHandles, drops the handles vector, then runs
  `graceful_shutdown_shards(...)`.

Client helpers (re-used from dispatch.rs's pattern):

- `complete_handshake(stream, agent_id)`.
- `send_frame(stream, frame)`.
- `read_one_frame(stream)`.
- `simple_encode(stream, text)`, `simple_recall(stream, cue)`,
  `simple_forget(stream, memory_id)`, `simple_bye(stream)`.

All in one file — no shared `tests/util/` extraction.

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| NopDispatcher's zero-vector embedding makes RECALL results meaningless | Documented limitation; 9.17 doesn't assert on content. The wire-shape assertions are the actual contract. |
| ENCODE may return ERROR on the NopDispatcher path (no real writer for some tests) | We branch: if ENCODE_RESP, run the follow-up FORGET on the returned memory_id; if ERROR, log + skip that step. Either way the binary survives the test. |
| Brain-server tests already aggregate to ~150 tests; one more 600-LOC binary slows CI | E2E is 4 tests; total wall-time ~30 s. Acceptable for the phase exit gate. |
| Shutdown drain budget timer fires under heavy CI load | 9.14's tests already cover budget edge cases; 9.17 uses the same fixture so any flakes there surface here too. We monitor. |
| Connection counter is shared between the data-plane test and the admin scrape, so `active` count is timing-sensitive | We assert `>= 1` or `<= N`, not exact values. |

---

## 7. Done criteria

- [ ] `crates/brain-server/tests/e2e.rs` ships with 4 integration tests.
- [ ] All 4 tests pass under `just docker-verify`.
- [ ] No regressions in any prior test binary.
- [ ] Phase doc 9.17 marked `[x]`.

---

## 8. What 9.17 explicitly defers

- **Subprocess smoke** — would exercise the real `main()` binary and
  signal-handler flow. Add as a follow-up CI job after 9.18.
- **Content-correctness assertions** — needs a real Dispatcher
  (Phase 6's BGE-small wiring is plumbed but the shard scaffold
  uses NopDispatcher; spec §05+§06 has the real path). v2.
- **SDK-driven E2E** — `brain-sdk-rust` is a Phase 13 effort
  (`spec/13_sdk_design/`). When that lands, the test can switch
  from hand-rolled frame helpers to the SDK without changing the
  scaffold.
- **Multi-client concurrency** — current tests use one client at a
  time. Multi-client + multi-shard fan-out is a stability test
  shape, not a smoke test; add as a 9.18 nice-to-have.
- **Cluster-mode E2E** — single-node only in v1.
- **Crash recovery cycle** — kill mid-encode + restart should
  recover via WAL. That's spec §16/06's acceptance criterion, not
  a smoke test; add as its own phase.

---

*Implement on approval.*
