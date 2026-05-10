# Phase 2 — Task 2.6: WAL segment writer (no fsync yet)

**Classification:** moderate. Mechanical file-management plus a 4 KB header. No `unsafe`, no syscalls beyond `std::fs::File`. Two design choices to flag: where the per-segment write buffer lives, and whether the segment knows about LSN allocation.

**Spec:** `spec/05_storage_arena_wal/04_wal_overview.md` (full), `05_wal_records.md` §1 (segment header), §17 (record packing), §18 (CRC semantics). Cross-checked `12_open_questions.md` — no relevant entries.

## 1. Scope

This task delivers `WalSegment` — a single `*.wal` segment file, owner of one append-only stream of `WalRecord`s. Specifically:

- `WalSegment::create_new(path, segment_seq, starting_lsn, shard_uuid)` — refuses to clobber; writes the 4 KB segment header on disk synchronously; positions for append at offset 4096.
- `append_record(&WalRecord)` — encodes the record (using 2.1's `WalRecord::encode_into`) into an in-memory write buffer. Does *not* hit disk.
- `flush()` — drains the in-memory buffer to the file via `File::write_all`. **No fsync** — that lands in 2.8 with `pwritev2(RWF_DSYNC)`.
- `size_bytes()` — header + on-disk records + buffered records. Lets the manager (2.9) decide when to roll over.
- `is_full()` — `size_bytes() >= WAL_SEGMENT_SIZE_BYTES`.
- `segment_id()`, `starting_lsn()` accessors for the manager.

**Out of scope** (later sub-tasks):

- LSN allocation (the segment receives records that already have their LSN set; allocation lives in the `Wal` type in 2.9).
- Segment rollover *decision* — the segment just exposes `is_full()` / `size_bytes()`; the manager (2.9) decides when to create the next segment.
- Recovery / open-existing path — reading a segment back is `WalReader` in 2.7; finding the tail of a crashed segment is part of recovery in 2.10. This task only handles create + append.
- `fallocate` / `O_DIRECT` / `pwritev2(RWF_DSYNC)` — all 2.8.
- Group commit — 2.8.
- Checkpoint coupling — 2.12.

## 2. Spec quotes that bind the design

> §05/04 §4 (segment names): "Segment names are 10-digit zero-padded sequence numbers. The `.wal` extension is for tools; the substrate identifies segments by the name pattern."  
> §05/04 §5 (append-only): "Records are appended at the tail. Records are never modified after writing. Records are never moved."  
> §05/05 §1 (segment header, 4096 bytes):
>
> | Offset | Size | Field | Type |
> |---|---|---|---|
> | 0 | 4 | magic | "BWAL" |
> | 4 | 4 | format_version | u32 LE |
> | 8 | 16 | shard_uuid | UUIDv7 |
> | 24 | 8 | segment_seq | u64 LE |
> | 32 | 8 | starting_lsn | u64 LE |
> | 40 | 8 | created_at | u64 LE |
> | 48 | 4 | header_crc32c | u32 LE |
> | 52 | 4044 | reserved | zero |
>
> §05/05 §17 (record packing): "Records are not padded to any alignment within a segment. They're packed back-to-back. The segment grows by appending records; the file's tail is the next free byte."
>
> §05/04 §10 (cold start): "The substrate creates `wal/0000000000.wal` with a 4 KB header. The first append (LSN 1) goes into this segment."

## 3. Spec ambiguities

### 3.1 Segment header CRC range — unambiguous this time

WAL segment header fields end at offset 48 (just before `header_crc32c`). The reserved tail is 4044 bytes (52..4096). No `[0..N]`-cuts-a-u64 pattern: every field aligns neatly. CRC32C covers bytes `[0..48]` of the header — every field that precedes the CRC.

Worth flagging only because the previous three CRC fields (slot, arena header, WAL record) each had a 4-byte off-by-typo. This one doesn't.

### 3.2 `created_at` precision

Spec says `u64 LE, unix nanoseconds`. We already use `SystemTime::now()` → `as_nanos() as u64` in `arena/file.rs::unix_nanos_now`. Reuse that pattern — extract to a shared helper, or duplicate locally?

**Plan: duplicate locally for now.** The helper is two lines; extracting a shared `time.rs` for two callers is premature. A later sub-task can DRY it up if a third caller appears.

## 4. Architecture

### 4.1 Constants

```rust
// in crates/brain-storage/src/wal/segment.rs

pub const WAL_SEGMENT_HEADER_LEN: usize = 4096;
pub const WAL_SEGMENT_MAGIC: [u8; 4] = *b"BWAL";
pub const WAL_SEGMENT_FORMAT_VERSION_V1: u32 = 1;
pub const WAL_SEGMENT_HEADER_CRC_COVERAGE_END: usize = 48;
```

`WAL_SEGMENT_SIZE_BYTES` already exists in `lib.rs`; reuse it.

### 4.2 Header struct

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct WalSegmentHeaderRaw {
    magic: [u8; 4],                          // 0..4
    format_version: u32,                     // 4..8
    shard_uuid: [u8; 16],                    // 8..24
    segment_seq: u64,                        // 24..32
    starting_lsn: u64,                       // 32..40
    created_at_unix_nanos: u64,              // 40..48
    header_crc32c: u32,                      // 48..52
    reserved: [u8; 4044],                    // 52..4096
}

unsafe impl bytemuck::Zeroable for WalSegmentHeaderRaw {}
unsafe impl bytemuck::Pod for WalSegmentHeaderRaw {}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<WalSegmentHeaderRaw>() == WAL_SEGMENT_HEADER_LEN);
    assert!(align_of::<WalSegmentHeaderRaw>() == 8);
    assert!(offset_of!(WalSegmentHeaderRaw, magic) == 0);
    assert!(offset_of!(WalSegmentHeaderRaw, format_version) == 4);
    assert!(offset_of!(WalSegmentHeaderRaw, shard_uuid) == 8);
    assert!(offset_of!(WalSegmentHeaderRaw, segment_seq) == 24);
    assert!(offset_of!(WalSegmentHeaderRaw, starting_lsn) == 32);
    assert!(offset_of!(WalSegmentHeaderRaw, created_at_unix_nanos) == 40);
    assert!(offset_of!(WalSegmentHeaderRaw, header_crc32c) == 48);
    assert!(offset_of!(WalSegmentHeaderRaw, reserved) == 52);
};
```

### 4.3 `WalSegment` shape

```rust
pub struct WalSegment {
    file: File,
    path: PathBuf,
    segment_seq: u64,
    starting_lsn: u64,
    shard_uuid: [u8; 16],
    /// Records appended but not yet flushed to file.
    write_buf: Vec<u8>,
    /// Bytes already written to the file (excluding header).
    bytes_on_disk: usize,
}
```

The buffer is unbounded — `flush()` is the caller's call. For 2.6, the buffer grows as records are appended; 2.8 will introduce the aligned page-sized buffer required for `O_DIRECT`.

### 4.4 API

```rust
impl WalSegment {
    pub fn create_new(
        path: impl AsRef<Path>,
        segment_seq: u64,
        starting_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<Self, WalSegmentError>;

    pub fn segment_seq(&self) -> u64;
    pub fn starting_lsn(&self) -> u64;
    pub fn shard_uuid(&self) -> [u8; 16];
    pub fn path(&self) -> &Path;

    /// Bytes used: header + records on disk + buffered records.
    pub fn size_bytes(&self) -> usize;

    /// True iff `size_bytes() >= WAL_SEGMENT_SIZE_BYTES`. The manager
    /// uses this to decide rollover.
    pub fn is_full(&self) -> bool;

    /// Append a record to the in-memory buffer. Returns the number of
    /// bytes the record contributes (`record.encoded_len()`).
    pub fn append_record(&mut self, record: &WalRecord) -> Result<usize, WalSegmentError>;

    /// Drain the in-memory buffer to the file. No fsync (2.8).
    pub fn flush(&mut self) -> Result<(), WalSegmentError>;
}
```

### 4.5 `create_new` flow

```rust
pub fn create_new(path, segment_seq, starting_lsn, shard_uuid) -> Result<Self> {
    // 1. Open with O_RDWR | O_CREAT | O_EXCL — refuse to clobber.
    let file = OpenOptions::new()
        .read(true).write(true).create_new(true).open(&path)?;

    // 2. Build and write the 4 KB header on disk.
    let mut header = WalSegmentHeaderRaw {
        magic: WAL_SEGMENT_MAGIC,
        format_version: WAL_SEGMENT_FORMAT_VERSION_V1,
        shard_uuid,
        segment_seq,
        starting_lsn,
        created_at_unix_nanos: unix_nanos_now(),
        header_crc32c: 0,
        reserved: [0; 4044],
    };
    let header_bytes = bytemuck::bytes_of(&header);
    header.header_crc32c = crc32c::crc32c(&header_bytes[0..WAL_SEGMENT_HEADER_CRC_COVERAGE_END]);
    let header_bytes_final = bytemuck::bytes_of(&header);
    file.write_all_at(header_bytes_final, 0)?; // pwrite-style; or write_all + seek

    // 3. Position cursor for append (offset 4096).
    file.seek(SeekFrom::Start(WAL_SEGMENT_HEADER_LEN as u64))?;

    Ok(Self { file, ..., write_buf: Vec::new(), bytes_on_disk: 0 })
}
```

Note on `write_all_at`: `std::os::unix::fs::FileExt::write_all_at` is stable on unix and writes at an absolute offset without touching the cursor. After it, we still need to `seek(SeekFrom::End(0))` or `SeekFrom::Start(HEADER_LEN)` before subsequent `write_all`s — easier to just use `write_all` since we're at offset 0 anyway. I'll use plain `write_all` for simplicity; the cursor naturally ends at offset 4096 after the header is written.

### 4.6 `append_record` and `flush`

```rust
pub fn append_record(&mut self, record: &WalRecord) -> Result<usize, WalSegmentError> {
    let len_before = self.write_buf.len();
    record.encode_into(&mut self.write_buf);
    Ok(self.write_buf.len() - len_before)
}

pub fn flush(&mut self) -> Result<(), WalSegmentError> {
    if self.write_buf.is_empty() {
        return Ok(());
    }
    self.file.write_all(&self.write_buf)?;
    self.bytes_on_disk += self.write_buf.len();
    self.write_buf.clear();
    Ok(())
}
```

`size_bytes()` = `WAL_SEGMENT_HEADER_LEN + bytes_on_disk + write_buf.len()`.

### 4.7 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum WalSegmentError {
    #[error("WAL segment io error: {0}")]
    Io(#[from] std::io::Error),
}
```

For 2.6, IO is the only failure mode. (We don't yet validate against a max size; the manager checks `is_full` and rotates. The encode path can't fail at this layer — `WalRecord::encode_into` is infallible up to a u32 payload check.)

### 4.8 Public surface

```
crates/brain-storage/src/wal/
├── kinds.rs       (existing — 2.1)
├── mod.rs         (update — re-export segment types)
├── payload.rs     (existing — 2.2)
├── record.rs      (existing — 2.1)
└── segment.rs     (NEW — this task)
```

Re-exports added to `wal/mod.rs`: `WalSegment`, `WalSegmentError`, `WAL_SEGMENT_HEADER_LEN`, `WAL_SEGMENT_MAGIC`, `WAL_SEGMENT_FORMAT_VERSION_V1`, `WAL_SEGMENT_HEADER_CRC_COVERAGE_END`.

`WAL_SEGMENT_SIZE_BYTES` already at the crate root; leave it there.

## 5. Trade-offs

| Option | Verdict | Why |
|---|---|---|
| **A. Segment owns an in-memory buffer (chosen)** | ✓ | Matches phase doc's "append … no sync" / "flush → write_all". Simple. Buffer policy decisions (size, alignment, O_DIRECT) move into the segment itself when 2.8 lands, keeping the API stable. |
| B. Segment writes-through to file on every append | ✗ | Each `append_record` would issue a syscall. Worse perf and inconsistent with the spec's group-commit pattern. |
| C. Buffer lives in the `Wal` type, segment is a `File` wrapper | ✗ | Phase doc 2.9 implies the segment owns its writes. Putting the buffer elsewhere fragments the API for no benefit. |
| **D. Segment is unaware of LSN allocation (chosen)** | ✓ | `WalRecord` already carries its LSN (set by the caller in 2.9's `Wal`). Segment treats records as opaque payloads with known size. Lets the segment be tested without an LSN allocator. |
| E. Segment allocates LSNs internally | ✗ | Conflates two concerns. The `Wal` type (2.9) needs LSN allocation that spans segment rolls; pushing it down to segment makes that harder. |
| **F. No fsync at this layer (chosen, per phase doc)** | ✓ | Phase doc explicitly defers fsync to 2.8. Records are durable only after group commit lands. Tests verify round-trip *after* `flush()`; durability is a 2.8 concern. |

## 6. Risks

- **No durability.** Records written via `append_record` + `flush` are in the page cache, not on stable storage. A crash before 2.8 lands loses the trailing records. This is the explicit phase-doc choice — flagged so anyone reading the code knows.
- **Buffer grows without bound.** If a caller appends 250 MiB worth of records without flushing, we hold 250 MiB in RAM. Acceptable: the `Wal` type (2.9) flushes on a schedule.
- **File handle survives across `flush` calls.** The kernel cursor advances after each `write_all`. If something `seek`s the file out from under us, subsequent appends land in the wrong place. We don't seek; `write_all` after `write_all` is sequential append. Document.
- **`create_new` fails on existing path.** Intentional — `O_EXCL` semantics. The manager (2.9) is responsible for picking a non-conflicting segment_seq.
- **Header `created_at` skew.** If the system clock jumps backward between segment creations, two segments might have `created_at` ordering inconsistent with their `segment_seq` ordering. Not a correctness issue (segment_seq is authoritative); just operational annoyance. Note in doc-comment.

## 7. Test plan

All tests use `tempfile::TempDir`. Pure-Rust; no docker-specific syscalls beyond what we already use.

1. **`create_new` writes a valid header.** After `create_new`, the file is exactly 4096 bytes; magic = "BWAL"; format_version = 1; segment_seq, starting_lsn, shard_uuid round-trip; reserved is zero; header CRC verifies over `[0..48]`.
2. **`create_new` refuses to clobber.** Creating twice at the same path returns an `io::ErrorKind::AlreadyExists`-wrapped error.
3. **`append_record` does not touch disk.** After `append_record` (no flush), the file is still 4096 bytes; `size_bytes()` includes the buffered record.
4. **`flush` is idempotent on empty buffer.** Calling `flush()` with no pending records is a no-op (file size unchanged).
5. **`flush` writes buffered bytes.** Append three records, flush; file is `4096 + Σ(record.encoded_len())` bytes; the records decode back via `WalRecord::decode_one` in LSN order. (This is the phase doc's done-when criterion: "Records can be written and read back via WalReader.")
6. **Mixed append/flush sequence.** Append → flush → append → flush. Final file contains all records, ordered, decodable.
7. **`size_bytes()` accounts for buffered + on-disk.** After append (no flush), `size_bytes() = header + buffer.len()`. After flush, the same total, just shifted from buffer to disk.
8. **`is_full()` reflects size_bytes.** Set a synthetic small `WAL_SEGMENT_SIZE_BYTES` via test-only constant override? — No, leave the constant; instead append enough records to approach 256 MiB? Too slow. Instead: test the boundary by checking `is_full()` math directly — `is_full() == (size_bytes() >= WAL_SEGMENT_SIZE_BYTES)`. Pure tautology; sufficient as a regression guard for the implementation. Alternative: test against a temporarily-shrunk constant via a `#[cfg(test)]` helper. **Plan: just assert the math, no synthetic constant.**
9. **Header layout const asserts compile.** `const _: () = { … };` blocks ensure size/offsets — pure compile-time, no test needed but worth listing.

**Total: 8 runtime tests.** Round-trip-via-decode (test #5) is the load-bearing one for the phase doc done-when.

## 8. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): WAL segment writer with 4 KB header (sub-task 2.6)`

Body covers:
- `WalSegment` shape; in-memory buffer + flush; no fsync (deferred to 2.8).
- Segment header layout, CRC over `[0..48]` (no typo this time).
- The deferrals: open_existing, fsync, fallocate, group commit, O_DIRECT — explicitly marked in module docs as 2.7–2.10 work.
- Phase-doc 2.6 entry: check the boxes.

Files touched:
- `crates/brain-storage/src/wal/mod.rs` (re-export).
- `crates/brain-storage/src/wal/segment.rs` (new, ~280 lines including tests).
- `docs/phases/phase-02-storage.md` (mark 2.6 done).

No new deps. Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-06.md` — confirm to proceed.
