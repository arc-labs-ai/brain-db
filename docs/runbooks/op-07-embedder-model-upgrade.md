# OP-07: Embedder model upgrade

**Severity:** **operator-triggered**. P3 when
planned; P2 if responding to an embedder-related
issue (poor recall quality, deprecated upstream,
tokenizer bug).
**Alert:** none routinely. May be linked to
`BrainRecallQualityDegraded` if the upgrade is the
remediation for a recall-quality incident.
**SLO impact:** during the transition, recall
quality is **mixed** — old memories carry the old
fingerprint and old embedding space, new memories
carry the new one. Until re-embed completes,
hybrid queries return a blend of "old space" and
"new space" hits and ranking is unreliable for
queries that span both populations.
**Estimated duration:** hours to days. The swap is
minutes; the re-embed phase dominates and scales
linearly (~5–10 ms per memory on CPU).
**Skill level:** comfortable with the substrate's
embedder lifecycle, model fingerprints, and the
re-embed worker. You should read `embedder-info`
output without consulting the docs.

When to upgrade:

- A newer model is empirically better on your
  workload (validated in staging — see Step 1).
- The current model is deprecated upstream or has
  a known tokenizer / weight bug.
- You're moving to a domain-specific or larger
  model.
- A security advisory affects the weights or
  tokenizer.

The staleness story (read this before you start):
every memory stores the *model fingerprint* —
hash of (config, tokenizer, weights, dim,
normalize-flag) — that produced its vector.
Replacing the model means existing vectors live in
the old embedding space; cosine distance against a
query embedded with the new model is **not
meaningful**. The substrate doesn't reject those
recalls — it returns degraded results, and the
degradation is invisible from the wire. Until
every stale memory has been re-embedded, you are
operating in a mixed state.

This runbook is a multi-phase maneuver: plan
(Step 1) → snapshot (Step 2) → swap (Steps 3–5)
→ re-embed (Steps 6–7) → verify (Step 8). The
re-embed phase is by far the longest.

---

## Am I in the right runbook?

Use this if you're planning to:

- Replace the embedder model with a different one
  (different weights, dim, tokenizer, or
  normalize-flag).
- Upgrade BGE-small to a newer revision when the
  fingerprint changes.
- Switch to a domain-specific embedder.

Use a different runbook if:

- **Recall is degraded but cause is unconfirmed** —
  start at [RB-06](rb-06-recall-degraded.md). The
  upgrade may not be the right remediation.
- **You're hot-reloading config that doesn't
  change the embedder fingerprint** — that's
  [OP-08](op-08-config-change-rollout.md).
- **You're doing a routine version restart** —
  that's [OP-01](op-01-rolling-restart.md).

If the new model has the same dim and produces an
identical fingerprint (rare — e.g. a bit-identical
re-package), no re-embed is needed and you're
doing a plain restart; use OP-01.

---

## Pre-flight checklist

Before starting:

- [ ] **Validate the new model in staging.** Don't
      skip this. Step 1 is the largest risk
      reducer in this runbook.
- [ ] **Estimate the re-embed cost.** Count
      memories, multiply by ~5–10 ms.
      ```bash
      brain-cli admin memory-count
      ```
      10M memories ≈ 50,000–100,000 CPU-seconds
      (~14–28 CPU-hours). Plan a window where
      this CPU load is acceptable, or rate-limit
      the worker.
- [ ] **Confirm dim compatibility.** If the new
      model emits a different vector dimension,
      the arena slot must still fit. v1.0's
      1600-byte slot accommodates up to
      1536-byte vectors. BGE-small is 384 dim ×
      4 bytes = 1536 bytes; a 768-dim model fits
      (3072 bytes does not — that's out of
      scope). Verify:
      ```bash
      brain-cli admin embedder-info | jq .dim
      ```
- [ ] **Take a fresh snapshot.** Step 2 will take
      another one immediately before the swap; a
      clean baseline an hour earlier is a useful
      extra checkpoint.
- [ ] **Verify off-server backups are healthy.**
      The pre-upgrade snapshot is your only
      rollback path. See
      [OP-02](op-02-snapshot-restore-drill.md) for
      the restore drill if you haven't exercised
      it recently.
