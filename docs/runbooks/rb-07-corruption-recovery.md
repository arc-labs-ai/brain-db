# RB-07: Recovery from corruption

**Severity:** **P1**.
**Alert:** chaos-detected (CRC failure, WAL truncation
mid-record, redb corruption, arena UUID mismatch).
**SLO impact:** affected shard down. May affect all
shards on the same host.
**Estimated duration:** 30 minutes to several hours
depending on snapshot freshness and shard size.
**Skill level:** **call for backup**. This runbook
involves destructive-shaped operations (file moves,
snapshot restores). If you've never done it before,
escalate to engineering early
([IR-03](ir-03-escalation-policy.md)).

The substrate detected internal inconsistency in its
durable state. **Fail-stop** behaviour kicked in:
the affected shard refuses to operate.

This runbook is the **most consequential** in the
collection. Read it once, slowly, before running any
commands.

---

## Am I in the right runbook?

You should see at least one of these:

- The substrate logged `ArenaOpenError: header CRC
  mismatch` or `shard UUID mismatch`.
- The substrate logged `RecoveryError: WAL truncated
  mid-record at LSN ...`.
- The substrate logged `RecoveryError: missing segment
  seg-...`.
- The substrate logged `MetadataOpenError: redb
  corruption detected`.
- The substrate logged `Bad CRC` during a runtime read
  (not just startup).
- The substrate refused to spawn one or more shards.

If the substrate is simply *down* (failed config,
permission denied, port in use), that's [RB-01](rb-01-substrate-down.md),
not this runbook. Make sure you're in the corruption
case before proceeding.

---

## Stop the bleeding

**Stop the substrate** so it doesn't make things
worse. Subsequent restart attempts can compound
corruption (rewriting partial WAL records, etc.).

```bash
sudo systemctl stop brain-server
```

For Docker / k8s, the equivalent.

Then:

1. **Page engineering.** This is a P1 with risk of
   permanent data loss. Engineering should be on
   the call from minute one.
2. **Capture a diagnostic bundle**
   ([DR-01](dr-01-diagnostic-bundle.md)). Especially
   the logs and the on-disk state.
3. **Preserve the broken data directory.** **Do not
   overwrite it.** Move it aside:

   ```bash
   sudo mv /var/lib/brain/data /var/lib/brain/data.broken.$(date +%s)
   ```

   You may need it for forensics later. The size is
   typically a few GB; keep it on the host until
   the incident postmortem is complete.

---

## Diagnose

### 1. What exactly is corrupted?

The specific error message tells you. Map to the
table:

| Error | Component | Recovery path |
|---|---|---|
| `ArenaOpenError: header CRC mismatch` | Arena header | Restore from snapshot |
| `ArenaOpenError: shard UUID mismatch` | Arena UUID | Likely a misplaced file; investigate before restoring |
| `RecoveryError: WAL truncated mid-record` | WAL (mid-segment) | Restore from snapshot |
| `RecoveryError: missing segment` | WAL | Restore from snapshot |
| `RecoveryError: bad CRC at LSN X` | WAL record CRC | Restore from snapshot |
| `MetadataOpenError: redb corruption` | redb metadata | Restore from snapshot |
| `Slot version mismatch` at runtime | Slot version drift | Bug — escalate; rebuild HNSW may help |
| Multiple shards | Likely hardware | Restore each from snapshot; replace hardware |

### 2. Is the corruption isolated or systemic?

- **One shard, others healthy:** focused recovery. The
  affected shard's data is bad; the others can keep
  serving while you work.
- **All shards, same host:** suspect hardware (bad
  disk, RAM, controller). Investigate before
  restoring to the same disk.
- **All shards, multiple hosts:** highly unusual.
  Either an upstream cause (a single shared file
  system; cloud-disk-volume corruption affecting
  several mounts) or a substrate-bug. Escalate
  aggressively.

### 3. How fresh is your most recent snapshot?

This is the question that determines how much data
you lose.

