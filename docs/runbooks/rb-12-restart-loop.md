# RB-12: Restart loop

**Severity:** **P1**.
**Alert:** `BrainRestartLoop` (process restart rate
above threshold; e.g., >3 restarts in 5 min).
**SLO impact:** total outage; the substrate is up for
seconds and then crashes again. Every restart loses
in-flight work.
**Estimated duration:** 30 minutes to 2 hours.
**Skill level:** comfortable with reading crash
backtraces and diagnosing startup-phase failures.

The substrate's supervisor (systemd, Docker, k8s) is
repeatedly restarting the brain-server process. Each
attempt either crashes during startup or shortly
after starting to serve.

---

## Am I in the right runbook?

You should see:

- Process starts (PID appears) and exits within seconds-
  to-minutes.
- Supervisor (systemd / Docker / k8s) is restarting
  it repeatedly.
- Restart count climbing in metrics or `systemctl
  status`.
- The substrate has not been stably running for any
  meaningful period in the last 10+ minutes.

If the substrate **starts once and then dies and
doesn't restart**, that's [RB-01](rb-01-substrate-down.md).
If it's **running and unresponsive**, that's
[RB-08](rb-08-unresponsive.md).

The defining symptom: **the supervisor sees the
process exit, restarts it, the process exits again,
and around it goes**.

---

## Stop the bleeding

A restart loop is destructive — each crash may leave
partial WAL writes, partial fsyncs. **Stop the
supervisor's restart logic** first:

For systemd:

```bash
sudo systemctl stop brain-server          # stop and pause restarts
```

For Docker:

```bash
docker update --restart=no brain-server
docker stop brain-server
```

For Kubernetes:

```bash
# Drop the StatefulSet replica count so it stops respawning.
kubectl scale statefulset brain-server --replicas=0
```

With the substrate stopped, you have time to
diagnose without the destructive restart cycle
making things worse.

Then:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle ([DR-01](dr-01-diagnostic-bundle.md))
   — particularly the logs from the recent crash
   cycles, while they're still in the journal.

---

## Diagnose

### 1. What's the crash signature?

For each crash, the supervisor's logs should show:

- The PID it started.
- The exit code or signal.
- Brain's own log output before exit.

```bash
# Recent restarts in journalctl.
sudo journalctl -u brain-server --since "30 min ago" \
    | grep -E 'Started brain-server|brain-server.service: '

# The substrate's own logs from the last attempt.
sudo journalctl -u brain-server --since "10 min ago"
```

Look for:

- **Exit code 0**: the substrate exited "cleanly" but
  the supervisor restarted it anyway. Usually a
  supervisor misconfiguration (always restart on
  exit, not just on failure).
- **Exit code 1-127**: brain-server returned a
  non-zero exit code. The log usually says why.
- **Signal SIGKILL (137)**: killed by OOM or
  supervisor. Check `dmesg | grep -i 'out of
  memory'`.
- **Signal SIGSEGV (139)**: substrate crashed
  (segfault). Substrate bug; need to escalate.
- **Signal SIGABRT (134)**: substrate aborted (panic,
  assertion failure). Substrate bug or unrecoverable
  state.

### 2. Is it crashing during startup?

```bash
# Approximate uptime of the last crashed instance.
sudo journalctl -u brain-server --since "10 min ago" \
    | grep -E 'starting|listening|stopping|exited'
```

If the substrate dies within seconds of starting,
the issue is in startup (config / disk / recovery).
If it dies after running for a few minutes, it's
likely after starting to serve (OOM, request-driven
crash).

### 3. Crash during WAL recovery?

The most insidious case: WAL replay encounters a bad
record, crashes the substrate, supervisor restarts,
recovery hits the same bad record, crashes again.

Look in the log for:

```
Replaying WAL segment seg-...wal
... (something happens, then crash)
```

If recovery is the cause, the substrate will fail
the same way every time. **Don't let it restart**;
that's why step 1 stopped the supervisor.

Move to [RB-07](rb-07-corruption-recovery.md) —
this is corruption.

### 4. Crash on a specific request?

If startup completes (`brain-server listening on
...`) but the process dies within seconds *after*
that, possibly a specific request is triggering the
crash.

```bash
# Last few requests before the crash.
sudo journalctl -u brain-server --since "10 min ago" \
    | grep -E 'request_id|memory_id' | tail -50
```

If you see a pattern (same `request_id` retried,
same operation type), that request is poison. A
client is retrying it; each retry kills the
substrate. Disable that client / agent at the LB
level temporarily to break the cycle.

### 5. OOM at startup?

```bash
sudo dmesg | tail -50 | grep -iE 'out of memory|oom-killer'
```

If OOM killed the substrate during startup, the
host doesn't have enough RAM for the substrate's
startup phase (HNSW rebuild is the usual culprit on
large shards).

