# IR-01: How to use these runbooks

**Audience:** anyone newly on call for Brain, or
operating Brain for the first time at meaningful scale.

**Goal:** read this once. Refer back when you forget the
flow. After this doc you should be able to take an alert
to resolution — diagnosis, fix, verification, follow-up —
without improvising the *shape* of the response, even on
your first day.

---

## The flow, in one diagram

```
   ┌───────────────────────────────────────────────┐
   │  1.  Alert fires (or you notice symptoms)     │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  2.  Acknowledge the page within target SLA   │
   │      (P1: 15 min;  P2: 30 min;  P3: 24 h)     │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  3.  Open the runbook the alert links to      │
   │      (or pick by symptom from the index)      │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  4.  Confirm "Am I in the right runbook?"     │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  5.  Open an incident channel; post the page  │
   │      summary and which runbook you're running │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  6.  Stop the bleeding (if on fire)           │
   │      Diagnose                                  │
   │      Remediate                                 │
   │      Verify                                    │
   └────────────────────┬──────────────────────────┘
                        ▼
   ┌───────────────────────────────────────────────┐
   │  7.  Wrap up: post-incident notes;            │
   │      decide whether to write a postmortem     │
   └───────────────────────────────────────────────┘
```

Seven steps. Each section below covers one in detail.

---

## 1. Something is wrong

There are two ways you'll find out:

**An alert fired.** Your pager / Slack / on-call platform
will have notified you. The notification carries:

- The **alert name** (e.g. `BrainHighLatency`).
- A **summary** of the metric / threshold that fired.
- A `runbook:` annotation pointing at the specific
  document in this directory (e.g. `RB-02`).
- A **severity** (P1–P4) baked into the alert config.

**Someone reports symptoms.** A teammate, a user, a
downstream service. You don't have an alert; you have
a symptom report ("the chatbot is slow," "recalls are
returning fewer results than usual").

For the symptom path:

1. Go to the [README](README.md) index.
2. Look at the **Incident runbooks** table.
3. Find the runbook whose linked alert *would have*
   fired given the symptom.
4. Open that runbook. Its "Am I in the right runbook?"
   section confirms the match.

If nothing matches, start with
[DR-02 — Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md)
to orient. You may be the first one to see a new failure
mode, in which case you should write a runbook for it
afterwards.

---

## 2. Acknowledge

Acknowledge the page within the target SLA for its
severity. Acknowledgement does **not** mean "I've fixed
it." It means "I am on this." The pager system stops
re-paging others; the incident clock starts.

| Severity | Ack target |
|---|---|
| P1 | 15 minutes |
| P2 | 30 minutes |
| P3 | 24 hours (next business day) |
| P4 | 1 week (opportunistic) |

If you can't ack in time (you're driving, you're in a
meeting that genuinely can't be left, you're not actually
on call), **the pager should route to the next responder
automatically**. Don't ack a page you can't immediately
work.

See [IR-02](ir-02-severity-ladder.md) for severity
definitions and [IR-03](ir-03-escalation-policy.md) for
the escalation rules.

---

## 3. Open the runbook

The alert's `runbook:` annotation points at a specific
file: `RB-02` → `rb-02-high-latency.md`. Open it.

If the alert doesn't have a `runbook:` annotation (rare,
but possible for new alerts), look up the alert name in
the [README index](README.md). If still nothing,
search by symptom.

**Don't skip the runbook just because you think you know
the answer.** Runbooks encode hard-won lessons from
previous incidents. The fast-path that's "obvious" to
you might be the slow-path that bit the last operator.

---

## 4. Confirm "Am I in the right runbook?"

Every incident runbook starts with an **Am I in the
right runbook?** section listing the concrete symptoms
that should match before you proceed.

If your symptoms don't match: stop. Look at related
runbooks. The same alert can sometimes fire for two
different underlying problems; the wrong runbook leads
to the wrong remediation.

If they match: continue.

If your symptoms *partially* match: pause and think.
Maybe your incident is a composite. Note what's off in
the incident channel and proceed cautiously — the
diagnostic steps will surface the real cause.

---

## 5. Open an incident channel

For P1 and P2 incidents, **open a dedicated incident
channel** (Slack, Teams, whatever your org uses) and
post:

```
:rotating_light: P1 incident: BrainHighLatency on prod
Started: 14:31 UTC
Runbook: RB-02 — High latency on a shard
Responders: <you>
Status: diagnosing
```