- [ ] **Identify the rollback decision point.**
      What recall quality signal triggers rollback?
      Write it down — improvising under pressure
      is how you end up living with a bad model.
- [ ] **Notify stakeholders.** "Embedder upgrade
      from `<old>` to `<new>`. Re-embed phase
      starting HH:MM UTC, expected duration X
      hours. Recall quality may be inconsistent
      during the transition window."
- [ ] **Schedule the re-embed for off-peak.** It
      is CPU-heavy and competes with serving
      traffic.

---

## Step 1 — Validate the new model in staging

**Do not skip this step.** A bad embedder is the
worst kind of regression: it doesn't crash, it
doesn't alert, it just makes every recall slightly
worse for the rest of the substrate's life.

In a staging environment cloned from prod (or a
representative subset):

```bash
sudo systemctl stop brain-server

# Update staging config to point at the new model.
sudo vim /etc/brain/brain.toml
# [embedder]
# model_path = "/var/lib/brain/models/bge-large-en-v1.5"

sudo systemctl start brain-server

# Verify the new model loaded.
brain-cli --endpoint staging admin embedder-info
```

Run a recall-quality test against a held-out
evaluation set, both on the new model and on a
baseline still running the old:

```bash
brain-cli --endpoint staging admin eval-recall \
    --test-set /var/lib/brain/eval/golden-queries.jsonl \
    --report /tmp/staging-recall-new.json

brain-cli --endpoint staging-baseline admin eval-recall \
    --test-set /var/lib/brain/eval/golden-queries.jsonl \
    --report /tmp/staging-recall-old.json

diff <(jq -S . /tmp/staging-recall-old.json) \
     <(jq -S . /tmp/staging-recall-new.json)
```

Decision criteria — **abort if any fail**:

- New model's nDCG@10 is **at least as good** as
  the old on the eval set. Equal is acceptable;
  worse is not. "Just slightly worse" is still
  worse, and you've spent the re-embed budget for
  nothing.
- New model's p99 `encode` latency is within 1.5×
  of the old. A 2× slower embedder changes
  [RB-02](rb-02-high-latency.md)'s baseline.
- Vector norm distribution is sane (not all near
  zero, not exploding).
- Tokenizer handles representative inputs without
  empty token sequences for non-trivial text.

If any criterion fails: **abort**. File an issue,
revisit the model choice. Don't proceed.

---

## Step 2 — Take a snapshot

The single safety net for the entire operation.
If the new model misbehaves despite the staging
validation, you restore from this snapshot — not
from a half-re-embedded state.

```bash
brain-cli admin snapshot take \
    --label "pre-op07-$(date -u +%Y%m%dT%H%M%SZ)"

brain-cli admin snapshots --latest
```

Confirm:

- The snapshot ID is recorded somewhere outside
  this host.
- The snapshot has reached your off-server backup
  destination ([OP-03](op-03-backup-verification.md)
  to verify the pipeline).
- The snapshot includes the current embedder
  model and its fingerprint (`brain-cli admin
  snapshot describe <id>`).

Restoring this snapshot — model and all — is
your rollback. Verify you know how before moving
on; rollback below references
[OP-02](op-02-snapshot-restore-drill.md).

---

## Step 3 — Update embedder config

Stage the new model on the prod host(s):

```bash
sudo mkdir -p /var/lib/brain/models/bge-large-en-v1.5
sudo cp -r /path/to/new/model/* \
    /var/lib/brain/models/bge-large-en-v1.5/

sudo chown -R brain:brain \
    /var/lib/brain/models/bge-large-en-v1.5
```

Update the config:

```bash
sudo vim /etc/brain/brain.toml
```

```toml
[embedder]
# Old:
# model_path = "/var/lib/brain/models/bge-small-en-v1.5"
# New:
model_path = "/var/lib/brain/models/bge-large-en-v1.5"
normalize = true   # confirm matches new model's expectation
```

Save. **Do not restart yet.** If you have multiple
hosts, update the config on all of them before
the restart so the rolling restart sees the same
intent everywhere.

---

## Step 4 — Restart the substrate

Follow [OP-01](op-01-rolling-restart.md) for the
restart mechanics. This step rides on top of it.

