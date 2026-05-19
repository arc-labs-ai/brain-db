# OP-02: Snapshot restore drill

**Severity:** **operator-triggered**.
**Alert:** none.
**SLO impact:** none (drill runs against staging,
not prod).
**Estimated duration:** 1-3 hours, depending on
snapshot size and transfer bandwidth.
**Skill level:** comfortable with the snapshot
restore procedure and the substrate's data
directory layout.

A backup you have never restored is a hope, not a
backup. OP-02 is the periodic exercise that turns
hope into evidence. You pick a recent production
snapshot, restore it to a *staging* substrate (a
separate host, or at minimum a separate data
directory on the same host), and verify that the
restored instance is functional — that shards come
up, that encode/recall round-trips work, that CRC
sweeps pass, and that the counts on disk match what
the snapshot claims to contain.

The drill is non-destructive with respect to
production. It only touches the staging instance.
Run it on a schedule (quarterly is typical) and
after any change to the backup pipeline, the
snapshot format, or the restore tooling.

---

## Am I in the right runbook?

Use this if you're:

- Running the periodic restore drill (quarterly or
  whatever cadence your team has agreed).
- Validating a new backup pipeline end-to-end
  before declaring it production-ready.
- Verifying that a specific snapshot is restorable
  *before* a real incident forces you to find out.
- Onboarding a new operator to the restore
  procedure.

Don't use this if:

- You're recovering production from corruption or
  data loss. That's [RB-07](rb-07-corruption-recovery.md).
- You only need to check that snapshot files exist
  and have plausible sizes. That's the lighter
  check in [OP-03](op-03-backup-verification.md).
- You're trying to migrate a substrate to new
  hardware. The mechanics overlap, but the intent
  is different; treat it as a planned migration,
  not a drill.

OP-02 is rehearsal. RB-07 is the real performance.
You want OP-02 to have happened many times before
RB-07 is ever opened.

---

## Pre-flight checklist

Before starting:

- [ ] **Scheduled time window.** Block 2-3 hours.
      The drill is not urgent, so don't squeeze it
      into 45 minutes between meetings.
- [ ] **Staging capacity available.** A spare host
      (or VM, or container) with at least as much
      disk as the production data directory, plus
      30% headroom for restore scratch space.
- [ ] **Network path from backup storage to
      staging.** S3 credentials, GCS service
      account, or whatever your backup destination
      is — verify the staging host can read.
- [ ] **No conflict with production.** Confirm the
      staging instance binds different ports
      (`9090` → `19090`, etc.) and uses a different
      data directory. A misconfigured staging
      pointing at the prod data dir is the worst
      possible outcome of a "non-destructive" drill.
- [ ] **Latest backup verification is recent.**
      You're picking a snapshot to drill against;
      ideally the backup-verification job
      ([OP-03](op-03-backup-verification.md))
      flagged it as readable in the last 24h.
- [ ] **Drill log file ready.** A scratchpad
      (ticket, doc, or `~/drill-$(date +%F).log`)
      where you'll record each step's outcome.
- [ ] **Reviewer / second pair of eyes.** Optional
      but encouraged. A drill watched by a
      colleague is a drill that's also a training
      exercise.

---

## Step 1 — Pick a snapshot to restore

List recent snapshots from the backup location:

```bash
brain-cli admin snapshot list --remote
```

Expected output (abbreviated):

```
LABEL                                 TAKEN_AT              SIZE   MANIFEST
nightly-20260518T0300Z                2026-05-18T03:00:12Z  41 GiB ok
pre-restart-20260517T2200Z            2026-05-17T22:00:04Z  41 GiB ok
nightly-20260517T0300Z                2026-05-17T03:00:09Z  40 GiB ok
```

**Choose a recent nightly**, not the absolute
freshest one. Picking yesterday's nightly (rather
than this morning's) exercises the "snapshot
survives long enough to be useful" property, which
is what a real RB-07 would also rely on.

Record the label in your drill log:

```
Snapshot under test: nightly-20260517T0300Z
Size: 40 GiB
Manifest: ok (per backup-verification job)
```

If the manifest column reads anything other than
`ok` — stop. You've just found a broken backup
during a drill, which is exactly what drills are
for. Open a ticket and switch to investigating
why; do not silently pick a different snapshot.

---

## Step 2 — Provision the staging instance

