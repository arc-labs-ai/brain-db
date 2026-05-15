# 16.08 Acceptance Test Suite

The structured test suite that v1 must pass for release.

## 1. The structure

Tests are organized into categories:

- **Unit tests** (in-source, per module).
- **Integration tests** (cross-module).
- **End-to-end tests** (full substrate via wire protocol).
- **Performance tests** (benchmarks).
- **Chaos tests** (failure injection).
- **Soak tests** (long-running).

All categories must pass for release.

## 2. Unit tests

Per-module Rust unit tests:

```bash
cargo test --workspace
```

Coverage target: ≥ 70% line coverage on substrate code (excluding test code itself).

Specific areas with required coverage:
- Wire protocol parsing.
- WAL append and read.
- Arena slot allocation.
- HNSW search.
- Recovery procedures.

## 3. Integration tests

Cross-module tests, in the substrate's test harness:

```bash
cargo test --test integration_*
```

Examples:
- `integration_wal_metadata.rs`: WAL writes propagate to metadata.
- `integration_arena_hnsw.rs`: arena writes are visible to HNSW.
- `integration_recovery.rs`: full recovery from various crash points.

## 4. End-to-end tests (E2E)

Full substrate via wire protocol:

```bash
./test/e2e/run.sh
```

Spins up:
- A real substrate process.
- Multiple SDK clients.
- Drives various operations.

Verifies:
- Wire protocol correctness.
- Behavior end-to-end.
- Multi-client concurrency.
- Authentication/authorization.

## 5. The "smoke test"

A short, fast test of basic functionality:

```bash
./test/smoke.sh
```

Steps:
1. Start substrate.
2. Connect with SDK.
3. ENCODE a memory.
4. RECALL with the same text.
5. Verify the memory is returned.
6. FORGET it.
7. RECALL again.
8. Verify it's gone.
9. Stop substrate.

Should complete in < 30 seconds. Run on every PR.

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
- For each persistence operation, kill the substrate at random points.
- Verify recovery is correct.
- Run 1000 iterations per operation.

Fault injection during recovery:
- Recovery should be idempotent (multiple attempts converge).
- Test by interrupting recovery itself.

## 11. Migration tests

Schema and binary migration:
- Load v1 data with v1 binary: works.
- Load v1 data with v1.x binary (newer): works.
- Load v1.x data with v1 binary: read-only or error (forward compatibility limited).

For each release, define migration tests.

## 12. Performance tests

The benchmarks defined in [02-04 latency / throughput / resource targets](02_latency_targets.md):

- Run nightly.
- Compare to previous results.
- Flag regressions.

## 13. Chaos tests

Failure injection:

- Kill substrate at random points.
- Inject I/O errors.
- Inject network failures.
- Inject memory pressure.
- Inject corruption.

For each: verify the substrate behaves per spec (recovers, fails gracefully, etc.).

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

## 16. SDK tests

Each SDK has its own test suite:

- Unit tests of the SDK.
- Integration with the substrate.
- Idempotency, retry, streaming.
- Cross-SDK compatibility (a memory written via Rust SDK is readable via Python SDK).

## 17. Documentation tests

Documentation correctness:
- Code examples in docs compile.
- Code examples run and produce expected output.
- Configuration examples are valid.

## 18. Security tests

- Fuzzing the wire protocol.
- Authentication bypass attempts.
- Resource exhaustion attempts.
- Verify the substrate handles all gracefully.

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

## 21. Entity layer acceptance (phase 16)

Phase-16 exit gate covers the knowledge layer's entity slice (statements, relations, queries arrive in later phases with their own acceptance entries).

### 21.1 Unit tests

```bash
cargo test -p brain-core knowledge
cargo test -p brain-metadata --lib
cargo test -p brain-protocol --lib
cargo test -p brain-sdk-rust --lib
```

Must include:

- Resolver tier 1 (exact / alias), tier 2 (trigram fuzzy), tier 5 (created) outcome cases — phase 16.5.
- Resolver adversarial-input cases (Unicode multi-byte, empty / whitespace-only candidates, very-long candidates, pathological trigram inputs like `"aaaaa..."`) — phase 16.9.
- `MergeRecord` v2 round-trip (rkyv archive + commit/read) — phase 16.7.1.
- `entity_merge_ops::merge_entity` + `unmerge_entity` pre-conditions and happy paths — phase 16.7.2.
- `Person` attribute encode / decode round-trip — phase 16.8.1.
- `ClientErrorEntityExt::entity_error` categorisation — phase 16.8.4.

### 21.2 Integration tests

```bash
cargo test -p brain-server --test knowledge_entity_wire
cargo test -p brain-server --test knowledge_entity_merge_wire
cargo test -p brain-server --test knowledge_entities_phase_exit
cargo test -p brain-server --test knowledge_compat
cargo test -p brain-sdk-rust --test knowledge_entity
```

Required to pass:

- Full lifecycle: create → get → update → rename → merge → unmerge → rename → list → tombstone (`knowledge_entities_phase_exit.rs`, 16.9.3).
- All 9 wire opcodes end-to-end (`knowledge_entity_wire.rs` + `knowledge_entity_merge_wire.rs`).
- SDK mock-server round-trips for every builder (`knowledge_entity.rs`).
- Substrate-only mode regression (`knowledge_compat.rs`, 15.5) — knowledge tables stay empty when no schema is declared.

### 21.3 Performance benches

```bash
cargo bench -p brain-metadata --bench entity_resolve
```

Targets per [§02 §2.2](02_latency_targets.md) at 100K entities:

- `tier1_exact_lookup`: p50 ≤ 1 ms, p99 ≤ 2 ms.
- `tier1_alias_lookup`: p50 ≤ 1 ms, p99 ≤ 2 ms.
- `tier2_full_resolve`: p50 ≤ 5 ms, p99 ≤ 30 ms.

Bench is **manual** in phase 16 (operator-run). CI integration with regression thresholds lands in phase 14.

### 21.4 Cross-acceptance

Verify that the substrate's existing acceptance suite (substrate phase 14) still passes after the knowledge layer was added — `cargo test --workspace` and the §2-§19 checks remain green.

---

*Continue to [`09_open_questions.md`](09_open_questions.md) for unresolved questions.*
