# 15.07 Chaos Testing

How Brain tests its failure-handling — the methodology and the tooling.

## 1. The principle

You can't trust failure-handling code without testing it. Brain's CI includes systematic failure injection — chaos testing.

## 2. The categories

- **Process kills**: random crashes during operations.
- **Disk failures**: I/O errors injected at random points.
- **Network failures**: packet loss, latency, disconnection.
- **Resource exhaustion**: OOM, CPU starvation.
- **Time anomalies**: clock skew, jumps.
- **Concurrency**: races, deadlocks.
- **Corruption**: bit flips in WAL, arena, metadata.

Each is exercised systematically.

## 3. The chaos framework

Brain ships a chaos-testing tool:

```bash
brain-chaos run --scenario crash-during-encode --duration 60s
```

The tool:
- Spins up a real Brain instance.
- Drives realistic load.
- Injects failures.
- Monitors behavior.
- Verifies invariants.

## 4. Crash testing

Repeatedly:
1. Start a substrate.
2. Apply mixed workload (encodes, recalls).
3. At random time during the workload: send SIGKILL.
4. Restart.
5. Verify:
   a. Recovery completes.
   b. All committed operations are durable.
   c. No half-committed state.

For 1000 iterations: must succeed in all.

## 5. The "kill at every byte" exhaustion

Advanced testing: kill the substrate after every byte of WAL write, every metadata write, etc.

This exhaustively covers torn-write scenarios. Run on dedicated test infrastructure; not part of every CI run.

Each iteration:
- Apply a single operation.
- Kill at byte N of the WAL write (or other persistence operation).
- Recover.
- Verify state consistency.

Catches issues at the boundary between "operation succeeded" and "didn't quite finish".

## 6. Disk fault injection

Using FUSE or similar:
- Injected I/O error at chosen byte offsets.
- Slow I/O (1-second latencies).
- Corrupted reads (bit flips).

The substrate should:
- Detect failed I/O via error codes.
- Detect corruption via CRCs.
- Respond appropriately (fail operation, halt, etc.).

Tools: `iox`, `pfault`, custom FUSE filesystem.

## 7. Network fault injection

Using `tc` (traffic control) on Linux:
- Add latency.
- Drop packets.
- Disconnect entirely.

Tests:
- The substrate's handling of slow clients.
- Connection retries.
- Timeouts working correctly.

## 8. Resource exhaustion

Using `cgroups`:
- Cap memory.
- Cap CPU.
- Cap disk space.

Tests:
- Memory pressure handling.
- CPU contention behavior.
- Disk-full responses.

## 9. Time anomalies

Mock the clock for tests:

```rust
let fake_clock = FakeClock::new();
fake_clock.set("2026-05-07T00:00:00Z");
fake_clock.advance(Duration::from_secs(60));
fake_clock.jump_backward(Duration::from_secs(3600));    // 1 hour back
```

Tests verify Brain handles time jumps gracefully (ordering preserved via monotonic time, etc.).

## 10. Concurrency testing

Tools:
- `loom` for Rust concurrency model checking.
- `miri` for unsafe-code checking.
- Custom stress tests with high contention.

These catch races, deadlocks, ordering bugs.

## 11. Corruption injection

Programmatic:
- Flip random bits in WAL files.
- Truncate WAL files at random points.
- Corrupt metadata file.

The substrate should:
- Detect via CRC.
- Refuse to continue / fail safely.
- Log appropriately.

## 12. The invariant checker

During chaos tests, an invariant checker continuously verifies:

- Memory count ≥ 0.
- Sum of slot states (active + tombstoned + free) = total slots.
- Each metadata row has corresponding arena slot.
- Each HNSW node has corresponding active memory.
- Etc.

Invariant violations indicate bugs to fix.

## 13. The "expected" vs "unexpected" failures

Some failures are expected:
- A killed substrate, when restarted, should recover.
- An OOM under heavy load, with backpressure, should shed cleanly.

Unexpected failures (panics, hangs, data loss in non-DR scenarios) are bugs.

The chaos suite distinguishes:

- ✓ Failed safely: expected.
- ✗ Crashed unexpectedly: bug.
- ✗ Lost data: bug.
- ✗ Returned wrong results: bug.

## 14. The "production-like" testing

Chaos tests should match production:

- Realistic data sizes.
- Realistic concurrent loads.
- Realistic operation mix.
- Realistic configurations.

Synthetic, undersized tests miss bugs that surface only at scale.

## 15. The CI cadence

Different test suites at different cadences:

- **Per commit**: smoke tests including basic chaos. ~5-10 minutes.
- **Nightly**: full chaos suite. ~hours.
- **Weekly**: extended chaos (long durations, edge cases). ~days.
- **Pre-release**: maximum chaos (every variation). ~days.

This balances coverage with CI cost.

## 16. The "production chaos" (advanced)

Some teams run chaos in production:

- Random shard restarts.
- Synthetic load tests.
- Latency injection.

Brain doesn't ship a production-chaos tool; some integrations exist (Litmus, Chaos Monkey). For deployments wanting this, integrate via the substrate's admin API.

## 17. The "game days"

Periodically, ops teams practice DR:

- Take a deployment.
- Inject a major failure.
- Recover.
- Document.

Game days ensure:
- DR procedures work.
- The team knows them.
- Tooling is in place.

Brain provides the substrate; the operational practice is up to the team.

## 18. The chaos as documentation

The chaos tests document expected behavior:

- "When we kill mid-write, recovery succeeds."
- "When the disk fails, we report errors but don't lose data."
- "When the embedder is unavailable, ENCODE fails but RECALL with cached cues works."

A new team member reads the chaos tests to understand expected behavior under failure.

## 19. The "regression"

When a bug is found and fixed:

- Add a chaos test that reproduces it.
- Verify the fix passes.
- Add to the regression suite.

This prevents the bug from returning.

## 20. The honest claim

Brain does extensive chaos testing. Failures are reproducible, recoverable, and documented.

But:
- Bugs slip through testing.
- Real-world conditions vary from test conditions.
- Edge cases compound.

The substrate is well-tested but not perfect. Operational vigilance (monitoring, alerts, runbooks) is the second line of defense.

Brain trusts its tests; trusts its operators more.

---

*Continue to [`08_open_questions.md`](08_open_questions.md) for unresolved questions.*
