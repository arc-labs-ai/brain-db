# RB-01: Substrate doesn't start

**Severity:** **P1**
**Alert:** `BrainSubstrateDown`
**SLO impact:** total outage — no operations served while the substrate is down.
**Estimated duration:** 15 minutes to 2 hours depending on root cause.
**Skill level:** confident with Linux, the substrate's data directory layout, and snapshot restore.

The substrate refuses to start, exits during boot, or
appears to start but never responds.

---

## Am I in the right runbook?

You should see **all** of these:

- The `brain-server` process is not listening on its
  wire port (no `LISTEN` on `9090` / your configured
  `listen_addr`).
- The metrics endpoint (`/metrics` on
  `metrics_addr`) is unreachable.
- The process either:
  - Is not running at all (`pgrep brain-server`
    returns nothing).
  - Is in a rapid restart loop (multiple
    short-lived PIDs in the last few minutes).
  - Is running but stuck during startup (process
    is alive, but no listener bound).

If the substrate *is* listening but **slow or
erroring**, you want [RB-02](rb-02-high-latency.md) or
[RB-08](rb-08-unresponsive.md), not this one.

If the process is in a restart loop specifically,
[RB-12](rb-12-restart-loop.md) is the more specific
runbook.

---

## Stop the bleeding

While the substrate is down, every client is failing.
Two things to do **immediately**, before diagnosing:

1. **Acknowledge the page** and open the incident
   channel ([IR-04](ir-04-incident-communication.md)).
2. **Collect a diagnostic bundle right now.** Logs
   from this exact moment are the most useful you'll
   ever have for this incident:

   ```bash
   sudo /usr/local/bin/collect-brain-bundle
   ```

   See [DR-01](dr-01-diagnostic-bundle.md). Don't
   skip this; once you start touching files, the
   record of the failure changes.

3. **If the substrate is in a rapid restart loop,
   stop it.** Each restart attempt may make the
   underlying problem worse (further WAL replay
   attempts on a torn record, etc.):

   ```bash
   sudo systemctl stop brain-server
   ```

   For Docker:

   ```bash
   docker stop brain-server
   ```

   For Kubernetes (scaling the StatefulSet to 0):

   ```bash
   kubectl scale statefulset brain-server --replicas=0
   ```

   This buys you breathing room. The substrate stays
   down for now; you'll start it cleanly after
   diagnosis.

---

## Diagnose

Each step below is a branch. Follow the one your
symptoms match.

### 1. What does the substrate say in its logs?

Almost every startup failure prints a single error
line to stderr before exiting. Read that line.

```bash
sudo journalctl -u brain-server --since "10 min ago" --reverse | head -50
```

For Docker:

```bash
docker logs --tail 200 brain-server
```

For Kubernetes:

```bash
kubectl logs <pod> --tail 200 --previous
```

The line you want looks like one of:

```
config error: missing required field `storage.data_dir`
config error: file not found: /etc/brain/config.toml
bind error: address already in use (port 9090)
ArenaOpenError: header CRC mismatch on /var/lib/brain/data/shard-0/arena.bin
ArenaOpenError: shard UUID mismatch
RecoveryError: WAL truncated mid-record at LSN 4271832
RecoveryError: missing segment seg-0000000003.wal
MetadataOpenError: redb corruption detected
Permission denied: /var/lib/brain/data
Out of memory: ...
```

Match the error to one of the branches below.

### 2. Config error

**Symptom:** log says `config error: ...`.

Validate the TOML:

```bash
brain-server --config /etc/brain/config.toml --check-config
```

(Or, if your version doesn't support `--check-config`,
parse the TOML with another tool:)

```bash
python3 -c "import tomllib; tomllib.load(open('/etc/brain/config.toml','rb'))"
```

Common config mistakes:

- Missing required field (`storage.data_dir`,
  `server.listen_addr`).
- Wrong type (string where int expected).
- Trailing comma or unclosed quote.
- Path that doesn't exist (`data_dir` pointing
  somewhere absent).

Fix the config; **don't start** until you've
verified the file parses. Proceed to *Remediate →
Fix config*.

### 3. Address in use

**Symptom:** log says `bind error: address already in use`.

Find the conflicting process:

```bash
sudo ss -tlnp 'sport = :9090'
sudo ss -tlnp 'sport = :9091'
sudo ss -tlnp 'sport = :9092'
```

(Adjust ports to your config.)

Likely cause: a previous `brain-server` didn't exit
cleanly. Less likely: some other service grabbed the
port.

Proceed to *Remediate → Free the port*.

### 4. Permission denied