```bash
# If snapshots are on the local host:
ls -la /var/lib/brain/data.broken*/<shard>/snapshots/
# Or the broken data dir's snapshot index.

# If snapshots are off-host (S3, etc.):
aws s3 ls s3://your-brain-snapshots/<shard>/ --recursive | tail
```

Look for the most recent **completed** snapshot.
(The snapshot worker writes a manifest at the end;
unfinished snapshots have no manifest and shouldn't
be used.)

Time delta between the snapshot and the failure =
the data you lose. Snapshots taken hourly mean ~1
hour of data loss; daily snapshots mean up to 24
hours; rare snapshots mean rare data.

If your most recent snapshot is older than acceptable
loss, you have two options:

- **Try harder for partial recovery.** Truncating the
  WAL to the point of corruption may save some data
  beyond the snapshot, at the cost of complexity.
  Engineering can guide.
- **Accept the loss.** Restore from the snapshot you
  have; the data after the snapshot is gone.

### 4. Was the WAL torn at the tail or mid-segment?

A torn record at the very *end* of the WAL (the most
recent segment) is normal — Brain handles trailing
truncations automatically and recovery completes.

A torn record *mid-segment* (somewhere in the middle
of an older segment) is not normal. It implies
disk corruption that happened after the bytes were
written and fsync'd. This is much rarer and points
at hardware.

The substrate logs the LSN of the tear. If the tear
is at a high LSN close to the latest, it's
recoverable via standard restore. If the tear is at
a low LSN (way back in history), the WAL is broken
fundamentally; restore from snapshot is the only
option.

### 5. Is the data file intact, just the WAL bad?

If only the WAL is corrupted but `arena.bin` and
`metadata.redb` are fine:

In theory, the substrate could rebuild a partial
WAL from the current `arena` + `metadata` state.

In practice, **v1 doesn't ship a tool for this**.
The recovery path expects the WAL to be the source
of truth; without it, the state is suspect.

Workaround: use the snapshot's WAL (taken at the
same time as the snapshot's arena/metadata). You
lose any data written after the snapshot. Restore
all three together.

---

## Remediate

This is where you must be careful. Each command
here is documented; don't improvise.

### Restore from snapshot — full procedure

This is the standard remediation. Follow it
sequentially.

```bash
# 1. Confirm substrate is stopped.
sudo systemctl status brain-server     # should be inactive

# 2. Confirm the broken data dir is moved aside.
ls -ld /var/lib/brain/data.broken.*

# 3. Identify the snapshot to restore.
ls -la /var/lib/brain/data.broken*/<shard>/snapshots/

# Pick the most recent completed snapshot. Note its
# manifest path and LSN.

# 4. Create the destination data directory.
sudo mkdir -p /var/lib/brain/data
sudo chown brain:brain /var/lib/brain/data
sudo chmod 700 /var/lib/brain/data

# 5. Restore.
#    Method depends on your snapshot system.
#    Brain ships with a snapshot-restore command:
sudo /usr/local/bin/brain-snapshot-restore \
    --from /var/lib/brain/data.broken.<timestamp>/<shard>/snapshots/<snap_id> \
    --to /var/lib/brain/data/<shard>

# Or if snapshots are off-host:
sudo /usr/local/bin/brain-snapshot-restore \
    --from s3://your-brain-snapshots/<shard>/<snap_id> \
    --to /var/lib/brain/data/<shard>

# 6. Verify permissions.
sudo chown -R brain:brain /var/lib/brain/data
sudo ls -la /var/lib/brain/data/

# 7. Repeat for each affected shard.
#    Other shards on the same host that were healthy
#    can be left alone — copy their data from
#    .broken back over (only the broken shard's
#    files are corrupted).
sudo cp -a /var/lib/brain/data.broken.<timestamp>/<healthy_shard>/ \
           /var/lib/brain/data/<healthy_shard>/
sudo chown -R brain:brain /var/lib/brain/data/<healthy_shard>/

# 8. Start the substrate.
sudo systemctl start brain-server

# 9. Watch logs for recovery completion.
sudo journalctl -u brain-server -f
```

Expected log lines on successful restore:

