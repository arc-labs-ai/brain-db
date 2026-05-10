//! Random-kill recovery property test.
//!
//! For 1000 deterministically-seeded iterations:
//!
//! 1. Create a fresh shard (arena + WAL).
//! 2. Append N records, each ack'd by `pwritev2(RWF_DSYNC)`.
//! 3. Clean-shutdown the WAL.
//! 4. Simulate a crash by truncating the segment file at a random byte
//!    offset (covers the "last `pwritev2` was partial" case as well as
//!    any cleanly-flushed prefix).
//! 5. Reopen and call `recover`.
//! 6. Assert that the set of recovered records is exactly the prefix that
//!    physically survived the truncation — no extras, no gaps.
//!
//! See `spec/16_benchmarks_acceptance/06_durability_criteria.md` §§2–3,9.
//!
//! ## Why file truncation, not `kill -9`
//!
//! The contract is purely about *file state after a crash*: the kernel
//! may have written any prefix of the last `pwritev2`. Truncation at a
//! random byte simulates that prefix space exactly — deterministically,
//! fast, and without OS-coupling. See plan §3.1 in
//! `.claude/plans/phase-02-task-11.md`.
//!
//! ## Reproducing a failure
//!
//! Failure messages include the iteration number and the LCG seed:
//!
//!   iter 437 seed=0x9B3D08F2E12A4901: <reason>
//!
//! Add a one-shot test against that exact seed to reproduce.

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_storage::arena::ArenaFile;
use brain_storage::recovery::{recover, InMemoryMetadataSink};
use brain_storage::wal::{EncodePayload, Lsn, Wal, WalPayload, WalRecord, WAL_SEGMENT_HEADER_LEN};
use std::fs;
use std::path::Path;

const N_RECORDS: u64 = 100;
/// Default smoke-test iteration count. Runs every `cargo test`.
const SMOKE_ITERATIONS: u64 = 100;
/// Full-sweep iteration count per spec §16/06 §2. Gated behind `#[ignore]`
/// because the full sweep takes ~3 minutes in the dev container. Run via
/// `cargo test --test random_kill -- --ignored` (or in CI).
const FULL_ITERATIONS: u64 = 1000;

/// Arbitrary 64-bit seed; the test loops `BASE_SEED + iter * GOLDEN`.
const BASE_SEED: u64 = 0xBADC_0FFE_E0DD_F00D;
/// Golden-ratio mixer for spreading iteration indices across the seed space.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

// ---------------------------------------------------------------------------
// RNG.
// ---------------------------------------------------------------------------

/// Hand-rolled LCG (Numerical Recipes constants). Deterministic per seed;
/// quality sufficient for the property under test.
fn rng_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

// ---------------------------------------------------------------------------
// Fixture helpers.
// ---------------------------------------------------------------------------

fn shard_uuid_from_seed(seed: u64) -> [u8; 16] {
    // Take 16 bytes from the seed's low half + high half; force the first
    // byte non-zero so the UUID isn't all-zero (which is the spec's
    // reserved "null" pattern).
    let lo = seed.to_le_bytes();
    let hi = seed.wrapping_mul(GOLDEN).to_le_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo);
    out[8..].copy_from_slice(&hi);
    if out[0] == 0 {
        out[0] = 1;
    }
    out
}

fn bytes16_from(seed: u64) -> [u8; 16] {
    let lo = seed.to_le_bytes();
    let hi = seed.wrapping_mul(GOLDEN).to_le_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo);
    out[8..].copy_from_slice(&hi);
    out
}