**Symptom:** log says `Permission denied` reading or
writing under the data dir.

```bash
sudo ls -ld /var/lib/brain/data
sudo ls -la /var/lib/brain/data | head
```

Expected: owned by the `brain` user (or whichever
user the systemd unit / Docker container runs as),
mode `0700` or similar.

Causes:

- A previous `chown` or `chmod` (often by an
  inexperienced operator).
- A new disk mount with the wrong ownership.
- Running brain-server as a different user than
  before.

Proceed to *Remediate → Fix permissions*.

### 5. Arena CRC / shard UUID mismatch

**Symptom:** log says `ArenaOpenError: header CRC
mismatch` or `shard UUID mismatch`.

This is **fail-stop** behaviour — the substrate
detected internal inconsistency and refused to
proceed. Honour that decision; don't force-start.

The arena's header CRC catches accidental damage to
the file (partial write, disk corruption, bit rot).
A UUID mismatch means a different shard's data is in
the directory — usually a misconfiguration (someone
swapped `data_dir` paths) or a botched manual
restore.

Don't edit the file. Don't `dd` over the header.
Proceed to *Remediate → Restore from snapshot*.

### 6. WAL truncation or missing segment

**Symptom:** log says `RecoveryError: WAL truncated
mid-record at LSN ...` or `missing segment seg-...`.

A truncation mid-record means the substrate was
killed during a write and the partial WAL record
can't be parsed. Brain handles trailing truncations
(the very last record) automatically; mid-record
truncations are flagged as corruption.

A missing segment is the more serious case: the WAL
expected segment N but found segment N+1 with no
record of N. Either someone deleted a segment, or
the filesystem lost one.

Proceed to *Remediate → Restore from snapshot*.

### 7. redb metadata corruption

**Symptom:** log says `MetadataOpenError: redb
corruption detected` or `redb` is in the error
message.

The redb metadata file (`metadata.redb`) failed
redb's internal integrity check. Usually disk
corruption, occasionally a redb bug.

Same outcome as arena corruption: don't try to
repair the file. Proceed to *Remediate → Restore
from snapshot*.

### 8. Out of memory at startup

**Symptom:** log mentions OOM, or the process is
killed by the OOM killer (`dmesg | grep -i killed`).

Brain's startup memory cost is dominated by the
embedder model (~130 MB) and the HNSW index rebuild
(scales with memory count). On a host that's tight
on RAM, startup can fail before serving any
requests.

```bash
free -h
dmesg | tail -30 | grep -i 'out of memory\|killed process'
```

Proceed to *Remediate → Add memory or split shards*.

### 9. Process alive but stuck

**Symptom:** `pgrep brain-server` returns a PID, but
the metrics port isn't responding and logs stop
mid-startup.

Probably stuck in WAL replay or HNSW rebuild on a
very large shard. Check log frequency — is anything
being logged? If yes, it's progressing (slowly); if
no, it's hung.

```bash
sudo ls -la /proc/$(pgrep brain-server)/fd | wc -l
sudo cat /proc/$(pgrep brain-server)/status | grep -i state
```

If it's making file descriptor activity, it's
working. Wait it out (could be 10-30 min for very
large shards). If genuinely hung, take a stack dump
([DR-04](dr-04-profiles-and-heap-dumps.md)) and
proceed to *Remediate → Hung startup*.

---

## Remediate

Match the branch you identified above.

### Fix config

1. Edit `/etc/brain/config.toml` to address the
   error.
2. Verify it parses (see Diagnose step 2).
3. Confirm the data directory referenced exists and
   is owned correctly.
4. Start:

   ```bash
   sudo systemctl start brain-server
   ```

5. Verify (see *Verify* below).

### Free the port

If a previous brain-server is the culprit:

```bash
sudo systemctl stop brain-server   # in case it's restarted
sudo pkill -f brain-server
sleep 2
sudo ss -tlnp 'sport = :9090'      # confirm port free
sudo systemctl start brain-server
```

If a *different* service is using the port (unusual),
either move the other service or change the port in
the config.

### Fix permissions

```bash
# Identify the user the substrate runs as.
systemctl show brain-server -p User --value
# Or for Docker, the container's user.

# Restore ownership.
sudo chown -R brain:brain /var/lib/brain/data
sudo chmod 700 /var/lib/brain/data
sudo systemctl start brain-server
```

Don't chmod 777 anything. That's not a fix.

### Restore from snapshot

This is the path for arena CRC, WAL corruption, redb
corruption, missing segments. The procedure is in
[RB-07](rb-07-corruption-recovery.md) — follow that
runbook from here.

The short version (full detail in RB-07):

