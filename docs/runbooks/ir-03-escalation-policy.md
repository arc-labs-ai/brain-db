# IR-03: Escalation policy

**Audience:** every on-call operator. Refer to this when
you're stuck, when an incident is going long, or when
you're not sure if you should pull in additional people.

The core message: **escalate sooner, not later**. The
most common pattern in postmortems is "the on-call
hesitated to escalate." This document defines when
escalation is appropriate and how to do it cleanly.

---

## When to escalate

Escalate immediately if **any** of the following is true:

1. **You don't have a working hypothesis** and you've
   been on the incident for the duration listed below
   for the severity:
   - P1: 30 minutes.
   - P2: 1 hour.
2. **The runbook's procedure didn't work** and you're
   not sure what to try next.
3. **Data integrity is at risk** and you're about to do
   something destructive (overwrite, delete, restore).
   *Always* escalate before that.
4. **Multiple incidents are firing at once** and you
   can't reasonably handle them solo.
5. **You feel out of your depth.** This is a valid
   reason on its own. Trust it.
6. **The incident has been going for >2 hours regardless
   of severity.** Even if you're making progress,
   another set of eyes helps and your judgment is
   probably degrading.

These are *minimums*. You can always escalate earlier.
The expensive failure mode is "the on-call burned 90
minutes alone before pulling in help"; the cheap one is
"the on-call escalated and the second responder said
'looks like you have it, I'll observe.'"

---

## When *not* to escalate

A few cases where waiting is appropriate:

- **You're actively executing the remediation.** If the
  fix is in flight and you'll know in 5 minutes whether
  it worked, finishing the verify step first is fine.
- **The incident is P3 or P4.** These don't have
  escalation paths in the same sense; they go to
  tickets and follow the team's standard process.
- **Verify is the only step left** and it's
  straightforward.

If you're tempted to wait *because you don't want to
disturb someone*: escalate anyway. The disturbance is
expected and budgeted; that's why an on-call rotation
exists.

---

## Who to escalate to

The escalation chain has three roles, in order:

### 1. Second operator / on-call backup

Most escalations stop here. Another operator joins,
either as a fresh set of eyes or to share the workload.

**When:**

- You've been on the incident >30 min for P1, >1 h for
  P2.
- You're stuck.
- The incident is going long and you need someone to
  take notes / handle comms while you focus.

**How:** page the secondary on-call through your pager
system. They acknowledge; you brief them in the
incident channel. Continue working together.

### 2. Engineering escalation

Pull in engineering when:

- You suspect a substrate bug (data integrity, panic
  message, unexpected behaviour).
- The remediation requires code changes or operations
  beyond the runbook's scope.
- You want approval before a destructive operation
  (restore, hard-forget, etc.).
- You've gone through the second-operator path and
  you're still stuck.

**How:**

1. Page the engineering on-call.
2. In the incident channel, post a structured handoff
   (template below).
3. Wait for engineering to ack. Continue working in
   parallel; don't stop diagnosing while waiting.

### 3. Leadership escalation

Pull in leadership when:

- The incident is going to materially affect product or
  business commitments (a customer SLA, a launch).
- Multiple teams need to coordinate.
- External communication is needed (status page,
  customer email, regulators).
- The incident is highly visible and may need
  organisational decision-making.

**How:** depends on your org. Most orgs have a
designated "incident commander" or similar role that
gets paged at this tier. Refer to your org's IR
playbook beyond this point.

---

## What to bring to an escalation

The single most expensive mistake in escalation is
**handing off without context**. The new responder
shouldn't have to ask "what's been tried?"

Bring all of the following:

### The handoff message

Post in the incident channel before paging:

```
:loudspeaker: ESCALATING to <role>
Incident: BrainHighLatency on shard 3 (prod)
Severity: P2 (considering upgrade to P1)
Started: 14:21 UTC
Duration: 1h 23min
Runbook: RB-02

Symptoms: p99 recall latency at 187-220ms sustained
on shard 3 only. Other shards normal.

What's been tried:
- Checked HNSW tombstone ratio (0.08, healthy)
- Checked CPU / RAM / disk (no exhaustion)
- Profiled shard 3's executor (no obvious hot loop)
- Restarted shard 3 (no improvement)

Current hypothesis: possible flume channel saturation
between Tokio and Glommio, but I can't verify without
adding instrumentation.

Next step: hand off to engineering for substrate-level
investigation.

Diagnostic bundle: <link to S3 or wherever bundles live>
```

This template comes from incident-response best
practice; the substrate doesn't enforce it but every
incident channel should use something similar.

### The diagnostic bundle

If you escalate to engineering, **always** include a
diagnostic bundle. See
[DR-01 — Collecting a diagnostic bundle](dr-01-diagnostic-bundle.md)
for what goes in it.

Roughly: logs, metric snapshots, OTel traces if
available, config files, snapshot manifest.

### The runbook you've been following

Engineering may not know which runbook applies. Link
it explicitly:

```
Runbook in use: [RB-02 high-latency](rb-02-high-latency.md)
Steps completed: 1, 2, 3
Stuck at: step 4 (no obvious embedder issue)
```

### What you tried *outside* the runbook

If you tried anything not in the runbook (improvising
during a P1 with urgent timing), say so. The new
responder needs to know what state the system is in.

```
Outside the runbook, also tried:
- Manual rebuild of shard 3 HNSW (`brain-cli rebuild-ann
  --shard 3`). No improvement.
- Forced a snapshot save (`brain-cli snapshot take`).
  Worked normally.
```

---

## After escalation: who owns what

Two patterns:

### "Handing off"

You completely transfer the incident to the new
responder. Use this when you're going off shift, or
when the incident has changed character and engineering
is taking the lead.

The new responder:

- Acknowledges the page.
- Reads the handoff message and the channel history.
- Posts in the channel: "I've taken over. Continuing
  to diagnose."
- Owns the incident from this point.

You:

- Stay reachable for ~30 minutes in case they have
  questions.
- Can leave the channel after that.

### "Adding"

The new responder joins without taking over. They're
extra eyes, taking notes, handling comms, or working a
parallel hypothesis.

Both of you:

- Continue in the channel.
- Coordinate explicitly. "I'm going to try X, you take
  Y." Don't both run the same command.
- Keep posting updates.

Pick "handing off" when you're stuck or out of energy.
Pick "adding" when you have a hypothesis but need help
executing.

---

## Escalation patterns to avoid

### "I'll just try one more thing"

The most common failure mode. You're 90 minutes into a
P1, the runbook didn't help, and you keep thinking
"the next thing I try will fix it." It usually
doesn't. Escalate.

### Silent escalation

Paging engineering without context, without a handoff
message, without a diagnostic bundle. The engineer
wakes up, joins the channel, and finds nothing to
work with. Now you've burned an engineer's sleep and
gained nothing.

Always page *and* post the structured handoff in the
channel.

### Escalating to multiple people simultaneously

Don't page engineering and leadership at the same
time. Engineering first; if engineering says "this is
beyond the substrate, we need leadership," escalate
again.

The exception: incidents that obviously affect external
commitments (customer SLA, public-facing outage). In
those cases, the comms work runs in parallel with the
technical work, so leadership pages early.

### Cosmetic escalation

"I've been working this alone too long; let me page
someone for cover." Don't. Either there's a substantive
reason to escalate (you're stuck, data is at risk, you
need help) or there isn't. If you genuinely just need
to take a break, swap with the secondary on-call —
that's a handoff, not escalation.

---

## Escalation during low-severity incidents

P3 and P4 don't have the same escalation structure
(no pages, no incident channels). But "escalation" of
a sort still happens:

- A P3 you can't figure out in your investigation
  hours becomes a ticket for engineering with what
  you've learned attached.
- A P3 that reveals a deeper problem during
  investigation gets *upgraded* (becomes P2 or P1,
  triggering the higher-severity escalation paths).

The pattern: P3 escalation looks like "filing a
better ticket," not paging.

---

## Page-handling at scale

If your deployment is large enough to have multiple
shards across multiple servers and a real 24/7 on-call
rotation, you'll want:

- **A primary** on-call (gets paged first).
- **A secondary** on-call (gets paged if primary
  doesn't ack within the SLA).
- **An engineering escalation** rotation (separate
  from on-call; gets pulled in per this doc).
- **A weekly hand-off** between primaries and
  secondaries.

If you don't have this scaffolding yet, your
deployment is small enough that ad-hoc paging works.
Set it up *before* the first P1.

---

## What to expect in the first 15 minutes after paging

Here's what a clean escalation looks like end-to-end:

```
14:31  Alert fires. You ack.
14:35  Open incident channel, post page summary.
15:00  Diagnose, work runbook. No progress.
15:31  30 min boundary. You ack to yourself: I'm stuck.
15:32  Post handoff template in channel.
15:33  Page secondary on-call.
15:36  Secondary acks page, joins channel.
15:38  Secondary reads handoff, asks two questions.
15:42  You answer. Both of you continue together.
15:50  Secondary suggests an angle you hadn't tried.
15:55  Trying secondary's hypothesis.
16:10  Hypothesis confirmed. Remediating.
16:20  Verified fixed.
16:25  Resolution summary posted. Channel closed.
```

Total time from page to fixed: ~110 minutes. Total
time spent solo before escalation: ~60 minutes.
Escalation cost: ~5 minutes of context handoff. Win.

The bad version of this same incident has you spending
3 hours solo, missing the angle the secondary saw in 5
minutes, and writing a postmortem with the action item
"escalate earlier."

---

## Related docs

- [IR-01 — How to use these runbooks](ir-01-how-to-use-runbooks.md)
- [IR-02 — Severity ladder](ir-02-severity-ladder.md)
- [IR-04 — Incident communication](ir-04-incident-communication.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [Postmortem template](postmortem-template.md)