fn gen_record(rng: &mut u64, slot: u64) -> WalRecord {
    let r1 = rng_next(rng);
    let r2 = rng_next(rng);
    let r3 = rng_next(rng);
    let r4 = rng_next(rng);

    let payload = EncodePayload {
        memory_id: MemoryId::pack(1, slot, 1),
        request_id: RequestId::from(bytes16_from(r1)),
        agent_id: AgentId::from(bytes16_from(r2)),
        context_id: ContextId(r3),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: bytes16_from(r4),
        text: format!("slot {slot}"),
        vector: vec![0.5; 384],
        edges: vec![],
    };
    WalRecord::from_typed(
        Lsn(0),
        0,
        1_700_000_000_000_000_000,
        r2 ^ r3,
        &WalPayload::Encode(payload),
    )
}

/// Pre-create the arena file so `recover` can reopen it after the WAL is
/// torn. Capacity sized for N_RECORDS plus headroom.
fn pre_create_arena(arena_path: &Path, shard_uuid: [u8; 16]) {
    let _arena =
        ArenaFile::open(arena_path, shard_uuid, 256).expect("arena pre-create should succeed");
    // Drop closes the file via munmap.
}

/// Write N records via `Wal::append`. Returns each record's encoded size,
/// in order — recovery's `expected_count` for a truncation offset is then
/// computed by walking these lengths from `WAL_SEGMENT_HEADER_LEN`.
fn write_records(
    wal_dir: &Path,
    shard_uuid: [u8; 16],
    rng: &mut u64,
) -> Result<Vec<usize>, String> {
    let mut wal = Wal::create(wal_dir, shard_uuid).map_err(|e| format!("Wal::create: {e}"))?;
    let mut lens = Vec::with_capacity(N_RECORDS as usize);
    for slot in 0..N_RECORDS {
        let r = gen_record(rng, slot);
        lens.push(r.encoded_len());
        wal.append(r).map_err(|e| format!("Wal::append: {e}"))?;
    }
    wal.shutdown().map_err(|e| format!("Wal::shutdown: {e}"))?;
    Ok(lens)
}

fn truncate_segment(seg_path: &Path, new_len: u64) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .open(seg_path)
        .map_err(|e| format!("open: {e}"))?
        .set_len(new_len)
        .map_err(|e| format!("set_len: {e}"))
}

