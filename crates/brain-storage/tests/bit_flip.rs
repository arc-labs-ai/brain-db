//! Bit-flip chaos test. Spec §15/07 §11 + §16/06.
//!
//! Companion to `random_kill.rs`. `random_kill` covers torn writes
//! (truncation); this file covers in-place byte corruption — a
//! cosmic ray flipping a bit inside a cleanly-written WAL segment.
//!
//! ## What we test
//!
//! 1. Create a shard (arena + WAL).
//! 2. Append N records (clean shutdown).
//! 3. Flip a single bit at a deterministic offset inside one of the
//!    records.
//! 4. Reopen and call `recover`.
//! 5. Assert that recovery either:
//!    - Returns `Err(_)` (fail-stop per spec §15/07 §11), OR
//!    - Returns only the prefix of records before the corrupted one
//!      (CRC mismatch causes the recovery to stop at the bad record).
//!
//! The contract is **"no silent corruption"** — recovery must NOT
//! return a corrupted record as if it were valid. Either the CRC
//! detects it (and recovery stops or skips) or the run fails-stop;
//! both are acceptable spec-faithful outcomes.

#![allow(clippy::cast_possible_truncation)]

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, RequestId};
use brain_storage::arena::ArenaFile;
use brain_storage::recovery::{recover, InMemoryMetadataSink};
use brain_storage::wal::{EncodePayload, Lsn, Wal, WalPayload, WalRecord, WAL_SEGMENT_HEADER_LEN};

const N_RECORDS: u64 = 20;
const VECTOR_DIM: usize = 384;

fn shard_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0] = 0xBE;
    u[1] = 0xEF;
    u[15] = 0xAA;
    u
}

fn bytes16_from(seed: u64) -> [u8; 16] {
    let lo = seed.to_le_bytes();
    let hi = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo);
    out[8..].copy_from_slice(&hi);
    out
}

fn gen_record(slot: u64) -> WalRecord {
    let payload = EncodePayload {
        memory_id: MemoryId::pack(1, slot, 1),
        request_id: RequestId::from(bytes16_from(slot * 7 + 1)),
        agent_id: AgentId::from(bytes16_from(slot * 11 + 2)),
        context_id: ContextId(slot * 13 + 3),
        kind: MemoryKind::Episodic,
        salience_initial: 0.5,
        embedding_model_fp: bytes16_from(slot * 17 + 4),
        text: format!("slot {slot}"),
        vector: vec![0.5; VECTOR_DIM],
        edges: vec![],
        request_hash: [0; 32],
        response_payload: vec![],
        deduplicate: false,
    };
    WalRecord::from_typed(
        Lsn(0),
        0,
        1_700_000_000_000_000_000,
        slot,
        &WalPayload::Encode(payload),
    )
}

fn pre_create_arena(arena_path: &Path) {
    let _arena = ArenaFile::open(arena_path, shard_uuid(), 256).expect("arena open");
}

fn write_records(wal_dir: &Path) -> Vec<usize> {
    let wal_dir_buf = wal_dir.to_path_buf();
    glommio::LocalExecutorBuilder::default()
        .name("bit-flip-wal")
        .spawn(move || async move {
            let wal = Wal::create(&wal_dir_buf, shard_uuid())
                .await
                .expect("Wal::create");
            let mut lens = Vec::with_capacity(N_RECORDS as usize);
            for slot in 0..N_RECORDS {
                let r = gen_record(slot);
                lens.push(r.encoded_len());
                wal.append(r).await.expect("Wal::append");
            }
            wal.shutdown().await.expect("Wal::shutdown");
            lens
        })
        .expect("executor spawn")
        .join()
        .expect("executor join")
}

fn find_segment(wal_dir: &Path) -> std::path::PathBuf {
    // Segment files are `<seq:010>.wal` per wal::segment_path.
    fs::read_dir(wal_dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x == "wal")
        })
        .expect("expected one *.wal segment")
}

