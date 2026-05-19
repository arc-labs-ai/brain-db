# DR-04: Taking a profile or heap dump

**When to use:** when metrics + logs + traces aren't
enough — you suspect a deeper problem (CPU hotspot,
memory leak, deadlock) and need to look inside the
running process. Less common than the other diagnostic
recipes; more powerful when needed.

This document covers:

- **CPU profiles** with `perf` (Linux).
- **Heap dumps** for memory analysis.
- **Stack dumps** for diagnosing deadlocks.

Brain is Rust + Tokio + Glommio. Some of the tooling
familiar from JVM or Go-based systems is different
here; the equivalents are listed.

---

## Decide what you need

A quick triage:

| Symptom | Tool |
|---|---|
| One core pegged at 100 % | CPU profile (`perf record`) |
| RSS growing without bound | Memory profile (`heaptrack` or `jemalloc` stats) |
| Process hangs but doesn't crash | Stack dump (`pstack`, `gdb`) |
| Latency spiky but no obvious hotspot | Off-CPU profile (`perf`, `bcc-tools`) |
| Specific allocation pattern | Per-callsite memory profile |
| Garbage-collection-like pauses | n/a — Rust has no GC |

When in doubt, take a CPU profile first. It's the
cheapest and answers the most questions.

---

## Prerequisites

- **Production access** ([DR-03](dr-03-safe-production-access.md))
  — profiling reads kernel-level data that's not
  exposed via the admin API.
- **`perf` installed.** On most Linux distributions,
  it's the `linux-perf-tools` or `linux-tools-$(uname -r)`
  package.
- **Debug symbols.** Brain ships release builds with
  basic debug info (`.debug_info`); for full
  resolution of inlined functions, you may need a
  debug-symbol package or the un-stripped binary.
- **Sufficient disk.** Profiles can be hundreds of MB.

If any of these are missing, do what you can. A
profile without good symbols is still useful (you'll
see addresses instead of function names; `addr2line`
can resolve them later).

---

## CPU profile with `perf`

The standard tool. Records call stacks of the process
at a high frequency for a short window; produces a
file you can analyse later or convert to a flame graph.

### Recording

```bash
# 30 seconds of profiling, all threads of the brain-server process.
sudo perf record \
    -F 99 \
    -g \
    -p $(pgrep -d, brain-server) \
    -- sleep 30
```

Flags:

- `-F 99` — sample at 99 Hz. ~3000 samples over 30 s.
  Higher rates capture finer detail but cost more.
- `-g` — record call stacks (not just leaf addresses).
- `-p` — restrict to specific PIDs. The `pgrep -d,`
  passes a comma-separated list if there's more than
  one process.
- `-- sleep 30` — sample for 30 s. Adjust the
  duration to fit the symptom (a transient spike
  needs a few seconds; a sustained issue needs
  longer).

The recording writes to `perf.data` in the current
directory. Roughly 10-50 MB for 30 seconds at 99 Hz.

### Inspecting

```bash
sudo perf report --stdio --no-children | head -50
```

Top functions by CPU time, sorted by hot-on-call-stack.
Look for the entries highest in the output — those
are where the CPU is going.

For interactive exploration:

```bash
sudo perf report   # ncurses UI
```

### Producing a flame graph