The staging instance must be **isolated from
production**. Three acceptable shapes:

1. **Separate host.** Cleanest. A spare VM, a
   dedicated test box, or a one-off cloud instance.
2. **Separate container on the same host.** Fine,
   if you trust the container boundary.
3. **Same host, separate data dir + separate
   ports.** Lowest cost, highest risk of operator
   error. Acceptable for solo deployments where
   nothing else exists.

Whichever shape you pick, write down four things:

| Property        | Production                | Staging                         |
|-----------------|---------------------------|---------------------------------|
| Data dir        | `/var/lib/brain`          | `/var/lib/brain-drill`          |
| Listen port     | `9090`                    | `19090`                         |
| Metrics port    | `9091`                    | `19091`                         |
| systemd unit    | `brain-server.service`    | `brain-server-drill.service`    |

Create the staging data directory:

```bash
sudo mkdir -p /var/lib/brain-drill
sudo chown brain:brain /var/lib/brain-drill
sudo chmod 0750 /var/lib/brain-drill
```

If you're using a separate host, install the same
`brain-server` binary version that production
runs. Restoring a snapshot taken by version `X`
into a binary at version `Y` is **not** what this
drill tests — that's a migration test, run it
separately. Match versions exactly.

Confirm the staging binary's version matches:

```bash
/usr/local/bin/brain-server --version
# Compare to: brain-cli --version against prod.
```

---

## Step 3 — Transfer the snapshot

Pull the chosen snapshot from the backup location
into a scratch directory on the staging host. Do
**not** pull it into the staging data directory
yet — keep transfer and restore as separate steps
so you can verify the bytes arrived intact.

```bash
SCRATCH=/var/lib/brain-drill/incoming
sudo -u brain mkdir -p "$SCRATCH"

brain-cli admin snapshot fetch \
    --label nightly-20260517T0300Z \
    --to "$SCRATCH"
```

Expected output (abbreviated):

```
Downloading manifest... ok
Downloading 12 snapshot parts...
  part-00 ... 3.4 GiB ... ok (crc verified)
  part-01 ... 3.4 GiB ... ok (crc verified)
  ...
Total: 40.8 GiB, 6m12s, avg 109 MiB/s
Manifest verified: 12/12 parts present, all CRCs match.
```

If any part fails its CRC, the snapshot is
corrupt — you've just found a broken backup, again,
which is again what drills are for. File the
finding under OP-03 follow-up and stop the drill.

Record transfer duration in the drill log. Over
time this becomes a useful baseline for "is our
backup pipeline degrading?".

---

## Step 4 — Run the restore

Restore the snapshot into the staging data
directory. The substrate's restore tool is
explicit about source and destination; there is no
default "restore into the running data dir"
shortcut, on purpose.

Make sure the staging server is **not running**
before restoring:

```bash
sudo systemctl status brain-server-drill || true
# Expected: inactive (dead) or unit not found.
```

Run the restore:

```bash
sudo -u brain brain-cli admin snapshot restore \
    --from /var/lib/brain-drill/incoming \
    --to   /var/lib/brain-drill/data \
    --label nightly-20260517T0300Z
```

Expected output (abbreviated):

```
Restoring snapshot nightly-20260517T0300Z
  manifest:   ok
  arena.bin:  written, CRC verified (4 shards)
  wal/:       12 segments restored, tail CRC verified
  metadata/:  redb files restored, integrity check passed
  knowledge/: skipped (no knowledge layer in snapshot)
  hnsw/:      indexes restored (will be re-validated on startup)
Restore complete in 4m51s.
```

The restore tool **must** verify CRCs as it
writes. If it doesn't, or if any CRC mismatch is
reported, treat the snapshot as broken — log it
and abort.

Confirm the staging data directory now looks like
a real substrate data dir:

```bash
ls -la /var/lib/brain-drill/data/
# Expected: arena.bin, wal/, metadata/, hnsw/, manifest.json
```

---

## Step 5 — Start the staging server

Bring the staging instance up:

```bash
sudo systemctl start brain-server-drill
sudo systemctl status brain-server-drill --no-pager
```

Tail the logs and watch for recovery to complete:

```bash
sudo journalctl -u brain-server-drill -f
```

