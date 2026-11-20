# 19.05 Acceptance Test Suite

The structured test suite that v1 must pass for release.

## 1. The structure

Tests are organized into categories:

- **Unit tests** (in-source, per module).
- **Integration tests** (cross-module).
- **End-to-end tests** (full node via wire protocol).
- **Performance tests** (benchmarks).
- **Chaos tests** (failure injection).
- **Soak tests** (long-running).

All categories must pass for release.

## 2. Unit tests

Per-module Rust unit tests:

```bash
cargo nextest run --workspace     # the sweep
cargo test --workspace --doc      # doctests, which nextest does not run
```

Both are required: `nextest` does not execute doctests, so a workspace run that omits the second command leaves every documented example unverified. `just test` runs the pair.

Coverage target: ≥ 70% line coverage on Brain code (excluding test code itself).

Specific areas with required coverage:
- Wire protocol parsing.
- WAL append and read.
- Arena slot allocation.
- HNSW search.
- Recovery procedures.

## 3. Integration tests

Cross-module tests, in Brain's test harness:

```bash
cargo test --test integration_*
```

Examples:
- `integration_wal_metadata.rs`: WAL writes propagate to metadata.
- `integration_arena_hnsw.rs`: arena writes are visible to HNSW.
- `integration_recovery.rs`: full recovery from various crash points.

## 4. End-to-end tests (E2E)

Full node via wire protocol. These are Rust integration tests against a real server, not a shell harness — they run inside the workspace sweep:

```bash
cargo nextest run -p brain-server
```

Each spins up:
- A real Brain process.
- Multiple concurrent clients.
- Drives various operations.

Verifies:
- Wire protocol correctness.
- Behavior end-to-end.
- Multi-client concurrency.
- Authentication/authorization.

## 5. The "smoke test"

A short, fast check of basic functionality. Two forms of it ship:

- **Release-build smoke** — `cargo build --release --bin brain-server`, part of `just prod-verify` and of CI, which catches a release-only build break that a debug test sweep would miss.
- **Container smoke** — the image-publish workflow starts the built image and waits for its health check to report healthy before the image is pushed, so a container that cannot come up never ships.

The behavioural round trip the steps below describe is covered by the integration tests of §4 rather than by a separate script:

1. Start Brain.
2. Connect a client.
3. ENCODE a memory.
4. RECALL with the same text.
5. Verify the memory is returned.
6. FORGET it.
7. RECALL again.
8. Verify it's gone.
9. Stop Brain.

Run on every PR.

## 6. The "feature parity" tests

For each operation in the spec:
- A test exists.
- It exercises the operation's contract.
- Edge cases are covered.

The list:
- ENCODE: 12 tests (basic, with metadata, idempotency, error cases, etc.).
- RECALL: 15 tests (basic, with filters, with text, edge cases).
- PLAN: 8 tests.
- REASON: 8 tests.
- FORGET: 10 tests (soft, hard, grace, force_reclaim).
- LINK / UNLINK: 6 tests.
- TXN_*: 6 tests.
- SUBSCRIBE: 5 tests.
- LLM_CREDENTIAL_*: 13 tests (put / get / delete / verify, scope targeting, permission-bit refusal, transport refusal, non-delegability).
- ADMIN_*: 10 tests.

Total: ~80 feature tests.

## 7. The "data correctness" tests

Verify data integrity:
- After ENCODE, the data is queryable.
- After FORGET, the data is gone.
- After UNLINK, the edge is gone.
- After restart, all of the above persist.

Each operation has a "round-trip" test: do it, verify the effect, sometimes also verify the inverse.

## 8. The "edge case" tests

For each operation:
- Empty inputs.
- Maximum-size inputs.
- Invalid inputs.
- Concurrent inputs.
- Idempotent retries.
- Authentication failures.

## 9. Concurrency tests

Multi-client scenarios:
- 100 concurrent ENCODEs to the same shard.
- 100 concurrent RECALLs.
- Mixed reads and writes.
- Verify no data corruption, no lost operations.

Run for 10 minutes; verify final state matches expectations.

## 10. Recovery tests

