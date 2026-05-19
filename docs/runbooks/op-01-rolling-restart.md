# OP-01: Rolling restart / version upgrade

**Severity:** **operator-triggered**.
**Alert:** none.
**SLO impact:** brief per-shard unavailability during
each restart (30 s – 2 min depending on shard size).
**Estimated duration:** 15 minutes to 2 hours
depending on number of shards and per-shard recovery
time.
**Skill level:** comfortable with the substrate's
graceful-shutdown contract and the deployment system
(systemd / Docker / k8s).

When to use this runbook:

- Deploying a new substrate version.
- Restarting to clear in-memory state (after a
  config change, after a long uptime, after a hot
  patch).
- Routine maintenance restart.

For an **emergency** restart during an incident, see
the relevant incident runbook
([RB-01](rb-01-substrate-down.md),
[RB-08](rb-08-unresponsive.md), [RB-12](rb-12-restart-loop.md)).
This runbook is for the planned, deliberate version.

---

## Am I in the right runbook?

Use this if you're planning to:

- Replace `brain-server` with a new binary version.
- Restart `brain-server` to apply a config change
  that doesn't support hot-reload.
- Restart the substrate during a planned maintenance
  window.

If you're in the middle of an incident: pick the
relevant RB-N. This OP runbook assumes the substrate
is healthy now.

---

## Pre-flight checklist

Before starting:

- [ ] **Decide whether you need a rolling restart at
      all.** Some changes (env vars affecting only
      newly-started extractors) may be a no-op at
      runtime; verify the change actually requires
      a restart.
- [ ] **Take a fresh snapshot.** A recent snapshot
      is your safety net.
      ```bash
      brain-cli admin snapshot take --label "pre-restart-$(date +%Y%m%d)"
      ```
- [ ] **Verify backups are healthy.** Off-server
      copies of the latest snapshot reachable? See
      [OP-03](op-03-backup-verification.md).
- [ ] **Validate the new version / config in
      staging.** A change tested in dev doesn't
      automatically work in prod; verify against a
      production-like instance.
- [ ] **Identify the rollback path.** If the new
      version is bad, what's the procedure? Document
      it; don't improvise.
- [ ] **Notify stakeholders.** "Maintenance window
      from HH:MM-HH:MM UTC; brief per-shard
      unavailability."
- [ ] **Run during low-traffic hours.** Each shard
      will be briefly unavailable during its
      restart; doing it at 03:00 UTC is much less
      disruptive than 14:00 UTC.

---

## Step 1 — Snapshot

Always before any restart:

```bash
brain-cli admin snapshot take --label "pre-restart-$(date -u +%Y%m%dT%H%M%SZ)"
brain-cli admin snapshots --latest
```

Confirm the snapshot is on local disk and reaches
your off-server backup location. If anything goes
wrong during the restart, this is what you restore
to.

---

## Step 2 — Verify pre-restart health

```bash
# All shards healthy.
brain-cli admin shards | jq '[.[].status] | unique'
# Expected: ["active"]

# No alerts firing.
# (Check your alertmanager UI.)

# Workers running.
brain-cli admin workers | jq '[.[] | select(.status != "running" and .status != "idle")]'
# Expected: []

# Recent successful encodes / recalls.
```

If anything is unhealthy, **don't restart yet.** Fix
the pre-existing problem first; you don't want to
co-mingle a restart with troubleshooting.

---

## Step 3 — Single-shard restart (if multi-shard)

For multi-shard deployments, **restart one shard
first** to validate. The substrate handles per-shard
restart by:

1. Stopping that shard's executor cleanly.
2. The shard's flume channels drain.
3. The new version comes up; recovery runs.
4. The shard rejoins the topology.

```bash
# Trigger restart of just shard 0 (deployment-dependent;
# this command may require a specific orchestration).
brain-cli admin shard restart --shard-id 0
```

(In practice, this often translates to restarting
the brain-server process for *that* shard's host /
container — for v1 single-process-per-host
deployments, this is the same as a full restart.)

Wait for the shard to come back:

```bash
# Watch logs.
sudo journalctl -u brain-server -f | grep 'shard_id=0'

# Verify health.
brain-cli admin shards --shard-id 0
# Should be: "active"
```

Hot smoke test:

```bash
brain-cli encode "rolling restart smoke $(date +%s)"
brain-cli recall "rolling restart"
```

Once verified, proceed to the next shard.

---

## Step 4 — Full restart (for single-process / single-shard)

For deployments where shards live in one process,
the restart is the standard supervisor command:

```bash
# systemd
sudo systemctl restart brain-server

# Docker
docker compose restart brain-server

# Kubernetes (rolling restart of a StatefulSet)
kubectl rollout restart statefulset brain-server
```