You should see recovery messages: WAL replay,
HNSW validation, shards transitioning to active.
This is the same recovery path a production
restart takes, so it gives you a real-world
estimate of how long recovery will take if RB-07
is ever invoked for real.

Wait until all shards report `active`:

```bash
for i in {1..60}; do
    STATUSES=$(brain-cli --addr 127.0.0.1:19090 admin shards \
        | jq -r '[.[].status] | unique | @csv')
    echo "[$i] shards: $STATUSES"
    [ "$STATUSES" = '"active"' ] && break
    sleep 10
done
```

Record the time from `systemctl start` to
"all shards active" in the drill log. That's your
realistic recovery-time-objective number.

---

## Step 6 — Smoke test the restored substrate

The substrate is up. Now prove it actually works.

**Round-trip an encode and recall:**

```bash
ADDR=127.0.0.1:19090
PROBE="drill probe $(date -u +%Y%m%dT%H%M%SZ)"

brain-cli --addr "$ADDR" encode "$PROBE"
brain-cli --addr "$ADDR" recall "drill probe"
```

The recall should return the just-written memory
near the top. If it doesn't, recovery is
incomplete or the HNSW didn't load.

**Probe pre-existing content.** Pick a memory you
know was in the snapshot — a known phrase from
production data, or a content-keyed lookup:

```bash
brain-cli --addr "$ADDR" recall "<phrase you know existed at snapshot time>"
```

You should get the same results you'd get against
production (at the snapshot's timestamp). If the
recall is empty or wildly different, something is
wrong with the index restore.

**Shard counts match the manifest:**

```bash
brain-cli --addr "$ADDR" admin shards \
    | jq '[.[].memory_count] | add'

# Compare to the manifest:
jq '.total_memory_count' /var/lib/brain-drill/data/manifest.json
```

The numbers should match exactly. A mismatch
means some memories didn't make it back — that's a
restore bug or a snapshot bug, and either is a
serious finding.

---

## Step 7 — Verify durability invariants

The hard part is over. Now run the standard
durability checks documented in
[DR-05](dr-05-verifying-durability-invariants.md)
against the staging instance:

```bash
# CRC sweep across the arena.
brain-cli --addr "$ADDR" admin verify --arena --full

# WAL chain integrity (tail and head).
brain-cli --addr "$ADDR" admin verify --wal

# Metadata consistency (redb integrity check).
brain-cli --addr "$ADDR" admin verify --metadata

# HNSW reachability spot check.
brain-cli --addr "$ADDR" admin verify --hnsw --sample 10000
```

All four must return clean. If any of them flag a
problem, the snapshot is not actually restorable
in the way it claims to be — record the failure
and stop the drill before tearing down (you may
need the staging instance for forensics).

If the original snapshot included a knowledge
layer, also confirm:

```bash
brain-cli --addr "$ADDR" admin knowledge status
# entity / statement / relation counts; tantivy index health.
```

---

## Step 8 — Tear down staging

Once the drill is successful, dismantle the
staging instance. Order matters — stop the server
*before* deleting its data, so you don't get
zombie file handles or partial-write debris on a
disk you're about to reformat.

```bash
sudo systemctl stop brain-server-drill
sudo systemctl status brain-server-drill --no-pager
# Expected: inactive (dead)
```

Confirm it's stopped before continuing:

```bash
pgrep -af brain-server-drill
# Expected: no output.
```

Wipe the staging data directory:

```bash
# Triple-check the path before running. This is destructive.
ls -ld /var/lib/brain-drill/data/
# Confirm it's the DRILL dir, not the production dir.

sudo rm -rf /var/lib/brain-drill/data
sudo rm -rf /var/lib/brain-drill/incoming
```

If you spun up a dedicated VM or container for the
drill, destroy it now. The longer staging stays
around, the more likely someone "borrows" it,
points an app at it, or treats it as
authoritative.

---

## Verify

The drill has succeeded when **all** of the
following are true:

- A snapshot was selected from the production
  backup location and its manifest verified.
- The snapshot was transferred to staging with
  every part's CRC confirmed.
- The restore tool wrote the data directory
  without CRC errors.
- The staging substrate started, recovered, and
  reached `shards.status: active` across all
  shards.
- An encode/recall round-trip succeeded against
  the restored instance.
- Known pre-existing content was recallable.
- Shard memory counts matched the manifest.
- DR-05's durability checks all returned clean.
- The staging instance was torn down without
  affecting production.

If any item is false, the drill failed. A failed
drill is not a wasted drill — it's a successful
discovery of a problem you would otherwise have
found at the worst possible moment.

---

## Rollback

The drill is non-destructive. There is nothing to
roll back; the staging instance was disposable by
design and production was never touched.

The only exception: if you accidentally pointed
the staging server at the production data
directory (a pre-flight violation), stop the
staging process immediately and run
[RB-07](rb-07-corruption-recovery.md) to assess
whether prod was damaged. This is rare but
documented because it has happened.

---

## Post-operation

In your drill log, record:

```
Snapshot under test:      nightly-20260517T0300Z
Snapshot age at drill:    27h
Transfer duration:        6m12s
Restore duration:         4m51s
Recovery duration:        8m20s   (start → all shards active)
Smoke test:               pass
DR-05 checks:             pass
Findings:                 none
Operator:                 <name>
Reviewer:                 <name or "none">
```

Post a one-line summary in the team channel:

```
:white_check_mark: OP-02 snapshot restore drill complete.
Snapshot nightly-20260517T0300Z restored to staging in 13m11s.
All durability checks pass. Next drill: <quarter>.
```

Update the team's drill-history doc (whatever you
use — a wiki page, a runbook appendix, a Notion
table). The history matters: degrading recovery
times over multiple drills is a leading indicator
that something is wrong with the snapshot
pipeline.