Crash recovery:
- For each persistence operation, kill Brain at random points.
- Verify recovery is correct.
- Run 1000 iterations per operation.

Fault injection during recovery:
- Recovery should be idempotent (multiple attempts converge).
- Test by interrupting recovery itself.

## 11. On-disk format tests

Every persistent artifact carries a format version in its header, and a mismatch is a **refusal, not a best-effort parse**:

- HNSW snapshot: `FORMAT_VERSION = 3`; a differing version returns `HeaderError::UnsupportedVersion` before any payload is parsed, and ahead of the CRC check (`crates/brain-index/src/persistence.rs:53-61,142-163`).
- WAL segment: `WAL_SEGMENT_FORMAT_VERSION_V1 = 1` (`crates/brain-storage/src/wal/segment.rs:47`).
- PQ codebook: its own version constant, checked on decode (`crates/brain-index/src/pq/codebook.rs:133,161`).

Tests to hold:
- Load current-format data with the current binary: works.
- Load data whose header version differs: rejected with the version error, naming the version found — never silently reinterpreted under the current layout.
- Corrupt a header's version field and recompute its CRC: still rejected, proving the version gate is independent of the integrity check.

The version check exists so that an incompatible layout fails loudly at open. There is no in-place converter and no conversion tool: a format change means a rebuild of the derived artifact from authoritative state, which §10 of [`06_complete_acceptance.md`](06_complete_acceptance.md) already requires of every derived index.

## 12. Performance tests

The benchmarks defined in [02. Performance Targets](02_performance_targets.md).

Two of them assert their targets and run nightly on a schedule (and on manual dispatch), with a miss panicking out of the bench so the workflow goes red:

```bash
cargo bench -p brain-planner --bench relation_traverse   # §2.4 traversal p50s
cargo bench -p brain-index   --bench lexical_retrieve    # §2.9 lexical path
```

`just prod-bench` runs that pair locally. The remaining benches report rather than assert, and are operator-run:

```
brain-embed       throughput         # embedder latency + concurrent rate; no absolute gate
brain-server      embed_contention   # embed vs shard-reactor contention, per pinned shard
brain-metadata    entity_resolve, statement_ops, relation_ops, schema_ops, audit_ops
brain-extractors  pattern_extract, llm_pipeline
brain-planner     retrieval, vsa_ops
brain-protocol    frame_codec
brain-storage     crc32c
brain-http        router, sse_encoder, end_to_end
```

Two of these carry environment preconditions rather than defaults: `throughput` and `embed_contention` both skip unless an embedding-model directory is named, because neither can run without real model weights. `embed_contention` is one process per measurement — it reports the intra-op width its own placement produced alongside the throughput and shard-side scheduling latency it measures, which is why it is not a Criterion routine.

## 13. Chaos tests

Failure injection:

- Kill Brain at random points.
- Inject I/O errors.
- Inject network failures.
- Inject memory pressure.
- Inject corruption.

For each: verify Brain behaves per spec (recovers, fails gracefully, etc.).

## 14. Soak tests

Long-running stability:

- 48 hours of continuous load.
- 1 week of moderate load (less common).
- Verify:
  - No memory leaks.
  - No accumulating errors.
  - No latency drift.

## 15. Compliance tests

For deployments needing compliance:

- Audit log integrity.
- Access control enforcement.
- Encryption at rest (when configured).
- Encryption in transit (TLS).

## 16. Wire-protocol conformance tests

Brain ships no client library; the contract a client must satisfy is the wire protocol itself. The conformance corpus pins that contract:

- **Golden fixtures.** Each fixture is a `(opcode, field-map) → request bytes → response bytes` triple. The request and response payloads are valid CBOR decodable by any standards-conformant CBOR reader — no Brain-supplied library required.
- **Byte-reproducibility.** Under the deterministic profile, encoding a given `(opcode, field-map)` reproduces the golden request bytes exactly, and the same request reproduces the golden response bytes exactly (round-trip byte-for-byte).
- **Cross-client interoperability.** A memory written via one client is readable by any other, because the on-wire bytes are canonical: the fixtures define the one correct encoding, and any conformant client converges on it.

