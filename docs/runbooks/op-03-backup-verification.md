# OP-03: Backup verification

**Severity:** **operator-triggered**.
**Alert:** `BrainSnapshotStale` (weekly cadence) or
manual cron-driven invocation.
**SLO impact:** none directly. But the cost of a
stale backup at the moment of need is total data
loss — there is no replication in v1.
**Estimated duration:** ~30 minutes weekly;
~2 hours quarterly when combined with a full restore
drill ([OP-02](op-02-snapshot-restore-drill.md)).
**Skill level:** comfortable with the snapshot
worker, the snapshot bundle layout, and your
off-server storage (S3 / GCS / Azure Blob).

A backup you haven't verified isn't a backup. It's
a hope. This runbook catches the broken-chain
failures *before* the incident: the snapshot
worker silently wedged three weeks ago; S3
credentials rotated and uploads have been 403-ing;
retention deleting yesterday's snapshot instead of
last year's; manifest present but the arena blob
inside it truncated.

OP-02 is the deeper exercise — stand up a fresh
node from a bundle and confirm it works end to
end. OP-03 is the weekly sweep that decides
whether OP-02 has anything worth running against.

---

## Am I in the right runbook?

Use this if you're:

- Running the weekly backup-health check.
- Responding to a `BrainSnapshotStale` or
  `BrainSnapshotUploadFailed` alert.
- About to do something destructive (rolling
  restart, schema toggle, version upgrade) and
  need to confirm a working backup first — see
  also [OP-01](op-01-rolling-restart.md) step 1.
- Auditing backup hygiene after onboarding new
  on-call.

If on-disk data is already corrupted, go to
[RB-07](rb-07-corruption-recovery.md). If the disk
is filling and the snapshot worker has stopped
because of that, see [RB-04](rb-04-disk-filling.md)
first.

---

## Pre-flight checklist

- [ ] **Read access to the off-server bucket.**
      Confirm credentials work *before* you need
      them.
- [ ] **Know what "recent" means here.** Snapshot
      every 6h → "<8h old"; daily → "<30h old".
