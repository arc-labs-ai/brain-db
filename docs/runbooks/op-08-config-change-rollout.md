# OP-08: Configuration change rollout

**Severity:** **operator-triggered**.
**Alert:** none routinely. Some config changes are
made in response to alerts (raising worker
concurrency per [RB-05](rb-05-worker-stuck.md),
lowering LLM budget per
[RB-14](rb-14-llm-cost-spike.md), etc.).
**SLO impact:** brief unavailability during restart
(if the changed field needs one); none for hot-
reloadable changes.
**Estimated duration:** 30 minutes to 2 hours.
**Skill level:** comfortable with the TOML config
and the deployment system (systemd / Docker / k8s).

The shortest path from "I want to change a config
value" to "it's in production" is `vim
/etc/brain/config.toml` and a restart. That path is
the wrong default: typos become restart-time
outages, semantically-valid-but-wrong values surface
only under load, and without version control there's
no diff and no audit. OP-08 adds four guardrails:
**validate**, **know the reload mode**, **stage**,
**keep the previous config trivially restorable**.
When an incident RB says "roll per OP-08," the RB
tells you *what* to change; this runbook tells you
*how*.

---

## Am I in the right runbook?

Use OP-08 if you're changing a value in
`/etc/brain/config.toml`, adding or removing a
section, changing a `BRAIN_*` env override, or
rolling a new ConfigMap.

Different runbook if you're rotating TLS certs
([OP-04](op-04-tls-cert-rotation.md)), auth tokens
([OP-05](op-05-auth-token-rotation.md)), LLM keys
([OP-06](op-06-llm-api-key-rotation.md)), upgrading
the embedder ([OP-07](op-07-embedder-model-upgrade.md)),
or deploying a new binary
([OP-01](op-01-rolling-restart.md)). For both
binary and config: validate the config with OP-08
first, then roll the binary with OP-01.

---

## Pre-flight checklist

- [ ] **Know what you're changing and why** —
      field, old value, new value, reason. If you
      can't articulate it, don't deploy it.
- [ ] **Source config in version control.** If
      today is the day you discover it isn't, stop
      and fix that first — see
      [DR-03](dr-03-safe-production-access.md).
- [ ] **Staging instance exists.** If none, escalate.
- [ ] **Reload mode identified** (Step 2).
- [ ] **Rollback path planned** — `config.toml.bak`,
      previous ConfigMap revision, or a git revert.
- [ ] **Stakeholders notified if restarting.**

---

## Step 1 — Validate the config

Two layers: **syntactic** (does it parse?) and
**semantic** (does it mean what you think?).

**Syntactic — TOML parse:**

```bash
python3 -c "import tomllib; tomllib.load(open('/tmp/config.toml.new','rb')); print('OK')"
```

**Semantic — `brain-server --check-config`:**

```bash
brain-server --check-config /tmp/config.toml.new
```

Catches unknown fields (typos: `shard.cont` vs
`shard.count`), wrong types, out-of-range values,
omitted required fields, and section conflicts.
Exit 0 means *structurally* valid — it does **not**
mean values are right. `arena.capacity_bytes =
1_000_000_000_000` parses fine; whether you have a
terabyte of disk is a different question.

**Diff against current production:**

```bash
diff -u /etc/brain/config.toml /tmp/config.toml.new
# Or, if configs live in git:
cd /path/to/config-repo && git diff HEAD -- production/config.toml
```

The diff should be **small** and **show only what
you intended**. Indentation churn or reordered
sections means your editor is fighting you — clean
up before deploying.

**Check for env-var overrides.** `BRAIN_*` env
vars override the file — if the field is also set
in the unit, the file change is silently ignored.

```bash
sudo systemctl show brain-server --property=Environment \
    | tr ' ' '\n' | grep '^BRAIN_'