```
{"target":"brain_storage::recovery","msg":"WAL recovery complete","records_replayed":0,...}
{"target":"brain_server::shard","msg":"shard spawned","shard_id":0,...}
```

If recovery fails again, **escalate** immediately —
don't try the same restore on another snapshot
without understanding why this one failed.

### Recover only the WAL (advanced, requires
engineering)

If the WAL has a mid-segment tear at a known LSN and
you want to keep data up to that LSN:

1. **Don't do this without engineering's involvement.**
2. The procedure (which engineering will guide):
   - Identify the LSN of the tear.
   - Truncate the WAL segment to a point safely before
     the tear (at a known-good record boundary).
   - The substrate will replay up to the truncation
     point and continue.
3. The risk: if you misjudge the truncation point,
   you can corrupt the arena or metadata replay.

This procedure isn't documented in detail here
specifically because it shouldn't be operator-driven.

### Replace hardware (if disk is the cause)

If diagnosis points at hardware (SMART errors,
multiple-shard failures on one host):

1. Restore to a **different** physical disk.
2. Replace the bad hardware.
3. After replacement, optionally restore to the new
   hardware (with rolling restart) to get back to
   the original layout.

The substrate runs from wherever the `data_dir`
points; moving data between disks is a config
change + file copy.

---

## Verify

```bash
# Substrate running on all shards.
brain-cli admin shards
# All shards should be "active", none "recovering" or "failed".

# Smoke test.
brain-cli encode "post-recovery smoke test $(date +%s)"
brain-cli recall "post-recovery smoke test"

# Verify durability invariants.
brain-cli admin verify-wal --shard <restored-shard>
brain-cli admin verify-arena --shard <restored-shard>
```

See [DR-05](dr-05-verifying-durability-invariants.md)
for the full sweep — run it on the restored shard.

Confirm the alert clears in Prometheus. Confirm
clients are happy by checking error rates.

---

## Post-incident

**Always** write a postmortem for RB-07. This is the
deepest kind of incident.

```
:white_check_mark: Resolved at HH:MM UTC.
Duration: <total>
Root cause: <e.g., disk SMART errors caused WAL CRC failures>.
Remediation: <restored shard 3 from snapshot taken at HH:MM; replaced
disk on host brain-prod-3>.
Data loss: <e.g., ~45 min of writes between snapshot and crash>.
Follow-up: TICKET-NNNN (...), TICKET-MMMM (...).
Postmortem: required.
```

Things the postmortem should cover:

- The exact sequence of events.
- How the corruption was detected (alert, customer
  report, etc.).
- How much data was lost.
- Whether snapshot cadence was adequate.
- Whether the runbook helped or got in the way.
- Hardware / infrastructure follow-ups.

---

## Prevention

This is the runbook with the biggest preventable
surface. Concrete actions:

- **Snapshot more often** if the data-loss window
  hurts. Hourly snapshots have ~1-hour worst-case
  loss. Quarter-hourly if you can afford the disk.
- **Verify snapshots** ([OP-03](op-03-backup-verification.md))
  periodically. A snapshot that doesn't restore
  cleanly is worse than no snapshot at all (false
  sense of security).
- **Run [OP-02](op-02-snapshot-restore-drill.md)
  drills.** Restoring a snapshot to a staging
  instance every quarter validates the whole
  recovery path before you need it under pressure.
- **Monitor disk health.** SMART errors precede
  corruption. Set alerts on
  `node_disk_read_errors_total`,
  `node_smart_failed`, etc.
- **Use ECC RAM** in production. Memory bit flips
  cause subtle corruption that traces back to bad
  hardware months later.
- **Run with enterprise-grade SSDs.** Consumer
  drives lie about durability (no FUA support);
  Brain's fsync-based durability story depends on
  the drive being honest.
- **Verify backups go off-host.** A snapshot stored
  only on the same disk as the broken data does
  nothing.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
- [OP-03 — Backup verification](op-03-backup-verification.md)
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [Concepts: durability and trust](../concepts/24-invariants-and-trust.md)

---

## Last validated

*Update on first use. RB-07 should ideally be
validated at least annually via a controlled drill
against a chaos-test scenario.*
