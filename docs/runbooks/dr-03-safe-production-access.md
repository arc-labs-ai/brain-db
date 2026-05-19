# DR-03: Safe production access

**When to use:** any time you need to log into a Brain
host or container during an incident. Even if you've
done it a hundred times, the rules here keep "I ssh'd
in to check something" from turning into "we lost
six hours of writes."

The rule of thumb: **production access is a privilege
that comes with constraints**. The constraints exist
because Brain is fail-stop and an inadvertent
destructive command bypasses the substrate's
self-protection.

---

## Three principles

1. **Read before you write.** Almost everything you need
   during an incident is observable — metrics, logs,
   admin endpoints. Logging in is for cases where the
   observable surface isn't enough.
2. **Prefer the admin API over the filesystem.** The
   substrate exposes admin RPCs for the things
   operators routinely do. Editing on-disk files
   manually is a last resort.
3. **Announce what you're doing.** Post in the incident
   channel before running any command that mutates
   state. Other responders need to know.

If a runbook says "log in and check X," it'll explicitly
say *log in*. If it doesn't say that, you probably
don't need to.

---

## What's safe via the admin API

Most operational work goes through admin HTTP endpoints
or the `brain-cli` tool. No production access needed.

| Task | Endpoint / CLI |
|---|---|
| Check shard health | `brain-cli admin shards` |
| Check worker status | `brain-cli admin workers` |
| Pause / resume a worker | `brain-cli admin worker pause <name>` |
| List snapshots | `brain-cli admin snapshots` |
| Take an ad-hoc snapshot | `brain-cli admin snapshot take` |
| Trigger HNSW rebuild | `brain-cli rebuild-ann --shard <n>` |
| Inspect WAL tail | `brain-cli admin wal-tail --count 100` |
| Check arena headers | `brain-cli admin arena-headers` |
| Hard-forget a memory | `brain-cli forget --hard <memory_id>` |

All of these are auditable, idempotent in the right
way, and don't risk corruption. Use them in preference
to filesystem operations whenever possible.

---

## When production access is necessary

These are the legitimate reasons:

- **The substrate is down.** No admin endpoint, so you
  need to inspect files directly.
- **You're running a procedure that requires
  filesystem operations** (snapshot restore, manual
  WAL truncation, file move).
- **Observability is also broken.** Logs aren't
  reaching the aggregator and you need to read them
  on disk.
- **You're capturing forensic state** (diagnostic
  bundle, heap dump) that can't be triggered via the
  admin API.

If your reason isn't in this list, look again at the
admin API. There's probably a way.

---

## Access patterns

How you connect depends on the deployment shape. Three
common ones.

### systemd / bare metal

```bash
ssh user@brain-host
journalctl -u brain-server --since "10 min ago"
sudo systemctl status brain-server
```

The `brain-server` process runs as a non-root user;
data is owned by that user (typically `brain`). To
inspect files:

```bash
sudo -u brain ls -la /var/lib/brain/data
```

Never `chown` the data directory to your interactive
user. The substrate won't be able to write to its own
data on restart.

### Docker

```bash
ssh user@brain-host
docker logs brain-server --since 10m
docker exec -it brain-server /bin/sh
```

Inside the container:

```sh
ls -la /var/lib/brain/data
cat /etc/brain/config.toml
```

To copy a file out (e.g., for a diagnostic bundle):

```bash
docker cp brain-server:/var/lib/brain/data/shard-3/arena.bin ./
```

### Kubernetes

```bash
kubectl get pods -l app=brain-server
kubectl logs <pod> --tail 1000 -p          # previous container's logs
kubectl exec -it <pod> -- /bin/sh
```

For per-shard inspection where shards are spread
across pods:

```bash
kubectl get pods -l app=brain-server -o wide
kubectl exec <pod-on-shard-3-node> -- ls -la /var/lib/brain/data
```

`kubectl cp` for file extraction:

```bash
kubectl cp <pod>:/var/lib/brain/data/shard-3/arena.bin ./
```

---

## Safe read commands

The following commands are safe to run on a healthy
production system. They only read state.

### Process state

```bash
ps -p $(pgrep brain-server) -o pid,vsz,rss,etime,cmd
```

PID, virtual memory, resident memory, elapsed time,
command. Useful for "is it running and roughly how
big is it."

### Open files

```bash
lsof -p $(pgrep brain-server) | wc -l
lsof -p $(pgrep brain-server) | grep -E 'arena|wal|metadata'
```

Counts open file descriptors; lists the substrate's
data files. If the count is climbing without bound,
that's a file-descriptor leak — separate problem.

### Resource usage

```bash
top -bn1 -p $(pgrep brain-server)
iostat -x 1 5
free -h
df -h /var/lib/brain
```

CPU per process, disk I/O, memory, disk free. The
basic four. Always safe.

### Network

```bash
ss -tnlp 'sport = :9090'        # is the wire port listening?
ss -tnlp 'sport = :9091'        # metrics?
ss -tnp | grep brain-server | wc -l   # current connections
```

Don't run a packet capture (`tcpdump`) on a
production wire port without explicit reason; data
flowing through Brain is often sensitive.

### Logs