docker inspect brain-server | jq '.[0].Config.Env[] | select(startswith("BRAIN_"))'
```

If the field is also an env override, change both
or remove the override.

---

## Step 2 — Identify reload requirements

Some fields are picked up on SIGHUP; others need a
full restart. Get this right before deploying.

**Hot-reloadable (SIGHUP):**

| Section | Fields |
|---|---|
| `[workers.*]` | `interval_secs`, `batch_size`, `enabled` (next worker tick) |
| `[server]` | `log_level`, structured-log fields (new logs) |
| `[server.connection]` | `max_idle_secs`, `max_inflight_per_connection` (new connections only) |
| `[knowledge.extractors]` | budgets, retry policy, thresholds (next run) |
| `[knowledge.llm]` | rate limits, daily spend cap (next call) |
| `[tls]` | `cert_path`, `key_path` re-read — see [OP-04](op-04-tls-cert-rotation.md) |

Trigger a reload:

```bash
sudo systemctl reload brain-server      # or: kill -HUP "$(pgrep brain-server)"
docker kill --signal=HUP brain-server
```

In Kubernetes, SIGHUP across a StatefulSet is
awkward — treat k8s deploys as restart-required by
default.

Verify the new value:

```bash
brain-cli admin config show workers.consolidation.interval_secs
```

**Restart-required:**

| Section | Fields | Why |
|---|---|---|
| `[shard]` | `count` | Sharding fixed at startup |
| `[storage.arena]` | `capacity_slots`, `path` | mmap established at startup |
| `[storage.wal]` | `path`, `segment_size_bytes`, `o_direct` | Opened at startup |
| `[storage.metadata]` | `path` | redb handle opened at startup |
| `[hnsw]` | `m`, `ef_construction`, `max_layers` | Index structure |
| `[embedder]` | `model`, `dim`, `tokenizer_path` | See [OP-07](op-07-embedder-model-upgrade.md) |
| `[server]` | `bind_addr`, `port`, `tls.enabled` | Sockets bound at startup |
| `[knowledge]` | `enabled` toggle | See [RB-11](rb-11-schema-toggle.md) |

For these, follow [OP-01](op-01-rolling-restart.md)
for the restart choreography (snapshot first, then
restart, then verify).

If you're not sure:

```bash
brain-server --check-config /tmp/config.toml.new --explain-reload
# Per-field: HOT-RELOADABLE / RESTART-REQUIRED.
```

---

## Step 3 — Stage the change in a staging instance

**Never** apply a non-trivial config change
directly to production. Staging is where you find
the difference between "valid" and "right."

1. **Deploy to staging** the same way you'd deploy
   to production (same path, ownership, reload
   signal).
2. **Watch logs at startup/reload.** Warnings like
   `config: unknown field X` or `ignoring
   deprecated field Y` are signals.
3. **Exercise the affected behaviour.** Worker
   interval → wait for a cycle. HNSW parameter →
   run recalls and check latency. LLM budget →
   run an extraction and watch the meter.
4. **Smoke the unrelated paths:**
   ```bash
   brain-cli encode "staging smoke $(date +%s)"
   brain-cli recall "staging smoke"
   brain-cli admin shards | jq '[.[].status] | unique'
   brain-cli admin workers | jq '[.[] | select(.status != "running" and .status != "idle")]'
   ```
5. **Soak ≥ 15 minutes** (longer for slow-cycle
   workers).

If staging is unhappy, diagnose there. Don't push
the same change to prod hoping it behaves
differently.

No staging instance? Stand one up
(`brain-server --config /tmp/staging.toml` against
a temp dir is 5 minutes of work), or apply to one
shard first per OP-01. Straight to prod is a last
resort.

---

## Step 4 — Deploy to production

**systemd:**

```bash
sudo cp /etc/brain/config.toml /etc/brain/config.toml.bak
sudo install -m 0640 -o brain -g brain /tmp/config.toml.new /etc/brain/config.toml

# Hot-reloadable:
sudo systemctl reload brain-server
# Restart-required → follow OP-01.
sudo systemctl restart brain-server

sudo journalctl -u brain-server -f
```

**Docker / Docker Compose:**

```bash
cp /opt/brain/config.toml /opt/brain/config.toml.bak
cp /tmp/config.toml.new /opt/brain/config.toml

docker kill --signal=HUP brain-server                       # hot
# or
docker compose up -d --force-recreate brain-server          # restart

docker logs -f brain-server
```

If config lives in env vars in
`docker-compose.yml`, edit the compose file (in
version control) and recreate — no SIGHUP path for
env vars.

**Kubernetes (ConfigMap):** ConfigMap changes don't
trigger pod reloads automatically. Roll the
StatefulSet.

```bash
kubectl create configmap brain-config \
    --from-file=config.toml=/tmp/config.toml.new \
    --dry-run=client -o yaml | kubectl apply -f -

kubectl rollout restart statefulset/brain-server
kubectl rollout status statefulset/brain-server --timeout=15m
kubectl logs -f statefulset/brain-server -c brain-server
```

---

## Step 5 — Verify

Three things must be true: the binary read the new
file, the new value is in effect, and the behaviour
you expected actually changed.

```bash
brain-cli admin config show <field>
```

If it shows the **old** value: reload didn't reach
the process (wrong PID/container), the field is
restart-required and you only SIGHUP'd, or an env
var is overriding the file.

Then confirm the behaviour: worker schedule in
`brain-cli admin workers`; HNSW parameter via
RECALL latency/recall@k; LLM budget in
`brain-cli admin llm budget`; connection limit in
`brain-cli admin connections`; knowledge toggle in
`brain-cli admin knowledge status`.

Watch dashboards ≥ 10 minutes. If latency, error
rate, or worker cycles regress — rollback.

---

## Rollback

Step 4's `cp .bak` is the whole point.

```bash
# systemd
sudo cp /etc/brain/config.toml.bak /etc/brain/config.toml
sudo systemctl reload brain-server    # or restart
brain-cli admin config show <field>