The channel exists for two reasons:

1. **Coordination.** If a second person joins (or you
   escalate to engineering), they have a written log of
   what's been tried so they don't repeat work.
2. **Communication.** Other teams can see the channel
   and ask questions instead of paging you separately.

For P3 / P4 there's no incident channel — a ticket is
fine.

See [IR-04 — Incident communication](ir-04-incident-communication.md)
for the full convention.

---

## 6. Work the runbook

Every runbook has the same shape:

### Stop the bleeding

**Only the runbook tells you whether this step applies
to your incident.** Some incidents need immediate
mitigation before diagnosis (drain traffic, isolate a
shard, pause a worker). The runbook spells out what.

If your runbook *has* a Stop-the-bleeding section, do
those things **before** diagnosing. The bleeding
matters; diagnosis can wait a few minutes.

### Diagnose

Step-by-step procedure with commands, expected output,
and **branches**:

```
1. Check brain_request_duration_ms_bucket
   - If p99 > 100 ms for the last 5 min: proceed to step 2.
   - If p99 < 100 ms but p999 > 500 ms: jump to step 5.
   - Otherwise: this isn't the right runbook; check RB-08.

2. ...
```

Don't skip a branch. The Diagnose section is structured
so following it gets you to the cause in a bounded number
of steps.

If a step's expected output doesn't match what you see:

- **Output is empty or missing entirely**: probably
  observability is down. Skip to [DR-02](dr-02-reading-traces-metrics-logs.md)
  for what to do.
- **Output is unexpected but the runbook doesn't list
  this case**: post it in the incident channel,
  continue cautiously to the next step, and add a note
  to update the runbook.

### Remediate

Once Diagnose has identified the cause, Remediate has
the fix steps. Often these are organised by root cause:

```
### If the root cause is X
1. Run `brain-cli ...`
2. Confirm ...

### If the root cause is Y
...
```

**Don't run the remediation for a root cause you
haven't confirmed.** That's the fastest way to make a
bad incident worse.

### Verify

After remediation, the runbook tells you how to confirm
the fix actually worked. Usually this is some specific
metric returning to normal, or a smoke-test command. Do
not skip Verify — "it looks fixed" is not "it is
fixed."

If Verify fails, the fix didn't work. Go back to
Diagnose; you've learned something new.

### Post-incident

Once Verify passes, the runbook tells you what to log
and what tickets to file. Usually:

- A short message in the incident channel marking
  resolved with timestamp.
- A summary of root cause + remediation.
- Any follow-up tickets (e.g., a code change to
  prevent recurrence).
- Whether to write a postmortem (see step 7).

---

## 7. Wrap up and learn

After every incident:

**Always** post a one-line summary in the incident
channel:

```
:white_check_mark: Resolved at 15:42 UTC.
Root cause: HNSW tombstone ratio exceeded 0.35
after a mass-forget. Triggered rebuild via brain-cli.
Follow-up: BRAIN-1234 (auto-rebuild threshold).
```

**Always** update any open ticket(s) with what you did.

**Sometimes** write a postmortem. The rule:

- **P1**: yes, always.
- **P2**: yes, unless the same incident has had a
  postmortem in the last 60 days.
- **P3**: optional; depends on whether there's
  something worth learning.
- **P4**: no.

Use [`postmortem-template.md`](postmortem-template.md).
Keep it blameless. Focus on systemic causes, not
individual decisions.

---

## When *not* to follow the runbook

There are exactly two cases:

1. **The runbook is wrong.** It tells you to run a
   command that doesn't exist, or to check a metric
   that doesn't exist, or its branching doesn't cover
   your situation. *Stop, post in the incident channel
   that the runbook is out of date, and proceed using
   your judgement.* Then update the runbook
   afterwards.
2. **You have a P1 with seconds-matter timing.** If
   waiting to read the runbook would extend the
   outage, do the obvious mitigation first (drain
   traffic, isolate the shard, restart the process —
   whichever is appropriate), then return to the
   runbook for diagnosis and verification.

Outside these two cases, **follow the runbook even if
you think you know the answer**. Runbooks encode
experience.

---

## What "Last validated" means

At the bottom of every runbook is a `Last validated:`
field. It's a date plus the name of the operator who
exercised the runbook (against a real incident or a
chaos-test scenario).

