# IR-02: Severity ladder and SLOs

**Audience:** every on-call operator. Refer back when you
need to remember the bar between P1 and P2, or when to
ticket vs page.

This document defines what each severity tier means,
when each applies, and how SLOs anchor the ladder. The
goal is that two operators looking at the same incident
agree on its severity.

---

## The ladder, at a glance

| Severity | Headline | Page? | Ack target | Working hours? |
|---|---|---|---|---|
| **P1** | Production down or imminent data loss | Yes, immediately | 15 min | Any time |
| **P2** | Major degradation, broadly user-visible | Yes | 30 min | Any time |
| **P3** | Minor degradation, partial impact | Ticket (no page) | 24 h | Next business day |
| **P4** | Informational, no current impact | Ticket | 1 week | Opportunistic |

Every alert in monitoring is tagged with one of these.
Every runbook in this directory declares the severity of
the incident it handles.

The bar between tiers is **user impact**, not technical
severity. A nuclear technical failure that no user sees
yet isn't P1; a small bug that's hitting every user *is*.

---

## P1 — Production down

**Definition.** Brain is not serving its core function
to a meaningful fraction of users, OR durable data is at
imminent risk.

**Concrete triggers:**

- Server process won't start (across all shards).
- Multiple shards refusing to spawn.
- Recall returning errors or empty results at >50 %
  rate.
- Encode returning errors at >5 % rate sustained.
- WAL fsync failing — every write is unacknowledged.
- Snapshot worker has been failing for >24 h.
- Disk is filling and will be full in <1 h.
- Recovery has failed and the substrate is refusing
  service.
- Active data corruption detected (CRC mismatches mid-
  WAL, slot version drift, etc.).

**Response.**

- **Page on-call immediately.** Acknowledge within 15
  minutes.
- **Open an incident channel** the moment you ack (don't
  wait until you've started diagnosing).
- **Escalate at 30 minutes** if you don't have a working
  hypothesis. See [IR-03](ir-03-escalation-policy.md).
- **Always write a postmortem.** Use
  [`postmortem-template.md`](postmortem-template.md).
- **Status updates every 15 minutes** during the
  incident.

**Example incidents:**

- "The server crashed and won't restart." (P1)
- "We can see recall is returning the wrong vectors
  because the model fingerprint check is failing." (P1
  — data risk)
- "Half the shards are unreachable; clients are timing
  out." (P1)

---

## P2 — Major degradation

**Definition.** Brain is serving its core function but
with significant degradation that a meaningful fraction
of users would notice.

**Concrete triggers:**

- Latency p99 above SLO target on one or more shards
  for >10 min.
- Recall quality measurably degraded (recall@10 below
  threshold).
- One shard refusing to spawn (others healthy).
- Encode latency spiking but not erroring.
- A background worker stuck and not catching up (e.g.,
  decay falling >24 h behind).
- Disk filling, will be full in 6–24 h.
- LLM extractor cost rate >2× expected.
- Connection acceptance saturating; new clients
  getting rejected.

**Response.**

- **Page** on-call. Acknowledge within 30 minutes.
- **Open incident channel.**
- **Escalate at 1 hour** if no working hypothesis.
- **Postmortem usually yes**, especially if it's a
  recurring problem or there's something to learn.
  Skip if the same incident had a postmortem in the
  last 60 days.
- **Status updates every 30 minutes** during the
  incident.

**Example incidents:**

- "p99 latency on shard 3 has been at 200ms for 15
  minutes." (P2)
- "The decay worker is paused; nothing's exploding but
  rankings are getting worse." (P2)
- "We're being rate-limited by Anthropic; LLM
  extractors are queueing." (P2)

---

## P3 — Minor degradation

**Definition.** A real problem, but not user-impacting
enough to disturb sleep. The substrate is working; some
edge case is wrong or some operational thing needs
attention.

**Concrete triggers:**

- One specific predicate's extractor failing at
  elevated rate (the rest are fine).
- Snapshot worker missed one cycle (next cycle should
  catch up).
- A single agent's memories showing inconsistency
  (one row in metadata, no matching arena slot, etc.).
- Slow growth in disk usage above projection (not
  imminent).