Specific to this operation:

- The substrate detects the new model on startup,
  loads it, computes its fingerprint, and logs:
  ```
  embedder loaded: model_path=... fingerprint=<new-hash> dim=768
  ```
- Existing memories are *not* touched by the
  restart. Their stored fingerprints remain the
  old hash.
- New encodes after the restart carry the new
  fingerprint.

If the new model fails to load (corrupt weights,
incompatible tokenizer config, dim mismatch with
the arena slot), `brain-server` will fail-stop
during startup. Don't try to skip past it; revert
the config and restart the old model (see
Rollback).

Wait for the restart to complete fully — per-shard
recovery, workers running, healthcheck green —
before Step 5.

---

## Step 5 — Verify new encodes use the new model

Confirm the fingerprint reported by the running
substrate matches the new model:

```bash
brain-cli admin embedder-info
```

Expected:

```
model_path:  /var/lib/brain/models/bge-large-en-v1.5
fingerprint: <new-hash>
dim:         768
normalize:   true
loaded_at:   <recent timestamp>
```

Cross-check the fingerprint against the value you
observed in staging (Step 1). They must match —
if not, the prod host is loading a different
artifact than staging did and the staging eval
doesn't apply.

Write/read smoke test:

```bash
brain-cli encode "op-07 verify $(date -u +%s)"

brain-cli admin model-fingerprints \
    | jq '.[] | select(.count > 0)'
```

Expected: at least one entry with the new
fingerprint (count ≥ 1). The old fingerprint is
still present in the output for now — that's the
population you'll re-embed next.

---

## Step 6 — Trigger re-embed for stale memories

The substrate ships with a re-embed worker that
walks memories whose stored fingerprint differs
from the currently-loaded model's, recomputes
their embeddings, and writes them back. Trigger
it explicitly — it does not run on its own.

Larger batch = better throughput, more CPU
contention with serving:

```bash
NEW_FP=$(brain-cli admin embedder-info | jq -r .fingerprint)

# Conservative: ~100 memories/sec.
brain-cli admin re-embed \
    --filter "fingerprint != $NEW_FP" \
    --batch-size 32 \
    --rate-limit 100

# Aggressive (off-peak only): saturate CPU.
brain-cli admin re-embed \
    --filter "fingerprint != $NEW_FP" \
    --batch-size 256
```

The filter `fingerprint != <new>` matches every
memory not already in the new embedding space —
everything written before Step 4.

The worker:

- Reads each stale memory's text and metadata.
- Re-embeds with the currently-loaded model.
- Writes a new vector to the arena (the slot is
  versioned; readers using `MemoryId`s see
  consistent updates).
- Updates the stored fingerprint.
- WAL-logs each batch before acknowledging it.

The worker is idempotent — interrupting it and
restarting picks up where it left off based on
the stored fingerprint. Safe to pause:

```bash
brain-cli admin re-embed pause
brain-cli admin re-embed resume
brain-cli admin re-embed cancel
```

---

## Step 7 — Monitor the re-embed progress

This is the long phase. Set up watch terminals
and check in periodically — don't sit and stare.

```bash
brain-cli admin re-embed status
```

Expected:

```
state:        running
total_target: 10,000,000
processed:    2,143,287
remaining:    7,856,713
throughput:   ~120 mem/sec (5-min avg)
eta:          ~18h 10m
errors:       0
last_batch:   <timestamp>
```

Or via the fingerprint census:

```bash
brain-cli admin model-fingerprints
# fingerprint        count       pct
# <new-hash>      2,143,287    21.4%
# <old-hash>      7,856,713    78.6%
```

Watch for:

- **Throughput collapse.** If memories/sec drops
  dramatically, something else is competing for
  CPU. Is recall latency also spiking? If so,
  you're in [RB-02](rb-02-high-latency.md)
  territory; pause the re-embed.
- **Error count climbing.** A handful (deleted
  memories, malformed text) may be tolerable; a
  steady stream is a bug — pause and read logs.
- **Disk pressure.** Re-embed creates WAL records
  and may grow the arena tombstone pool. Monitor
  via [RB-04](rb-04-disk-filling.md).