The most readable form. Install Brendan Gregg's
[FlameGraph](https://github.com/brendangregg/FlameGraph)
scripts (one-shot clone):

```bash
git clone https://github.com/brendangregg/FlameGraph /opt/flamegraph

sudo perf script > perf.script
/opt/flamegraph/stackcollapse-perf.pl perf.script > perf.folded
/opt/flamegraph/flamegraph.pl perf.folded > perf.svg
```

Open `perf.svg` in any browser. Width of each frame =
fraction of CPU time; depth = call stack. Hover for
details.

### What to look for

- **A single frame eating most of the width.** That's
  the hotspot.
- **An unexpected function high in the stack.** A
  function you didn't expect to be hot is the next
  hypothesis.
- **Lots of small unrelated frames.** Indicates the
  workload is genuinely spread; the bottleneck may
  not be CPU at all (could be I/O wait).

---

## Off-CPU profile

Useful when the substrate is *slow* but CPU is *low* —
something's waiting (disk, network, lock). `perf`'s
default mode only catches running threads; an off-CPU
profile catches the waits.

### Recording

The standard tool is `offcputime` from
[bcc-tools](https://github.com/iovisor/bcc). On most
distros:

```bash
sudo apt install bpfcc-tools     # or equivalent

# 30 seconds of off-CPU sampling for brain-server.
sudo offcputime-bpfcc -p $(pgrep brain-server) -f 30 > offcpu.folded
```

### Visualising

Convert to a flame graph the same way as the CPU
profile:

```bash
/opt/flamegraph/flamegraph.pl \
    --color=io --title='off-cpu' \
    < offcpu.folded > offcpu.svg
```

Width = fraction of *off-CPU* time. A wide
`fsync_call` frame in this view means writes are
waiting on disk. A wide `flume::recv_async` frame
means a shard is waiting for inbound requests
(probably benign).

---

## Memory profile

Rust uses the system allocator (or `jemalloc` if
configured); there's no GC, so memory profiling is
about allocation patterns and growth, not pauses.

### Quick memory snapshot

The cheapest signal:

```bash
ps -p $(pgrep brain-server) -o pid,vsz,rss,pmem,cmd
```

If RSS is growing over time, you've confirmed a
growth pattern. Now you need to know *where*.

### Heap dump with `heaptrack`

[heaptrack](https://github.com/KDE/heaptrack) records
every allocation; the GUI shows where allocations
happen and which call paths leak most.

Attach to a running process:

```bash
sudo heaptrack -p $(pgrep brain-server)
```

Run for some minutes (depending on how slowly the
leak manifests), then Ctrl-C. heaptrack writes
`heaptrack.brain-server.<pid>.gz`.

Open the GUI on a workstation:

```bash
heaptrack_gui heaptrack.brain-server.<pid>.gz
```

The "Flame graph" tab shows allocations by call
path. Look for paths that are allocating without
bound (no corresponding free).

> Heaptrack adds significant overhead (~2-5x). Don't
> run it on a production-hot instance — attach to a
> staging copy that reproduces the symptom, or to a
> low-traffic shard.

### jemalloc stats

If Brain is built with jemalloc (check
`brain-server --version` output or build flags), the
allocator can dump per-allocation-class stats to a
log. The mechanism is `MALLOC_CONF=stats_print:true`
at startup; once running, you can also trigger a dump
via signal:

```bash
kill -USR2 $(pgrep brain-server)
```

Look in the substrate's stderr for the dump. Useful
for confirming "the leak is in 1024-byte buckets"
without instrumenting the code.

---

## Stack dump

When the substrate is hung — connections aren't
being accepted, but the process is alive — a stack
dump tells you what every thread is doing right now.

### With pstack

```bash
sudo pstack $(pgrep brain-server)
```

Prints one stack trace per thread. Look for:

- Threads stuck in `read` / `recv` / `fsync` — that's
  I/O wait, possibly normal.
- Threads stuck in `__lll_lock_wait` — that's
  contention on a futex; lock contention.
- Threads stuck in a Brain function — your hypothesis
  for the hang.

`pstack` is sometimes called `eu-stack` (from
elfutils) on distros that don't ship the gdb wrapper.

### With gdb

For deeper inspection:

```bash
sudo gdb -p $(pgrep brain-server) -batch \
    -ex 'thread apply all bt' \
    -ex 'quit'
```

Same output as `pstack` but with more context (gdb
can show register values, locals, etc., if symbols
are present).

For an interactive session:

```bash
sudo gdb -p $(pgrep brain-server)
(gdb) thread apply all bt
(gdb) thread 3
(gdb) frame 5
(gdb) print some_variable
(gdb) detach
(gdb) quit
```

Be quick. Attaching to a process via `ptrace` *pauses*
all its threads. Spend more than a few seconds in
gdb and clients will time out.

---

## What to do with what you've collected

A profile or dump is a *file*; the value is in
analysis. Common workflows:

### Solo analysis

You see the bottleneck in the flame graph; you know
what it means; you proceed with the runbook (or file
a ticket with the file attached).

### Escalation

The profile shows something engineering needs to look
at. Attach to your diagnostic bundle
([DR-01](dr-01-diagnostic-bundle.md)) before
escalating. The engineer reads it in their environment.

### Postmortem

The profile becomes evidence in the postmortem. Include
the flame graph as an image; describe what it shows;
link to the action items it informs.

---

## Profiling impact on production

Take this seriously. Profiling has costs:

| Tool | Overhead | When safe |
|---|---|---|
| `perf record -F 99 -g` | ~1-5 % | Anytime; brief windows. |
| `perf record -F 999 -g` | ~5-15 % | Brief windows only. |
| `offcputime` | low for off-cpu; near zero for cpu | Anytime. |
| `heaptrack` | 2-5x | Staging / low-traffic only. |
| `gdb` attach | freezes all threads | Seconds, never minutes. |
| `pstack` | freezes all threads briefly | Seconds. |

For a P1 where every minute matters, `perf record`
for 30 seconds is fine — the overhead is negligible
and the data is invaluable. `heaptrack` on a hot prod
shard is not fine; replicate the symptom in staging.

---

## After-action: cleaning up

After you've captured what you needed:

- **Delete the perf.data files.** They're big and
  contain in-memory state you don't want lying around.
- **Sanitise flame graphs.** Function names may
  contain function-parameter hints that reveal
  internal structure; if you're sharing externally,
  redact.
- **Detach gdb cleanly** (`detach`, not Ctrl-C). An
  un-detached ptrace can leave the process in a
  weird state.

---

## Related runbooks

- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-02 — Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)
- [RB-02 — High latency on a shard](rb-02-high-latency.md)
- [RB-03 — Memory pressure / OOM](rb-03-memory-pressure.md)
- [RB-08 — Substrate becoming unresponsive](rb-08-unresponsive.md)

---

## Last validated

*Update on first use.*
