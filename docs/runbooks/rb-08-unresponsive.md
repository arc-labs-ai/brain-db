# RB-08: Substrate becoming unresponsive

**Severity:** **P1**.
**Alert:** composite (combinations of timeout rate +
no progress on workers + healthcheck failures).
**SLO impact:** clients timing out. The substrate
*looks* up (process alive, port listening) but isn't
serving requests.
**Estimated duration:** 30 minutes to 90 minutes.
**Skill level:** comfortable with stack dumps, async
runtimes, and reading traces.

The substrate is in an intermediate state — not down
in the [RB-01](rb-01-substrate-down.md) sense, not
just slow in the [RB-02](rb-02-high-latency.md)
sense. It's *stuck*. Requests aren't completing.

---

## Am I in the right runbook?

You should see:

- The brain-server process is running (pgrep returns
  a PID).
- The metrics endpoint may respond, or may also be
  hung.
- The wire port (9090) is listening, but new
  connections either don't get a HELLO response or
  hang after.
- Request latency goes to infinity (timeout) for
  some or all ops.
- Healthcheck (`/healthz`) is failing or timing out.

If the substrate **definitely isn't running**, that's
[RB-01](rb-01-substrate-down.md). If it's just
*slow* (responses arrive, late), that's
[RB-02](rb-02-high-latency.md).

The defining symptom of this runbook: **the process
is alive but not making progress.**

---

## Stop the bleeding

This is the most ambiguous failure mode. Defensive
action:

1. Page on-call; this is P1.
2. **Capture state immediately** — diagnostic bundle
   ([DR-01](dr-01-diagnostic-bundle.md)) and a stack
   dump ([DR-04](dr-04-profiles-and-heap-dumps.md)):

   ```bash
   sudo /usr/local/bin/collect-brain-bundle
   sudo gdb -p $(pgrep brain-server) -batch \
       -ex 'thread apply all bt' \
       -ex 'quit' > stackdump.txt
   ```

   Once you restart, this state is gone. The bundle
   plus stack dump is the only evidence of what was
   wrong.

3. **Decide: restart or wait?** The substrate has a
   chance of recovering on its own if the hang is
   transient (e.g., a slow LLM API call eventually
   returning). It has zero chance if the hang is a
   genuine deadlock. The deciding factor:
   - **Recent log activity?** If logs are still
     ticking, the substrate is doing something. Wait
     5-10 min; see if it recovers.
   - **No log activity for >5 min?** Likely
     deadlocked. Restart is the only option.

---

## Diagnose

### 1. Is the process really alive?

```bash
pgrep brain-server                   # PID
ps -p $(pgrep brain-server) -o pid,ppid,vsz,rss,stat,etime,cmd
```

The `stat` column matters:

- `R`: running (using CPU). Good — it's working.
- `S`: sleeping. Most threads will be in `S` waiting
  on I/O / timers. Normal.
- `D`: uninterruptible sleep (in a kernel call,
  usually disk I/O). Many in `D` means disk is hung.
- `Z`: zombie. Process died and parent hasn't
  reaped. Effectively dead.
- `T`: stopped. Did you ctrl-Z it? Unlikely in
  production.

`R` mixed with `S` is healthy. Many `D`s suggests
underlying I/O issues; the substrate is waiting on
the OS.

### 2. Is the metrics endpoint responsive?

```bash
timeout 2 curl -fsS http://127.0.0.1:9091/metrics | head -5
```

- **Returns metrics:** the metrics serving thread is
  alive; the substrate may be partially functional.
- **Times out:** the substrate is hung at a level
  that even metrics can't escape. Worse case.
- **Connection refused:** the listener isn't bound;
  the substrate may have crashed (back to
  [RB-01](rb-01-substrate-down.md)).

### 3. What do the metrics say?

If metrics are responsive:

```promql
# Has anything updated recently?
brain_uptime_seconds                      # should match real uptime
rate(brain_request_total[1m])             # currently zero?
brain_request_in_flight                   # how many requests stuck?
brain_worker_last_run_unixtime            # any worker activity?
```

- **`brain_request_in_flight` very high and not
  draining:** requests piling up faster than they
  complete. Could be one shard hung, dispatcher
  saturated, etc.
- **Zero request rate but stable in-flight count:**
  no new requests being accepted, or hung in the
  accept loop.
- **All worker `last_run` stale:** the executor
  scheduler is also stuck. Substrate is wedged.

### 4. Read the stack dump

The stack dump captured earlier shows what every
thread is doing right now. Look for patterns:

**Healthy threads:**
- `tokio` workers in `epoll_wait` (sleeping on
  network).
- `glommio` workers in `io_uring_enter` (sleeping on
  storage).
- A few threads in `futex_wait` for short-term
  coordination.

**Suspicious threads:**
- Many threads in `futex_wait` on the same address —
  lock contention or a lost wake.
- Threads in `read`/`write` for extended time
  (>seconds) without progress — I/O hung.
- A thread in a Brain function with a deep stack
  (looks recursive) — possibly stack overflow or
  unbounded recursion.

A specific anti-pattern to look for: a Tokio task
blocked on a flume `recv_async` while the
corresponding Glommio task is *also* blocked on a
flume `send_async` for the same shard. That's a
deadlock at the Tokio-Glommio boundary.

### 5. Logs

```bash
sudo journalctl -u brain-server --since "10 min ago" | tail -100
```

Look for:

- **The last few lines before the hang.** What was
  the substrate doing? Often the answer is in the
  last log line.
- **ERRORs in the lead-up.** A recent ERROR may have
  triggered the hang.
- **Repeated lines.** A loop hammering the same code
  path (often a retry storm).

