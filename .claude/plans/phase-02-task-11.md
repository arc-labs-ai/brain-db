# Phase 2 — Task 2.11: Random-kill recovery test

**Classification:** moderate. A single integration test file (~250 lines) that loops over many seeded iterations, but the contract being tested is the phase's load-bearing durability promise: *recovery never loses an acked record and never invents one.*

**Spec:** `spec/16_benchmarks_acceptance/06_durability_criteria.md` (full — particularly §§2, 3, 9 covering durability after crash and recovery completeness).

## 1. Scope

Add `crates/brain-storage/tests/random_kill.rs` — an integration test (i.e. lives in `tests/`, not `src/#[cfg(test)]`) that for many random seeds:

1. Creates a fresh shard (arena + WAL).
2. Writes N records via `Wal::append` (each ack'd → durable).
3. Cleanly shuts down the WAL.
4. **Simulates a crash** by truncating the segment file at a random byte offset.
5. Reopens the arena and runs `recover()`.
6. Verifies that:
   - Every record whose encoded bytes fit entirely inside the truncated file is recovered.
   - No record that was cut off is recovered.
   - The recovered LSN set is contiguous (no gaps, no extras).

Done when:

- [x] 1000 iterations, 0 failures.

In:

- One integration test file under `tests/`.
- A small hand-rolled LCG (no `rand` dep) so each iteration is fully deterministic by seed.
- Failure messages print the seed for reproducibility (per phase doc).
- A few hand-picked "sentinel" truncation points (truncate at the header end → 0 records; truncate at file size → all records; truncate exactly between two records → first half) included as a smoke test alongside the seeded loop.

Out:

- **Real-process subprocess kill.** Phase doc says "drops handles abruptly"; we interpret as "simulate the post-crash file state." A real `kill -9` test would mean spawning a child via `std::process::Command`, which complicates fixtures and adds OS-coupling for marginal coverage. The file-truncation simulation captures the exact invariant the spec cares about (§16/06 §3: "all that received success are durable"). Subprocess-kill testing is a Phase 9 / staging-environment concern.
- **Concurrent appenders.** Our `Wal::append` is `&mut self` (per SD-2.9-1). The phase doc's "concurrent operations" wording is reinterpreted as "many records in rapid sequence via one writer" — which is what spec §16/06 §3's contract requires (the group committer batches them internally; concurrency at the `pwritev2` boundary is the relevant level). The concurrency *within* the committer was already exercised in 2.8's `batching_amortizes_fsyncs` and `many_concurrent_records_all_durable` tests.
- **Random kinds.** All test records are `Encode` (the primary case per §16/06 §2). Random variation lives in slot index, payload bytes, agent_id, context_id, timestamp.
- **Random N per iteration.** N is fixed at 100 per phase doc. The randomness is in the seed for content + the truncation point.

## 2. Spec quotes that bind the design

> **§16/06 §2 (WAL durability):** "Run 1000 iterations; expect 100% success."
>
> **§16/06 §3 (group commit durability):** "Tested: 100 concurrent ENCODEs; kill mid-batch. All that received success are durable."
>
> **§16/06 §9 (recovery completeness):** "Apply 10K operations; kill at random points; restart; verify state matches expected."
>
> **§16/06 §20 (combined certification):** "No data loss for committed operations. No corruption. No state machine bugs."
>
> **§05/08 §4:** "If CRC fails, this is the truncation point — stop here." (The contract `recover()` relies on; the random-kill test validates it across many torn-write offsets.)

## 3. Design decisions

### 3.1 Why file truncation, not real `kill -9`

| Option | Verdict | Why |
|---|---|---|
| **A. File truncation at a random byte (chosen)** | ✓ | Exactly the post-crash file state the spec cares about: "the kernel got partway through `pwritev2`". Deterministic, fast, no OS coupling. The contract being verified is: *recovery handles any prefix of the WAL as if everything past the cut was never written*. |
| B. `kill -9` on a subprocess | ✗ | More setup; OS-specific signal handling; flaky in tmpfs/Docker; adds little signal — `pwritev2(RWF_DSYNC)` already guarantees that ack'd-bytes survive a kernel-level kill. The interesting case (torn writes) is exactly what truncation simulates. |
| C. Hook into `flush_durable` to fail mid-write | ✗ | Needs a test-only seam; less faithful to spec semantics. |
| D. Use proptest with shrinking | ✗ | The failure isn't useful to shrink — the only knob is the truncation offset, and proptest's shrinking would slow each failure-recovery cycle. We print the seed on panic, which is sufficient for reproduction. |

### 3.2 RNG choice

Hand-rolled LCG (single `u64` state). Avoids pulling `rand` into dev-deps (which would then transit through `getrandom`/`zerocopy`/etc.). The LCG's quality is fine for this property test — we're not doing cryptography, just deterministic generation of records and truncation offsets.

```rust
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state
}
```

(Numerical Recipes constants, standard for non-cryptographic deterministic RNGs.)

### 3.3 Seed convention

`base_seed + iter * golden_ratio_u64` per iteration. Failure messages print both the iteration number and the seed. A future targeted re-run can run `BRAIN_KILL_TEST_SEED=<hex>` against a one-shot test invocation (env var, parsed at test entry).

### 3.4 Test runtime

Per iteration cost:
- Tempdir setup: ~1 ms.
- `Wal::create` + 100 `append` (each `pwritev2(RWF_DSYNC)`): ~10–30 ms on docker tmpfs (each fsync ~50–300 µs).
- Truncation: <1 ms.
- `recover` (scan 100 records): ~1–3 ms.

Estimated: ~20–40 ms × 1000 iterations = **20–40 seconds**. Acceptable for `cargo test` (CI), but slow enough that I'll consider an env-var to override iteration count if it bites. For 2.11 default: run all 1000.

### 3.5 Determinism of `expected_count`

After writing N records sequentially via `Wal::append`, the segment file's layout is:

```
[ 4 KiB header | record_1 bytes | record_2 bytes | ... | record_N bytes ]
```

(No padding — we deviated from O_DIRECT padding in SD-2.8-1.) So records pack tightly. Computing `expected_count` for a given truncation offset is straightforward:

```rust
let mut cursor = WAL_SEGMENT_HEADER_LEN as u64;
let mut expected = 0u64;
for &len in &record_lens {
    if cursor + len as u64 <= trunc_offset {
        expected += 1;
        cursor += len as u64;
    } else {
        break;
    }
}
```

The `<=` is important — a truncation exactly *at* the end of a record means that record is fully present.

### 3.6 Sentinel cases

Before the random loop, run three deterministic sentinel cases that cover boundary conditions:

| Truncation point | Expected count |
|---|---|
| `WAL_SEGMENT_HEADER_LEN` | 0 (just the header; no records) |
| `WAL_SEGMENT_HEADER_LEN + sum(record_lens[0..50])` | 50 (exact boundary mid-stream) |
| `file_size` | 100 (no truncation; everything survives) |

These exercise edge cases the random RNG might not hit.

### 3.7 Why all-Encode records

Phase doc §16/06 §2's primary case is ENCODE. Forget/Reclaim/etc. don't add coverage to the random-kill property — they share the WAL framing layer with Encode. Keeping the record type uniform also keeps the test's `expected_count` computation simple (we just sum per-record encoded lengths).

## 4. Architecture

### 4.1 Files

- `crates/brain-storage/tests/random_kill.rs` (new, ~280 lines).
- `crates/brain-storage/Cargo.toml` — add `brain-core` to `[dev-dependencies]` (the test constructs `MemoryId`, `AgentId`, etc. directly).

### 4.2 Test layout

```rust
// crates/brain-storage/tests/random_kill.rs

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_storage::arena::ArenaFile;
use brain_storage::recovery::{recover, InMemoryMetadataSink};
use brain_storage::wal::{
    EncodePayload, Lsn, Wal, WalPayload, WalRecord, WAL_SEGMENT_HEADER_LEN,
};

const N_RECORDS: u64 = 100;
const ITERATIONS: u64 = 1000;
const BASE_SEED: u64 = 0xBADC0FFEE0DDF00D;
const GOLDEN: u64 = 0x9E3779B97F4A7C15;

fn rng_next(state: &mut u64) -> u64 { /* LCG */ }
fn gen_record(rng: &mut u64, slot: u64) -> WalRecord { /* Encode */ }
fn shard_uuid_from_seed(seed: u64) -> [u8; 16] { /* deterministic non-zero */ }

fn run_iteration(seed: u64) -> Result<(), String> {
    // Setup tempdir + arena.
    // Write N records via Wal::append, collect record_lens.
    // Wal::shutdown.
    // Pick truncation offset in [HEADER_LEN, file_size].
    // Compute expected_count.
    // ArenaFile::open + recover().
    // Verify report.records_replayed == expected_count, sink LSNs match.
}

#[test]
fn sentinel_truncation_points() {
    // 3 boundary cases (header end / exact boundary / no truncation).
}

#[test]
fn random_kill_recovery_1000_iterations() {
    let mut failures: Vec<String> = Vec::new();
    for iter in 0..ITERATIONS {
        let seed = BASE_SEED.wrapping_add(iter.wrapping_mul(GOLDEN));
        if let Err(e) = run_iteration(seed) {
            failures.push(format!("iter {iter} seed={seed:#018x}: {e}"));
            if failures.len() >= 5 { break; }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures observed (first 5 shown):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
```

### 4.3 Verification assertions

For each iteration:

1. `report.records_replayed == expected_count`.
2. `sink.applied().len() as u64 == expected_count`.
3. The sink's LSN keys equal `1..=expected_count` exactly (proves contiguity).
4. `report.records_discarded == 0` (no TXN markers in the test).
5. `report.records_skipped == 0` (fresh sink, `durable_lsn = 0`).

## 5. Trade-offs

| Question | Choice | Why |
|---|---|---|
| File truncation vs subprocess kill | Truncation | §3.1 — same invariant, no OS coupling. |
| RNG | Hand-rolled LCG | No new dep. |
| `proptest` vs raw loop | Raw loop | Shrinking on this property isn't useful; raw loop is faster and prints seeds. |
| Iteration count | 1000 (phase doc) | If runtime hurts, add env var override; not in 2.11 scope. |
| Record kind variety | All Encode | Encode + record framing is the primary contract; other kinds share the framing. |
| Concurrent appenders | No | API is `&mut self` (SD-2.9-1); group-commit batching already tested in 2.8. |

## 6. Risks

- **Test runtime.** 1000 × 100 fsyncs ≈ 20–40 sec. Acceptable for CI. If it bites, add `BRAIN_KILL_TEST_ITERS=NNN` env-var override; not blocking 2.11.
- **Tempdir cleanup on each iteration.** `tempfile::TempDir` cleans up via Drop. 1000 dirs over the test run, never simultaneously alive. Fine.
- **Truncation-point bias.** LCG might cluster offsets near the file end (low-bit bias). Mitigate by using high bits of the LCG output for the offset selection.
- **Determinism across architectures.** LCG is deterministic per-seed; tempdir paths differ but don't enter the RNG. Test should be deterministic across x86_64 / aarch64.
- **Failure noise on torn-write detection.** If `WalReader` reports `MidSegmentCorruption` instead of clean truncation, the test would fail with a different error. Truncation always occurs in the *last* (and only) segment, so 2.7's tail-vs-mid rule treats it as a clean end. Verified in 2.7 + 2.8 + 2.10 tests; reproduces here.

## 7. Test plan

The whole task is a test plan; assertions per §4.3.

Sanity tests (besides the main 1000-iter test):

- `sentinel_truncation_points`: three deterministic boundary cases (no truncation, exact mid-stream boundary, header-only).

If `random_kill_recovery_1000_iterations` finds a failure, the printed seed lets a developer re-run a single iteration via a separate `cargo test -- exact-seed` invocation. Documented in a comment at the top of the file.

## 8. Estimated commit shape

One commit on `feature/brain-storage`:

> `test(brain-storage): random-kill recovery property (sub-task 2.11)`

Body covers:
- File truncation as the spec-faithful crash simulation.
- LCG-seeded determinism.
- Sentinel cases + 1000-iter loop.
- Phase doc 2.11 entry: check the box.

Files: as in §4.1. Dev-dep added: `brain-core` (path dep, already in regular deps; just adding to dev-dependencies block — though strictly speaking it's already accessible via the regular-dep path in `cargo test`; I'll verify and only add if needed).

Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container. The new integration test runs as part of `--all-targets`.

---

PLAN READY: see `.claude/plans/phase-02-task-11.md` — confirm to proceed.