/// Flip the bit at `(byte_offset, bit_offset)` in `path`. Asserts the
/// offset is within file bounds.
fn flip_bit(path: &Path, byte_offset: u64, bit_offset: u8) {
    assert!(bit_offset < 8);
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open");
    f.seek(SeekFrom::Start(byte_offset)).expect("seek");
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf).expect("read");
    buf[0] ^= 1u8 << bit_offset;
    f.seek(SeekFrom::Start(byte_offset)).expect("seek back");
    f.write_all(&buf).expect("write");
    f.sync_all().expect("sync");
}

/// The contract: recovery is non-silent on a corrupted byte. Returns
/// `Ok(n)` where `n` is the number of records recovered (≤ original
/// count) or `Err(_)`. Either is spec-faithful — the assertion is
/// "no silent corruption", not a specific error shape.
fn recover_count(wal_dir: &Path, arena_path: &Path) -> Result<u64, String> {
    let mut arena = ArenaFile::open(arena_path, shard_uuid(), 256).expect("arena open");
    let mut sink = InMemoryMetadataSink::new();
    match recover(&mut arena, wal_dir, shard_uuid(), &mut sink) {
        Ok(_summary) => Ok(sink.applied().len() as u64),
        Err(e) => Err(format!("recover error: {e:?}")),
    }
}

#[test]
fn flipping_bit_in_record_payload_is_detected() {
    let tmp = tempfile::tempdir().expect("tmp");
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");
    fs::create_dir_all(&wal_dir).expect("mkdir");
    pre_create_arena(&arena_path);

    let record_lens = write_records(&wal_dir);
    assert_eq!(record_lens.len(), N_RECORDS as usize);

    let seg_path = find_segment(&wal_dir);
    let seg_size = fs::metadata(&seg_path).expect("stat").len();
    assert!(seg_size > WAL_SEGMENT_HEADER_LEN as u64);

    // Pick an offset inside the *second* record's payload region.
    // First record starts at WAL_SEGMENT_HEADER_LEN; offset by one
    // record plus 32 bytes (skips the record header CRC region to
    // hit payload bytes).
    let first_len = record_lens[0] as u64;
    let flip_offset = WAL_SEGMENT_HEADER_LEN as u64 + first_len + 32;
    assert!(
        flip_offset < seg_size,
        "flip offset out of range: {flip_offset} vs {seg_size}"
    );

    flip_bit(&seg_path, flip_offset, 3);

    // Recovery must not silently return the corrupted record as valid.
    // Either it errors out, or it stops at the bad record (returning
    // the prefix). Both outcomes are spec-faithful.
    match recover_count(&wal_dir, &arena_path) {
        Ok(n) => {
            assert!(
                n < N_RECORDS,
                "recovery returned {n} records — corruption was silent (spec §15/07 §11 violation)"
            );
        }
        Err(_) => {
            // Fail-stop is the strictest valid behaviour.
        }
    }
}

#[test]
fn flipping_bit_in_segment_header_is_detected() {
    let tmp = tempfile::tempdir().expect("tmp");
    let arena_path = tmp.path().join("arena.bin");
    let wal_dir = tmp.path().join("wal");
    fs::create_dir_all(&wal_dir).expect("mkdir");
    pre_create_arena(&arena_path);

    let _record_lens = write_records(&wal_dir);
    let seg_path = find_segment(&wal_dir);

    // Flip a bit in the segment header (byte 4 is inside the shard
    // UUID region per `spec/05_storage_arena_wal/03_wal_layout.md`).
    flip_bit(&seg_path, 4, 0);

    match recover_count(&wal_dir, &arena_path) {
        Ok(n) => {
            // Segment header corruption should be caught; if recovery
            // returns Ok, it should not have recovered any records.
            assert_eq!(
                n, 0,
                "recovery silently consumed records from a corrupted-header segment"
            );
        }
        Err(_) => {
            // Fail-stop — the strict, expected outcome.
        }
    }
}
