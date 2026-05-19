# DR-01: Collecting a diagnostic bundle

**When to use:** any time you escalate, prepare a
postmortem, or send state to support / engineering. Also
useful as the *first* thing you do during a P1, so the
substrate's state at the moment of failure is preserved
even if you have to restart.

**Output:** a single directory (or tarball) containing
logs, metric snapshots, traces, configuration, and
sanity-check outputs. Roughly 50-500 MB depending on
log volume.

---

## Why bother

Three reasons:

1. **State preservation.** Once you restart the
   substrate, the in-memory state is gone. A
   diagnostic bundle freezes that state for later
   inspection.
2. **Handoff hygiene.** When you escalate
   ([IR-03](ir-03-escalation-policy.md)), the next
   responder needs to start from your context, not
   re-derive it.
3. **Postmortem evidence.** Six weeks later when you're
   writing the postmortem, the bundle is the only
   reliable record. Memory and Slack history degrade.

Build the bundle **early** in an incident, even if you
don't know yet whether you'll need it. It's cheap to
collect and expensive to recreate.

---

## The standard bundle

A bundle has eight components. Most are quick; one or
two require coordination.

```
brain-bundle-<timestamp>/
├── logs/
│   ├── brain-server.log         (last 6 h of substrate logs)
│   ├── system.log               (kernel / systemd / docker)
│   └── otel.log                 (OpenTelemetry collector if present)
├── metrics/
│   ├── prometheus-snapshot.json (last 1 h of all brain_* metrics)
│   ├── alert-history.json       (alerts in last 24 h)
│   └── grafana-screenshots/
├── traces/
│   └── slow-traces.json         (a handful of slow OTel traces)
├── config/
│   ├── brain-config.toml
│   ├── docker-compose.yml       (or equivalent)
│   └── env.txt                  (BRAIN_* env vars; redacted)
├── state/
│   ├── shard-list.json          (output of admin /v1/admin/shards)
│   ├── worker-status.json       (output of /v1/admin/workers)
│   ├── snapshot-manifest.json   (list of snapshots and ages)
│   └── version.txt              (brain-server --version)
├── sanity-checks/
│   ├── wal-tail.json            (last 100 WAL records, headers only)
│   ├── arena-headers.json       (header CRCs for each shard)
│   └── disk-usage.json
├── INCIDENT.md                  (description of what's wrong)
└── README.md                    (auto-generated; describes contents)
```

A complete-bundle script lives at the bottom of this
doc. Run it; it produces the structure above.

---

## What each piece is for

### logs/

- **brain-server.log**: the substrate's stdout/stderr,
  filtered to the last 6 hours by default. If the
  incident is going long, expand the window.
- **system.log**: `journalctl` or `dmesg` if you're on
  systemd; Docker / Kubernetes container logs if
  containerised. Catches OOM-killer events, kernel
  panics, hardware errors.
- **otel.log**: if you're running an OpenTelemetry
  collector locally, its logs may show signal drops or
  pipeline errors useful for explaining missing
  traces.

### metrics/

- **prometheus-snapshot.json**: a snapshot of all
  `brain_*` metrics for the last 1 hour. Captures
  rates, percentiles, gauges. The most useful single
  file for diagnosis.
- **alert-history.json**: which alerts have fired in
  the last 24 hours, including ones that
  auto-resolved. Often the *resolved* alert just
  before the current one is the first hint of root
  cause.