```bash
journalctl -u brain-server --since "1 hour ago" | tail -200
journalctl -u brain-server --since "1 hour ago" -p err
```

Or for Docker / k8s, the equivalents from the previous
section. Safe to run anytime.

### Data file inspection

```bash
ls -la /var/lib/brain/data/
du -sh /var/lib/brain/data/*
file /var/lib/brain/data/shard-0/arena.bin
```

`ls`, `du`, `file` are all safe. They read directory
listings and file metadata; they don't open the
files for reading.

Don't `cat` the arena file — it's a binary blob,
your terminal won't enjoy it, and you've gained
nothing.

---

## Dangerous commands

A categorised list of commands that have caused
incidents in the past. Don't run any of these without
a specific reason and explicit announcement in the
incident channel.

### Destructive but recoverable

These do something significant but the substrate has a
documented recovery path. Still announce before
running.

```bash
sudo systemctl restart brain-server    # restart
sudo systemctl stop brain-server       # stop
docker restart brain-server
kubectl rollout restart sts/brain-server
```

A restart drops in-memory state (HNSW, embedder cache)
and forces WAL replay on start. Usually fine; can take
seconds-to-minutes depending on shard size.

### Destructive and risky

These can cause data loss or corruption if done at the
wrong moment.

```bash
sudo kill -9 $(pgrep brain-server)              # hard kill
rm /var/lib/brain/data/.../wal/seg-*.wal         # delete WAL segments
rm /var/lib/brain/data/.../snapshots/<id>/       # delete snapshot
truncate /var/lib/brain/data/.../arena.bin       # truncate arena
echo > /var/lib/brain/data/.../shard.uuid        # nuke UUID
mv /var/lib/brain/data/<n>/ /tmp/                # move shard files
```

Hard kill loses in-flight writes (they didn't get a
chance to fsync). Deleting a WAL segment may corrupt
replay. Truncating the arena is unambiguously bad.

Anything that touches the data directory needs:

1. The substrate fully stopped (`systemctl stop brain-server`
   has completed).
2. A current snapshot to restore from if it goes wrong.
3. Explicit announcement in the incident channel.

### Forbidden

These don't have a legitimate reason during an
incident. If you find yourself wanting to do these,
escalate instead.

- `chmod`-ing the data directory.
- Editing `arena.bin` or `metadata.redb` with a hex
  editor.
- `dd` over the data directory.
- `mkfs` on the data device. (Yes, this has happened
  in someone's career.)
- Skipping config validation by editing the binary
  directly.

If something requires one of these to fix, you're not
fixing it; you're making it unfixable. Escalate.

---

## The "production-write" boundary

A useful mental boundary: are you about to *change*
production state, or just *observe* it?

- **Observing** — `ls`, `cat`, `ps`, `du`, `journalctl`,
  `curl http://.../metrics`, `brain-cli ... list`,
  `brain-cli ... stats`. Safe; run as needed.
- **Changing** — anything that writes (admin RPCs that
  mutate, file edits, restart, kill, etc.). Announce
  in the channel first; pause to confirm.

When in doubt, ask in the incident channel before
running. The 30-second delay to confirm is much cheaper
than a 6-hour recovery.

---

## Sudo and shell history

Some practical hygiene:

- **Use `sudo` per-command, not `sudo -i`.** A
  long-lived root shell during an incident is a
  liability — you forget you're root and run
  something destructive without thinking.
- **Verify shell history is being recorded.** Most
  production hosts log shell history to a central
  audit log. Your commands during an incident need to
  be reviewable.
- **Don't disable history.** `unset HISTFILE` or
  `set +o history` is appropriate only for handling
  literal secrets (rare). If you find yourself doing
  it for any other reason, you're hiding from
  yourself.
- **Use `set -x`** in scripts you write live during
  incidents. Echoing each command before running is a
  cheap form of audit and lets you catch typos before
  they execute.

---

## Capturing things on the way out

Before you log out at the end of an incident:

- **Save shell history** (or confirm it auto-saved).
- **Note any temporary files** you created. Delete
  them, or move them to a known location.
- **Revoke any temporary credentials** if you
  generated them.
- **Update the runbook's `Last validated:`** field if
  you exercised it.

Don't leave artifacts behind. The next operator should
find the host in the state you found it (modulo the
incident resolution).

---

## Multi-operator coordination

If two operators are both connected during an
incident, coordinate explicitly:

- **One driver, one observer.** The driver runs
  commands; the observer reads alongside, posts
  notes in the channel, catches typos.
- **Don't both type into the same shell.** If you're
  pair-debugging, use separate sessions and post what
  you're running.
- **Hand off explicitly.** "I'm stepping away for 5
  min; please don't run anything until I'm back" or
  "you're the driver now."

The collaboration is fine. Silent parallel commands
are how two people accidentally restart twice.

---

## Related runbooks

- [IR-03 — Escalation policy](ir-03-escalation-policy.md)
- [IR-04 — Incident communication](ir-04-incident-communication.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-05 — Verifying durability invariants](dr-05-verifying-durability-invariants.md)
- [RB-07 — Recovery from corruption](rb-07-corruption-recovery.md)

---

## Last validated

*Update on first use.*