- **Serving latency.** This is the SLO the
  re-embed is most likely to dent. Throttle if
  p99 climbs past your alert threshold:
  ```bash
  brain-cli admin re-embed pause
  brain-cli admin re-embed resume --rate-limit 50
  ```

For 10M memories at 100 mem/sec, expect ~28
hours. At 200 mem/sec, ~14 hours. Don't expect a
perfectly linear schedule.

If the operation spans more than 24 hours, brief
the next on-call shift.

---

## Step 8 — Verify

Once `re-embed status` reports `complete`:

```bash
brain-cli admin re-embed status
# state: complete
# processed: 10,000,000
# remaining: 0
# errors: 0
```

Confirm the fingerprint census shows only the new
model:

```bash
brain-cli admin model-fingerprints
# fingerprint        count        pct
# <new-hash>     10,000,000    100.0%
```

If any stale fingerprint remains with a non-zero
count, investigate before declaring complete. The
likely cause is the worker errored on those
memories. Resolve and re-run the worker against
the residual:

```bash
brain-cli admin re-embed --filter "fingerprint != $NEW_FP"
```

Repeat the recall quality eval — this time
against production:

```bash
brain-cli admin eval-recall \
    --test-set /var/lib/brain/eval/golden-queries.jsonl \
    --report /tmp/prod-recall-post-upgrade.json

diff <(jq -S . /tmp/staging-recall-new.json) \
     <(jq -S . /tmp/prod-recall-post-upgrade.json)
```

Expected: prod recall quality matches (within
noise) the staging-new baseline. If it does *not*,
something differs between staging and prod —
don't roll back yet, diagnose first via
[RB-06](rb-06-recall-degraded.md).

Final durability check — make sure the upgrade
didn't dent the invariants. See
[DR-05](dr-05-verifying-durability-invariants.md)
for the full procedure.

```bash
brain-cli admin verify --invariants all
```

If clean, take a fresh post-upgrade snapshot:

```bash
brain-cli admin snapshot take \
    --label "post-op07-$(date -u +%Y%m%dT%H%M%SZ)"
```

---

## Rollback

**Decide quickly.** The longer the new model has
been serving and the more memories have been
re-embedded into it, the more expensive rollback
gets; eventually it becomes a *forward* operation
(run OP-07 again with the old model as the new).

### Before Step 6 (no re-embedding yet)

Easy case. Stale memories are still in the old
space; only post-restart encodes used the new
model.

1. Stop the substrate.
2. Revert `/etc/brain/brain.toml` to the old
   `model_path`.
3. Restart per [OP-01](op-01-rolling-restart.md).
4. The handful of memories encoded with the new
   model are now stale relative to the restored
   old model. Re-embed them:
   ```bash
   OLD_FP=$(brain-cli admin embedder-info | jq -r .fingerprint)
   brain-cli admin re-embed --filter "fingerprint != $OLD_FP"
   ```
   This should complete in seconds.

### During or after Step 6 (re-embedding underway or complete)

Don't try to "un-re-embed" in place. Restore the
pre-upgrade snapshot from Step 2:

1. Cancel the re-embed worker:
   ```bash
   brain-cli admin re-embed cancel
   ```
2. Stop the substrate.
3. Restore the Step-2 snapshot per
   [OP-02](op-02-snapshot-restore-drill.md). The
   snapshot includes the old model config and
   old fingerprints throughout.
4. Restart.
5. Verify `embedder-info` shows the old
   fingerprint and `model-fingerprints` shows
   only the old hash.

**Data written between the snapshot and the
restore moment is lost.** That's the cost of this
rollback path. If you can't tolerate the loss,
you're not doing a rollback — you're doing an
emergency forward fix, and you need
[RB-06](rb-06-recall-degraded.md).

### If the new model is unloadable at startup

(Step 4 failure mode.) The substrate fail-stops
during boot. Revert the config file (one line)
and restart with the old model; nothing else has
changed. The easiest rollback in the runbook.

---

## Post-operation

Post in your team channel:

```
:white_check_mark: OP-07 embedder upgrade complete at HH:MM UTC.
Model: <old-name> (<old-fingerprint>) -> <new-name> (<new-fingerprint>).
Memories re-embedded: N.
Re-embed duration: Xh Ym.
Recall quality (post): nDCG@10 = <value> (baseline: <value>).
Issues encountered: <none / list>.
Snapshots: pre=<label>, post=<label>.
```

Update operational docs:

- The fingerprint registry — new fingerprint,
  model version, date upgraded, reason.
- Monitoring thresholds calibrated to the old
  model's latency baseline. The new model likely
  has a different `encode` latency profile;
  update [RB-02](rb-02-high-latency.md)'s
  reference numbers if needed.
- Eval set baseline numbers so the next OP-07
  starts from the correct comparison.

Follow-up review schedule:

- 24 hours: SLOs back to normal under serving
  load?
- 7 days: any latent issues surfaced by less
  common query patterns?
- 30 days: confirm no regression vs. the
  pre-upgrade baseline on real workload metrics.

---

## Pitfalls

### Skipping Step 1 (staging validation)

The single most expensive mistake in this
runbook. You re-embed 10M memories, burn
50,000–100,000 CPU-seconds, and discover the new
model is worse. Now you're rolling back, losing
hours of writes. Validate in staging. Always.

### Re-embedding during peak hours

The re-embed worker is CPU-heavy. Running it at
14:00 UTC on a high-traffic Tuesday will dent
serving latency and possibly trigger
[RB-02](rb-02-high-latency.md). Schedule for
off-peak; throttle aggressively if you must run
during business hours.

### Assuming "mixed mode" is fine

It isn't. While re-embed is in progress, a recall
query embedded with the new model is being
compared (via cosine distance) against some
vectors in the new space and some in the old.
Distances are not comparable across spaces.
Rankings during this window are **not meaningful**
for queries that span both populations. Treat
this as a degraded mode, not a steady state.

### Trying to "use both models simultaneously"

There's no supported mode where the substrate
serves the old model for old memories and the
new for new ones, fusing results. The fingerprint
exists to *detect* the mismatch, not *resolve* it.
If you're tempted to build that, stop — finish
the re-embed.

### Forgetting to re-warm caches

The embedder cache (and any LLM-extractor caches
keyed by embedder fingerprint) are invalidated by
the fingerprint change. Expect a cold-cache period
after Step 4: encode latency will be elevated for
the first minutes to hours of new traffic until
the caches refill. Don't mistake the cold-cache
spike for a model regression.

### Re-embedding without enough disk headroom

Re-embed writes WAL records and may allocate
fresh arena space if vectors changed dim. Confirm
at least 20% free disk on arena and WAL volumes
before Step 6. See
[RB-04](rb-04-disk-filling.md) for thresholds.

### Not snapshotting before Step 4

The Step-2 snapshot is the only clean rollback.
Skipping it because "the upgrade looked fine in
staging" leaves you with no exit if prod behaves
differently. Snapshots are cheap; their absence
is expensive.

### Confusing fingerprint with model name

The fingerprint is a hash of (config, tokenizer,
weights, dim, normalize-flag). Two artifacts
named `bge-small-en-v1.5` from different sources
may produce different fingerprints. The substrate
trusts the fingerprint, not the name. When
validating, compare fingerprints, not filenames.

### Restarting mid-re-embed without coordination

The re-embed worker is checkpointed and
resumable, so a restart doesn't lose progress.
But running [OP-01](op-01-rolling-restart.md)
during an active re-embed is two operations
overlapping — harder to attribute any regression.
Finish the re-embed, then restart.

---

## Related runbooks

- [OP-01 — Rolling restart](op-01-rolling-restart.md)
  — used by Step 4.
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
  — used by the post-Step-6 rollback path.
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)
  — for embedder-adjacent config changes that
  don't change the fingerprint.
- [RB-06 — Recall degraded](rb-06-recall-degraded.md)
  — the runbook OP-07 is often the remediation
  *for*; also the place to start if post-upgrade
  recall quality is worse than expected.
- [RB-02 — High latency](rb-02-high-latency.md)
  — if the re-embed phase dents serving latency,
  or if the new model's `encode` latency is
  meaningfully higher.
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)
  — used by Step 8's final integrity check.

---

## Last validated

*Update on first use.*