- **grafana-screenshots/**: screenshots of the
  Overview dashboard plus any per-shard dashboards
  that look unusual. Manual; tedious but worth it.

### traces/

If you have OpenTelemetry tracing enabled, capture 3-5
slow traces representative of the symptoms. Each
trace shows the request flowing through Tokio →
flume → Glommio → storage → response. The bottleneck
shows up as the long span.

If you don't have tracing, this directory is empty.
Consider adding tracing infrastructure to your
deployment if it's missing; it's invaluable in
hindsight.

### config/

- **brain-config.toml**: the exact configuration the
  running server is using.
- **docker-compose.yml** or **k8s yaml**: the
  deployment manifest if applicable.
- **env.txt**: `BRAIN_*` environment variables.
  **Redact secrets** before saving.

### state/

Admin endpoints that capture the substrate's
self-reported state at the moment of collection.

- **shard-list.json**: shard health, recovery status,
  per-shard worker scheduler status.
- **worker-status.json**: each background worker's
  last-run timestamp, cycles completed, last error.
- **snapshot-manifest.json**: list of snapshots, sizes,
  ages. Critical for restore decisions.
- **version.txt**: `brain-server --version` output.

### sanity-checks/

Heavy reads of the durable state. Don't run these on
a healthy production system as a matter of routine
(they're expensive); during an incident they're
appropriate.

- **wal-tail.json**: the last ~100 WAL records'
  headers (LSN, record type, CRC). Detects WAL
  truncation, CRC failures, suspicious gaps.
- **arena-headers.json**: each shard's `arena.bin`
  header bytes + CRC verification.
- **disk-usage.json**: `du -sh` per shard's data dir,
  plus df output.

### INCIDENT.md

A short markdown file you write describing what
prompted the bundle:

```
# Incident description

When: 2024-09-12 14:31 UTC
Alert: BrainHighLatency on shard 3
Severity: P2
What's wrong: p99 latency on shard 3 is 187ms sustained
What's been tried (link to incident channel): ...
Hypothesis: ...
```

The bundle is useless without context. Always include
this file.

---

## The collection script

Copy-paste-runnable for the common deployment shape.
Adjust paths and addresses for your install.

```bash
#!/usr/bin/env bash
# collect-brain-bundle.sh — capture a diagnostic bundle.
set -euo pipefail

TS="$(date -u +%Y%m%d-%H%M%SZ)"
DEST="/tmp/brain-bundle-${TS}"
ADMIN_ADDR="${BRAIN_ADMIN_ADDR:-127.0.0.1:9092}"
METRICS_ADDR="${BRAIN_METRICS_ADDR:-127.0.0.1:9091}"
DATA_DIR="${BRAIN_DATA_DIR:-/var/lib/brain/data}"

mkdir -p "$DEST"/{logs,metrics,traces,config,state,sanity-checks}

echo "Collecting bundle at $DEST ..."

# --- logs ---
if command -v journalctl &> /dev/null; then
    journalctl -u brain-server --since "6 hours ago" \
        > "$DEST/logs/brain-server.log" 2>&1 || true
    journalctl --since "6 hours ago" -p err \
        > "$DEST/logs/system.log" 2>&1 || true
else
    docker logs brain-server --since 6h \
        > "$DEST/logs/brain-server.log" 2>&1 || true
fi

# --- metrics ---
# Prometheus snapshot of brain_* metrics for last hour.
curl -sG "http://${METRICS_ADDR}/api/v1/query_range" \
    --data-urlencode 'query={__name__=~"brain_.*"}' \
    --data-urlencode "start=$(date -u -d '1 hour ago' +%s)" \
    --data-urlencode "end=$(date -u +%s)" \
    --data-urlencode 'step=15s' \
    > "$DEST/metrics/prometheus-snapshot.json" || true

# --- config ---
cp -p /etc/brain/config.toml "$DEST/config/brain-config.toml" 2>/dev/null || true
env | grep '^BRAIN_' | grep -vE '_(KEY|TOKEN|SECRET|PASSWORD)=' \
    > "$DEST/config/env.txt" || true

# --- state ---
brain-cli --addr "$ADMIN_ADDR" admin shards \
    > "$DEST/state/shard-list.json" 2>&1 || true
brain-cli --addr "$ADMIN_ADDR" admin workers \
    > "$DEST/state/worker-status.json" 2>&1 || true
brain-cli --addr "$ADMIN_ADDR" admin snapshots \
    > "$DEST/state/snapshot-manifest.json" 2>&1 || true
brain-server --version > "$DEST/state/version.txt" 2>&1 || true

# --- sanity-checks ---
brain-cli --addr "$ADMIN_ADDR" admin wal-tail --count 100 \
    > "$DEST/sanity-checks/wal-tail.json" 2>&1 || true
brain-cli --addr "$ADMIN_ADDR" admin arena-headers \
    > "$DEST/sanity-checks/arena-headers.json" 2>&1 || true
du -sh "$DATA_DIR"/* \
    > "$DEST/sanity-checks/disk-usage.json" 2>&1 || true
df -h "$DATA_DIR" \
    >> "$DEST/sanity-checks/disk-usage.json" 2>&1 || true

# --- INCIDENT.md placeholder ---
cat > "$DEST/INCIDENT.md" <<EOF
# Incident description

When: $(date -u)
Severity: TODO
Alert: TODO
What's wrong: TODO
Runbook: TODO
Hypothesis: TODO
Incident channel: TODO
EOF

# --- tarball ---
tar -czf "${DEST}.tar.gz" -C /tmp "$(basename "$DEST")"

echo "Bundle ready: ${DEST}.tar.gz"
echo "  Size: $(du -h "${DEST}.tar.gz" | cut -f1)"
echo "  Edit INCIDENT.md inside before sharing."
```

Save as `/usr/local/bin/collect-brain-bundle` (or
similar), chmod +x, and run during the first 5 minutes
of an incident.

---

## Where to put the bundle

The bundle contains operationally-sensitive data
(config, possibly user-affecting state from logs). Pick
a destination accordingly:

- **Object storage (S3, GCS) with restricted access.**
  Standard for orgs that have it. Permissions limited
  to on-call + engineering.
- **Internal file share.** Acceptable for smaller
  setups.
- **Slack upload.** Only for very small bundles (<10
  MB) and only if the channel is restricted.
- **Email.** Avoid. Bundles are too big and email
  retention rules are unclear.

Never:

- Push to a public repo. Even an apparently-redacted
  config may leak agent IDs or other sensitive
  identifiers.
- Paste into a public Slack channel.
- Attach to a public ticketing system.

---

## Redaction

Before sharing externally (with a vendor support team,
say), redact:

- API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  auth tokens).
- Customer-identifying agent IDs, if your deployment
  treats agent IDs as PII.
- Any PII that ended up in logs (rare in Brain, since
  the substrate stores text it didn't author — but
  double-check for the deployment).
- Internal hostnames / IPs if they imply network
  topology you don't want to share.

The collection script's `env.txt` filter already
excludes obvious secret variables; double-check
manually anyway. Logs are the dangerous part — grep
for likely sensitive patterns before sharing.

---

## Bundle freshness

A bundle is a snapshot. Two minutes after collection,
the substrate's state may have moved on. If the
incident lasts hours, collect a *second* bundle near
the end (post-remediation) so the postmortem has
both "during" and "after" snapshots.

For short P1s, one bundle is fine.

For a postmortem, you typically want:

- A bundle from near the start of the incident
  (captures the failure mode).
- A bundle from after resolution (captures the
  remediated state).
- Optionally: a "baseline" bundle from a previous
  healthy period to contrast against.

---

## When automated collection fails

If the substrate is *deeply* broken, the collection
script may fail (admin endpoint not responding, etc.).
Capture what you can:

1. Logs first. `journalctl` or `docker logs` work
   even when the substrate itself is down.
2. Filesystem state. `ls -laR /var/lib/brain/data | head`
   shows whether files are present and what their
   sizes are.
3. Config and env. These don't depend on a running
   substrate.
4. System state. `top`, `df`, `free`, `dmesg`. These
   are independent of Brain.

A partial bundle is much more useful than no bundle.
Capture what you can and note in `INCIDENT.md` what
couldn't be captured and why.

---

## Related runbooks

- [IR-03 — Escalation policy](ir-03-escalation-policy.md)
- [DR-02 — Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)

---

## Last validated

*Update on first use.*