If the log is *silent* (no recent lines at all),
that's the strongest sign of a deep hang.

### 6. Specific subsystems

The hang's *location* matters for remediation:

#### Storage hang

Symptoms: WAL fsync metrics stopped; threads in
`fsync` syscall.

Possible cause: disk is unresponsive. Check disk
health:

```bash
sudo dmesg | tail -50 | grep -i 'sd[a-z]\|nvme\|i/o error\|timeout'
sudo iostat -x 1 5
```

If the disk is hung at the OS level, no
substrate-side fix will help. Replace / remount the
disk, then restart.

#### Embedder hang

Symptoms: requests piling up; embedder cache hit
rate dropped to zero; threads in `bert_forward` for
extended time.

Possible cause: a stuck inference. Rare but possible
(model weights corrupted, candle / pytorch internal
deadlock).

Restart is the fix.

#### Network / Tokio hang

Symptoms: connection accept counts dropped; many
Tokio worker threads in `futex_wait`.

Possible cause: Tokio runtime contention or a
specific task that holds something across `.await`.

Restart is the fix.

#### Worker / Glommio hang

Symptoms: workers' `last_run` all stale; Glommio
threads spinning.

Possible cause: a worker stuck in an infinite loop;
or `spawn_local` panicked and the executor is
broken.

Restart is the fix.

### 7. Multi-shard or one-shard?

If only one shard's metrics are stale and others are
fine, you can isolate it:

- Drain traffic from the bad shard at the LB level if
  possible.
- Use admin API to query shard health:

   ```bash
   brain-cli admin shards
   ```

If multiple shards are hung simultaneously, the
substrate-wide runtime is implicated; restart is
necessary.

---

## Remediate

### Restart the substrate

For most hangs, this is the answer:

```bash
sudo systemctl restart brain-server
```

This sends SIGTERM, gives the substrate up to 30
seconds to drain, then SIGKILL if it doesn't
exit. Drains aren't possible from a hung state, so
expect a hard kill.

After restart:

- Recovery runs from the WAL (chapter 18 of
  concepts). Anything fsync'd before the hang is
  preserved.
- Anything in-flight at the moment of the hang is
  lost (but the client didn't get an ack, so
  retries — with same `request_id` — will land
  cleanly).
- The substrate comes back in 30 s - 2 min depending
  on shard size.

### If restart doesn't help

If the substrate restarts to the same hang, that's
[RB-12](rb-12-restart-loop.md) — the hang isn't
transient; it's reproducing.

Possibilities:

- The hang is in the recovery path (specific WAL
  record triggers it).
- A specific request is being retried by clients
  and triggering the same hang.
- Storage is genuinely broken.

Switch to RB-12 for the restart-loop diagnosis.

### Fail over the host (if multi-host)

If you're running multiple Brain instances behind a
load balancer (rare in v1 since there's no
replication, but possible at the LB layer if
clients are tolerant of per-instance loss):

- Drain traffic from the hung host.
- Investigate offline (no client impact).
- Restore service when ready.

Most v1 deployments are single-host, so this isn't
an option.

### Reset the disk (if disk hung)

If the disk subsystem is hung at the OS level:

```bash
sudo systemctl stop brain-server
sudo umount /var/lib/brain      # may fail if disk is wedged
# May need a reboot of the host.
```

If the disk is genuinely wedged in the kernel, a
host reboot may be necessary. After reboot, Brain
recovers from WAL.

---

## Verify

```bash
# Process alive, responsive.
curl -fsS http://127.0.0.1:9091/metrics | head -5

# Healthcheck happy.
curl -fsS http://127.0.0.1:9091/healthz

# Smoke encode/recall.
time brain-cli encode "smoke $(date +%s)"
time brain-cli recall "smoke"

# All shards active.
brain-cli admin shards
```

The clearing alert depends on which one fired —
typically a healthcheck or `BrainHighLatency`-style
metric returns to normal.

If you captured a stack dump, analyse it after the
incident — that's the evidence engineering needs to
prevent recurrence.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Symptom: substrate stopped responding to requests at HH:MM; metrics
endpoint also unresponsive.
Root cause hypothesis: <e.g., deadlock at Tokio-Glommio boundary;
captured in stackdump.txt for engineering>.
Remediation: restart (recovered cleanly).
User impact: ~Xm of complete unavailability.
Follow-up: TICKET-NNNN (engineering investigation of stack dump).
Postmortem: required (P1).
```

Postmortem rule: **always** for RB-08. Hang incidents
are usually substrate bugs and the postmortem feeds
directly into engineering's fix.

Critical: **attach the stack dump and diagnostic
bundle** to the postmortem. Without them, engineering
is debugging blind.

---

## Prevention

- **Stack dumps before restart** is the single most
  important habit. Without them, post-restart
  forensics is much harder.
- **Healthcheck timeout alerts** catch hangs that
  pure-latency alerts miss. A request that never
  completes doesn't show up in p99 — it shows up as
  a timeout. Alert on timeout rate too.
- **Connection-accept rate** as a leading indicator.
  If the substrate has stopped accepting new
  connections but isn't crashed, that's the symptom.
- **Periodic exercise** of the recovery path —
  [OP-02](op-02-snapshot-restore-drill.md) drills
  catch issues that would be acute under stress.
- **Engineering reviews of postmortems.** RB-08's
  root causes tend to be subtle (deadlocks, lost
  wakes, runtime quirks). Engineering reading every
  RB-08 postmortem improves the substrate over time.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [RB-02 — High latency](rb-02-high-latency.md)
- [RB-12 — Restart loop](rb-12-restart-loop.md)
- [RB-13 — Connection saturation](rb-13-connection-saturation.md)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)

---

## Last validated

*Update on first use.*