Move to [RB-03](rb-03-memory-pressure.md) — this is
memory pressure manifesting at startup.

### 6. Permissions changed?

If a recent operation (deploy, manual `chown`,
volume remount) broke permissions, the substrate
exits with "Permission denied" on its first file
access during startup. Fix per
[RB-01](rb-01-substrate-down.md)'s permissions
branch.

### 7. Configuration mid-deploy

If a deploy is in progress, the new version may
have an incompatible config. Check:

- What changed in the deploy?
- Was config validated?
- Roll back the deploy.

Rolling back is often the fastest path to a stable
restart loop than diagnosing a code regression.

---

## Remediate

### If startup is corrupting the WAL

Don't let it restart. Move to
[RB-07](rb-07-corruption-recovery.md) — restore from
snapshot.

### If a poison request is the cause

Block the source temporarily:

- LB / WAF level: filter the offending agent ID or
  IP.
- Brain-side: disable the agent's token if your
  auth setup supports it.

Then:

1. Restart the substrate.
2. Capture a stack dump or repro the request in a
   staging environment.
3. Escalate to engineering with the stack dump and
   the offending request shape.

### If OOM at startup

Reduce startup memory footprint:

- Lower `embedder.cache_size` in config.
- If multi-shard, consider starting shards
  sequentially rather than all at once (deployment-
  level work).
- Add RAM if the host doesn't have enough.

See [RB-03](rb-03-memory-pressure.md) for the full
diagnosis.

### If permissions broke

```bash
sudo chown -R brain:brain /var/lib/brain/data
sudo chmod 700 /var/lib/brain/data
sudo systemctl start brain-server
```

(See RB-01.)

### If recent deploy is the cause

Roll back:

```bash
# Docker:
docker pull brain-server:<previous-tag>
# update the service to use the previous tag, redeploy

# Kubernetes:
kubectl rollout undo statefulset brain-server

# systemd / bare metal:
sudo apt install brain-server=<previous-version>
# or downgrade however your package manager works
```

After rollback, re-enable the supervisor and verify.

### If substrate bug (segfault / panic during normal operation)

Capture the bundle, escalate to engineering. Don't
keep restarting in hopes the bug disappears.

Workarounds:

- **Disable a feature.** If a specific extractor is
  panicking, disable it (`[extractor.X]
  enabled = false`).
- **Restore to a previous version.** Same as the
  deploy-rollback case.
- **Bypass the bad code path.** If the bug is in
  HNSW maintenance, pause that worker.

These are stopgaps, not fixes.

---

## Verify

After remediation:

```bash
# Re-enable supervisor.
sudo systemctl start brain-server          # restart enabled by default

# Confirm stable.
sleep 60                                    # wait a full minute
sudo systemctl is-active brain-server       # active
sudo systemctl show brain-server -p NRestarts --value
```

The restart count should be stable (not climbing).
Watch for at least 5 minutes to confirm.

Smoke test:

```bash
brain-cli encode "post-restart smoke $(date +%s)"
brain-cli recall "smoke"
```

The `BrainRestartLoop` alert clears once the restart
rate is normal.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Symptom: substrate restart-looping for ~Xm, supervisor restarted Y times.
Root cause: <e.g., OOM during HNSW rebuild after a config change reduced
host RAM>.
Remediation: <e.g., reverted config, host has full RAM now>.
User impact: total outage during the loop.
Stack dump / bundle: <link>.
Follow-up: TICKET-NNNN.
Postmortem: required (P1).
```

Postmortem rule: **always** for RB-12. Restart loops
are usually substrate bugs or deployment mistakes;
both are worth documenting.

---

## Prevention

- **Capture stack dumps automatically.** A
  supervisor configured to capture a core dump on
  abnormal exit gives engineering the data they
  need without manual capture.
- **Don't auto-restart blindly.** A supervisor that
  retries 3 times and then *stops* (rather than
  retrying forever) prevents corruption from
  compounding. Configure exit-code-aware restart
  policies.
- **Validate configs before deploy.** Use a config-
  management pipeline that runs `brain-server
  --check-config` against the candidate.
- **Canary deploys.** Deploy a new version to one
  shard / one instance first; verify it's stable
  for 10+ minutes before rolling out broadly.
- **Capacity planning.** Don't deploy onto hosts
  that are tight on RAM; the headroom is what keeps
  startup OOMs from cascading into restart loops.
- **Have a rollback plan.** Every deploy should
  have a documented rollback procedure. Test it
  occasionally.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [RB-03 — Memory pressure / OOM](rb-03-memory-pressure.md)
- [RB-07 — Recovery from corruption](rb-07-corruption-recovery.md)
- [RB-08 — Substrate becoming unresponsive](rb-08-unresponsive.md)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)
- [OP-01 — Rolling restart / version upgrade](op-01-rolling-restart.md)

---

## Last validated

*Update on first use.*