fn expected_count_for_truncation(record_lens: &[usize], trunc_offset: u64) -> u64 {
    let mut cursor = WAL_SEGMENT_HEADER_LEN as u64;
    let mut count = 0u64;
    for &len in record_lens {
        let next = cursor + len as u64;
        if next <= trunc_offset {
            count += 1;
            cursor = next;
        } else {
            break;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// The core invariant check.
// ---------------------------------------------------------------------------

fn run_iteration(seed: u64, trunc_strategy: TruncStrategy) -> Result<(), String> {
    let mut rng = seed;
    let shard_uuid = shard_uuid_from_seed(seed);

    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");

    pre_create_arena(&arena_path, shard_uuid);

    let record_lens = write_records(&wal_dir, shard_uuid, &mut rng)?;

    let seg_path = wal_dir.join("0000000000.wal");
    let file_size = fs::metadata(&seg_path)
        .map_err(|e| format!("metadata: {e}"))?
        .len();

    let trunc_offset = match trunc_strategy {
        TruncStrategy::HeaderOnly => WAL_SEGMENT_HEADER_LEN as u64,
        TruncStrategy::ExactMidBoundary => {
            // Truncate exactly at the boundary between records[49] and records[50].
            let mut cursor = WAL_SEGMENT_HEADER_LEN as u64;
            for &len in &record_lens[..50] {
                cursor += len as u64;
            }
            cursor
        }
        TruncStrategy::None => file_size,
        TruncStrategy::Random => {
            // Pick a uniformly-random offset in [HEADER_LEN, file_size].
            let range = file_size - WAL_SEGMENT_HEADER_LEN as u64 + 1;
            let pick = rng_next(&mut rng);
            WAL_SEGMENT_HEADER_LEN as u64 + (pick % range)
        }
    };

    truncate_segment(&seg_path, trunc_offset)?;
    let expected = expected_count_for_truncation(&record_lens, trunc_offset);

    let mut arena =
        ArenaFile::open(&arena_path, shard_uuid, 256).map_err(|e| format!("arena reopen: {e}"))?;
    let mut sink = InMemoryMetadataSink::new();
    let (report, _allocator) = recover(&mut arena, &wal_dir, shard_uuid, &mut sink)
        .map_err(|e| format!("recover: {e}"))?;

    // -- Assertions. --

    if report.records_replayed != expected {
        return Err(format!(
            "records_replayed: expected {expected}, got {} (trunc_offset={trunc_offset}, file_size={file_size})",
            report.records_replayed,
        ));
    }
    if report.records_skipped != 0 {
        return Err(format!(
            "records_skipped should be 0 (fresh sink), got {}",
            report.records_skipped
        ));
    }
    if report.records_discarded != 0 {
        return Err(format!(
            "records_discarded should be 0 (no TXN markers), got {}",
            report.records_discarded
        ));
    }
    let sink_lsns: Vec<u64> = sink.applied().keys().copied().collect();
    let expected_lsns: Vec<u64> = (1..=expected).collect();
    if sink_lsns != expected_lsns {
        return Err(format!(
            "sink LSN set mismatch (trunc_offset={trunc_offset}): expected {expected_lsns:?}, got {sink_lsns:?}",
        ));
    }
    if expected > 0 && report.next_lsn != expected + 1 {
        // After replay, next_lsn is one past the last applied LSN.
        return Err(format!(
            "next_lsn: expected {}, got {} (trunc_offset={trunc_offset})",
            expected + 1,
            report.next_lsn,
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum TruncStrategy {
    /// Truncate to exactly the segment header — no records survive.
    HeaderOnly,
    /// Truncate exactly at the boundary between record 50 and record 51.
    ExactMidBoundary,
    /// No truncation — all records survive.
    None,
    /// Random offset in `[HEADER_LEN, file_size]`.
    Random,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn sentinel_header_only_truncation() {
    // Seed picked deterministically; not the LCG-derived per-iter seed.
    run_iteration(0xC0FF_EE00_DEC0_DE01, TruncStrategy::HeaderOnly)
        .expect("header-only truncation case");
}

#[test]
fn sentinel_mid_boundary_truncation() {
    run_iteration(0xC0FF_EE00_DEC0_DE02, TruncStrategy::ExactMidBoundary)
        .expect("exact mid-boundary truncation case");
}

#[test]
fn sentinel_no_truncation() {
    run_iteration(0xC0FF_EE00_DEC0_DE03, TruncStrategy::None)
        .expect("no-truncation case (file intact)");
}

fn run_seeded_sweep(iterations: u64) -> Result<(), String> {
    let mut failures: Vec<String> = Vec::new();
    for iter in 0..iterations {
        let seed = BASE_SEED.wrapping_add(iter.wrapping_mul(GOLDEN));
        if let Err(e) = run_iteration(seed, TruncStrategy::Random) {
            failures.push(format!("iter {iter} seed={seed:#018x}: {e}"));
            // Bail after the first 5 so the panic message stays readable.
            if failures.len() >= 5 {
                break;
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} failure(s) observed (first 5 shown):\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

/// Smoke version of the property test. Runs on every `cargo test`.
///
/// 100 iterations × 100 records ≈ 20 seconds in the dev container.
#[test]
fn random_kill_recovery_smoke() {
    run_seeded_sweep(SMOKE_ITERATIONS).unwrap();
}

/// Full-sweep version per spec §16/06 §2 ("Run 1000 iterations; expect
/// 100% success"). Gated behind `#[ignore]` because the full sweep is
/// slow (~3 minutes in the dev container).
///
/// Run with: `cargo test -p brain-storage --test random_kill -- --ignored`
#[test]
#[ignore = "slow; full 1000-iteration sweep — invoke with --ignored in CI"]
fn random_kill_recovery_1000_iterations() {
    run_seeded_sweep(FULL_ITERATIONS).unwrap();
}