Wait for it to come back:

```bash
# Healthcheck
curl -fsS http://127.0.0.1:9091/healthz
# Or with retry until healthy:
for i in {1..30}; do
    curl -fsS http://127.0.0.1:9091/healthz && break
    sleep 2
done
```

For Kubernetes, the rollout completes when the
StatefulSet's `currentRevision` matches its
`updateRevision`:

```bash
kubectl rollout status statefulset/brain-server --timeout=10m
```

---

## Step 5 — Verify

Run the standard post-restart checks:

```bash
# Process running.
pgrep brain-server

# All shards back online.
brain-cli admin shards | jq '[.[].status] | unique'
# Expected: ["active"]

# Recovery completed.
brain-cli admin workers
# Workers should be running again, not "recovering".

# Smoke test.
brain-cli encode "verify $(date +%s)"
brain-cli recall "verify"

# Connections accepted.
ss -tlnp 'sport = :9090'
```

For a version upgrade, also verify the new version
is what's running:

```bash
brain-cli --version
brain-cli admin server-version
```

Expected: matches the new release.

Watch the dashboards for at least 10 minutes:

- Latency back to baseline.
- Error rate near zero.
- Worker cycles completing normally.
- No restart-loop pattern emerging.

---

## Step 6 — (For multi-shard) Continue with remaining shards

If shard 0 came back cleanly and the substrate is
healthy, proceed with the remaining shards:

```bash
for SHARD in 1 2 3 4 5; do
    brain-cli admin shard restart --shard-id "$SHARD"
    # Wait for it to come back.
    for i in {1..60}; do
        STATUS=$(brain-cli admin shards --shard-id "$SHARD" | jq -r '.status')
        [ "$STATUS" = "active" ] && break
        sleep 5
    done
    # Pause between shards to let metrics settle.
    sleep 60
done
```

The `sleep 60` between shards is intentional —
restarting all shards at once gives no chance to
observe a regression on the first restart before it
hits all of them. The conservative pattern is one
shard at a time, with a 1-minute observation gap.

---

## Rollback

If the new version misbehaves:

### If you noticed quickly (one shard restarted)

```bash
# Roll back to the previous binary.
# Method depends on your deployment.

# systemd / package:
sudo apt install brain-server=<previous-version>

# Docker:
docker compose down
# Edit docker-compose.yml to use the previous image tag
docker compose up -d

# Kubernetes:
kubectl rollout undo statefulset brain-server
```

The substrate will restart with the old version. The
shard you'd already updated will come back on the
old version too (via the same restart path).

### If multiple shards are on the new version

The same rollback applies, but you've now lost the
"observe one shard first" safety. Watch closely
during the roll-back.

### If data was corrupted

(Unlikely from a simple restart, but if you suspect
it.) Restore the pre-restart snapshot from Step 1.
See [RB-07](rb-07-corruption-recovery.md).

---

## Post-operation

Post in your team channel:

```
:white_check_mark: Rolling restart complete at HH:MM UTC.
Version: <old> → <new>.
Duration: Xm.
Shards restarted: N.
Issues encountered: <none / list>.
```

If anything unexpected happened (even if not bad
enough to roll back), file a follow-up ticket so
the deploy procedure can improve.

---

## Pitfalls

### Restarting all shards at once

If your supervisor restarts all shards
simultaneously (because of how `systemctl restart`
works on a single-process multi-shard binary), you
lose the per-shard rollout safety. Some
deployments wrap this with a "drain, restart,
verify" loop per shard; if yours doesn't, plan for
short total-outage during restart.

### Not waiting for recovery

A shard isn't ready as soon as the process starts
listening — it still needs to replay any post-
snapshot WAL records and rebuild the HNSW. For
large shards this is minutes. Watch
`shards.status: "active"`, not just port-listening.

### Restarting during an alert

Don't restart while an incident-shaped alert is
firing unless restarting is the planned response. A
restart during a corruption-detection alert can
make corruption worse.

### Forgetting to verify off-server backups

Step 1's snapshot is on the local disk. If the disk
dies during the restart (unrelated event but
possible), the snapshot is gone too. Verify it
reached off-server storage before you start.

### Heroic restart cycles

If a restart doesn't fix the problem, don't keep
restarting hoping it will. After 2-3 unsuccessful
restarts, you're in [RB-12](rb-12-restart-loop.md)
territory; switch to that runbook.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [RB-12 — Restart loop](rb-12-restart-loop.md)
- [OP-02 — Snapshot restore drill](op-02-snapshot-restore-drill.md)
- [OP-03 — Backup verification](op-03-backup-verification.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use. OP-01 should be exercised
several times a year — every routine restart is an
opportunity to validate the runbook.*
