# 05.06 WAL Durability: O_DIRECT, RWF_DSYNC, Group Commit

The WAL's durability mechanism. This is where Linux kernel primitives meet the substrate's per-record guarantees.

## 1. The durability commitment

When the substrate acknowledges a state-mutating operation, it has guaranteed:

1. The WAL record is on stable storage (fsync semantics).
2. All earlier records are also on stable storage (no out-of-order durability).

Properly speaking, the second guarantee is implied by the first (the WAL is sequential), but it's worth stating explicitly.

After acknowledgment, the substrate can crash and the operation will be recovered.

## 2. The kernel primitives

Three kernel primitives form the substrate's durability machinery:

### 2.1 O_DIRECT

When opening WAL segment files, the substrate uses `O_DIRECT`:

```rust
let fd = unsafe {
    libc::open(
        path.as_ptr(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_DIRECT,
        0o600,
    )
};
```

`O_DIRECT` semantics:
- Writes go through the kernel directly to the device, bypassing the page cache.
- Buffers must be aligned to the device's block size (typically 4 KB).
- Buffer lengths must be multiples of the block size.
- No double-buffering (kernel page cache + user buffer).

For the WAL, this is appropriate: we never re-read the bytes from the file (except during recovery, when the page cache being clean is fine). Avoiding the page cache means:
- No memory used for caching pages we won't re-read.
- More predictable latency (no page-cache writeback variance).

