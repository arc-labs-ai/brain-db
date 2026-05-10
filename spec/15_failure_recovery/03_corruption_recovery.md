# 15.03 Corruption Recovery

How Brain handles data corruption — detection, mitigation, and recovery.

## 1. The corruption types

- **Arena corruption**: a vector slot is bad (vector contains nonsense or wrong checksum).
- **WAL corruption**: a WAL record's CRC fails.
- **Metadata corruption**: redb's internal checks detect inconsistency.
- **HNSW corruption**: index references a non-existent memory; structure is malformed.

## 2. Detection

### Arena

Each slot has a CRC over the vector + metadata. Read path checks:

```rust
let computed_crc = crc32c(slot_data);
if computed_crc != slot.stored_crc {
    return Err(Error::SlotCorruption);
}
```

CRC mismatches are logged critical and the slot is treated as missing.

### WAL

Each WAL record has a CRC. Replay path checks per record. CRC mismatch halts replay.

### Metadata (redb)

redb has internal consistency checks. On open, redb verifies the database.

```rust
match Database::open(path) {
    Ok(db) => /* OK */,
    Err(redb::Error::DatabaseError(...)) => /* corruption */,
    Err(other) => /* other failure */,
}
```

Corrupt redb files are quarantined; the substrate refuses to use them.

### HNSW

The HNSW maintenance worker periodically validates:

- Each node references a valid memory.
- The graph is connected.
- Node degrees are within bounds.

Issues trigger a rebuild.

## 3. Response: arena slot corruption

When a slot's CRC fails:

```
1. Log critical.
2. Mark the slot as corrupt in metadata (a flag).
3. Treat operations on this memory as NotFound.
4. Trigger an alert to the operator.
```

The corrupt slot is quarantined. Other slots continue to work.

The operator decides:
- Restore from backup if the data is critical.
- Accept the loss if not.
- Investigate the cause (hardware issue?).

## 4. Response: WAL corruption

If a WAL record's CRC fails during replay:

```
1. Log critical.
2. Halt replay at that point.
3. Refuse to come up.
4. Alert.
```

Continuing past corruption could miss subsequent records. The substrate is conservative.

The operator must:
- Investigate the corruption.
- Restore from backup.
- Or, if the corruption is at the tail (a torn write), accept and restart with truncation.

## 5. Response: metadata corruption

If redb detects corruption:

```
1. Log critical.
2. Refuse to start.
3. Alert.
```

redb's design makes corruption rare (it has internal checks). When it happens, restoration is the path.

## 6. Response: HNSW corruption

If the integrity checker finds issues:

```
1. Log warning (or error if severe).
2. Trigger rebuild.
3. The rebuild constructs a fresh HNSW from the metadata.
4. After rebuild, the HNSW is healthy.
```

HNSW rebuild is much faster than restoring from backup. Most HNSW issues self-heal via rebuild.

## 7. Restoration from snapshot

For severe corruption (arena or metadata):

```bash
# Stop the substrate
systemctl stop brain

# Backup the current (corrupt) state for forensics
mv /var/lib/brain/data /var/lib/brain/data-corrupt-2026-05-07

# Restore from snapshot
brain-cli snapshot restore <snapshot-name> --confirm

# Start
systemctl start brain
```

Data after the snapshot is lost. RPO is bounded by snapshot frequency.

## 8. The snapshot cadence

For deployments wanting low RPO:

- Snapshot every hour: max 1h data loss on full restore.
- Snapshot every 6 hours: max 6h.
- Daily: max 24h.

Snapshot cost (CPU, disk) is moderate; choose based on RPO requirements.

## 9. The "WAL replay over restored snapshot"

A snapshot captures state at LSN N. After restoring:

- Some WAL records may exist with LSN > N.
- These would be replayed — possibly continuing past the corruption point.

The substrate detects this:

```
After restore: snapshot at LSN 10000
WAL exists with LSNs 10001-15000
WAL record at LSN 12345 was the corrupt one

If we replay everything: we'd hit the corruption again.
If we don't replay: lose data.
```