**Why this matters:** runbooks drift. The substrate
changes, command flags change, dashboard panels get
renamed. A runbook that hasn't been exercised in a year
is unreliable.

The on-call rotation should periodically run through
runbooks during low-load hours (a "fire drill" pattern)
to keep the validated dates current. The
[OP-02 Snapshot restore drill](op-02-snapshot-restore-drill.md)
and similar operational runbooks are designed for this
kind of periodic exercise.

When you exercise a runbook:

1. Run through it end to end.
2. Note anything that's out of date (in the incident
   channel for live ones, or directly in the doc for
   drills).
3. Update the `Last validated:` field with today's
   date and your name.

---

## A worked example

Walk through this once to internalise the flow.

**14:31:** Slack pings:

```
:rotating_light: ALERT - BrainHighLatency
Shard 3, p99 = 187ms (threshold 100ms) for 10 min
Runbook: RB-02
Severity: P2
```

**14:31:01:** Acknowledge the page. The on-call clock
starts.

**14:32:** Open [RB-02](rb-02-high-latency.md). Confirm
in "Am I in the right runbook?" that the symptoms
match (yes — high p99, sustained, one shard).

**14:33:** Open `#brain-incidents` and post:

```
:rotating_light: P2: BrainHighLatency on shard 3 (prod).
Started: 14:21 UTC (alert at 14:31). Runbook: RB-02.
Responder: <me>. Status: diagnosing.
```

**14:34:** Work the Diagnose section.

- Step 1: identify slow operation. Grafana shows
  `recall` is the slow op.
- Step 2: check resource exhaustion. `top` shows no
  CPU pegging. RAM normal.
- Step 3: check HNSW health. `tombstone_ratio = 0.41`.
  Bingo.

**14:36:** Post in the channel:

```
Cause: HNSW tombstone ratio 0.41 on shard 3.
Remediating by RB-02 step 3.
```

**14:37:** Run `brain-cli rebuild-ann --shard 3`.
Rebuild starts. Latency briefly spikes higher (rebuild
is heavy) but the new index will replace the
tombstone-heavy one once ready.

**14:42:** Rebuild completes. Tombstone ratio drops to
`0.00`.

**14:43:** Verify — `brain_request_duration_ms_bucket`
back under 100ms.

**14:44:** Post:

```
:white_check_mark: Resolved. Tombstone ratio was 0.41;
rebuilt the HNSW. Back at 0.00. p99 normal.
Total user impact: ~25 min of slow recall on shard 3.
No postmortem (well within RB-02's expected envelope).
Filing BRAIN-1234 to lower the auto-rebuild threshold.
```

**14:45:** Done. Update `Last validated:` on RB-02 with
today's date and your name.

Total time from page to fixed: ~13 minutes. Total time
on call: ~30 minutes including write-up.

---

## Tips that aren't in any specific runbook

A few patterns experienced operators learn the hard way:

- **Read the whole runbook before starting.** Even if
  it's long. You'll know what's coming and what
  decisions are coming.
- **Take notes in the incident channel as you go.**
  Not just outcomes — the commands you ran and what
  they showed. Future-you in the postmortem will
  thank you.
- **Don't run "just one thing to see" outside the
  runbook.** That's the riskiest pattern in incident
  response. If the runbook didn't say to run it, it's
  a guess.
- **Escalate sooner, not later.** Most postmortems
  conclude "we should have escalated 20 minutes
  earlier." See [IR-03](ir-03-escalation-policy.md).
- **The substrate is fail-stop.** If it refused to
  start, it had a reason. Don't force-restart through
  a recovery failure; that's how data gets corrupted
  worse.

---

## Related docs

- **[IR-02 — Severity ladder](ir-02-severity-ladder.md)** —
  P1–P4 definitions, ack targets, ticket vs page.
- **[IR-03 — Escalation policy](ir-03-escalation-policy.md)** —
  when and how to page engineering.
- **[IR-04 — Incident communication](ir-04-incident-communication.md)** —
  incident channels, status updates, stakeholder
  comms.
- **[DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)** —
  what to collect when you escalate.
- **[Postmortem template](postmortem-template.md)** —
  blameless postmortem format.
- **[README](README.md)** — the index, with the alert →
  runbook mapping.

---

## Last validated

*Not yet exercised against a live incident. Update this
field on first use.*