Reference: `O_DIRECT` is defined in `<fcntl.h>` and the kernel UAPI header [`include/uapi/asm-generic/fcntl.h`](https://github.com/torvalds/linux/blob/master/include/uapi/asm-generic/fcntl.h).

### 2.2 RWF_DSYNC

When writing WAL records, the substrate uses the `RWF_DSYNC` flag in `pwritev2`:

```rust
const RWF_DSYNC: u32 = 0x00000002;

unsafe {
    libc::syscall(
        libc::SYS_pwritev2,
        fd,
        iovecs.as_ptr(),
        iovecs.len(),
        offset,
        RWF_DSYNC,
    )
}
```

`RWF_DSYNC` semantics:
- The write is performed.
- The kernel ensures the data is on stable storage before returning.
- Equivalent to `pwritev` followed by `fdatasync`, but in a single syscall.

The numeric value `0x2` is from the Linux UAPI [`include/uapi/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

`fdatasync` (as opposed to `fsync`) syncs only data, not metadata changes that don't affect data accessibility. For WAL appends to existing segments, this is appropriate — the file's size update is metadata that the recovery doesn't strictly need (CRC failures at the truncation boundary will detect the truncation).

For new segment files, the substrate `fsync`s the parent directory after creating a new segment, ensuring the directory entry is durable.

### 2.3 io_uring

Rather than calling `pwritev2` synchronously, the substrate submits writes via io_uring:

```rust
let mut sqe = ring.next_submission_entry();
sqe.set_op(io_uring::opcode::Writev::CODE)
   .set_fd(fd)
   .set_addr(iovecs.as_ptr() as u64)
   .set_len(iovecs.len() as u32)
   .set_offset(offset)
   .set_rw_flags(RWF_DSYNC);
ring.submit();
```

The io_uring submission queues the write; a completion arrives later via the completion queue. The writer task awaits the completion before acknowledging.

io_uring's value here:
- Multiple writes can be in flight simultaneously (different shards' WALs).
- The syscall overhead is amortized across submissions.
- Modern kernels (5.8+) support `pwritev2` via io_uring with proper semantics.

The [`liburing`](https://github.com/axboe/liburing) library provides the userspace abstraction. The Glommio runtime wraps it.

## 3. The buffer

The WAL writer maintains an aligned buffer for accumulating records:

```rust
struct WalWriter {
    fd: RawFd,
    file_offset: u64,
    buffer: AlignedBuffer,           // 4 KB aligned, 64 KB capacity
    pending_records: Vec<PendingRecord>,
}
```

The buffer's size is configurable; default 64 KB. Larger buffers gather more records per flush but increase per-flush latency.

When a record is appended:
1. Serialize the record into the buffer at the current write offset.
2. Track the record in `pending_records` (LSN, awakener channel).
3. Schedule a group-commit flush (see § 4 below).

When a flush happens:
1. Round the buffer's used size up to the next 4 KB boundary (padding with zero bytes that recovery will interpret as a CRC-failed record and stop at).
2. Submit a `pwritev2` with `RWF_DSYNC` for the buffer's content.
3. Wait for completion.
4. Notify all pending records' awakeners.
5. Advance `file_offset` and reset the buffer for the next batch.

## 4. Group commit timing

Two triggers fire a group commit:

1. **Time-based:** A 100 µs timer (the group-commit window). When the timer fires, flush whatever's in the buffer.
2. **Size-based:** The buffer fills (reaches 60 KB out of 64 KB capacity). Flush immediately.

The 100 µs window:
- Short enough that p50 latency for a single-record write is dominated by fsync, not waiting.
- Long enough to gather meaningful batches under load (1000 records/sec → 1 record per 100 µs window typical; 10K rec/sec → 1-2 records per window).

For very low-rate workloads, every record fsyncs alone; group commit doesn't help, but the latency floor is fsync alone. For high-rate workloads, group commit amortizes fsync over many records.

## 5. The fsync latency floor

On modern NVMe, a single fsync (with FUA) takes ~50-200 µs. With group commit, each record's wait time is dominated by:

- Time waiting for the group-commit window to close: 0-100 µs (avg 50 µs).
- Time for the actual fsync: 50-200 µs.

Per-record wait time: 50-300 µs typical, depending on load and storage. For the substrate's overall p99 < 25 ms target, the WAL is a small fraction.

## 6. Pre-allocation

When a new segment is created, the substrate uses `fallocate` to pre-allocate its full size (256 MiB):

```rust
unsafe {
    libc::fallocate(
        fd,
        0,           // mode 0 = allocate
        0,           // offset
        SEGMENT_SIZE as i64,  // 256 MiB
    );
}
```

Pre-allocation:
- Reduces filesystem metadata churn during the segment's life (extent already allocated).
- Provides early warning of disk-full conditions (fails at segment creation, not mid-record).
- Improves write performance (writes overwrite already-allocated blocks rather than triggering extent allocation).

The pre-allocated file appears full to `ls -l` but is mostly zeros (sparse if the FS allows; explicit zero blocks otherwise).

## 7. Segment rollover

When the active segment fills (reaches ~256 MiB), a rollover occurs:

1. The current group commit completes (flushing the last records into the old segment).
2. A new segment file is created.
3. The new segment's header is written and fsync'd.
4. The directory containing segments is fsync'd (so the new file's directory entry is durable).
5. Subsequent records go to the new segment.

The rollover briefly stalls writes (the writer task is busy with the new-segment setup). On NVMe, this is < 5 ms.

## 8. The durability protocol

For a single record, the timeline:

```
T=0 µs:   Record is appended to in-memory buffer.
T=10 µs:  Group-commit window timer started (or extended).
T=110 µs: Window closes; flush triggered.
T=110 µs: pwritev2 with RWF_DSYNC submitted via io_uring.
T=160 µs: Kernel begins write to NVMe.
T=210 µs: NVMe acknowledges write.
T=210 µs: Kernel returns completion to io_uring.
T=215 µs: Substrate notifies record's awaiter.
```

Plus or minus device variance. Typical p50 ~150 µs, p99 ~500 µs.

## 9. Failure of the underlying device

If `pwritev2` returns an error (device removed, OOM in the kernel, etc.), the substrate:

1. Logs the error.
2. Marks the WAL as "broken"; no further records can be written.
3. Existing in-flight records receive errors.
4. The substrate transitions to a degraded state where reads are still possible but writes fail with `WalUnavailable`.

Recovery from this state requires operator intervention (replace storage, check disk, restart the substrate).

## 10. The fsync-on-data-only choice

We use `fdatasync` semantics (via `RWF_DSYNC`) rather than full `fsync`. This means:

- Data writes are synchronized.
- File metadata changes (file size, modification time) might not be synchronized.

For the WAL, file size changes are not strictly needed for durability:

- If we crash after a write but before the size update is durable, recovery reads the file expecting the old size.
- The data we wrote is on disk (the data sync ensured it).
- Recovery will see records past the "logical" file end, validate their CRCs, and process them.

This works because the WAL is append-only and CRC-checked. We don't depend on the file size as the truth; we depend on CRC-validated records.

## 11. Cross-shard fsync independence

Each shard has its own WAL. Different shards' fsyncs are independent:

- Different file descriptors.
- Different io_uring submissions.
- Different kernel-side queues.

A slow disk write on shard A doesn't delay shard B's fsyncs. This matters for latency tail at scale: per-shard isolation prevents one slow shard from poisoning the cluster.

## 12. Battery-backed write caches and the SSD's role

On enterprise SSDs with FUA (Force Unit Access), `RWF_DSYNC` semantics include flushing to non-volatile media. On consumer SSDs without FUA, the data may sit in a volatile DRAM cache that survives only as long as the device's super-capacitor allows.

For deployments using consumer SSDs, the substrate's durability is "best effort" — the kernel reports the write as durable, but a power loss may still lose recently-written records.

We recommend enterprise SSDs with FUA for production. For development or test, consumer SSDs are fine.

## 13. The reverse case: too-frequent fsync

If the workload has many small writes and the group-commit window doesn't gather enough records to amortize, fsync overhead can dominate. Symptoms:

- High WAL write latency (p99 > 1 ms).
- Low write throughput (much less than the device's write IOPS suggest).

Mitigations:

- Increase the group-commit window (default 100 µs; up to ~500 µs).
- Increase the buffer size (default 64 KB; up to a few MB).
- Coalesce salience updates (already done).

These knobs are exposed in configuration; operators tune as needed.

---

*Continue to [`07_write_path.md`](07_write_path.md) for the full ENCODE write path.*