- A non-critical alert firing for the first time
  (deserves investigation but isn't an emergency).

**Response.**

- **Ticket only — no page.** Operators handle during
  business hours.
- Acknowledge / triage within 24 hours.
- **No incident channel** unless triage reveals it's
  actually higher severity.
- **No postmortem** unless triage reveals it's the
  visible part of a deeper issue.
- **Status updates** as needed; not on a fixed cadence.

**Example incidents:**

- "One memory in shard 2 has a slot CRC mismatch.
  Quarantined automatically; no other affected."
  (P3)
- "Tombstone ratio on shard 1 reached 0.20; not bad
  enough to rebuild yet but worth watching." (P3)

---

## P4 — Informational

**Definition.** No current impact. Something you'd
want to know about but don't need to fix now.

**Concrete triggers:**

- A deprecated configuration field is in use; it'll
  break in a future release.
- A non-critical metric is missing from monitoring
  (didn't cause an outage, but the next outage might
  be harder to debug).
- An extractor is producing more `SkippedFilter`
  audits than expected — informational, possibly a
  schema-tuning opportunity.

**Response.**

- **Ticket.** No page.
- Acknowledge within a week.
- No incident channel, no postmortem, no status
  updates.

**Example incidents:**

- "Found a small typo in our schema's prompt." (P4)
- "Latency on shard 0 is 5ms slower than shard 1.
  Both well within SLO." (P4)

---

## How severity is assigned

**By the alert config.** Most severities are baked
into the alert. `BrainSubstrateDown` is configured as
P1; `BrainHighLatency` is configured as P2; etc. When
the alert fires, the severity travels with it.

**By the runbook.** Every runbook declares its expected
severity at the top. If you opened the runbook in
response to an alert with a different severity, **the
alert's severity wins by default** — but you should
note the mismatch in the incident channel.

**By the operator (rare).** During an incident, the
on-call may upgrade or downgrade severity based on
what they learn. Example: a P2 alert fires, but
diagnosis reveals a deeper data-corruption problem —
upgrade to P1.

Severity changes during an incident are **explicit**:
the operator posts in the channel, "Upgrading to P1
because of <observation>." Don't silently treat a P2
as if it's P3 because it's not bothering you yet.

---

## Severity transitions during an incident

It's normal for severity to evolve:

- **Upgrade** (P2 → P1): diagnosis reveals worse impact
  than the alert detected. Open an incident channel
  if you hadn't, page additional responders if
  appropriate.
- **Downgrade** (P1 → P2): mitigation has reduced user
  impact, but the underlying problem isn't fully
  fixed yet. Keep the incident channel open. Continue
  status updates at the new cadence.
- **Resolve** (any → no incident): the substrate is
  back at SLO. Post resolved with a summary; close
  the channel; file follow-up tickets.

Always post the transition. Other responders need to
know.

---

## SLOs

Brain's SLO targets are the user-visible bars for what
counts as "working":

| Metric | Target | Window |
|---|---|---|
| Recall p99 latency | < 100 ms | rolling 1 h |
| Recall success rate | > 99.9 % | rolling 1 h |
| Encode p99 latency | < 50 ms | rolling 1 h |
| Encode success rate | > 99.95 % | rolling 1 h |
| Recall recall@10 | > 0.92 | rolling 24 h |
| Snapshot freshness | < 2 h since last successful | continuous |

These targets are alert thresholds. When you see a P1
or P2 alert, the metric breached its target. SLOs are
how Brain *promises* what it does, and how alerts
choose what to interrupt humans for.

**Error budget.** Across a 30-day window, the
substrate is allowed to violate each SLO by some small
amount before action is required (typically: 0.1 % of
the window for latency, 0.001 % for success rate). The
remaining budget is the "error budget." If a deployment
burns through its error budget, that's a planning
signal — not an alert, but worth a quarterly review.

Concrete numbers vary per deployment. Production-tier
deployments often target stricter SLOs. Development
deployments may not have formal SLOs at all.

---

## Edge cases

### Data-risk severity

Anything that puts durable data at risk is P1 by
definition, even if no user is currently impacted.

Examples:

- A bug detected in the WAL replay path. No data lost
  yet, but the next crash might cause loss. P1.
- A slot version corrupted in the arena. Recall still
  works; future reclaim might land in the wrong slot.
  P1.

The rule: **if the substrate is still serving but the
*next* incident could be data loss, it's P1.**

### Security severity

Security incidents are P1 by default and require
additional handling (notification chain, possibly
external comms). The runbooks here focus on
operational incidents; security incidents follow your
organisation's separate security-IR playbook.

Examples that *are* P1 security:

- An auth token leak.
- A TLS misconfiguration exposing a previously-
  internal port.
- An unexpected admin command from outside the
  expected source IPs.

### Capacity vs availability

A capacity problem (close to disk full, close to
connection cap) is **P2 by default** because it has a
defined fuse: when capacity runs out, it becomes P1.

The bar: if you're more than 6 hours from the fuse
blowing, P2. If less than 1 hour, upgrade to P1.

### Multi-region considerations

v1 is single-region. If you're running Brain across
multiple regions via external orchestration (not
native — Brain doesn't replicate), severity is
per-region. A P1 in one region while another is
healthy is still a P1 — your users in that region
are still affected.

---

## Severity ≠ urgency ≠ effort

These three are related but distinct:

- **Severity** is *how much user impact this has now*.
- **Urgency** is *how fast does this need to be
  addressed* (often a function of severity, but can
  differ — e.g. a P3 that has a 2-hour deadline before
  it becomes a P2).
- **Effort** is *how much work the fix is*.

A P3 incident can require a week of engineering effort
to fix properly (that's fine, it's still P3 because
user impact is small). A P1 can sometimes be fixed in
five minutes (still P1 because the impact while it was
happening was large).

Don't let "this is easy to fix" make you under-rate the
severity. Severity is about impact, not difficulty.

---

## How to argue about severity

Sometimes operators or stakeholders disagree about
severity. The process:

1. **State the disagreement explicitly.** "I think this
   is P2; you think P1. Let's calibrate."
2. **Find the trigger that anchors it.** "What's the
   user-visible symptom?" Map that to the table above.
3. **If still disagreeing, escalate.** Engineering or
   product leadership has the call.
4. **Document the decision** in the incident channel.

Don't argue forever. Pick a severity, declare it,
move on. Severity can change as you learn more.

---

## Related docs

- [IR-01 — How to use these runbooks](ir-01-how-to-use-runbooks.md)
- [IR-03 — Escalation policy](ir-03-escalation-policy.md)
- [IR-04 — Incident communication](ir-04-incident-communication.md)
- [Postmortem template](postmortem-template.md)