1. **Pick the snapshot to restore.** Most recent
   known-good. List with:

   ```bash
   brain-cli admin snapshots --list-offsite
   ```

   …or from your snapshot storage location.

2. **Move (not delete) the broken data directory:**

   ```bash
   sudo mv /var/lib/brain/data /var/lib/brain/data.broken.$(date +%s)
   ```

   You may need the broken state later for forensics.

3. **Restore the snapshot into the data directory:**

   ```bash
   sudo /usr/local/bin/brain-snapshot-restore \
       --from <snapshot-location> \
       --to /var/lib/brain/data
   sudo chown -R brain:brain /var/lib/brain/data
   ```

4. **Start the substrate.** It'll replay WAL records
   from the snapshot's `durable_lsn` forward (you'll
   lose data written after the snapshot was taken;
   that's the trade-off).

   ```bash
   sudo systemctl start brain-server
   ```

5. **Verify** (next section).

If you have **no usable snapshot**, escalate to
engineering immediately. The substrate can sometimes
recover from a more aggressive truncation; doing so
without engineering input risks further damage.

### Add memory or split shards

If startup OOMed:

- **Short term:** increase the host's RAM if you can
  (cloud resize, larger machine).
- **Medium term:** reduce per-shard memory by
  splitting into more shards (smaller HNSWes per
  shard). This is a redeploy; not an in-place fix.
- **Workaround for one boot:** lower the embedder
  cache size (`embedder.cache_size = 100`) and other
  RAM-heavy settings in config; this lets the
  substrate at least come up, even if degraded.

### Hung startup

If the substrate is alive but appears stuck:

- **Wait first.** Large shards legitimately take 10-30
  minutes to rebuild HNSW. Don't kill prematurely.
- **Take a stack dump** with `pstack` or `gdb`
  ([DR-04](dr-04-profiles-and-heap-dumps.md)). If
  all threads are in `hnsw_rebuild` functions, it's
  progressing slowly but normally.
- **If genuinely hung** (thread in `__lll_lock_wait`
  with no progress for 5+ minutes), escalate. This is
  a substrate bug.

---

## Verify

Once the substrate is back up, confirm it's actually
working:

```bash
# Process is up.
pgrep brain-server

# Metrics endpoint responds.
curl -fsS http://127.0.0.1:9091/metrics | head -5

# Wire port is listening.
ss -tlnp 'sport = :9090'

# Smoke-test encode/recall.
brain-cli encode "smoke test memory $(date +%s)"
brain-cli recall "smoke test"
```

Expected: encode returns a `MemoryId`, recall
returns the just-encoded memory.

Then check that all shards are healthy:

```bash
brain-cli admin shards
```

Every shard should be `active`, not `recovering` or
`failed`.

Finally, check the alert is clearing in Prometheus /
Grafana. `BrainSubstrateDown` should resolve within
a minute of the substrate starting to serve.

---

## Post-incident

In the incident channel, post the resolution
summary:

```
:white_check_mark: Resolved at HH:MM UTC. Total downtime ~Xm.
Root cause: <one-line>.
Remediation: <what you did>.
Follow-ups: TICKET-NNNN ...
Postmortem: required (P1).
```

Always write a postmortem for RB-01 incidents.
P1, by definition. See
[`postmortem-template.md`](postmortem-template.md).

Action items to consider:

- Did the runbook lead you to the cause efficiently?
  If not, update it.
- Was the diagnostic bundle complete? Add anything
  missing to [DR-01](dr-01-diagnostic-bundle.md).
- Did the underlying root cause need a code change?
  File a ticket.
- Was a snapshot fresh enough to restore from
  cleanly? If not, revisit your snapshot cadence
  ([OP-03](op-03-backup-verification.md)).

---

## Prevention

- **Run [OP-02](op-02-snapshot-restore-drill.md)
  periodically.** A restore drill catches "the
  snapshot mechanism is broken" before a real
  incident depends on it.
- **Use a config-management tool** that validates
  the TOML before deploying. Last-minute config
  edits in production are how you ended up here.
- **Monitor disk health.** SMART errors precede
  arena / WAL corruption by hours-to-days, usually.
  Add disk-health alerts.
- **Don't share `data_dir` between processes.** The
  single-writer invariant ([DR-05](dr-05-verifying-durability-invariants.md))
  depends on it.

---

## Related runbooks

- [RB-04 — Disk filling](rb-04-disk-filling.md)
- [RB-07 — Recovery from corruption](rb-07-corruption-recovery.md)
- [RB-12 — Restart loop](rb-12-restart-loop.md)
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)

---

## Last validated

*Update on first use.*