- [ ] **Know the retention policy** (e.g. "7 daily
      + 4 weekly + 12 monthly").
- [ ] **At least 5 GB of scratch disk** for the
      Step 4 download.
- [ ] **No incident is active.**
- [ ] **Decide if today is the quarterly drill.**
      If yes, plan to chain into
      [OP-02](op-02-snapshot-restore-drill.md)
      after Step 5.

---

## Step 1 — Confirm snapshot worker is running

If the worker is wedged, every other step will
look fine for a while and then fall off a cliff.
Start here.

```bash
brain-cli admin worker show snapshot
```

Expected:

```
worker:        snapshot
status:        running          # or "idle" between cycles
last_cycle:    2026-05-18T03:00:14Z   (16h ago)
last_success:  2026-05-18T03:00:42Z
last_error:    <none>
cycles_total:  428
cycles_failed: 0
```

What to verify:

- `status` is `running` or `idle` — never
  `errored`, `panicked`, or `stopped`.
- `last_success` is recent relative to your
  cadence. Snapshot every 6h and `last_success`
  18h ago = silently missed cycles.
- `cycles_failed` is zero or near-zero recently.

If anything is wrong, **stop and investigate** —
later checks would be exercising stale data.
Recurring failures = [RB-05](rb-05-worker-stuck.md):

```bash
brain-cli admin worker logs snapshot --since 7d --level warn
```

---

## Step 2 — Verify recent local snapshots

The worker writes bundles to the local snapshot
directory before shipping them off-server. Confirm
the local view first.

```bash
brain-cli admin snapshots --list --limit 10
```

```
ID                CREATED              SIZE     LABEL
01HXKQR8...       2026-05-18T03:00Z    4.2 GiB  scheduled
01HXG6E2...       2026-05-17T03:00Z    4.2 GiB  scheduled
01HXBJYS...       2026-05-16T03:00Z    4.1 GiB  scheduled
```

Sanity checks:

- **Newest is recent** — aligns with the cadence
  from Step 1.
- **Sizes are stable.** Half yesterday's is
  suspicious — truncated bundle, or a mass-forget
  ([RB-09](rb-09-mass-forget.md)).
- **No gaps.** Daily cadence with a 3-day hole =
  missed cycles; cross-check Step 1.

Then on disk:

```bash
ls -lh /var/lib/brain/snapshots/ | tail -20
df -h /var/lib/brain/snapshots
```

You want at least 2× a single snapshot of free
space so the next cycle can write. At 95% on this
volume, jump to [RB-04](rb-04-disk-filling.md).

---

## Step 3 — Verify off-server transfers

The step most teams skip and most teams regret. A
snapshot on the same host as its source data is
not a backup — it's a copy.

Compare local and remote (S3 shown; substitute
`gcloud storage ls -l "gs://..."` for GCS):

```bash
aws s3 ls s3://brain-snapshots/prod/ --recursive \
    | sort -r | head -40
```

What to verify:

- **Every local snapshot from the last N days has
  a corresponding remote object.** Newest local
  *must* be present remote.
- **Remote timestamps are close to local.** A
  4-hour upload lag = back-pressured uploader.
- **Object sizes match.** 4.2 GiB local, 200 MiB
  remote = truncated upload masquerading as
  success.

A quick diff:

```bash
brain-cli admin snapshots --list --format json --limit 30 \
    | jq -r '.[].id' | sort > /tmp/local-ids.txt

aws s3 ls s3://brain-snapshots/prod/ --recursive \
    | grep -oE '[0-9A-HJKMNP-TV-Z]{26}' \
    | sort -u > /tmp/remote-ids.txt

comm -23 /tmp/local-ids.txt /tmp/remote-ids.txt
# Anything printed = "exists locally, missing remote".
```

Non-empty output is a finding. The newest ID
appearing there is an alert. For multi-region
setups, repeat per region.

---

## Step 4 — Sample-check a snapshot's integrity

Existence is necessary but not sufficient. A
manifest can list files that aren't there; a file
can be present but its CRC mangled in transit.

Pick a recent — but not the newest — snapshot.
(The newest may still be uploading.)

```bash
SNAP_ID=$(brain-cli admin snapshots --list --limit 5 --format json \
    | jq -r '.[1].id')

mkdir -p /tmp/brain-snap-verify
aws s3 cp --recursive \
    "s3://brain-snapshots/prod/$SNAP_ID/" \
    "/tmp/brain-snap-verify/$SNAP_ID/"

brain-cli admin snapshot inspect "/tmp/brain-snap-verify/$SNAP_ID"
```

`inspect` lists the bundle: `manifest.json`,
`arena.bin`, `wal.tail`, `metadata.redb`,
`hnsw/<shard>.bin`, plus (knowledge layer)
`knowledge.redb`, `tantivy/`,
`extractor-cache.redb`. Each entry has a declared
size and checksum.

Then re-hash and compare:

```bash
brain-cli admin snapshot verify "/tmp/brain-snap-verify/$SNAP_ID"
```

Expected: `verify: OK (N files, total X GiB)`.

A failed verify means the bundle is bad. Don't
delete it — leave it for forensics — but do **not**
count it as a healthy backup. Sample one more
bundle to see whether the failure is isolated.
Multiple consecutive failures → escalate per
[IR-04](ir-04-incident-communication.md).

```bash
rm -rf /tmp/brain-snap-verify
```

---

## Step 5 — Verify the retention policy is being honored

A policy that doesn't run buries useful recent
snapshots in noise and inflates storage bills. A
policy that runs too aggressively — or against
the wrong bucket — silently destroys the older
backups you were counting on.

```bash
brain-cli admin snapshot policy show
```

```
policy:
  keep_daily:    7
  keep_weekly:   4
  keep_monthly:  12
  total_max:     23 bundles (approx)
  bucket:        s3://brain-snapshots/prod
```

Compare against reality:

```bash
# Local
brain-cli admin snapshots --list --limit 50 | wc -l

# Off-server
aws s3 ls s3://brain-snapshots/prod/ \
    | grep -oE '[0-9A-HJKMNP-TV-Z]{26}' \
    | sort -u | wc -l
```

Counts should be close and within the policy's
bound. Tolerate small overshoot during a retention
run; flag 2× overshoot or monotonic week-over-week
growth.

Then verify the *shape* of retention, not just the
count:

```bash
aws s3 ls s3://brain-snapshots/prod/ --recursive \
    | awk '{print $1, $2, $4}' | sort
```

Eyeball: 7 daily slots, 4 weekly, 12 monthly? Or
60 daily and nothing older? Shape tells you
whether the worker is making the right choices.

Finally — confirm retention points at the right
bucket. Cross-check the `bucket` field from `policy
show` against what you actually use. A prefix
overlapping with a data bucket is an incident:
stop the retention worker, open
[IR-04](ir-04-incident-communication.md), file a
postmortem ([postmortem-template](postmortem-template.md)).

---

## Step 6 — Quarterly: run a full restore drill

Steps 1–5 confirm snapshots *exist and look right*.
They do not confirm a snapshot can actually be used
to bring a substrate back up. The only thing that
confirms that is doing it.

Once a quarter — and after any change to the
snapshot worker, bundle format, retention policy,
or off-server path — follow
[OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
end to end against a recent bundle. If it's been
more than 100 days, do it this week.

---

## Verify

After the steps above, state the result in the
team channel so the next on-call doesn't guess.

```
:white_check_mark: OP-03 weekly verification — 2026-05-19
- Snapshot worker: running, last success 16h ago.
- Local snapshots: 10 most recent, no gaps.
- Off-server (s3://brain-snapshots/prod): 10/10 present, sizes match.
- Sample integrity (01HXG6E2...): verify OK.
- Retention: 23 bundles total, shape matches policy.
- Next quarterly drill (OP-02): 2026-07-15.
```

If anything was not green, list it under
`Findings:` with the ticket ID and the follow-up
action. Re-run OP-03 after the fix lands.

---

## Rollback

OP-03 is read-only. There is nothing to roll back.

The single exception is Step 5: if you found a
misconfigured retention policy and *paused* it,
that's a deliberate action, not a rollback. Re-
enable only after the config is corrected.

---

## Post-operation

- **Record the result** somewhere durable. The
  point of weekly cadence is the trend.
- **File tickets for every finding**, even small
  ones. A 38-hour gap this week becomes a two-week
  gap next month if no one tracks it.
- **Update `Last validated`** below if anything in
  the runbook itself needed correction.
- **For serious findings** — bad-bucket retention,
  multiple verify failures, worker silently wedged
  — open an incident
  ([IR-04](ir-04-incident-communication.md)) and
  file a postmortem
  ([postmortem-template](postmortem-template.md)).
  Near-misses are the most valuable kind.

---

## Pitfalls

### False sense of security from "snapshots exist"

`aws s3 ls` showing objects is not the same as a
recoverable bundle. Truncated uploads, silently
corrupted files, manifests pointing at the wrong
arena — all show as "present" in a bare listing.
Step 4 is the only thing that catches these. Don't
skip it because Steps 1–3 looked clean.

### Manifest fine, files corrupted

The opposite failure: `manifest.json` reads
cleanly but the actual arena / redb / HNSW
payloads were damaged in transit or by a bad block
device. `brain-cli admin snapshot verify` catches
this. Run it; read its output.

### Retention deletion on the wrong bucket

Retention configs are easy to mistype; the blast
radius is "everything older than today, silently".
A prefix overlapping with a data bucket — you find
out months later when someone asks "do we have a
snapshot from before the schema change?" and the
answer is no. The bucket-name check in Step 5 is
not paranoia.

### Only checking the newest snapshot

If the worker started silently failing three
weeks ago, the newest snapshot might still be
there from the last successful cycle. The checks
that catch this are the gap analysis in Step 2
and the local-vs-remote diff in Step 3.

### Verifying one region only

With cross-region replication, OP-03 should walk
each region. A single-region check passes during
a regional outage — exactly when you'd most need
the cross-region copy.

### Conflating OP-03 with OP-02

OP-03 is cheap and weekly. OP-02 is expensive and
quarterly. Skipping OP-02 because "we ran OP-03
last week" leaves you unable to actually restore.
Skipping OP-03 because "we did the big drill last
quarter" leaves the chain to rot in between.

### Leaving scratch space behind

Step 4 downloads a multi-GB bundle. Forgetting the
`rm -rf` eats breathing room for the next snapshot
cycle. The cleanup is not optional.

---

## Related runbooks

- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)
- [RB-04 — Disk filling](rb-04-disk-filling.md)
- [RB-07 — Corruption recovery](rb-07-corruption-recovery.md)
- [OP-01 — Rolling restart](op-01-rolling-restart.md)
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
- [IR-04 — Incident communication](ir-04-incident-communication.md)
- [Postmortem template](postmortem-template.md)

---

## Last validated

*Update on first use.*
