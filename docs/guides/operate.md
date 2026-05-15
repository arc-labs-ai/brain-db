# Operate Brain

Day-to-day operator guide. Assumes Brain is installed and
configured ([install.md](install.md), [configure.md](configure.md)).

## Daemon lifecycle

Run brain-server under a process supervisor that restarts on
crash. systemd is the canonical choice:

```ini
# /etc/systemd/system/brain-server.service
[Unit]
Description=Brain cognitive substrate
After=network.target

[Service]
Type=simple
User=brain
Group=brain
ExecStart=/usr/local/bin/brain-server --config /etc/brain/config.toml
Restart=on-failure
RestartSec=5s

# Resource caps — tune for your host.
LimitNOFILE=65536
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
```

```bash
systemctl enable --now brain-server
systemctl status brain-server
journalctl -u brain-server -f
```

## Verifying health

Three endpoints answer:

```bash
curl -s http://<metrics-addr>/healthz   # → "ok\n"
curl -s http://<metrics-addr>/metrics | head
curl -s http://<admin-addr>/v1/workers  # list background workers
```

The Phase 12 observability stack ([observability.md](../observability.md))
is the operator's primary surface. Wire up Prometheus + Grafana +
Alertmanager per that guide.

## Routine ops

### Take a snapshot

Snapshots are full point-in-time copies of a shard. Use them
before risky operations (config changes, version upgrades).

```bash
brain-cli snapshot create --shard 0 --addr <admin-addr>
brain-cli snapshot list  --addr <admin-addr>
```

The `snapshot` background worker runs daily by default and prunes
to the most-recent-N retention. Manual snapshots survive that
pruning until explicitly deleted.

### Rebuild HNSW

Trigger when tombstone ratio climbs (see
[RB-6](../runbooks/recall-degraded.md)):

```bash
brain-cli rebuild-ann --shard 0 --addr <admin-addr>
```

Reads keep working during the rebuild against the old index; the
new one swaps atomically when ready.

### Inspect config

```bash
brain-cli config --addr <admin-addr>
brain-cli config --key embedder.model --addr <admin-addr>
```

Live config mutation (POST /v1/config) is currently 501 (tracker
`phase-11/runtime-config-set`). To change config, edit the TOML
and restart.

### Tail logs

If you set `[logging] format = "json"`, every line is one JSON
object. The standard inspection pattern:

```bash
journalctl -u brain-server -f \
  | jq 'select(.level == "ERROR" or .level == "WARN")'
```

For Loki / Elastic, the JSON layer's field shape is documented in
[observability.md §3](../observability.md#3-logs).

## When something's wrong

The 10 runbooks in `docs/runbooks/` cover the common failure
modes. Each is linked from the corresponding alert in
`alerts/brain-rules.yml`. The flow is:

1. Alert fires in Alertmanager.
2. Alert annotation links to the runbook.
3. Operator follows the runbook steps.
4. If unresolved at "Escalate if": gather evidence, file an issue.

The 10 runbooks:

- [RB-1 Substrate doesn't start](../runbooks/substrate-down.md)
- [RB-2 High latency](../runbooks/high-latency.md)
- [RB-3 Memory pressure](../runbooks/memory-pressure.md)
- [RB-4 Disk filling](../runbooks/disk-filling.md)
- [RB-5 Worker stuck](../runbooks/worker-stuck.md)
- [RB-6 Recall degraded](../runbooks/recall-degraded.md)
- [RB-7 Corruption recovery](../runbooks/corruption-recovery.md)
- [RB-8 Unresponsive](../runbooks/unresponsive.md)
- [RB-9 Mass FORGET aftermath](../runbooks/mass-forget.md)
- [RB-10 Network partition (v2)](../runbooks/network-partition.md)

## Backup + restore

Brain's durability story is **WAL-before-ack + snapshots**. The
WAL alone is enough to recover from any non-corruption failure;
snapshots are the floor for catastrophic corruption (see
[RB-7](../runbooks/corruption-recovery.md)).

**Backup cadence:**
- WAL retention: 4 GiB / shard (default; spans hours-to-days of
  activity).
- Snapshots: daily background worker; retains N most-recent.
- Off-host: copy `data_dir` to S3 / object storage during scheduled
  windows. The data dir is consistent if either (a) the substrate
  is stopped, or (b) you copy a snapshot rather than the live
  arena/WAL.

**Restore:**
```bash
systemctl stop brain-server
# replace data_dir contents from the off-host backup
systemctl start brain-server
```
On restart, brain-server replays the WAL forward from the snapshot
LSN. Recovery time is proportional to retained WAL, typically
seconds for the default 4 GiB.

## Capacity planning

Spec §16/08 capacity-planning is the authoritative reference.
Operator rules of thumb:

| Signal | Threshold | Action |
|---|---|---|
| `brain_request_duration_ms{op="encode",quantile=p99}` | > 25 ms | Investigate — see [RB-2](../runbooks/high-latency.md) |
| `process_memory_resident_bytes` | > 80 % of host RAM | Scale up host or shard out |
| `brain_hnsw_tombstone_ratio` | > 0.3 sustained | Trigger rebuild |
| Disk free | < 24 h projected | See [RB-4](../runbooks/disk-filling.md) |
| `rate(brain_request_total{status="error"}[5m])` | > 1 % | Investigate logs |

The Phase 12.4 Grafana dashboards visualise all of these.