# Docker
cp /opt/brain/config.toml.bak /opt/brain/config.toml
docker kill --signal=HUP brain-server
# or: docker compose up -d --force-recreate brain-server

# Kubernetes
kubectl rollout undo statefulset/brain-server
kubectl rollout status statefulset/brain-server --timeout=15m
```

(If you also edited a ConfigMap, `rollout undo`
won't revert it — revert in your config repo, re-
apply, then roll.)

**No backup exists.** Reconstruct from version
control:

```bash
cd /path/to/config-repo
git show <previous-commit>:production/config.toml > /tmp/config.toml.restore
# Validate (Step 1), then deploy (Step 4).
```

If neither a `.bak` nor git history exists, this is
also an OP-08 *process* incident — file a follow-up
to put configs in version control before the next
change.

---

## Post-operation

Post in your team channel:

```
Config rollout complete at HH:MM UTC.
Field(s): <field> = <old> → <new>
Reload mode: hot / restart
Duration: Xm.
Issues: <none / list>.
Rollback exercised: <no / yes — reason>.
```

Commit:

```bash
cd /path/to/config-repo
git add production/config.toml
git commit -m "prod: <field> <old> -> <new> (<reason>)"
git push
```

Anything you learned — undocumented field, gap in
`--check-config`, deploy quirk — file a follow-up.
The OP-08 process gets better incrementally.

---

## Pitfalls

- **Editing production directly with `vim`.** No
  version control, no diff, no rollback. Always
  edit a *copy*, validate, deploy.
- **Restarting all shards simultaneously.** Naive
  `systemctl restart` on a multi-shard host loses
  the "observe one shard first" safety. For non-
  trivial changes, use OP-01's per-shard pattern.
- **Mismatched env vs file.** You changed a field
  and SIGHUP'd; `config show` still reports the
  old value because `BRAIN_*` in the unit takes
  precedence. Change both or remove the override.
- **"It parsed, so it must be right."**
  `--check-config` doesn't know `arena.capacity_slots
  = 100` is too low or `hnsw.ef_search = 1` will
  tank recall. Validation is necessary, not
  sufficient — that's what staging is for.
- **Reload signal hits the wrong process.**
  `kill -HUP $(pgrep brain-server)` SIGHUPs all
  matches (dev + prod on the same box, mid-
  upgrade). Prefer `systemctl reload brain-server`.
- **Path change without telling systemd.** Unit
  reads `--config /etc/brain/config.toml`; you
  moved the file. Binary still reads the old path.
  Keep the path stable or update the unit and
  `daemon-reload`.
- **Restarting during an unrelated alert.** Mixing
  a rollout with an active incident mingles
  failure modes. Resolve, then roll.
- **Heroic re-rolls.** A "fixed" version only
  tested against the original failure isn't fully
  tested. Re-do Step 3.
- **Forgetting to commit.** Six weeks later the
  host is re-provisioned from config-management
  with the *old* file. Commit before you log off.

### Prevention

OP-08 wants to obsolete itself. Things that make
this runbook less necessary: configs in version
control (PR-reviewed); CI runs `--check-config` on
every PR; deploys via Ansible/Puppet/templating;
a standing staging instance; per-field reload-mode
docs baked into `--check-config --explain-reload`;
read-only `/etc/brain/config.toml` for humans
(owned by root, written only by config-management).

---

## Related runbooks

- [OP-01 — Rolling restart](op-01-rolling-restart.md)
- [OP-04 — TLS certificate rotation](op-04-tls-cert-rotation.md)
- [OP-05 — Auth token rotation](op-05-auth-token-rotation.md)
- [OP-06 — LLM API key rotation](op-06-llm-api-key-rotation.md)
- [OP-07 — Embedder model upgrade](op-07-embedder-model-upgrade.md)
- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
  (config-error branch)
- [RB-02 — High latency](rb-02-high-latency.md)
- [RB-03 — Memory pressure](rb-03-memory-pressure.md)
- [RB-04 — Disk filling](rb-04-disk-filling.md)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md)
- [RB-06 — Recall degraded](rb-06-recall-degraded.md)
- [RB-13 — Connection saturation](rb-13-connection-saturation.md)
- [RB-14 — LLM cost spike](rb-14-llm-cost-spike.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)

---

## Last validated

*Update on first use.*