## 17. Documentation tests

Documentation correctness:
- Code examples in docs compile.
- Code examples run and produce expected output.
- Configuration examples are valid.

## 18. Security tests

- Fuzzing the wire protocol.
- Authentication bypass attempts.
- Resource exhaustion attempts.
- Verify Brain handles all gracefully.

## 19. The "release blocker" gating

For release:

```
GATE 1: Unit tests pass (100%).
GATE 2: Integration tests pass.
GATE 3: E2E tests pass.
GATE 4: Smoke test passes.
GATE 5: Performance targets met (or documented exceptions).
GATE 6: Chaos tests pass.
GATE 7: Soak tests pass (at least 48h).
GATE 8: Compliance tests pass.
GATE 9: Documentation tests pass.
GATE 10: Security tests pass (no findings).
```

All gates must pass. Exceptions need explicit waiver from the release manager.

## 20. The CI orchestration

Tests run in CI:

- **Per-PR**: unit, integration, smoke, fast E2E. ~15-30 minutes.
- **Per-merge to main**: above + full E2E + light performance. ~1-2 hours.
- **Nightly**: all tests including soak, chaos. ~6-12 hours.
- **Pre-release**: full suite + extended soak. ~24-48 hours.

This staged approach catches issues early without blocking development.

## 21. Entity layer acceptance

The entity slice of the typed graph carries its own acceptance set; statements, relations and queries carry theirs.

### 21.1 Unit tests

```bash
cargo test -p brain-core knowledge
cargo test -p brain-metadata --lib
cargo test -p brain-protocol --lib
cargo test -p brain-protocol --test conformance
```

Must include:

- Resolver tier 1 (exact / alias), tier 2 (trigram fuzzy), tier 5 (created) outcome cases.
- Resolver adversarial-input cases: Unicode multi-byte, empty / whitespace-only candidates, very-long candidates, and pathological trigram inputs like `"aaaaa..."` — the input class where a trigram index degenerates to a near-full scan.
- `MergeRecord` v2 round-trip (rkyv archive + commit/read).
- `entity_merge_ops::merge_entity` + `unmerge_entity` pre-conditions and happy paths.
- `Person` attribute encode / decode round-trip.
- `ClientErrorEntityExt::entity_error` categorisation.

### 21.2 Integration tests

```bash
cargo test -p brain-server --test knowledge_entity_wire
cargo test -p brain-server --test knowledge_entity_merge_wire
cargo test -p brain-server --test knowledge_entities_phase_exit
cargo test -p brain-server --test knowledge_compat
cargo test -p brain-protocol --test conformance_typed_graph
```

Required to pass:

- Full lifecycle: create → get → update → rename → merge → unmerge → rename → list → tombstone (`knowledge_entities_phase_exit.rs`).
- All 9 wire opcodes end-to-end (`knowledge_entity_wire.rs` + `knowledge_entity_merge_wire.rs`).
- wire-conformance round-trips for every opcode (golden fixtures).
- Schemaless mode regression (`knowledge_compat.rs`) — knowledge tables stay empty when no schema is declared.

### 21.3 Performance benches

```bash
cargo bench -p brain-metadata --bench entity_resolve
```

Targets per [§2.2](02_performance_targets.md) at 100K entities:

- `tier1_exact_lookup`: p50 ≤ 1 ms, p99 ≤ 2 ms.
- `tier1_alias_lookup`: p50 ≤ 1 ms, p99 ≤ 2 ms.
- `tier2_full_resolve`: p50 ≤ 5 ms, p99 ≤ 30 ms.

`entity_resolve` is an operator-run bench: it reports, it does not assert. The two benches that assert their targets in-process and run nightly are `relation_traverse` and `lexical_retrieve` (§12).

### 21.4 Cross-acceptance

Verify that Brain's existing acceptance suite still passes with the typed graph enabled — `cargo test --workspace` and the existing correctness / performance / durability checks remain green.

---

*Continue to [`06_complete_acceptance.md`](06_complete_acceptance.md) for the combined acceptance gate.*