If the drill found a problem, file follow-up
tickets:

- Broken backup → ticket against the backup
  pipeline.
- Slow restore → ticket against capacity /
  storage tier.
- Restore tool bug → ticket against `brain-cli`.
- Procedure ambiguity → PR against this runbook.

Schedule the next drill before you close the
ticket. "Quarterly" is meaningless without a date.

---

## Pitfalls

### Restoring on top of production

The worst possible outcome: a staging instance
configured with the production data directory
path. The restore tool will happily overwrite
prod. Defenses: separate hostnames, separate data
dir paths, separate systemd units, and a
pre-flight check that lists the data dir before
restoring.

### "It restored, ship it"

A successful restore is not the same as a
functional substrate. Skipping the smoke test and
the DR-05 checks means you've validated the bytes
moved, not that they mean anything. Run Steps 6
and 7 every time.

### Drilling against an unrepresentative snapshot

If you always drill against the same small test
snapshot, you're testing a code path, not a
production scenario. Pick a recent *real*
production snapshot — its size, its shard
topology, its memory count are what you need to
validate against.

### Picking the freshest snapshot

The very newest snapshot (taken minutes ago) is
the easiest to restore — it's still in the
pipeline's hot cache. Picking yesterday's nightly
exercises the cold-storage path, which is what
RB-07 would actually use.

### Leaving staging running

A "temporary" staging instance left running for
weeks becomes a data hazard. Someone discovers
it, points a tool at it, and now there are two
sources of truth. Tear it down the same day.

### Not noticing a silent restore-time degradation

Recovery time creeping up by 20% per quarter
across drills is something only the drill log
will reveal. Record durations every time, and
look at the trend at least once a year.

### Skipping the drill because "we did it last
quarter"

Quarterly means quarterly. The whole point of a
drill is to catch the failure mode that develops
between drills. If you skip one, you've turned a
3-month confidence window into a 6-month one
without noticing.

### Confusing the drill with the real thing

Don't merge OP-02 and RB-07 in your head. OP-02
is calm, scheduled, against staging. RB-07 is
under pressure, urgent, against production. They
share the same restore command and that's it.

---

## Related runbooks

- [RB-07 — Corruption recovery](rb-07-corruption-recovery.md)
  — the real-incident version of restoring from
  snapshot.
- [OP-03 — Backup verification](op-03-backup-verification.md)
  — the lighter-weight check that runs between
  drills.
- [OP-01 — Rolling restart](op-01-rolling-restart.md)
  — relevant for the "take a snapshot before
  restart" step.
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
  — collect a bundle from staging if the drill
  surfaces something weird.
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)
  — the checks invoked from Step 7.
- [IR-04 — Incident communication](ir-04-incident-communication.md)
  — if the drill reveals a broken backup that
  would have caused a real incident, communicate
  it as a near-miss.

---

## Last validated

*Update on first use.*