Operator decides:
- Full replay (will hit corruption; manual intervention).
- Partial replay (up to LSN 12344; data loss from 12345 onwards).

The CLI exposes this:

```bash
brain-cli wal-replay --shard <id> --max-lsn 12344
```

For surgical recovery.

## 10. The "rebuild from arena" fallback

If metadata is corrupt but arena is intact:

- The arena holds vectors with embedded slot metadata.
- A specialized recovery tool reads the arena and rebuilds metadata.

```bash
brain-cli recovery rebuild-metadata --shard <id>
```

This is a slow, manual operation. Bounded by arena scan time.

Useful when:
- Metadata corruption is severe.
- No usable snapshot.
- The arena is intact.

The recovery is best-effort; some metadata (text, edges) may not be reconstructable. Memories' vectors and slot metadata are.

## 11. The "rebuild from WAL"

If both metadata and arena are damaged but WAL is intact:

- The WAL has records of every ENCODE.
- A recovery tool can rebuild metadata and arena from WAL.

```bash
brain-cli recovery rebuild-from-wal --shard <id>
```

This is even slower than from-arena. Bounded by WAL size.

## 12. The "no recovery" worst case

If WAL, metadata, and arena are all damaged:

- And no snapshot exists.
- And no off-shard replication.

Then data is lost.

This is why backups matter. Brain's design provides multiple defenses; at least one must be intact.

## 13. The "data center loss"

For a single-region deployment:

- Disaster (fire, etc.) destroys the data center.
- All in-DC backups are also gone.

For protection:
- Off-site backups.
- Cross-region replication (v2).

These are operational practices outside Brain's primary scope; Brain provides the snapshot mechanism, and operators ship snapshots off-site.

## 14. The audit-trail integrity

Audit logs are critical for forensic investigation:

- Append-only.
- Hash-chained (each entry includes the hash of the previous).

Tampering breaks the chain; auditors detect.

For long-term storage, audit logs go to immutable storage (S3 with object lock, etc.).

## 15. The "post-corruption" investigation

After corruption is recovered:

- Capture forensic data (the corrupt state, dmesg logs, hardware diagnostics).
- Determine the cause:
  - Hardware? (memory errors, disk issues)
  - Operator action?
  - Substrate bug?
  - Cosmic rays?
- Adjust posture:
  - Better hardware (ECC, RAID).
  - Better procedures (test before production).
  - Bug fix in substrate.

## 16. The "verify after recovery"

After a recovery from snapshot:

- Run a consistency check (`brain-cli verify`).
- Verify metric counts match expectations.
- Test sample queries.

```bash
brain-cli verify --shard <id>
# Checks: arena/metadata/HNSW consistency, sample queries return reasonable results.
```

Verification gives confidence the recovery worked.

## 17. The "rebuild HNSW after restore"

After restoring metadata, the HNSW may be stale (if from-snapshot was used and the snapshot's HNSW is older).

Trigger rebuild:

```bash
brain-cli rebuild-ann <shard-id>
```

This ensures the HNSW matches the restored metadata.

## 18. The corruption rate expectation

In well-engineered systems:

- Hardware corruption: ~10⁻¹² per byte read (with ECC).
- Software bugs causing corruption: very rare (with good testing).
- Operator-caused corruption: depends on practices.

For a Brain shard with 100 GB of data, hardware corruption at 10⁻¹² implies one corrupt bit every ~10 PB read. For a busy shard reading ~10 GB/s, that's once every ~10 days.

In practice, corruption is much rarer due to additional safeguards (filesystem CRCs, drive's own ECC).

When corruption does happen, it's almost always:
- A specific drive failure.
- A memory failure.
- A bug.

Each is recoverable with the procedures above.

---

*Continue to [`04_data_loss_scenarios.md`](04_data_loss_scenarios.md) for data loss scenarios.*
